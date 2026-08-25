// Port of src/picotorrent/bittorrent/session.{hpp,cpp} and
// torrenthandle.{hpp,cpp}.
//
// The original wrapped Rasterbar-libtorrent; this port wraps librqbit, a
// pure-Rust BitTorrent engine. The session reads its settings from the same
// SQLite configuration the C++ version used (libtorrent.* keys, the
// listen_interface table and rate limit settings) and persists torrent
// metadata (labels, added/completed timestamps, queue position) in the same
// `torrent` table. Fast-resume state is handled by librqbit's JSON session
// persistence instead of the torrent_resume_data table.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Local};
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, ManagedTorrent,
    Session as RqbitSession, SessionOptions, SessionPersistenceConfig, TorrentStatsState,
    generate_azereus_style,
};

use crate::core::configuration::{Configuration, ConnectionProxyType};
use crate::core::database::Database;
use crate::core::environment::Environment;

use super::torrentstatus::{State, TorrentStatus};

/// Parameters for adding a torrent - port of addparams.hpp.
#[derive(Clone, Default)]
pub struct AddParams {
    pub save_path: Option<String>,
    pub start_torrent: bool,
    pub only_files: Option<Vec<usize>>,
    pub label_id: Option<i32>,
}

/// Per-torrent metadata persisted in the `torrent` table.
#[derive(Clone)]
struct TorrentMeta {
    added_on: DateTime<Local>,
    completed_on: Option<DateTime<Local>>,
    label_id: Option<i32>,
    queue_position: i64,
    /// Session-local finished state from the previous tick (None = not yet
    /// observed this session). Used to fire the completion notification only on
    /// a real not-finished -> finished transition, never for torrents already
    /// complete when first seen (librqbit reports finished=true at startup from
    /// stale fastresume, which would otherwise pop a false toast and, worse,
    /// suppress the real one after a recheck+re-download).
    prev_finished: Option<bool>,
}

pub struct FileEntry {
    pub name: String,
    pub length: u64,
    pub included: bool,
    pub progress: f32,
}

/// Result of resolving a magnet's metadata (see `Session::resolve_magnet`).
pub enum MagnetOutcome {
    /// Reconstructed .torrent bytes, ready to feed the add-torrent dialog.
    Resolved(Vec<u8>),
    /// Resolution failed/timed out; carries the original magnet uri so the UI
    /// can fall back to adding it directly.
    Failed(String),
}

#[derive(PartialEq)]
pub enum TrackerRowKind {
    /// A peer-discovery source pseudo-row (DHT / LSD / PeX).
    Source,
    /// A "Tier #N" group header.
    Tier,
    /// A real tracker URL.
    Tracker,
}

pub struct TrackerRow {
    pub kind: TrackerRowKind,
    /// Text for the URL column (tracker rows are indented).
    pub label: String,
    pub status: String,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub fails: u32,
    pub next_announce: Option<std::time::SystemTime>,
}

impl TrackerRow {
    fn source(label: &str, status: String) -> Self {
        TrackerRow {
            kind: TrackerRowKind::Source,
            label: label.to_string(),
            status,
            seeders: None,
            leechers: None,
            fails: 0,
            next_announce: None,
        }
    }
    /// A tier heading row in the Trackers tab - a label with no statistics of
    /// its own, grouping the trackers announced together.
    fn tier(label: String) -> Self {
        TrackerRow {
            kind: TrackerRowKind::Tier,
            label,
            status: String::new(),
            seeders: None,
            leechers: None,
            fails: 0,
            next_announce: None,
        }
    }
}

pub struct PeerEntry {
    pub addr: String,
    pub state: String,
    pub fetched_bytes: u64,
    pub pieces: u32,
}

pub struct Session {
    rt: tokio::runtime::Runtime,
    // Behind RwLocks so preferences can be applied without restarting the
    // app: the whole librqbit session is torn down and rebuilt with the new
    // options, and its JSON persistence restores the torrents.
    inner: Arc<std::sync::RwLock<Arc<RqbitSession>>>,
    api: Arc<std::sync::RwLock<Api>>,
    db: Arc<Database>,
    meta: Arc<Mutex<HashMap<String, TorrentMeta>>>,
    errors: Arc<Mutex<Vec<String>>>,
    /// Info-hashes of torrents that just finished, for the UI to notify on.
    completed_events: Arc<Mutex<Vec<String>>>,
    ipfilter_active: Arc<std::sync::atomic::AtomicBool>,
    /// Torrents paused by the queue scheduler (as opposed to by the user).
    queue_paused: Arc<Mutex<std::collections::HashSet<String>>>,
    /// librqbit's JSON session folder (holds the per-torrent `.bitv`
    /// fastresume files), for force-recheck.
    session_path: std::path::PathBuf,
}

/// Translate the settings database into librqbit's `SessionOptions`.
///
/// The one place a setting becomes engine configuration. Settings with no
/// librqbit equivalent are read and dropped here rather than at the call site,
/// so what is and is not honoured is visible in one function.
fn build_session_options(cfg: &Configuration, env: &Environment) -> SessionOptions {
    // Peer ID: Azureus-style `-NT-` prefix, or a fully random id (no client
    // fingerprint) in anonymous mode.
    let anonymous = cfg.get_bool("libtorrent.anonymous_mode");
    let peer_id = if anonymous {
        let mut bytes = [0u8; 20];
        rand::fill(&mut bytes[..]);
        librqbit::Id20::new(bytes)
    } else {
        let mut version = env!("CARGO_PKG_VERSION")
            .split('.')
            .map(|p| p.parse::<u8>().unwrap_or(0));
        generate_azereus_style(
            *b"NT",
            (
                version.next().unwrap_or(0),
                version.next().unwrap_or(0),
                version.next().unwrap_or(0),
                0,
            ),
        )
    };

    // Listen port from the listen_interface table (default 6881).
    let listen_port = cfg
        .get_listen_interfaces()
        .first()
        .map(|i| i.port as u16)
        .unwrap_or(6881);

    // Rate limits are stored in KB/s like the original.
    let download_bps = if cfg.get_bool("libtorrent.enable_download_rate_limit") {
        cfg.get_int("libtorrent.download_rate_limit")
            .and_then(|kb| NonZeroU32::new((kb * 1024).max(0) as u32))
    } else {
        None
    };
    let upload_bps = if cfg.get_bool("libtorrent.enable_upload_rate_limit") {
        cfg.get_int("libtorrent.upload_rate_limit")
            .and_then(|kb| NonZeroU32::new((kb * 1024).max(0) as u32))
    } else {
        None
    };

    // SOCKS proxy support (librqbit supports SOCKS5).
    let proxy_type =
        ConnectionProxyType::from_i64(cfg.get_int("libtorrent.proxy_type").unwrap_or(0));
    let socks_proxy_url = match proxy_type {
        ConnectionProxyType::Socks5 | ConnectionProxyType::Socks4 => {
            let host = cfg.get_string("libtorrent.proxy_host").unwrap_or_default();
            let port = cfg.get_int("libtorrent.proxy_port").unwrap_or(0);
            if host.is_empty() || port == 0 {
                None
            } else {
                Some(format!("socks5://{host}:{port}"))
            }
        }
        ConnectionProxyType::Socks5Password => {
            let host = cfg.get_string("libtorrent.proxy_host").unwrap_or_default();
            let port = cfg.get_int("libtorrent.proxy_port").unwrap_or(0);
            let user = cfg
                .get_string("libtorrent.proxy_username")
                .unwrap_or_default();
            let pass = cfg
                .get_string("libtorrent.proxy_password")
                .unwrap_or_default();
            if host.is_empty() || port == 0 {
                None
            } else {
                Some(format!("socks5://{user}:{pass}@{host}:{port}"))
            }
        }
        _ => None,
    };

    // eMule/PeerGuardian IP filter (port of the ipfilter.* settings) -
    // librqbit loads the blocklist itself, from a file:// or http(s) URL.
    let blocklist_url = ipfilter_url(cfg);

    SessionOptions {
        blocklist_url,
        disable_dht: !cfg.get_bool("libtorrent.enable_dht"),
        disable_dht_persistence: false,
        dht_config: Some(librqbit::dht::PersistentDhtConfig {
            config_filename: Some(env.get_application_data_path().join("dht.json")),
            ..Default::default()
        }),
        fastresume: true,
        persistence: Some(SessionPersistenceConfig::Json {
            folder: Some(env.get_session_state_path()),
        }),
        peer_id: Some(peer_id),
        listen_port_range: Some(listen_port..listen_port + 1),
        enable_upnp_port_forwarding: true,
        socks_proxy_url,
        // Proxy scope (engine patch 0006), matching PicoTorrent's opt-in
        // proxy_peers / proxy_trackers / proxy_hostnames. Only meaningful when
        // a proxy is configured above; harmless otherwise.
        proxy_peers: cfg.get_bool("libtorrent.proxy_peers"),
        proxy_trackers: cfg.get_bool("libtorrent.proxy_trackers"),
        proxy_hostnames: cfg.get_bool("libtorrent.proxy_hostnames"),
        ratelimits: LimitsConfig {
            download_bps,
            upload_bps,
        },
        // PeX toggle (engine patch 0004). The prefs checkbox stores
        // libtorrent.enable_pex; disable_pex is its inverse.
        disable_pex: !cfg.get_bool("libtorrent.enable_pex"),
        // Anonymous mode (engine patch 0007): random peer id above + suppress
        // the client version in the extended handshake.
        anonymize: anonymous,
        // Require MSE/PE encryption on outgoing connections (engine patch 0003
        // seam + bittorrent::mse).
        stream_transform: if cfg.get_bool("libtorrent.require_outgoing_encryption") {
            Some(Arc::new(crate::bittorrent::mse::MseTransform))
        } else {
            None
        },
        // Accept incoming MSE/PE peers (engine patch 0005 accept-path seam).
        // Always installed: plaintext peers pass through untouched, so this
        // only ever *adds* the ability to talk to encrypted-only peers. When
        // libtorrent.require_incoming_encryption is set, plaintext is refused.
        incoming_transform: Some(Arc::new(crate::bittorrent::mse::IncomingMseTransform {
            require: cfg.get_bool("libtorrent.require_incoming_encryption"),
        })),
        ..Default::default()
    }
}

/// Clear the UDP port the DHT persisted in `dht.json`, keeping the routing
/// table and peer store. Port 0 makes librqbit bind an OS-assigned one.
fn reset_dht_port(path: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(obj) = json.as_object_mut() else {
        return;
    };
    obj.insert("addr".into(), "0.0.0.0:0".into());
    let _ = std::fs::write(path, json.to_string());
}

/// Move every file of a torrent from one folder to another, keeping the
/// relative structure. Uses rename, falling back to copy+delete for
/// cross-drive moves.
fn move_files(old_folder: &str, new_folder: &str, files: &[PathBuf]) -> std::io::Result<()> {
    for rel in files {
        let from = std::path::Path::new(old_folder).join(rel);
        if !from.exists() {
            continue;
        }
        let to = std::path::Path::new(new_folder).join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::rename(&from, &to).is_err() {
            std::fs::copy(&from, &to)?;
            std::fs::remove_file(&from)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QueueKind {
    Download,
    Seed,
}

/// Which torrents the queue scheduler should pause or resume, honouring all
/// three libtorrent limits at once: `active_limit` caps the *total* running
/// torrents, while `active_downloads` / `active_seeds` cap each kind. Any
/// limit of 0 or less means unlimited.
///
/// Candidates are `(hash, queue_position, kind, running)` where `running` is
/// true for torrents currently active and false for ones the scheduler paused
/// earlier (and may now resume). Returns `(to_pause, to_resume)`. Torrents the
/// USER paused are not passed in, so they are never touched.
fn decide_queue(
    active_limit: i64,
    active_downloads: i64,
    active_seeds: i64,
    mut candidates: Vec<(String, i64, QueueKind, bool)>,
) -> (Vec<String>, Vec<String>) {
    // Lowest queue position wins the available slots.
    candidates.sort_by_key(|(_, pos, _, _)| *pos);

    let cap = |n: i64| if n <= 0 { i64::MAX } else { n };
    let (total_cap, dl_cap, seed_cap) =
        (cap(active_limit), cap(active_downloads), cap(active_seeds));

    let (mut total, mut dl, mut seed) = (0i64, 0i64, 0i64);
    let mut pause = Vec::new();
    let mut resume = Vec::new();

    for (hash, _pos, kind, running) in candidates {
        let sub_ok = match kind {
            QueueKind::Download => dl < dl_cap,
            QueueKind::Seed => seed < seed_cap,
        };
        if total < total_cap && sub_ok {
            total += 1;
            match kind {
                QueueKind::Download => dl += 1,
                QueueKind::Seed => seed += 1,
            }
            if !running {
                resume.push(hash);
            }
        } else if running {
            pause.push(hash);
        }
    }
    (pause, resume)
}

/// file://-or-http URL for the configured IP filter, if enabled and present.
fn ipfilter_url(cfg: &Configuration) -> Option<String> {
    if !cfg.get_bool("ipfilter.enabled") {
        return None;
    }
    let path = cfg.get_string("ipfilter.file_path")?;
    if path.is_empty() {
        return None;
    }
    if path.starts_with("http://") || path.starts_with("https://") {
        return Some(path);
    }
    if !std::path::Path::new(&path).exists() {
        tracing::warn!("IP filter file does not exist: {path}");
        return None;
    }
    Some(format!("file:///{}", path.replace('\\', "/")))
}

/// Parameters collected by the create-torrent dialog.
pub struct CreateTorrentParams {
    pub source: PathBuf,
    pub trackers: Vec<String>,
    pub comment: String,
    pub private: bool,
    pub piece_length: Option<u32>,
    pub version: crate::bittorrent::torrent_create::TorrentVersion,
    pub output: PathBuf,
    pub add_to_session: bool,
}

/// Outcome of a background create-torrent run, polled by the UI tick.
pub enum CreateTorrentOutcome {
    Created {
        name: String,
        bytes: Vec<u8>,
        /// Folder containing the source data - used as the save path when
        /// adding the new torrent to the session so it seeds in place.
        save_path: Option<String>,
        add_to_session: bool,
    },
    Failed(String),
}

/// Hash a folder or file into a finished .torrent.
///
/// Async and off the UI thread: hashing is bounded by disk speed and a large
/// folder takes minutes.
async fn build_torrent(params: CreateTorrentParams) -> Result<CreateTorrentOutcome> {
    use crate::bittorrent::torrent_create::{self, TorrentVersion};

    let bytes = match params.version {
        // v1: librqbit builds the info dict; we clone it to attach trackers,
        // comment and the private flag, then re-serialize.
        TorrentVersion::V1 => {
            let created = librqbit::create_torrent(
                &params.source,
                librqbit::CreateTorrentOptions {
                    name: None,
                    piece_length: params.piece_length,
                },
            )
            .await?;

            let mut meta = created.as_info().clone();
            if let Some(first) = params.trackers.first() {
                meta.announce = Some(first.as_bytes().into());
            }
            meta.announce_list = params
                .trackers
                .iter()
                .map(|t| vec![t.as_bytes().into()])
                .collect();
            if !params.comment.is_empty() {
                meta.comment = Some(params.comment.as_bytes().into());
            }
            meta.created_by = Some(crate::buildinfo::user_agent().into_bytes().into());
            meta.info.private = params.private;

            let mut bytes = Vec::new();
            bencode::bencode_serialize_to_writer(&meta, &mut bytes)?;
            bytes
        }
        // v2 / hybrid: our own BEP 52 builder (librqbit is v1-only). Hashing is
        // blocking file I/O, so run it off the async worker.
        TorrentVersion::V2 | TorrentVersion::Hybrid => {
            let source = params.source.clone();
            let trackers = params.trackers.clone();
            let comment = params.comment.clone();
            let private = params.private;
            let piece_length = params.piece_length;
            let version = params.version;
            let created_by = crate::buildinfo::user_agent();
            tokio::task::spawn_blocking(move || {
                torrent_create::build(&torrent_create::CreateInput {
                    source: &source,
                    version,
                    piece_length,
                    trackers: &trackers,
                    comment: &comment,
                    private,
                    created_by,
                })
            })
            .await??
            .bytes
        }
    };

    tokio::fs::write(&params.output, &bytes).await?;

    let name = params
        .source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let save_path = params
        .source
        .parent()
        .map(|p| p.to_string_lossy().into_owned());

    Ok(CreateTorrentOutcome::Created {
        name,
        bytes,
        save_path,
        add_to_session: params.add_to_session,
    })
}

impl Session {
    /// Start the engine and restore whatever was running last time.
    ///
    /// Owns its own tokio runtime: every method here is synchronous with a
    /// `block_on` inside, so callers on the UI thread never see a future.
    pub fn new(env: &Environment, db: Arc<Database>, cfg: &Configuration) -> Result<Session> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("pt-session")
            .build()?;

        let default_save_path = cfg
            .get_string("default_save_path")
            .map(PathBuf::from)
            .unwrap_or_else(Environment::get_downloads_path);

        let opts = build_session_options(cfg, env);

        let inner = match rt.block_on(RqbitSession::new_with_opts(default_save_path.clone(), opts))
        {
            Ok(session) => session,
            Err(err) => {
                // The DHT remembers the exact UDP port it last bound and binds
                // it again verbatim. Windows hands out chunks of the ephemeral
                // range to Hyper-V/WSL on every boot, so a port that worked
                // yesterday can come back "forbidden by its access permissions"
                // and take startup down with it. Forget the port (the routing
                // table stays) and let the OS pick a free one.
                tracing::warn!("session startup failed ({err:#}) - retrying on a fresh DHT port");
                reset_dht_port(&env.get_application_data_path().join("dht.json"));
                rt.block_on(RqbitSession::new_with_opts(
                    default_save_path,
                    build_session_options(cfg, env),
                ))?
            }
        };
        let api = Api::new(inner.clone(), None);

        let session = Session {
            rt,
            inner: Arc::new(std::sync::RwLock::new(inner)),
            api: Arc::new(std::sync::RwLock::new(api)),
            db,
            meta: Arc::new(Mutex::new(HashMap::new())),
            errors: Arc::new(Mutex::new(Vec::new())),
            completed_events: Arc::new(Mutex::new(Vec::new())),
            ipfilter_active: Arc::new(std::sync::atomic::AtomicBool::new(
                ipfilter_url(cfg).is_some(),
            )),
            queue_paused: Arc::new(Mutex::new(std::collections::HashSet::new())),
            session_path: env.get_session_state_path(),
        };

        session.load_torrent_meta();
        // Heal any duplicate/gapped queue positions persisted before positions
        // were compacted on removal.
        session.normalize_queue_positions();

        // Resume the torrents that were running when the previous session
        // shut down (librqbit persists its shutdown pause).
        let running: Vec<String> = {
            use rusqlite::OptionalExtension;
            session
                .db
                .with(|conn| {
                    conn.query_row(
                        "select value from persistent_object where key = 'session.running'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                })
                .ok()
                .flatten()
                .map(|v| {
                    v.split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        if !running.is_empty() {
            session
                .rt
                .spawn(resume_after_restore(session.rq(), running));
        }

        Ok(session)
    }

    /// The current librqbit session.
    fn rq(&self) -> Arc<RqbitSession> {
        self.inner.read().unwrap().clone()
    }

    /// The current API facade.
    fn rq_api(&self) -> Api {
        self.api.read().unwrap().clone()
    }

    /// Tear the librqbit session down and rebuild it with options derived
    /// from the (changed) configuration - "apply preferences without
    /// restart". Torrent state comes back through the JSON persistence.
    pub fn apply_settings(&self, env: &Environment, cfg: &Configuration) {
        let opts = build_session_options(cfg, env);
        let default_save_path = cfg
            .get_string("default_save_path")
            .map(PathBuf::from)
            .unwrap_or_else(Environment::get_downloads_path);

        self.ipfilter_active.store(
            ipfilter_url(cfg).is_some(),
            std::sync::atomic::Ordering::Relaxed,
        );

        let inner_slot = self.inner.clone();
        let api_slot = self.api.clone();
        let errors = self.errors.clone();

        self.rt.spawn(async move {
            let old = inner_slot.read().unwrap().clone();
            // librqbit's stop() pauses everything and persists the pause -
            // remember what was running so the new session resumes it.
            let running = running_hashes(&old);
            old.stop().await;

            match RqbitSession::new_with_opts(default_save_path, opts).await {
                Ok(new_session) => {
                    let new_api = Api::new(new_session.clone(), None);
                    *inner_slot.write().unwrap() = new_session.clone();
                    *api_slot.write().unwrap() = new_api;
                    tracing::info!("session rebuilt with new settings");
                    resume_after_restore(new_session, running).await;
                }
                Err(err) => {
                    let msg = format!("Failed to apply settings: {err:#}");
                    tracing::error!("{msg}");
                    errors.lock().unwrap().push(msg);
                }
            }
        });
    }

    /// Load per-torrent metadata from the `torrent` table for torrents
    /// restored by librqbit's session persistence.
    fn load_torrent_meta(&self) {
        /// info_hash, queue position, label, added on, completed on - one row
        /// of the `torrent` table, named because the tuple is unreadable.
        type MetaRow = (String, i64, Option<i32>, Option<i64>, Option<i64>);

        let rows: Vec<MetaRow> = self
            .db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "select info_hash, queue_position, label_id, added_on, completed_on \
                     from torrent",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?;
                rows.collect()
            })
            .unwrap_or_default();

        let mut meta = self.meta.lock().unwrap();
        for (hash, queue_position, label_id, added_on, completed_on) in rows {
            meta.insert(
                hash,
                TorrentMeta {
                    added_on: added_on
                        .and_then(|ts| DateTime::from_timestamp(ts, 0))
                        .map(|dt| dt.with_timezone(&Local))
                        .unwrap_or_else(Local::now),
                    completed_on: completed_on
                        .and_then(|ts| DateTime::from_timestamp(ts, 0))
                        .map(|dt| dt.with_timezone(&Local)),
                    label_id,
                    queue_position,
                    prev_finished: None,
                },
            );
        }
    }

    /// A handle to the session runtime, for callers that need to drive their
    /// own future on it rather than through one of the methods here.
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// Whether a blocklist was configured for this session (the status bar
    /// indicator).
    pub fn ipfilter_active(&self) -> bool {
        self.ipfilter_active
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Force recheck (port of the libtorrent force_recheck): librqbit has no
    /// direct API, so forget the torrent (keep files) and re-add it from its
    /// own metadata bytes - the fresh add hash-checks the existing data.
    pub fn recheck(&self, hash: &str) {
        let Some((bytes, output_folder, paused)) = self.torrent_readd_info(hash) else {
            return;
        };
        let Some(handle) = self.find(hash) else {
            return;
        };

        let rq = self.rq();
        let errors = self.errors.clone();
        let id = librqbit::api::TorrentIdOrHash::Id(handle.id());
        // librqbit's fastresume file. After deleting the torrent we remove it
        // ourselves so the re-add does a FULL re-hash: librqbit only spot-checks
        // the saved bitfield, and on Windows it can fail to delete this file
        // while it's still memory-mapped - so a stale "complete" bitfield would
        // otherwise survive and the torrent would wrongly seed missing data.
        let bitv = self.session_path.join(format!("{hash}.bitv"));

        self.rt.spawn(async move {
            if let Err(err) = rq.delete(id, false).await {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("Failed to recheck torrent: {err:#}"));
                return;
            }

            // delete().await has returned, so librqbit has dropped the mmap;
            // the file can now be removed even on Windows.
            let _ = tokio::fs::remove_file(&bitv).await;

            let opts = AddTorrentOptions {
                paused,
                output_folder: Some(output_folder),
                overwrite: true,
                ..Default::default()
            };
            if let Err(err) = rq
                .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
                .await
            {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("Failed to re-add torrent for recheck: {err:#}"));
            }
        });
    }

    /// Move storage (port of libtorrent move_storage): forget the torrent
    /// (keep files), move the data, re-add pointing at the new folder.
    pub fn move_storage(&self, hash: &str, new_folder: &str) {
        let Some((bytes, old_folder, paused)) = self.torrent_readd_info(hash) else {
            return;
        };
        let Some(handle) = self.find(hash) else {
            return;
        };

        // Relative paths of all files in the torrent.
        let files: Vec<PathBuf> = handle
            .metadata
            .load_full()
            .map(|m| {
                m.file_infos
                    .iter()
                    .map(|fi| fi.relative_filename.clone())
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            return;
        }

        let rq = self.rq();
        let errors = self.errors.clone();
        let id = librqbit::api::TorrentIdOrHash::Id(handle.id());
        let new_folder = new_folder.to_string();

        self.rt.spawn(async move {
            if let Err(err) = rq.delete(id, false).await {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("Failed to move torrent: {err:#}"));
                return;
            }

            if let Err(err) = move_files(&old_folder, &new_folder, &files) {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("Failed to move torrent data: {err:#}"));
                // Fall through and re-add at the OLD location so the torrent
                // is not lost.
            }

            let target = if std::path::Path::new(&new_folder).join(&files[0]).exists() {
                new_folder
            } else {
                old_folder
            };

            let opts = AddTorrentOptions {
                paused,
                output_folder: Some(target),
                overwrite: true,
                ..Default::default()
            };
            if let Err(err) = rq
                .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
                .await
            {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("Failed to re-add moved torrent: {err:#}"));
            }
        });
    }

    /// Enforce the active-downloads / active-seeds limits (port of
    /// libtorrent's queueing): the lowest queue positions run, the rest are
    /// paused by the scheduler. Torrents paused by the USER are left alone.
    pub fn enforce_queue(
        &self,
        rows: &[TorrentStatus],
        active_limit: i64,
        active_downloads: i64,
        active_seeds: i64,
    ) {
        let mut candidates = Vec::new();

        {
            let queue_paused = self.queue_paused.lock().unwrap();
            for row in rows {
                use QueueKind::*;
                let entry =
                    |kind, running| (row.info_hash.clone(), row.queue_position, kind, running);
                match row.state {
                    State::Downloading | State::DownloadingMetadata => {
                        candidates.push(entry(Download, true))
                    }
                    State::DownloadingPaused if queue_paused.contains(&row.info_hash) => {
                        candidates.push(entry(Download, false))
                    }
                    State::Uploading => candidates.push(entry(Seed, true)),
                    State::UploadingPaused if queue_paused.contains(&row.info_hash) => {
                        candidates.push(entry(Seed, false))
                    }
                    _ => {}
                }
            }
        }

        let (pause, resume) =
            decide_queue(active_limit, active_downloads, active_seeds, candidates);

        {
            let mut queue_paused = self.queue_paused.lock().unwrap();
            for hash in &pause {
                queue_paused.insert(hash.clone());
            }
            for hash in &resume {
                queue_paused.remove(hash);
            }
        }

        for hash in pause {
            self.pause(&hash);
        }
        for hash in resume {
            self.resume(&hash);
        }
    }

    /// Manual resume also clears the scheduler's claim on the torrent, so
    /// it is treated as user-started until the limits say otherwise.
    pub fn clear_queue_pause(&self, hash: &str) {
        self.queue_paused.lock().unwrap().remove(hash);
    }

    /// Move a torrent one step up or down in the queue.
    pub fn queue_move(&self, hash: &str, up: bool) {
        let updates: Vec<(String, i64)> = {
            let mut meta = self.meta.lock().unwrap();

            let mut order: Vec<String> = meta.keys().cloned().collect();
            order.sort_by_key(|h| meta[h].queue_position);

            let Some(idx) = order.iter().position(|h| h == hash) else {
                return;
            };
            let other = if up {
                match idx.checked_sub(1) {
                    Some(i) => i,
                    None => return,
                }
            } else if idx + 1 < order.len() {
                idx + 1
            } else {
                return;
            };

            order.swap(idx, other);

            // Normalize to contiguous positions (removals leave gaps).
            for (i, h) in order.iter().enumerate() {
                if let Some(m) = meta.get_mut(h) {
                    m.queue_position = i as i64;
                }
            }

            order
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), i as i64))
                .collect()
        };

        let _ = self.db.with(|conn| {
            for (hash, pos) in &updates {
                conn.execute(
                    "update torrent set queue_position = ?1 where info_hash = ?2",
                    rusqlite::params![pos, hash],
                )?;
            }
            Ok(())
        });
    }

    /// Piece bitfield for the piece progress bar: (bitfield bytes - one bit
    /// per piece, MSB first - and the total piece count). Uses the
    /// with_chunk_tracker visibility patch (see vendor/librqbit/PATCHES.md).
    pub fn piece_map(&self, hash: &str) -> Option<(Vec<u8>, usize)> {
        let handle = self.find(hash)?;
        let total = handle.metadata.load_full()?.lengths.total_pieces() as usize;
        if total == 0 {
            return None;
        }
        let bytes = handle
            .with_chunk_tracker(|ct| ct.get_have_pieces().as_bytes().to_vec())
            .ok()?;
        Some((bytes, total))
    }

    /// Everything needed to forget + re-add a torrent: its metadata bytes,
    /// output folder and paused state.
    fn torrent_readd_info(&self, hash: &str) -> Option<(Vec<u8>, String, bool)> {
        let handle = self.find(hash)?;
        let metadata = handle.metadata.load_full()?;
        let bytes = metadata.torrent_bytes.to_vec();

        let details = self
            .rq_api()
            .api_torrent_details(librqbit::api::TorrentIdOrHash::Id(handle.id()))
            .ok()?;
        let paused = matches!(handle.stats().state, TorrentStatsState::Paused);

        Some((bytes, details.output_folder, paused))
    }

    /// Build a .torrent file in the background (hashing can take a while);
    /// the result lands in `slot`, which the UI polls on its tick.
    pub fn create_torrent(
        &self,
        params: CreateTorrentParams,
        slot: Arc<Mutex<Option<CreateTorrentOutcome>>>,
    ) {
        self.rt.spawn(async move {
            let outcome = match build_torrent(params).await {
                Ok(outcome) => outcome,
                Err(err) => CreateTorrentOutcome::Failed(format!("{err:#}")),
            };
            *slot.lock().unwrap() = Some(outcome);
        });
    }

    /// The TCP port peers are accepted on, once one has been bound.
    pub fn listen_port(&self) -> Option<u16> {
        self.rq().tcp_listen_port()
    }

    /// Drain the errors collected since the last call.
    ///
    /// Drained rather than read: these are polled by the refresh tick to raise
    /// a toast, and leaving them in place would raise the same one every tick.
    pub fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut self.errors.lock().unwrap())
    }

    /// Record an error from background work, where there is no caller to
    /// return it to. Logged as well as queued, so it survives being missed.
    fn push_error(&self, err: String) {
        tracing::error!("{err}");
        self.errors.lock().unwrap().push(err);
    }

    /// Add a torrent from a .torrent file's contents or a magnet link.
    ///
    /// Runs on the session runtime: for magnet links librqbit resolves the
    /// metadata before returning, which can take a long time (or forever for
    /// dead magnets), so this must never block the UI thread.
    pub fn add_torrent(&self, source: AddTorrentSource, params: AddParams) {
        let add = match &source {
            AddTorrentSource::TorrentFileBytes(bytes) => AddTorrent::from_bytes(bytes.clone()),
            AddTorrentSource::MagnetUri(uri) => AddTorrent::from_url(uri.clone()),
        };

        let opts = AddTorrentOptions {
            paused: !params.start_torrent,
            output_folder: params.save_path.clone(),
            only_files: params.only_files.clone(),
            overwrite: true,
            ..Default::default()
        };

        let inner = self.rq();
        let db = self.db.clone();
        let meta = self.meta.clone();
        let errors = self.errors.clone();

        self.rt.spawn(async move {
            match inner.add_torrent(add, Some(opts)).await {
                Ok(AddTorrentResponse::Added(_, handle))
                | Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => {
                    Self::on_torrent_added(&db, &meta, &handle, &source, &params);
                }
                Ok(AddTorrentResponse::ListOnly(_)) => {}
                Err(err) => {
                    let msg = format!("Failed to add torrent: {err:#}");
                    tracing::error!("{msg}");
                    errors.lock().unwrap().push(msg);
                }
            }
        });
    }

    /// Resolve a magnet's metadata (file list, sizes, name) WITHOUT adding it,
    /// so the UI can show the same add dialog as for a .torrent file. Uses
    /// librqbit's `list_only` mode, which fetches the info dict over DHT/peers
    /// and returns a reconstructed .torrent, bounded by a timeout. The outcome
    /// (bytes, or the original uri on failure) is pushed to `slot` for the UI
    /// to pick up on its next tick.
    pub fn resolve_magnet(&self, uri: String, slot: Arc<Mutex<Vec<MagnetOutcome>>>) {
        let inner = self.rq();
        self.rt.spawn(async move {
            let opts = AddTorrentOptions {
                list_only: true,
                ..Default::default()
            };
            let outcome = match tokio::time::timeout(
                std::time::Duration::from_secs(90),
                inner.add_torrent(AddTorrent::from_url(uri.clone()), Some(opts)),
            )
            .await
            {
                Ok(Ok(AddTorrentResponse::ListOnly(r))) => {
                    MagnetOutcome::Resolved(r.torrent_bytes.to_vec())
                }
                Ok(Err(e)) => {
                    tracing::warn!("magnet metadata resolve failed: {e:#}");
                    MagnetOutcome::Failed(uri)
                }
                Ok(Ok(_)) => MagnetOutcome::Failed(uri),
                Err(_) => {
                    tracing::warn!("magnet metadata resolve timed out");
                    MagnetOutcome::Failed(uri)
                }
            };
            slot.lock().unwrap().push(outcome);
        });
    }

    /// One-shot import of every torrent from a PicoTorrent database. For each
    /// torrent not already present, reconstructs a `.torrent` (or magnet) plus
    /// its save path and adds it; librqbit rechecks the on-disk files to
    /// recover progress. Returns `(imported, skipped_already_present)`.
    pub fn import_from_picotorrent(&self, pico_db: &std::path::Path) -> Result<(usize, usize)> {
        use crate::core::pico_import::{ImportSource, read_torrents};

        let entries = read_torrents(pico_db)?;
        let existing: std::collections::HashSet<String> =
            self.meta.lock().unwrap().keys().cloned().collect();

        let mut imported = 0;
        let mut skipped = 0;
        for entry in entries {
            if existing.contains(&entry.info_hash) {
                skipped += 1;
                continue;
            }
            let source = match entry.source {
                ImportSource::TorrentBytes(bytes) => AddTorrentSource::TorrentFileBytes(bytes),
                ImportSource::Magnet(uri) => AddTorrentSource::MagnetUri(uri),
            };
            self.add_torrent(
                source,
                AddParams {
                    save_path: entry.save_path,
                    start_torrent: true,
                    only_files: None,
                    label_id: entry.label_id,
                },
            );
            imported += 1;
        }
        Ok((imported, skipped))
    }

    /// Record a newly added torrent in the database.
///
/// librqbit persists its own session state; this is the app's half - label,
/// save path, added timestamp and the original source, none of which the
/// engine knows or keeps.
fn on_torrent_added(
        db: &Arc<Database>,
        meta: &Arc<Mutex<HashMap<String, TorrentMeta>>>,
        handle: &Arc<ManagedTorrent>,
        source: &AddTorrentSource,
        params: &AddParams,
    ) {
        let hash = handle.info_hash().as_string();
        let now = Local::now();

        {
            let mut meta = meta.lock().unwrap();
            let queue_position = meta.len() as i64;
            meta.entry(hash.clone()).or_insert(TorrentMeta {
                added_on: now,
                completed_on: None,
                label_id: params.label_id,
                queue_position,
                prev_finished: None,
            });
        }

        let _ = db.with(|conn| {
            conn.execute(
                "insert into torrent (info_hash, queue_position, label_id, added_on) \
                 values (?1, (select count(*) from torrent), ?2, ?3) \
                 on conflict (info_hash) do nothing",
                rusqlite::params![hash, params.label_id, now.timestamp()],
            )
        });

        // Port of the torrent_magnet_uri table behaviour.
        if let AddTorrentSource::MagnetUri(uri) = source {
            let save_path = params.save_path.clone().unwrap_or_default();
            let _ = db.with(|conn| {
                conn.execute(
                    "insert or replace into torrent_magnet_uri (info_hash, magnet_uri, save_path) \
                     values (?1, ?2, ?3)",
                    rusqlite::params![hash, uri, save_path],
                )
            });
        }
    }

    /// Pause one torrent. Unknown hashes are ignored: the list and the engine
    /// are refreshed on a tick, so a stale row can outlive its torrent.
    pub fn pause(&self, hash: &str) {
        if let Some(handle) = self.find(hash)
            && let Err(err) = self.rt.block_on(self.rq().pause(&handle))
        {
            self.push_error(format!("Failed to pause torrent: {err:#}"));
        }
    }

    /// Resume one torrent, ignoring an unknown hash.
    pub fn resume(&self, hash: &str) {
        if let Some(handle) = self.find(hash)
            && let Err(err) = self.rt.block_on(self.rq().unpause(&handle))
        {
            self.push_error(format!("Failed to resume torrent: {err:#}"));
        }
    }

    /// Remove a torrent, and its downloaded data when `delete_files`.
    ///
    /// Also clears the app's own row for it - otherwise the next start would
    /// restore a torrent the engine no longer has.
    pub fn remove(&self, hash: &str, delete_files: bool) {
        if let Some(handle) = self.find(hash) {
            let id = librqbit::api::TorrentIdOrHash::Id(handle.id());
            if let Err(err) = self.rt.block_on(self.rq().delete(id, delete_files)) {
                self.push_error(format!("Failed to remove torrent: {err:#}"));
                return;
            }
        }

        self.meta.lock().unwrap().remove(hash);

        let _ = self.db.with(|conn| {
            conn.execute(
                "delete from torrent_magnet_uri where info_hash = ?1",
                [hash],
            )?;
            conn.execute("delete from torrent where info_hash = ?1", [hash])
        });

        // Close the gap the removal left, otherwise the next added torrent
        // (positioned at meta.len()) collides with an existing position and two
        // rows show the same "#".
        self.normalize_queue_positions();
    }

    /// Compact queue positions to a contiguous 0..N-1 (sorted by current
    /// position), keeping every "#" unique with no gaps.
    fn normalize_queue_positions(&self) {
        let updates: Vec<(String, i64)> = {
            let mut meta = self.meta.lock().unwrap();
            let mut order: Vec<String> = meta.keys().cloned().collect();
            order.sort_by_key(|h| meta[h].queue_position);
            for (i, h) in order.iter().enumerate() {
                if let Some(m) = meta.get_mut(h) {
                    m.queue_position = i as i64;
                }
            }
            order
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), i as i64))
                .collect()
        };

        let _ = self.db.with(|conn| {
            for (hash, pos) in &updates {
                conn.execute(
                    "update torrent set queue_position = ?1 where info_hash = ?2",
                    rusqlite::params![pos, hash],
                )?;
            }
            Ok::<_, rusqlite::Error>(())
        });
    }

    /// Assign a label, or clear it with `None`. Stored by this app; librqbit
    /// has no concept of labels.
    pub fn set_label(&self, hash: &str, label_id: Option<i32>) {
        if let Some(meta) = self.meta.lock().unwrap().get_mut(hash) {
            meta.label_id = label_id;
        }

        let _ = self.db.with(|conn| {
            conn.execute(
                "update torrent set label_id = ?1 where info_hash = ?2",
                rusqlite::params![label_id, hash],
            )
        });
    }

    /// Change which files of a torrent are wanted, from the Files tab.
    ///
    /// Indices are into the torrent's own file list. Deselecting everything is
    /// refused by the engine, which is why the tab keeps at least one ticked.
    pub fn update_only_files(&self, hash: &str, only_files: Vec<usize>) {
        // An empty selection would make the torrent 0 bytes "wanted" (and it
        // persists that way) - always keep at least one file included.
        if only_files.is_empty() {
            self.push_error(String::from("At least one file must be included."));
            return;
        }
        if let Some(handle) = self.find(hash)
            && let Err(err) = self.rt.block_on(
                self.rq()
                    .update_only_files(&handle, &only_files.into_iter().collect()),
            )
        {
            self.push_error(format!("Failed to update file selection: {err:#}"));
        }
    }

    /// Whether a torrent with this info hash is in the session.
    ///
    /// The mutating methods below silently do nothing for an unknown hash,
    /// which is right for the UI (it can only ever pass hashes it just listed)
    /// but wrong for the web API, where a typo would otherwise look like a
    /// success. Cheaper than scanning `torrents()` just to find out.
    pub fn exists(&self, hash: &str) -> bool {
        self.find(hash).is_some()
    }

    /// Look up a live torrent by info hash. `None` for an unparseable hash as
    /// well as an unknown one - both mean "nothing to act on" to every caller.
    fn find(&self, hash: &str) -> Option<Arc<ManagedTorrent>> {
        let id = librqbit::api::TorrentIdOrHash::parse(hash).ok()?;
        self.rq().get(id)
    }

    /// Session-wide transfer rates for the status bar.
    pub fn session_rates(&self) -> (i64, i64) {
        let stats = self.rq().stats_snapshot();
        (
            (stats.download_speed.mbps * 1024.0 * 1024.0) as i64,
            (stats.upload_speed.mbps * 1024.0 * 1024.0) as i64,
        )
    }

    /// DHT node count for the status bar; None when DHT is disabled.
    pub fn dht_nodes(&self) -> Option<i64> {
        self.rq()
            .get_dht()
            .map(|dht| dht.stats().routing_table_size as i64)
    }

    /// Build status snapshots for every torrent in the session.
    pub fn torrents(&self, labels: &HashMap<i32, String>) -> Vec<TorrentStatus> {
        let handles: Vec<Arc<ManagedTorrent>> = self
            .rq()
            .with_torrents(|torrents| torrents.map(|(_, h)| h.clone()).collect());

        let mut result = Vec::with_capacity(handles.len());
        let mut completed_now: Vec<String> = Vec::new();

        {
            let mut meta_map = self.meta.lock().unwrap();

            for handle in &handles {
                let hash = handle.info_hash().as_string();
                let stats = handle.stats();

                let queue_position = meta_map.len() as i64;
                let meta = meta_map.entry(hash.clone()).or_insert_with(|| TorrentMeta {
                    added_on: Local::now(),
                    completed_on: None,
                    label_id: None,
                    queue_position,
                    prev_finished: None,
                });

                // Notify only on a genuine not-finished -> finished transition
                // this session; prev_finished == None (first observation) means
                // the torrent was already complete at startup, so no toast.
                if meta.prev_finished == Some(false) && stats.finished {
                    let name = handle.name().unwrap_or_else(|| hash.clone());
                    tracing::info!("torrent finished: {name}");
                    self.completed_events.lock().unwrap().push(name);
                }
                meta.prev_finished = Some(stats.finished);

                // Record the completion timestamp for the "Completed On" column.
                if stats.finished && meta.completed_on.is_none() {
                    meta.completed_on = Some(Local::now());
                    completed_now.push(hash.clone());
                }

                let has_metadata = handle.metadata.load().is_some();
                let paused = matches!(stats.state, TorrentStatsState::Paused);

                let state = match stats.state {
                    TorrentStatsState::Initializing => State::CheckingFiles,
                    TorrentStatsState::Error => State::Error,
                    TorrentStatsState::Paused => {
                        if stats.finished {
                            State::UploadingPaused
                        } else {
                            State::DownloadingPaused
                        }
                    }
                    TorrentStatsState::Live => {
                        if !has_metadata {
                            State::DownloadingMetadata
                        } else if stats.finished {
                            State::Uploading
                        } else {
                            State::Downloading
                        }
                    }
                };

                let (down_rate, up_rate, peers_current, peers_total, eta) = stats
                    .live
                    .as_ref()
                    .map(|live| {
                        let down = live.download_speed.mbps * 1024.0 * 1024.0;
                        let up = live.upload_speed.mbps * 1024.0 * 1024.0;
                        let remaining = stats.total_bytes.saturating_sub(stats.progress_bytes);
                        let eta = if down > 1.0 && remaining > 0 {
                            Some(std::time::Duration::from_secs_f64(remaining as f64 / down))
                        } else {
                            None
                        };
                        (
                            down as i64,
                            up as i64,
                            live.snapshot.peer_stats.live as i64,
                            live.snapshot.peer_stats.seen as i64,
                            eta,
                        )
                    })
                    .unwrap_or((0, 0, 0, 0, None));

                let progress = if stats.total_bytes > 0 {
                    stats.progress_bytes as f32 / stats.total_bytes as f32
                } else {
                    0.0
                };

                let ratio = if stats.progress_bytes > 0 {
                    stats.uploaded_bytes as f32 / stats.progress_bytes as f32
                } else {
                    0.0
                };

                let output_folder = self
                    .rq_api()
                    .api_torrent_details(librqbit::api::TorrentIdOrHash::Id(handle.id()))
                    .map(|d| d.output_folder)
                    .unwrap_or_default();

                // Real seed counts + availability from the per-peer bitfields
                // (per_peer_have_pieces visibility patch). Availability is the
                // "distributed copies" approximation: the sum of every
                // connected peer's completion fraction.
                let total_pieces = handle
                    .metadata
                    .load_full()
                    .map(|m| m.lengths.total_pieces() as u64)
                    .unwrap_or(0);
                let (seeds_current, availability) = match (handle.live(), total_pieces) {
                    (Some(live), total) if total > 0 => {
                        let peers = live.per_peer_have_pieces();
                        let seeds = peers.iter().filter(|(_, have)| *have >= total).count();
                        let avail: f32 = peers
                            .iter()
                            .map(|(_, have)| *have as f32 / total as f32)
                            .sum();
                        (seeds as i64, avail)
                    }
                    _ => (0, -1.0),
                };
                // The engine's live count includes seeds - the Peers column
                // shows the non-seed peers, like the original.
                let peers_current = (peers_current - seeds_current).max(0);

                result.push(TorrentStatus {
                    added_on: meta.added_on,
                    all_time_download: stats.progress_bytes as i64,
                    all_time_upload: stats.uploaded_bytes as i64,
                    availability,
                    completed_on: meta.completed_on,
                    download_payload_rate: down_rate,
                    error: stats.error.clone().unwrap_or_default(),
                    eta,
                    info_hash: hash.clone(),
                    label_id: meta.label_id,
                    label_name: meta
                        .label_id
                        .and_then(|id| labels.get(&id).cloned())
                        .unwrap_or_default(),
                    name: handle.name().unwrap_or_else(|| hash.clone()),
                    paused,
                    peers_current,
                    peers_total,
                    progress,
                    queue_position: meta.queue_position,
                    ratio,
                    save_path: output_folder,
                    seeds_current,
                    // Swarm-wide totals need a tracker scrape, which librqbit
                    // doesn't do - show the connected count.
                    seeds_total: seeds_current,
                    state,
                    total_wanted: stats.total_bytes as i64,
                    total_wanted_remaining: stats.total_bytes.saturating_sub(stats.progress_bytes)
                        as i64,
                    upload_payload_rate: up_rate,
                });
            }
        }

        for hash in completed_now {
            let _ = self.db.with(|conn| {
                conn.execute(
                    "update torrent set completed_on = ?1 where info_hash = ?2",
                    rusqlite::params![Local::now().timestamp(), hash],
                )
            });
        }

        result
    }

    /// Drain the info-hashes of torrents that finished since the last call.
    pub fn take_completions(&self) -> Vec<String> {
        std::mem::take(&mut self.completed_events.lock().unwrap())
    }

    /// File list for the Files tab.
    pub fn files(&self, hash: &str) -> Vec<FileEntry> {
        let Some(handle) = self.find(hash) else {
            return Vec::new();
        };

        let stats = handle.stats();
        let only_files = handle.only_files();

        let Some(metadata) = handle.metadata.load_full() else {
            return Vec::new();
        };

        metadata
            .file_infos
            .iter()
            .enumerate()
            .map(|(idx, fi)| {
                let progress_bytes = stats.file_progress.get(idx).copied().unwrap_or(0);
                FileEntry {
                    name: fi.relative_filename.to_string_lossy().into_owned(),
                    length: fi.len,
                    included: only_files
                        .as_ref()
                        .map(|of| of.contains(&idx))
                        .unwrap_or(true),
                    progress: if fi.len > 0 {
                        progress_bytes as f32 / fi.len as f32
                    } else {
                        1.0
                    },
                }
            })
            .collect()
    }

    /// Peer list for the Peers tab.
    pub fn peers(&self, hash: &str) -> Vec<PeerEntry> {
        let Ok(id) = librqbit::api::TorrentIdOrHash::parse(hash) else {
            return Vec::new();
        };

        let Ok(snapshot) = self.rq_api().api_peer_stats(id, Default::default()) else {
            return Vec::new();
        };

        let mut peers: Vec<PeerEntry> = snapshot
            .peers
            .into_iter()
            .map(|(addr, stats)| PeerEntry {
                addr,
                state: stats.state.to_string(),
                fetched_bytes: stats.counters.fetched_bytes,
                pieces: stats.counters.downloaded_and_checked_pieces,
            })
            .collect();

        peers.sort_by(|a, b| a.addr.cmp(&b.addr));
        peers
    }

    /// Rows for the Trackers tab: the DHT/LSD/PeX peer-discovery sources, then
    /// the torrent's trackers grouped into announce tiers, each joined with the
    /// latest announce stats (seeds/leeches/next-announce/fails) that the
    /// vendored tracker-comms records (see PATCHES.md). Paused torrents report
    /// "Paused" and drop their (now stale) live counts.
    pub fn tracker_rows(
        &self,
        hash: &str,
        tr: &crate::ui::translator::Translator,
    ) -> Vec<TrackerRow> {
        let Some(handle) = self.find(hash) else {
            return Vec::new();
        };
        let paused = matches!(handle.stats().state, TorrentStatsState::Paused);
        let info_hash = handle.info_hash();
        let rq = self.rq();

        let mut rows = Vec::new();

        // Peer-discovery sources. librqbit can't attribute per-torrent peer
        // counts to a source and has no LSD, so these are status-only.
        let dht_status = if paused {
            tr.i18n("tracker_paused")
        } else {
            match rq.get_dht() {
                Some(dht) => tr.i18n1(
                    "tracker_dht_working",
                    &dht.stats().routing_table_size.to_string(),
                ),
                None => tr.i18n("tracker_disabled"),
            }
        };
        rows.push(TrackerRow::source("DHT", dht_status));
        rows.push(TrackerRow::source("LSD", tr.i18n("tracker_not_supported")));
        let pex_on = crate::core::configuration::Configuration::new(self.db.clone())
            .get_bool("libtorrent.enable_pex");
        let pex_status = if paused {
            tr.i18n("tracker_paused")
        } else if pex_on {
            tr.i18n("tracker_enabled")
        } else {
            tr.i18n("tracker_disabled")
        };
        rows.push(TrackerRow::source("PeX", pex_status));

        let stats: std::collections::HashMap<String, librqbit::TrackerStat> =
            rq.tracker_stats_snapshot(info_hash).into_iter().collect();

        // Announce tiers; fall back to a single tier of all trackers (magnets
        // have no announce-list, so no tier structure is known).
        let mut tiers = rq.tracker_tiers_snapshot(info_hash);
        if tiers.is_empty() {
            let mut flat: Vec<String> = handle
                .shared()
                .trackers
                .iter()
                .map(|u| u.to_string())
                .collect();
            flat.sort();
            if !flat.is_empty() {
                tiers.push(flat);
            }
        }

        for (i, tier) in tiers.iter().enumerate() {
            rows.push(TrackerRow::tier(format!("Tier #{i}")));
            for url in tier {
                let s = stats.get(url);
                let row = if paused {
                    TrackerRow {
                        kind: TrackerRowKind::Tracker,
                        label: format!("    {url}"),
                        status: tr.i18n("tracker_paused"),
                        seeders: None,
                        leechers: None,
                        fails: s.map(|s| s.fails).unwrap_or(0),
                        next_announce: None,
                    }
                } else {
                    TrackerRow {
                        kind: TrackerRowKind::Tracker,
                        label: format!("    {url}"),
                        // Engine emits the literal "Working" (translate it);
                        // error strings pass through as-is.
                        status: s
                            .map(|s| s.status.clone())
                            .filter(|s| !s.is_empty())
                            .map(|st| {
                                if st == "Working" {
                                    tr.i18n("tracker_working")
                                } else {
                                    st
                                }
                            })
                            .unwrap_or_else(|| tr.i18n("tracker_updating")),
                        seeders: s.and_then(|s| s.seeders),
                        leechers: s.and_then(|s| s.leechers),
                        fails: s.map(|s| s.fails).unwrap_or(0),
                        next_announce: s.and_then(|s| s.next_announce),
                    }
                };
                rows.push(row);
            }
        }
        rows
    }

    /// Magnet URI for a torrent (used by the copy-magnet context menu item).
    pub fn magnet_uri(&self, hash: &str, name: &str) -> String {
        format!("magnet:?xt=urn:btih:{hash}&dn={}", urlencode(name))
    }

    /// Shut the session down, flushing fast-resume state first.
    ///
    /// Must run before the process exits: without it every torrent re-hashes
    /// its data on the next start, which is what an unclean exit costs.
    pub fn stop(&self) {
        // librqbit's stop() pauses every torrent and PERSISTS that pause, so
        // a restored session would come back fully paused. Remember which
        // torrents were actually running so the next startup can resume them.
        let running = running_hashes(&self.rq());
        let _ = self.db.with(|conn| {
            conn.execute(
                "insert or replace into persistent_object (key, value) values \
                 ('session.running', ?1)",
                [running.join(",")],
            )
        });

        self.rt.block_on(self.rq().stop());
    }
}

/// Hashes of all torrents that are not paused.
fn running_hashes(rq: &RqbitSession) -> Vec<String> {
    rq.with_torrents(|torrents| {
        torrents
            .filter(|(_, h)| !matches!(h.stats().state, TorrentStatsState::Paused))
            .map(|(_, h)| h.info_hash().as_string())
            .collect()
    })
}

/// Unpause the given torrents once they reappear from session persistence
/// (the restore happens shortly after the session is created).
async fn resume_after_restore(rq: Arc<RqbitSession>, hashes: Vec<String>) {
    let mut remaining: std::collections::HashSet<String> = hashes.into_iter().collect();

    for _ in 0..20 {
        if remaining.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let found: Vec<(String, Arc<ManagedTorrent>)> = remaining
            .iter()
            .filter_map(|hash| {
                librqbit::api::TorrentIdOrHash::parse(hash)
                    .ok()
                    .and_then(|id| rq.get(id))
                    .map(|handle| (hash.clone(), handle))
            })
            .collect();

        for (hash, handle) in found {
            if let Err(err) = rq.unpause(&handle).await {
                tracing::debug!("could not resume restored torrent {hash}: {err:#}");
            }
            remaining.remove(&hash);
        }
    }
}

pub enum AddTorrentSource {
    TorrentFileBytes(Vec<u8>),
    MagnetUri(String),
}

/// Percent-encode one query-string value (RFC 3986 unreserved set kept).
///
/// Hand-rolled rather than pulled in: this escapes tracker parameters and
/// magnet fields, which is a handful of call sites and no edge cases beyond
/// "escape everything that is not unreserved".
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// Queue scheduler decisions: lowest positions run, excess pauses,
    /// freed slots resume, limit <= 0 means unlimited, and active_limit caps
    /// the two sub-limits combined.
    #[test]
    fn queue_decisions() {
        use super::QueueKind::{Download, Seed};
        let d = |h: &str, pos: i64, running: bool| (h.to_string(), pos, Download, running);
        let s = |h: &str, pos: i64, running: bool| (h.to_string(), pos, Seed, running);
        // Unlimited total cap for the sub-limit-focused cases.
        let unl = 0;

        // Over the download limit: the highest positions pause.
        let (pause, resume) = super::decide_queue(
            unl,
            2,
            unl,
            vec![d("a", 0, true), d("b", 1, true), d("c", 2, true)],
        );
        assert_eq!(pause, vec!["c"]);
        assert!(resume.is_empty());

        // Freed slot: the lowest auto-paused position resumes.
        let (pause, resume) = super::decide_queue(
            unl,
            2,
            unl,
            vec![d("a", 0, true), d("b", 1, false), d("c", 2, false)],
        );
        assert!(pause.is_empty());
        assert_eq!(resume, vec!["b"]);

        // Position order beats insertion order.
        let (pause, resume) =
            super::decide_queue(unl, 1, unl, vec![d("high", 5, true), d("low", 1, false)]);
        assert_eq!(pause, vec!["high"]);
        assert_eq!(resume, vec!["low"]);

        // Unlimited resumes everything the scheduler paused.
        let (pause, resume) =
            super::decide_queue(unl, 0, unl, vec![d("a", 0, true), d("b", 1, false)]);
        assert!(pause.is_empty());
        assert_eq!(resume, vec!["b"]);

        // Downloads and seeds are capped independently: 1 dl + 1 seed run even
        // though there are two of each.
        let (pause, resume) = super::decide_queue(
            unl,
            1,
            1,
            vec![
                d("d0", 0, true),
                d("d1", 1, true),
                s("s0", 2, true),
                s("s1", 3, true),
            ],
        );
        assert_eq!(pause, vec!["d1", "s1"]);
        assert!(resume.is_empty());

        // active_limit caps the total across both kinds: sub-limits would allow
        // 2 dl + 2 seed = 4, but active_limit=2 keeps only the two lowest
        // positions running regardless of kind.
        let (pause, resume) = super::decide_queue(
            2,
            5,
            5,
            vec![
                d("d0", 0, true),
                s("s0", 1, true),
                d("d1", 2, true),
                s("s1", 3, true),
            ],
        );
        assert_eq!(pause, vec!["d1", "s1"]);
        assert!(resume.is_empty());
    }

    /// The ipfilter.* settings map to a blocklist URL librqbit can load.
    #[test]
    fn ipfilter_settings_produce_blocklist_url() {
        use crate::core::configuration::Configuration;
        use crate::core::database::Database;
        use std::sync::Arc;

        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db);

        // Disabled -> no URL.
        assert!(super::ipfilter_url(&cfg).is_none());

        let file = std::env::temp_dir().join("nanotorrent-ipfilter-test.p2p");
        std::fs::write(&file, "test:1.2.3.0-1.2.3.255\n").unwrap();

        cfg.set("ipfilter.enabled", &true);
        cfg.set("ipfilter.file_path", &file.to_string_lossy().into_owned());

        let url = super::ipfilter_url(&cfg).unwrap();
        assert!(url.starts_with("file:///"), "was: {url}");
        assert!(url.ends_with("nanotorrent-ipfilter-test.p2p"), "was: {url}");

        // http URLs pass through untouched.
        cfg.set(
            "ipfilter.file_path",
            &String::from("https://example.com/list.p2p"),
        );
        assert_eq!(
            super::ipfilter_url(&cfg).as_deref(),
            Some("https://example.com/list.p2p")
        );

        let _ = std::fs::remove_file(&file);
    }

    /// End-to-end check of the create-torrent pipeline: build a torrent for
    /// a temp file with trackers/comment/private flag and verify the
    /// written .torrent parses back with everything attached.
    #[test]
    fn build_torrent_roundtrip() {
        let dir = std::env::temp_dir().join("nanotorrent-create-test");
        std::fs::create_dir_all(&dir).unwrap();
        let payload = dir.join("payload.bin");
        std::fs::write(&payload, vec![0x5Au8; 300 * 1024]).unwrap();
        let output = dir.join("out.torrent");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let outcome = rt
            .block_on(super::build_torrent(super::CreateTorrentParams {
                source: payload,
                trackers: vec![
                    String::from("http://tracker.example.com/announce"),
                    String::from("udp://tracker2.example.com:6969"),
                ],
                comment: String::from("test comment"),
                private: true,
                piece_length: Some(256 * 1024),
                version: crate::bittorrent::torrent_create::TorrentVersion::V1,
                output: output.clone(),
                add_to_session: false,
            }))
            .unwrap();

        let super::CreateTorrentOutcome::Created { name, bytes, .. } = outcome else {
            panic!("expected Created");
        };
        assert_eq!(name, "payload.bin");

        let written = std::fs::read(&output).unwrap();
        assert_eq!(written, bytes);

        let parsed = librqbit::torrent_from_bytes::<librqbit::ByteBufOwned>(&written).unwrap();
        assert_eq!(
            parsed.announce.as_ref().map(|a| a.as_ref()),
            Some(&b"http://tracker.example.com/announce"[..])
        );
        assert_eq!(parsed.announce_list.len(), 2);
        assert!(parsed.info.private);
        assert_eq!(
            parsed.comment.as_ref().map(|c| c.as_ref()),
            Some(&b"test comment"[..])
        );
        assert_eq!(parsed.info.piece_length, 256 * 1024);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Helper for manual end-to-end testing: creates a .torrent file for a
    /// generated payload file. Run with:
    ///   NANOTORRENT_TEST_TORRENT_DIR=<dir> cargo test make_test_torrent -- --ignored
    #[test]
    #[ignore = "writes a test torrent to NANOTORRENT_TEST_TORRENT_DIR"]
    fn make_test_torrent() {
        let dir = std::path::PathBuf::from(std::env::var("NANOTORRENT_TEST_TORRENT_DIR").unwrap());
        let payload = dir.join("nanotorrent-test-payload.bin");
        std::fs::write(&payload, vec![0xABu8; 512 * 1024]).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let torrent = rt
            .block_on(librqbit::create_torrent(
                &payload,
                librqbit::CreateTorrentOptions {
                    name: Some("nanotorrent-test"),
                    ..Default::default()
                },
            ))
            .unwrap();

        std::fs::write(
            dir.join("nanotorrent-test.torrent"),
            torrent.as_bytes().unwrap(),
        )
        .unwrap();
    }
}

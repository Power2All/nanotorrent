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

use crate::bittorrent::metainfo;
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
    /// The v1/v2 info hashes, computed once from the info dict.
    ///
    /// Cached because `list()` runs on the one-second UI tick and hashing is
    /// proportional to the info dictionary, which for a torrent with many
    /// files runs to hundreds of kilobytes - re-hashing every torrent every
    /// second would be pure waste. `None` means "not computed yet", which is
    /// also the state a magnet sits in until its metadata arrives.
    info_hashes: Option<(Option<String>, Option<String>)>,
}

/// Not downloaded at all.
pub const PRIORITY_SKIP: i64 = 0;
/// The default, and the level a file with no stored row is at.
pub const PRIORITY_NORMAL: i64 = 1;
pub const PRIORITY_HIGH: i64 = 2;
pub const PRIORITY_MAX: i64 = 3;

/// A priority level as a word, for the log.
fn priority_name(level: i64) -> &'static str {
    match level {
        PRIORITY_SKIP => "skip",
        PRIORITY_HIGH => "high",
        PRIORITY_MAX => "maximum",
        _ => "normal",
    }
}

/// File indices in the order the engine should ask for them, most wanted first.
///
/// Highest priority first, and within a level the original file order, so a
/// torrent where nothing has been prioritised comes out exactly as it went in.
/// Skipped files are still on the list: they have been taken out of
/// `only_files` already, and leaving them off here would only mean the engine
/// filling the gap itself.
///
/// Stable rather than sorted by name: upstream sorts by filename because many
/// torrents have a random file order, but once someone has said which files
/// matter, second-guessing the rest of the order is not this function's job.
fn priority_order(count: usize, stored: &HashMap<usize, i64>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..count).collect();
    let level = |i: &usize| stored.get(i).copied().unwrap_or(PRIORITY_NORMAL);
    // Descending by level; `sort_by_key` is stable, so equal levels keep the
    // file order they came in with.
    order.sort_by_key(|i| std::cmp::Reverse(level(i)));
    order
}

/// Where row `at` ends up, in a queue of `len` rows.
///
/// Split out because this is the arithmetic worth checking: every one of these
/// is an off-by-one waiting to happen, and none of them is visible from a test
/// that has to stand a real session up first.
///
/// Moving saturates rather than wrapping - "down" from the last row stays put
/// instead of jumping to the top, which is what every list in every application
/// does and what anyone holding the key down expects.
fn queue_target(at: usize, len: usize, to: QueueMove) -> usize {
    let last = len.saturating_sub(1);
    match to {
        QueueMove::Top => 0,
        QueueMove::Bottom => last,
        QueueMove::Up => at.saturating_sub(1),
        QueueMove::Down => (at + 1).min(last),
    }
}

/// Where a torrent should go in the queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueueMove {
    Top,
    Up,
    Down,
    Bottom,
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

    /// Attach live peer counts to a discovery-source row.
    ///
    /// `None` leaves the columns empty rather than showing zeros: nothing is
    /// connected through this source right now, which is not the same claim as
    /// "this source found a swarm with no seeds in it".
    fn with_counts(mut self, counts: Option<(u32, u32)>) -> Self {
        if let Some((seeds, leeches)) = counts {
            self.seeders = Some(seeds);
            self.leechers = Some(leeches);
        }
        self
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

/// Something that happened to a torrent, delivered to every subscriber.
///
/// Lifecycle is derived by diffing the live torrent set against the previous
/// tick rather than hooked into each add/remove call site, so a torrent added
/// by the UI, the web API, IPC or a plugin all raise the same event through
/// one path.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    TorrentAdded { hash: String, name: String },
    /// Asked to add a torrent the session already holds.
    ///
    /// Not an error - re-adding is harmless and the torrent is there either
    /// way - but it is not an add, and reporting it as one made a batch of
    /// three look like it had added one.
    TorrentDuplicate { hash: String, name: String },
    TorrentCompleted { hash: String, name: String },
    TorrentRemoved { hash: String, name: String },
    /// Background work failed where there was no caller to return it to.
    Error(String),
}

/// Where a new torrent should actually be written.
///
/// The incomplete folder keeps partial files off the destination disk until
/// they are worth having there; [`crate::bittorrent::manager`] moves them on
/// when the torrent finishes.
///
/// Only applied when the caller did not choose a folder. Someone who picked a
/// save path in the Add dialog said where they want it, and quietly writing
/// somewhere else - even temporarily - is the kind of surprise that ends with a
/// full scratch disk and no idea why.
///
/// ponytail: no per-torrent record of the intended destination, so a custom
/// save path simply opts out of the incomplete folder. Storing the intent per
/// torrent is the upgrade if anyone wants both at once.
fn incomplete_folder(cfg: &Configuration, params: &AddParams) -> Option<String> {
    if params.save_path.is_some() || !cfg.get_bool("downloads.incomplete_enabled") {
        return params.save_path.clone();
    }
    cfg.get_string("downloads.incomplete_path")
        .filter(|p| !p.is_empty())
        .or_else(|| params.save_path.clone())
}

/// Per-torrent share limits, keyed by info hash.
///
/// Only rows that set at least one of the two are returned, so the common case
/// (nobody has overridden anything) is an empty map rather than a row per
/// torrent.
fn read_share_overrides(db: &Arc<Database>) -> HashMap<String, (Option<f64>, Option<i64>)> {
    db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT info_hash, ratio_limit, seed_time_limit FROM torrent \
             WHERE ratio_limit IS NOT NULL OR seed_time_limit IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, Option<f64>>(1)?, r.get::<_, Option<i64>>(2)?),
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    })
    .unwrap_or_default()
}

/// Carry out what a share limit decided.
///
/// Pausing is recorded in the database as well as done to the engine: a torrent
/// stopped for hitting its ratio must stay stopped across a restart, and
/// librqbit's own persistence is the only other thing that remembers, which the
/// queue scheduler would happily undo.
async fn apply_share_action(
    rq: &Arc<RqbitSession>,
    db: &Arc<Database>,
    meta: &Arc<Mutex<HashMap<String, TorrentMeta>>>,
    events: &EventBus,
    hash: &str,
    action: crate::bittorrent::limits::ShareAction,
) {
    use crate::bittorrent::limits::ShareAction;

    let Ok(id) = librqbit::api::TorrentIdOrHash::parse(hash) else {
        return;
    };

    match action {
        ShareAction::Pause => {
            let Some(handle) = rq.get(id) else { return };
            let name = handle.name().unwrap_or_else(|| hash.to_string());
            if let Err(err) = rq.pause(&handle).await {
                report_error(events, format!("Failed to pause {name}: {err:#}"));
                return;
            }
            tracing::info!("{name} reached its share limit and was paused");
        }
        ShareAction::Remove { with_data } => {
            let name = rq
                .get(id)
                .and_then(|h| h.name())
                .unwrap_or_else(|| hash.to_string());
            if let Err(err) = rq.delete(id, with_data).await {
                report_error(events, format!("Failed to remove {name}: {err:#}"));
                return;
            }
            meta.lock().unwrap().remove(hash);
            let _ = db.with(|conn| {
                conn.execute("delete from torrent_magnet_uri where info_hash = ?1", [hash])?;
                conn.execute("delete from torrent where info_hash = ?1", [hash])
            });
            tracing::info!("{name} reached its share limit and was removed");
            events.emit(SessionEvent::TorrentRemoved {
                hash: hash.to_string(),
                name,
            });
        }
    }
}

/// Report a background failure: logged once, then handed to every subscriber.
///
/// A free function rather than a method because the tasks that raise these have
/// no `&self` - they own a cloned bus and nothing else. Logging here rather than
/// where the event is consumed means a headless build, which has no UI draining
/// anything, still gets the error in its log.
fn report_error(events: &EventBus, message: String) {
    tracing::error!("{message}");
    events.emit(SessionEvent::Error(message));
}

/// Fan-out to every subscriber.
///
/// Cloned into background tasks, which have no `&self` to emit through - the
/// same reason the error queue this replaces was an `Arc<Mutex<..>>`.
#[derive(Clone)]
pub struct EventBus(Arc<Mutex<Vec<std::sync::mpsc::Sender<SessionEvent>>>>);

impl EventBus {
    fn new() -> EventBus {
        EventBus(Arc::new(Mutex::new(Vec::new())))
    }

    /// Subscribe for the lifetime of the returned receiver.
    ///
    /// These are unbounded std channels, so a subscriber that stops draining
    /// grows one. Every subscriber here drains on a timer: the UI on its
    /// refresh tick, the plugin host on its own thread.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<SessionEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.0.lock().unwrap().push(tx);
        rx
    }

    /// Deliver to everyone still listening.
    pub fn emit(&self, event: SessionEvent) {
        // A send fails only once the receiver is dropped, which is how a
        // subscriber unregisters itself - no explicit unsubscribe needed.
        self.0
            .lock()
            .unwrap()
            .retain(|tx| tx.send(event.clone()).is_ok());
    }
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
    /// Lifecycle fan-out. Replaces a `Vec<String>` of completions that was
    /// filled inside `torrents()` and drained only by the Slint UI: a headless
    /// build never called the former and never ran the latter, so completions
    /// were both undetected and (once the web API started calling `torrents()`)
    /// accumulated forever.
    events: EventBus,
    /// Set once the bound interface has gone missing, so the pause happens on
    /// the transition rather than on every tick.
    binding_lost: Arc<std::sync::atomic::AtomicBool>,
    /// Torrents paused by the queue scheduler (as opposed to by the user).
    queue_paused: Arc<Mutex<std::collections::HashSet<String>>>,
    /// librqbit's JSON session folder (holds the per-torrent `.bitv`
    /// fastresume files), for force-recheck.
    session_path: std::path::PathBuf,
    /// The one HTTP client for anything this session fetches itself - web
    /// seeds, today. Built once from the settings, which is safe because
    /// changing the proxy already tears the whole session down and rebuilds
    /// it, so a stale client cannot outlive the setting that made it.
    http: reqwest::Client,
}

/// Translate the settings database into librqbit's `SessionOptions`.
///
/// The one place a setting becomes engine configuration. Settings with no
/// librqbit equivalent are read and dropped here rather than at the call site,
/// so what is and is not honoured is visible in one function.
/// The Azureus-style peer id for a version string: `-NT<major><minor><patch>0-`
/// followed by 12 random bytes.
///
/// Each version component is one character from librqbit's 64-entry alphabet
/// (`0-9A-Za-z.-`), so 0.2.0 reads `-NT0200-` and 0.2.10 reads `-NT02A0-`.
/// Anything past 63 has no character to map to and librqbit unwraps a None, so
/// the clamp below is what keeps a future 0.2.64 from panicking at startup
/// rather than merely misreporting itself.
fn azureus_peer_id(version: &str) -> librqbit::Id20 {
    let mut parts = version
        .split('.')
        .map(|p| p.parse::<u8>().unwrap_or(0).min(63));
    generate_azereus_style(
        *b"NT",
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            0,
        ),
    )
}

/// Work out what strict mode allows, from the settings and this machine.
///
/// Returns the restrictions to build with, or the reason not to start at all.
/// Kept next to the session because refusing to start IS the feature: a
/// component that cannot be covered must not run, and protection that is not
/// in place must stop the client rather than be quietly skipped.
fn strict_limits(cfg: &Configuration) -> Result<crate::core::netguard::Restrictions> {
    use crate::core::netguard::{Decision, Intent, decide, look_up};

    let bind_interface = cfg
        .get_string("network.bind_interface")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    let intent = Intent {
        strict: cfg.get_bool("network.strict"),
        proxy: crate::core::http::proxy_url(cfg).is_some(),
        bind_interface: bind_interface.clone(),
    };

    // Only looked up when one was named, so a machine with no VPN pays nothing
    // for enumerating its interfaces on every session rebuild.
    let facts = bind_interface.as_deref().map(look_up);

    match decide(&intent, facts) {
        Decision::Open => Ok(Default::default()),
        Decision::Restricted(limits) => {
            if limits.any() {
                tracing::info!(
                    "strict mode: not starting {}",
                    limits.describe().join(", ")
                );
            }
            Ok(limits)
        }
        Decision::Refuse(why) => {
            tracing::error!("refusing to start: {why}");
            Err(anyhow::anyhow!("{why}"))
        }
    }
}

fn build_session_options(
    cfg: &Configuration,
    env: &Environment,
    limits: crate::core::netguard::Restrictions,
) -> SessionOptions {
    // Peer ID: Azureus-style `-NT-` prefix, or a fully random id (no client
    // fingerprint) in anonymous mode.
    let anonymous = cfg.get_bool("libtorrent.anonymous_mode");
    let peer_id = if anonymous {
        let mut bytes = [0u8; 20];
        rand::fill(&mut bytes[..]);
        librqbit::Id20::new(bytes)
    } else {
        azureus_peer_id(env!("CARGO_PKG_VERSION"))
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
        // Bind every socket - peers, trackers, DHT, uTP - to one interface.
        //
        // Empty means today's behaviour: the OS picks a source address from
        // its routing table. Naming a VPN's interface is stronger than routing
        // torrent traffic through a proxy, because the operating system
        // enforces it rather than each component remembering to ask: when the
        // tunnel drops the interface goes with it and every bound socket fails
        // at once, instead of quietly reverting to the real address.
        bind_device_name: cfg
            .get_string("network.bind_interface")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        // librqbit 9 folds the three DHT switches into one Option - None is
        // "no DHT at all", and persistence is a field inside it.
        dht: (cfg.get_bool("libtorrent.enable_dht") && !limits.disable_dht)
            .then(|| librqbit::DhtSessionConfig {
                persistence: Some(librqbit::dht::DhtPersistenceConfig {
                    config_filename: Some(env.get_application_data_path().join("dht.json")),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        fastresume: true,
        persistence: Some(SessionPersistenceConfig::Json {
            folder: Some(env.get_session_state_path()),
        }),
        peer_id: Some(peer_id),
        // How peers see this client in the BEP 10 extended handshake's `v`
        // string. librqbit 9 takes it as an option (it needed a patch in 8);
        // without it peers show NanoTorrent as "rqbit" while its peer id says
        // `-NT-`, and the two identities disagree. Both derive from
        // CARGO_PKG_VERSION so they cannot drift apart. Anonymous mode needs
        // nothing extra here - patch 0003 drops `v` entirely in that mode.
        client_name_and_version: Some(crate::buildinfo::client_id()),
        // uTP (BEP 29) alongside TCP when asked for. Off by default: it is
        // a second socket, on UDP, and upstream still calls it experimental.
        listen: Some(librqbit::ListenerOptions {
            mode: if cfg.get_bool("libtorrent.enable_utp") && !limits.disable_utp {
                librqbit::ListenerMode::TcpAndUtp
            } else {
                librqbit::ListenerMode::TcpOnly
            },
            listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, listen_port).into(),
            // ListenerOptions has its own copy of this, and
            // SessionOptions::ipv4_only feeds only the stream connector and
            // the DHT. Without it here the listener binds [::] regardless,
            // which on Windows cannot be pinned to a v4-only interface.
            ipv4_only: limits.ipv4_only,
            // Off in strict mode: UPnP asks the local router to open a
            // port, which both announces this machine on the LAN and is
            // meaningless when the traffic leaves through a tunnel anyway.
            enable_upnp_port_forwarding: !limits.disable_upnp,
            ..Default::default()
        }),
        // Local service discovery (BEP 14). The preference has existed since
        // the PicoTorrent settings were imported and defaults to ON; it was
        // stored but never applied, because librqbit 8 could not do it.
        disable_local_service_discovery: !cfg.get_bool("libtorrent.enable_lsd")
            || limits.disable_lsd,
        // Strict mode with a v4-only tunnel: a v6 socket would route around
        // the binding through the ordinary interface, which is the leak the
        // binding was for.
        ipv4_only: limits.ipv4_only,
        connect: Some(librqbit::ConnectionOptions {
            proxy_url: socks_proxy_url,
            ..Default::default()
        }),
        // Proxy scope (engine patch 0004), matching PicoTorrent's opt-in
        // proxy_peers / proxy_trackers / proxy_hostnames. Only meaningful when
        // a proxy is configured above; harmless otherwise.
        proxy_peers: cfg.get_bool("libtorrent.proxy_peers"),
        proxy_trackers: cfg.get_bool("libtorrent.proxy_trackers"),
        proxy_hostnames: cfg.get_bool("libtorrent.proxy_hostnames"),
        ratelimits: LimitsConfig {
            download_bps,
            upload_bps,
        },
        // PeX toggle (engine patch 0003). The prefs checkbox stores
        // libtorrent.enable_pex; disable_pex is its inverse.
        disable_pex: !cfg.get_bool("libtorrent.enable_pex"),
        // Anonymous mode (engine patch 0003): random peer id above + suppress
        // the client version in the extended handshake.
        anonymize: anonymous,
        // Require MSE/PE encryption on outgoing connections (engine patch 0002
        // seam + bittorrent::mse).
        stream_transform: if cfg.get_bool("libtorrent.require_outgoing_encryption") {
            Some(Arc::new(crate::bittorrent::mse::MseTransform))
        } else {
            None
        },
        // Accept incoming MSE/PE peers (engine patch 0002 accept-path seam).
        // Always installed: plaintext peers pass through untouched, so this
        // only ever *adds* the ability to talk to encrypted-only peers. When
        // libtorrent.require_incoming_encryption is set, plaintext is refused.
        incoming_transform: Some(Arc::new(crate::bittorrent::mse::IncomingMseTransform {
            require: cfg.get_bool("libtorrent.require_incoming_encryption"),
        })),
        ..Default::default()
    }
}

/// Move an unreadable `session.json` aside so startup can continue.
///
/// librqbit treats a *missing* index as an empty session but an *unreadable*
/// one as fatal, so a single truncated write takes the whole app down with
/// "error deserializing session database: EOF while parsing a value" and no
/// way back in. Patch 0006 stops it happening again (the write was not fsynced
/// before its rename); this is what lets an already-broken profile start.
///
/// Renamed, never deleted: it is the only record of where each torrent was
/// saving to - that lives in this file and nowhere else, as the `torrent`
/// table has no save path column. Nothing is re-added automatically for the
/// same reason: guessing a save path could send a finished download to the
/// wrong folder or trigger a full re-download.
///
/// The `<hash>.torrent` files beside it are untouched, so the torrents can be
/// re-added by hand.
fn quarantine_unreadable_session_index(session_path: &std::path::Path) {
    let index = session_path.join("session.json");

    // Only act on a file that exists AND fails to parse. A healthy index must
    // never be moved aside, and a missing one is already handled.
    match std::fs::read(&index) {
        Err(_) => return, // missing or unreadable: librqbit copes with both
        Ok(bytes) if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok() => return,
        Ok(_) => {}
    }

    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let quarantined = session_path.join(format!("session.json.corrupt-{stamp}"));
    if let Err(err) = std::fs::rename(&index, &quarantined) {
        // Leave it in place: the error below is still more useful than the
        // deserialize failure the caller would otherwise hit.
        tracing::error!("cannot move the corrupt session index aside: {err}");
        return;
    }

    let orphans = std::fs::read_dir(session_path)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "torrent"))
                .count()
        })
        .unwrap_or(0);

    tracing::error!(
        "the session index was corrupt (a truncated write, usually an unclean \
         shutdown) and has been moved to {}. Starting with an empty session. \
         {orphans} torrent(s) remain in {} and can be re-added - their data is \
         intact, but their save paths were only recorded in the index.",
        quarantined.display(),
        session_path.display(),
    );
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
                    // Set below along with the comment and the private flag, so
                    // that v1 and v2 (which builds its own metainfo) agree on
                    // exactly one place where trackers are attached.
                    trackers: Vec::new(),
                    piece_length: params.piece_length,
                },
                &librqbit::spawn_utils::BlockingSpawner::new(1),
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
            meta.info.data.private = params.private;

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

        let opts = build_session_options(cfg, env, strict_limits(cfg)?);

        // Before librqbit reads it: an unreadable index is fatal there, and a
        // torrent client that will not launch is worse than one that lost its
        // list. See the function for why nothing is re-added automatically.
        quarantine_unreadable_session_index(&env.get_session_state_path());

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
                    build_session_options(cfg, env, strict_limits(cfg)?),
                ))?
            }
        };
        let api = Api::new(inner.clone(), None);

        // Built here so a bad proxy setting stops the session from starting
        // rather than being discovered later by a web seed going direct.
        let http = crate::core::http::client(cfg)
            .map_err(|e| anyhow::anyhow!("cannot build an HTTP client for web seeds: {e}"))?;

        let session = Session {
            rt,
            inner: Arc::new(std::sync::RwLock::new(inner)),
            api: Arc::new(std::sync::RwLock::new(api)),
            db,
            meta: Arc::new(Mutex::new(HashMap::new())),
            events: EventBus::new(),
            binding_lost: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            queue_paused: Arc::new(Mutex::new(std::collections::HashSet::new())),
            session_path: env.get_session_state_path(),
            http,
        };

        session.spawn_low_disk_guard(cfg);
        // Before the metadata is loaded, so nothing is read for a torrent that
        // is about to be forgotten.
        {
            let present: std::collections::HashSet<String> = session
                .rq()
                .with_torrents(|torrents| {
                    torrents
                        .map(|(_, handle)| handle.info_hash().as_string())
                        .collect()
                });
            session.forget_missing_torrents(&present);
        }
        session.load_torrent_meta();
        // Heal any duplicate/gapped queue positions persisted before positions
        // were compacted on removal.
        session.normalize_queue_positions();

        // Lifecycle detection, on a timer the session owns. It must not hang
        // off `torrents()`: that is called by the UI refresh tick and by web
        // API requests, so on a headless build with nothing connected it never
        // runs, and a download could finish with no event raised at all.
        session.spawn_scan_task();
        session.spawn_speed_scheduler();
        session.spawn_share_limit_guard();

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
        // A refusal here keeps the session that is already running rather than
        // rebuilding into one that would leak. The settings are saved either
        // way - the user can see what they asked for and fix it - but nothing
        // goes on the network under a promise this build cannot keep.
        let limits = match strict_limits(cfg) {
            Ok(limits) => limits,
            Err(err) => {
                tracing::error!("settings not applied: {err}");
                report_error(&self.events, format!("{err}"));
                return;
            }
        };
        let opts = build_session_options(cfg, env, limits);
        let default_save_path = cfg
            .get_string("default_save_path")
            .map(PathBuf::from)
            .unwrap_or_else(Environment::get_downloads_path);

        let inner_slot = self.inner.clone();
        let api_slot = self.api.clone();
        let events = self.events.clone();

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
                    report_error(&events, msg);
                }
            }
        });
    }

    /// Load per-torrent metadata from the `torrent` table for torrents
    /// restored by librqbit's session persistence.
    /// Drop rows for torrents the engine no longer has.
    ///
    /// Removing a torrent through the application cascades - `torrent_tracker`,
    /// `torrent_tag` and `torrent_file_priority` all reference `torrent` with
    /// ON DELETE CASCADE, and foreign keys are on. This is for the times that
    /// does not happen: a session folder deleted by hand, a crash between the
    /// engine's write and ours, a profile copied from elsewhere. Those leave a
    /// `torrent` row with no torrent, and with it every per-torrent setting
    /// that hangs off it.
    ///
    /// Deleting the `torrent` row is enough - the cascade takes the rest. Run
    /// once at startup, after the engine has restored its own list, so "the
    /// engine does not have it" is a fact rather than a race.
    fn forget_missing_torrents(&self, present: &std::collections::HashSet<String>) {
        let stale: Vec<String> = self
            .db
            .with(|conn| {
                let mut stmt = conn.prepare("select info_hash from torrent")?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|hash| !present.contains(hash))
            .collect();

        if stale.is_empty() {
            return;
        }
        tracing::info!(
            "forgetting {} torrent(s) the engine no longer has",
            stale.len()
        );
        let _ = self.db.with(|conn| {
            for hash in &stale {
                // The magnet table has no foreign key to `torrent`, so it is
                // the one thing the cascade does not reach.
                conn.execute(
                    "delete from torrent_magnet_uri where info_hash = ?1",
                    [hash],
                )?;
                conn.execute("delete from torrent where info_hash = ?1", [hash])?;
            }
            Ok::<_, rusqlite::Error>(())
        });
        self.meta.lock().unwrap().retain(|hash, _| !stale.contains(hash));
    }

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
                    info_hashes: None,
                },
            );
        }
    }

    /// A handle to the session runtime, for callers that need to drive their
    /// own future on it rather than through one of the methods here.
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// The running half of the kill switch.
    ///
    /// `strict_limits` refuses at start-up; this catches the interface going
    /// away while the client is running, which is the case people actually
    /// hit - a VPN drops, its adapter disappears, and every socket quietly
    /// falls back to the ordinary route. Returns whether the binding is
    /// intact, for the status bar.
    ///
    /// Nothing is resumed when the interface comes back. Torrents paused here
    /// are indistinguishable from torrents the user paused, and guessing wrong
    /// would start traffic nobody asked to start.
    pub fn watch_binding(&self, cfg: &Configuration) -> bool {
        use std::sync::atomic::Ordering::Relaxed;

        let name = cfg
            .get_string("network.bind_interface")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let Some(name) = name.filter(|_| cfg.get_bool("network.strict")) else {
            self.binding_lost.store(false, Relaxed);
            return true;
        };

        let present = crate::core::netguard::look_up(&name).exists;
        if !crate::core::netguard::just_lost(&self.binding_lost, present) {
            return present;
        }

        tracing::error!("network interface \"{name}\" is gone - pausing everything");
        self.push_error(format!(
            "Network interface \"{name}\" is no longer present. All torrents have been paused."
        ));
        let handles: Vec<Arc<ManagedTorrent>> = self
            .rq()
            .with_torrents(|torrents| torrents.map(|(_, h)| h.clone()).collect());
        for handle in handles {
            let _ = self.rt.block_on(self.rq().pause(&handle));
        }
        false
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
        let events = self.events.clone();
        let id = librqbit::api::TorrentIdOrHash::Id(handle.id());
        // librqbit's fastresume file. After deleting the torrent we remove it
        // ourselves so the re-add does a FULL re-hash: librqbit only spot-checks
        // the saved bitfield, and on Windows it can fail to delete this file
        // while it's still memory-mapped - so a stale "complete" bitfield would
        // otherwise survive and the torrent would wrongly seed missing data.
        let bitv = self.session_path.join(format!("{hash}.bitv"));

        self.rt.spawn(async move {
            if let Err(err) = rq.delete(id, false).await {
                report_error(&events, format!("Failed to recheck torrent: {err:#}"));
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
                report_error(&events, format!("Failed to re-add torrent for recheck: {err:#}"));
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
        let events = self.events.clone();
        let id = librqbit::api::TorrentIdOrHash::Id(handle.id());
        let new_folder = new_folder.to_string();

        self.rt.spawn(async move {
            if let Err(err) = rq.delete(id, false).await {
                report_error(&events, format!("Failed to move torrent: {err:#}"));
                return;
            }

            if let Err(err) = move_files(&old_folder, &new_folder, &files) {
                report_error(&events, format!("Failed to move torrent data: {err:#}"));
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
                report_error(&events, format!("Failed to re-add moved torrent: {err:#}"));
            }
        });
    }

    /// Point a torrent at data that has already been moved, without moving
    /// anything.
    ///
    /// The counterpart to [`Session::move_storage`]: that one relocates the
    /// FILES, this one relocates the TORRENT. It is what you want when the data
    /// was moved outside the application - by hand, onto another drive, or
    /// restored from a backup to a different path - which otherwise leaves the
    /// torrent erroring or re-downloading everything it already has.
    ///
    /// Re-adding at the new folder makes librqbit verify what is there, so an
    /// intact copy comes straight back as complete, and a partial one keeps
    /// whatever pieces check out. Nothing is deleted either way: the worst case
    /// for a wrong folder is a recheck that finds nothing.
    pub fn set_location(&self, hash: &str, new_folder: &str) {
        let Some((bytes, _old_folder, paused)) = self.torrent_readd_info(hash) else {
            return;
        };
        let Some(handle) = self.find(hash) else {
            return;
        };

        // Where the data has to be for this to be a relocation rather than a
        // fresh download.
        //
        // librqbit is handed an explicit output folder, and an explicit one is
        // used VERBATIM - it does not append the torrent's own directory the
        // way it does for its default folder. A multi-file torrent's relative
        // paths exclude that directory too (BEP 3 puts it in `info.name`), so
        // the folder wanted here is the one that DIRECTLY contains the files.
        //
        // Picking the parent instead is the easy mistake, and an expensive one:
        // the files are simply not found, empty ones are created in their place
        // and the whole torrent downloads again. So try the obvious correction
        // before giving up - if <picked>/<torrent name> is where the data
        // actually is, use that.
        let meta = handle.metadata.load_full();
        let first = meta
            .as_ref()
            .and_then(|m| m.file_infos.first().map(|fi| fi.relative_filename.clone()));
        let picked = std::path::Path::new(new_folder);
        let mut new_folder = new_folder.to_string();
        let mut first_missing = false;

        if let Some(rel) = &first
            && !picked.join(rel).exists()
        {
            let nested = handle.name().map(|n| picked.join(n));
            match nested.filter(|n| n.join(rel).exists()) {
                Some(n) => {
                    tracing::info!("data is one level down; using {}", n.display());
                    new_folder = n.to_string_lossy().into_owned();
                }
                None => first_missing = true,
            }
        }

        let rq = self.rq();
        let events = self.events.clone();
        let id = librqbit::api::TorrentIdOrHash::Id(handle.id());

        self.rt.spawn(async move {
            if first_missing {
                report_error(
                    &events,
                    format!("No data found in {new_folder} - it will be downloaded again."),
                );
            }

            // `false`: the files stay exactly where they are. This forgets the
            // torrent, not the data.
            if let Err(err) = rq.delete(id, false).await {
                report_error(&events, format!("Failed to set the location: {err:#}"));
                return;
            }

            let opts = AddTorrentOptions {
                paused,
                output_folder: Some(new_folder),
                // The files are expected to be there already - that is the
                // whole point - so this is a verify, not a clobber.
                overwrite: true,
                ..Default::default()
            };
            if let Err(err) = rq
                .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
                .await
            {
                report_error(&events, format!("Failed to re-add at the new location: {err:#}"));
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
    ///
    /// This is the port announced to trackers, which is the number worth
    /// showing: with UPnP it is the one that has to be reachable.
    pub fn listen_port(&self) -> Option<u16> {
        self.rq().announce_port()
    }

    /// Record an error from background work, where there is no caller to
    /// return it to.
    fn push_error(&self, err: String) {
        report_error(&self.events, err);
    }

    /// Add a torrent from a .torrent file's contents or a magnet link.
    ///
    /// Runs on the session runtime: for magnet links librqbit resolves the
    /// metadata before returning, which can take a long time (or forever for
    /// dead magnets), so this must never block the UI thread.
    pub fn add_torrent(&self, source: AddTorrentSource, params: AddParams) {
        // Cloned before the spawn below: the task outlives this borrow of
        // `self`, so the client has to travel with it rather than be reached
        // for later.
        let http = self.http.clone();
        let mut opts = AddTorrentOptions {
            paused: !params.start_torrent,
            output_folder: incomplete_folder(&Configuration::new(self.db.clone()), &params),
            only_files: params.only_files.clone(),
            overwrite: true,
            ..Default::default()
        };

        let add = match &source {
            AddTorrentSource::TorrentFileBytes(bytes) => {
                // A v2-ONLY torrent has nothing the engine can drive: no
                // `pieces`, and piece hashes that are merkle roots rather than
                // SHA-1. It is handed over in a v1-shaped form instead, with
                // its real identity and its real metadata restored through the
                // engine seams. A hybrid needs none of this - its v1 half is
                // what every client uses and what we have always used.
                use crate::bittorrent::v2::V2Prep;
                match crate::bittorrent::v2::prepare(bytes) {
                    Err(err) => {
                        let msg = format!("Failed to add torrent: {err}");
                        tracing::error!("{msg}");
                        report_error(&self.events, msg);
                        return;
                    }
                    Ok(V2Prep::V2Only(prepared)) => {
                        tracing::info!(
                            "adding a v2-only torrent ({} files, {} pieces) as {}",
                            prepared.files,
                            prepared.pieces,
                            prepared.wire_hash.as_string()
                        );
                        opts.override_info_hash = Some(prepared.wire_hash);
                        opts.override_info_bytes = Some(prepared.info_bytes.into());
                        opts.piece_verifier = Some(prepared.verifier);
                        // Serving hashes is what lets someone else bootstrap
                        // this torrent from a v2 magnet, and installing this
                        // is also what turns on the v2 handshake bit.
                        opts.hash_provider = prepared.hashes;
                        AddTorrent::from_bytes(prepared.synthetic)
                    }
                    Ok(V2Prep::Hybrid { secondary, hashes }) => {
                        // Driven as v1, but announced under both hashes so the
                        // v2 half of the swarm can find us too.
                        tracing::info!(
                            "adding a hybrid torrent, also announcing as {}",
                            secondary.as_string()
                        );
                        opts.secondary_info_hash = Some(secondary);
                        opts.hash_provider = hashes;
                        AddTorrent::from_bytes(bytes.clone())
                    }
                    Ok(V2Prep::V1Only) => AddTorrent::from_bytes(bytes.clone()),
                }
            }
            AddTorrentSource::MagnetUri(uri) => {
                // Promotes a hybrid magnet's v1 hash into `xt` where the
                // engine will find it, and turns a v2-only link into a
                // message rather than "didn't contain a BTv1 infohash".
                let hashes = crate::bittorrent::v2::magnet_hashes(uri);
                if hashes.v1.is_some()
                    && let Some(v2) = hashes.v2
                {
                    // A hybrid magnet names both; announce under both.
                    let mut truncated = [0u8; 20];
                    truncated.copy_from_slice(&v2[..20]);
                    opts.secondary_info_hash = Some(librqbit::Id20::new(truncated));
                } else if hashes.is_v2_only() {
                    // v2-only. The info dict arrives over BEP 9, but the piece
                    // hashes do not - they come from the peer over the BEP 52
                    // hash exchange. One object does all of it and then becomes
                    // the torrent's piece verifier, so the layers it collected
                    // are exactly what pieces are checked against.
                    tracing::info!("resolving a v2-only magnet");
                    let magnet = std::sync::Arc::new(crate::bittorrent::v2::V2Magnet::new());
                    opts.metadata_interceptor = Some(magnet.clone());
                    opts.piece_verifier = Some(magnet);
                }
                match crate::bittorrent::v2::normalise_magnet(uri) {
                    Ok(fixed) => AddTorrent::from_url(fixed),
                    Err(err) => {
                        tracing::error!("{err}");
                        report_error(&self.events, err);
                        return;
                    }
                }
            }
        };

        let inner = self.rq();
        let db = self.db.clone();
        let meta = self.meta.clone();
        let events = self.events.clone();

        self.rt.spawn(async move {
            match inner.add_torrent(add, Some(opts)).await {
                Ok(AddTorrentResponse::Added(_, handle)) => {
                    Self::on_torrent_added(&db, &meta, &handle, &source, &params);
                    // BEP 19. Only a real .torrent can carry `url-list`, and
                    // only after the torrent exists can a peer be attached to
                    // it - which is why this is here and not at build time.
                    if let AddTorrentSource::TorrentFileBytes(bytes) = &source {
                        crate::bittorrent::webseed::spawn_all(
                            inner.clone(),
                            &tokio::runtime::Handle::current(),
                            &handle,
                            bytes,
                            http.clone(),
                        );
                    }
                }
                // Already present. Still recorded - on_torrent_added only
                // inserts what is missing, and a re-added magnet refreshes its
                // stored URI - but announced separately, because silently
                // treating this as an add is what made "add 3, get 1" look
                // like torrents were being dropped.
                Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => {
                    Self::on_torrent_added(&db, &meta, &handle, &source, &params);
                    let hash = handle.info_hash().as_string();
                    let name = handle.name().unwrap_or_else(|| hash.clone());
                    tracing::info!("already in the session, not added again: {name}");
                    events.emit(SessionEvent::TorrentDuplicate { hash, name });
                }
                Ok(AddTorrentResponse::ListOnly(_)) => {}
                Err(err) => {
                    let msg = format!("Failed to add torrent: {err:#}");
                    tracing::error!("{msg}");
                    report_error(&events, msg);
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
        // Same normalisation as the add path, so the dialog and the add agree
        // on which magnets are usable rather than failing at different points.
        let uri = match crate::bittorrent::v2::normalise_magnet(&uri) {
            Ok(fixed) => fixed,
            Err(err) => {
                tracing::warn!("{err}");
                report_error(&self.events, err);
                slot.lock().unwrap().push(MagnetOutcome::Failed(uri));
                return;
            }
        };
        let inner = self.rq();
        self.rt.spawn(async move {
            let mut opts = AddTorrentOptions {
                list_only: true,
                ..Default::default()
            };
            // The preview needs the same treatment as the add, or a v2-only
            // magnet would resolve in the Add dialog and then fail on commit.
            if crate::bittorrent::v2::magnet_hashes(&uri).is_v2_only() {
                let magnet = std::sync::Arc::new(crate::bittorrent::v2::V2Magnet::new());
                opts.metadata_interceptor = Some(magnet);
            }
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
                info_hashes: None,
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

    /// The announce list this torrent should be using.
    ///
    /// `torrent_tracker` holds the WHOLE list once a torrent has been edited,
    /// not just the additions. Empty means nobody has touched it, and the
    /// .torrent's own list stands.
    ///
    /// Taking ownership of the whole list is what makes "remove" possible: the
    /// announce list is not part of the info dict, so replacing it changes
    /// nothing about the torrent's identity - see patch 0019.
    fn effective_tiers(&self, hash: &str, cfg: &Configuration) -> Vec<Vec<String>> {
        let stored = cfg.tracker_tiers(hash);
        if !stored.is_empty() {
            return stored;
        }
        // First edit: seed from what the torrent is announcing to now, so a
        // removal has something to remove FROM. The engine's own tier map is
        // the source, not the flat set, so the grouping survives the first edit
        // rather than collapsing the moment anyone touches it.
        let Some(handle) = self.find(hash) else {
            return Vec::new();
        };
        let tiers: Vec<Vec<String>> = self
            .rq()
            .tracker_tiers_snapshot(handle.info_hash())
            .into_iter()
            .map(|tier| tier.iter().map(|u| u.to_string()).collect())
            .collect();
        if !tiers.is_empty() {
            return tiers;
        }
        // A magnet has no announce-list, so no tiers are known: everything it
        // has goes in one.
        match handle
            .shared()
            .trackers
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
        {
            flat if flat.is_empty() => Vec::new(),
            flat => vec![flat],
        }
    }

    /// Add a tracker to one torrent, and start using it.
    ///
    /// The list is stored and the torrent re-added with it. The data is left
    /// alone - the re-add points at the same folder and librqbit verifies what
    /// is there, exactly as [`Session::move_storage`] does.
    /// `tier` is which announce tier to put it in; past the end means a new
    /// tier of its own, which is how "add a tier" is spelled.
    pub fn add_tracker(&self, hash: &str, url: &str, tier: usize) -> bool {
        let cfg = Configuration::new(self.db.clone());
        let url = url.trim().to_owned();
        if !cfg.is_tracker_url(&url) {
            return false;
        }
        let mut tiers = self.effective_tiers(hash, &cfg);
        // A tracker announced from two tiers would be asked twice and counted
        // twice, so it moves rather than being duplicated.
        for existing in tiers.iter_mut() {
            existing.retain(|t| *t != url);
        }
        match tiers.get_mut(tier) {
            Some(existing) => existing.push(url),
            None => tiers.push(vec![url]),
        }
        cfg.set_trackers(hash, &tiers);
        self.readd_with_trackers(hash, &cfg);
        true
    }

    /// Replace one tracker's URL, leaving it in its tier.
    pub fn edit_tracker(&self, hash: &str, from: &str, to: &str) -> bool {
        let cfg = Configuration::new(self.db.clone());
        let to = to.trim().to_owned();
        if !cfg.is_tracker_url(&to) {
            return false;
        }
        let mut tiers = self.effective_tiers(hash, &cfg);
        let mut found = false;
        for tier in tiers.iter_mut() {
            for url in tier.iter_mut() {
                if url == from {
                    *url = to.clone();
                    found = true;
                }
            }
        }
        if !found {
            return false;
        }
        cfg.set_trackers(hash, &tiers);
        self.readd_with_trackers(hash, &cfg);
        true
    }

    /// Stop using a tracker, whether it was added by hand or came in the file.
    pub fn remove_tracker(&self, hash: &str, url: &str) {
        let cfg = Configuration::new(self.db.clone());
        let mut tiers = self.effective_tiers(hash, &cfg);
        for tier in tiers.iter_mut() {
            tier.retain(|t| t != url);
        }
        cfg.set_trackers(hash, &tiers);
        self.readd_with_trackers(hash, &cfg);
    }

    /// How many tiers this torrent has, for the "which tier" picker.
    pub fn tracker_tier_count(&self, hash: &str) -> usize {
        self.effective_tiers(hash, &Configuration::new(self.db.clone()))
            .len()
    }

    /// Re-add a torrent so its stored extra trackers take effect.
    fn readd_with_trackers(&self, hash: &str, cfg: &Configuration) {
        let Some((bytes, folder, paused)) = self.torrent_readd_info(hash) else {
            return;
        };
        let Some(handle) = self.find(hash) else {
            return;
        };
        let tiers = cfg.tracker_tiers(hash);
        let trackers = cfg.extra_trackers(hash);

        let rq = self.rq();
        let events = self.events.clone();
        let id = librqbit::api::TorrentIdOrHash::Id(handle.id());

        self.rt.spawn(async move {
            // false: the files stay exactly where they are. This is a
            // re-registration, not a removal.
            if let Err(err) = rq.delete(id, false).await {
                report_error(&events, format!("Failed to update trackers: {err:#}"));
                return;
            }
            let opts = AddTorrentOptions {
                paused,
                output_folder: Some(folder),
                overwrite: true,
                trackers: Some(trackers),
                // The stored list is the whole announce list, not additions to
                // the file's - which is what lets a tracker be removed at all.
                replace_trackers: true,
                // ...and the grouping with it, so the announce order the user
                // arranged is the one the engine uses.
                tracker_tiers: Some(tiers),
                ..Default::default()
            };
            if let Err(err) = rq
                .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
                .await
            {
                report_error(&events, format!("Failed to re-add torrent: {err:#}"));
            }
        });
    }

    /// Announce to every tracker again, now.
    ///
    /// The announce is part of building a torrent's peer stream, which happens
    /// when it goes live - so this stops and starts it, which is what makes the
    /// announce happen rather than a request sent on the side. librqbit has no
    /// standalone "announce now", and adding one would mean reaching into the
    /// tracker loop's timer; this reaches the same place through the front door.
    ///
    /// A paused torrent is left alone. It is not announcing, and starting it
    /// because someone asked for a reannounce would be answering a different
    /// question - a noisy one, on a torrent they had deliberately stopped.
    pub fn reannounce(&self, hash: &str) {
        let Some(handle) = self.find(hash) else {
            return;
        };
        if matches!(handle.stats().state, TorrentStatsState::Paused) {
            tracing::debug!("not reannouncing {hash}: it is paused");
            return;
        }

        if let Err(err) = self.rt.block_on(async {
            self.rq().pause(&handle).await?;
            self.rq().unpause(&handle).await
        }) {
            self.push_error(format!("Failed to reannounce: {err:#}"));
            return;
        }
        tracing::info!("reannounced {hash}");
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

    /// Move a torrent within the queue.
    ///
    /// The queue is what `active_limit` and friends work down: position 0 is
    /// started first and stopped last. Until now it could only be reordered by
    /// removing and re-adding, which is not a reordering so much as a
    /// workaround.
    ///
    /// Positions are renumbered from scratch afterwards rather than swapped in
    /// place, so a list that has drifted - two torrents on the same number
    /// after some earlier removal - comes out consistent instead of preserving
    /// the drift.
    pub fn move_in_queue(&self, hash: &str, to: QueueMove) {
        {
            let mut meta = self.meta.lock().unwrap();
            let mut order: Vec<String> = meta.keys().cloned().collect();
            order.sort_by_key(|h| meta[h].queue_position);

            let Some(at) = order.iter().position(|h| h == hash) else {
                return;
            };
            let target = queue_target(at, order.len(), to);
            if target == at {
                return;
            }

            let moved = order.remove(at);
            order.insert(target, moved);
            for (i, h) in order.iter().enumerate() {
                if let Some(m) = meta.get_mut(h) {
                    m.queue_position = i as i64;
                }
            }
        }
        // Writes the new numbers out, and is also what keeps this correct when
        // the list had duplicates to begin with.
        self.normalize_queue_positions();
    }

    /// Set one torrent's own ratio limit, or clear it.
    ///
    /// `None` writes NULL, which the share-limit guard reads as "follow the
    /// global setting" - deliberately distinct from `Some(0.0)`, which is this
    /// torrent saying it has no limit at all. A single number could not carry
    /// both meanings, which is why the column is nullable.
    pub fn set_ratio_limit(&self, hash: &str, limit: Option<f64>) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "update torrent set ratio_limit = ?1 where info_hash = ?2",
                rusqlite::params![limit, hash],
            )
        });
        tracing::debug!(
            "{hash} ratio limit -> {}",
            limit.map_or(String::from("global"), |r| format!("{r:.2}"))
        );
    }

    /// One torrent's own share limits, as (ratio, minutes of seeding).
    ///
    /// `None` in either means it follows the global setting; `Some(0.0)` /
    /// `Some(0)` mean this torrent has no limit at all. A caller showing these
    /// in a form has to keep the two apart - which is why they are read back
    /// rather than inferred from the global values.
    pub fn share_overrides(&self, hash: &str) -> (Option<f64>, Option<i64>) {
        self.db
            .with(|conn| {
                conn.query_row(
                    "select ratio_limit, seed_time_limit from torrent where info_hash = ?1",
                    [hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .unwrap_or((None, None))
    }

    /// The other half of the same override: minutes of seeding, or `None` to
    /// follow the global setting. `read_share_overrides` already reads this
    /// column - until now nothing wrote it.
    pub fn set_seed_time_limit(&self, hash: &str, limit: Option<i64>) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "update torrent set seed_time_limit = ?1 where info_hash = ?2",
                rusqlite::params![limit, hash],
            )
        });
        tracing::debug!(
            "{hash} seed time limit -> {}",
            limit.map_or(String::from("global"), |m| format!("{m}m"))
        );
    }

    /// Read the stored priorities for one torrent, as a map of file index to
    /// level. Files nobody has touched are absent and count as Normal.
    pub fn file_priorities(&self, hash: &str) -> HashMap<usize, i64> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "select file_index, priority from torrent_file_priority \
                     where info_hash = ?1",
                )?;
                let rows = stmt
                    .query_map([hash], |r| {
                        Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<HashMap<_, _>>>()?;
                Ok(rows)
            })
            .unwrap_or_default()
    }

    /// Set one file's priority and apply the result to the engine.
    ///
    /// Two things come out of the same table. Anything at [`PRIORITY_SKIP`] is
    /// left out of `only_files`, which is the include toggle the Files tab has
    /// always had; everything else becomes an ordering, most wanted first,
    /// which is what patch 0017 added a way to hand over.
    pub fn set_file_priority(&self, hash: &str, file_index: usize, priority: i64) {
        let priority = priority.clamp(PRIORITY_SKIP, PRIORITY_MAX);
        let _ = self.db.with(|conn| {
            match priority {
                // Normal is the default, so it is stored by NOT storing it -
                // a torrent nobody has fiddled with costs no rows at all.
                PRIORITY_NORMAL => conn.execute(
                    "delete from torrent_file_priority where info_hash = ?1 and file_index = ?2",
                    rusqlite::params![hash, file_index as i64],
                ),
                _ => conn.execute(
                    "insert into torrent_file_priority (info_hash, file_index, priority) \
                     values (?1, ?2, ?3) \
                     on conflict(info_hash, file_index) do update set priority = ?3",
                    rusqlite::params![hash, file_index as i64, priority],
                ),
            }
        });
        tracing::debug!(
            "{hash} file {file_index} set to {}",
            priority_name(priority)
        );
        self.apply_file_priorities(hash);
    }

    /// Push the stored priorities into the engine.
    ///
    /// Called after a change and again when a torrent starts, because the
    /// ordering lives in the live state and a torrent that was paused has none.
    pub fn apply_file_priorities(&self, hash: &str) {
        let Some(handle) = self.find(hash) else {
            return;
        };
        let Some(metadata) = handle.metadata.load_full() else {
            // A magnet with no info dictionary yet has no files to order.
            return;
        };
        let count = metadata.file_infos.len();
        let stored = self.file_priorities(hash);

        // Skipped files come out of the download entirely. Everything else
        // stays in, whatever its level.
        let wanted: std::collections::HashSet<usize> = (0..count)
            .filter(|i| stored.get(i).copied().unwrap_or(PRIORITY_NORMAL) != PRIORITY_SKIP)
            .collect();
        if wanted.len() != count
            && let Err(err) = self
                .rt
                .block_on(self.rq().update_only_files(&handle, &wanted))
        {
            self.push_error(format!("Failed to apply file selection: {err:#}"));
            return;
        }

        handle.set_file_priorities(priority_order(count, &stored));
    }

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
    /// A label's id from its name, case-insensitively. `None` when no label
    /// by that name exists - callers decide whether that is worth saying.
    pub fn label_id(&self, name: &str) -> Option<i32> {
        Configuration::new(self.db.clone())
            .get_labels()
            .into_iter()
            .find(|l| l.name.eq_ignore_ascii_case(name))
            .map(|l| l.id)
    }

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
                    info_hashes: None,
                });

                // Completion is detected by the scan task in `new`, not here:
                // this function is called only when something asks for the list,
                // which on a headless build may be never.
                meta.prev_finished = Some(stats.finished);

                // Record the completion timestamp for the "Completed On" column.
                if stats.finished && meta.completed_on.is_none() {
                    meta.completed_on = Some(Local::now());
                    completed_now.push(hash.clone());
                }

                let has_metadata = handle.metadata.load().is_some();
                let paused = matches!(stats.state, TorrentStatsState::Paused);

                let state = match stats.state {
                    TorrentStatsState::Initializing { .. } => State::CheckingFiles,
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

                // Computed on the first tick that has metadata, then reused.
                // A magnet has no info dict until it resolves, so this stays
                // None and the panel falls back to showing the id.
                if meta.info_hashes.is_none()
                    && let Ok(computed) =
                        handle.with_metadata(|m| metainfo::info_hashes(&m.info_bytes))
                {
                    meta.info_hashes = Some(computed);
                }
                let (info_hash_v1, info_hash_v2) = meta.info_hashes.clone().unwrap_or_default();

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
                    .map(|m| m.lengths().total_pieces() as u64)
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
                    info_hash_v1,
                    info_hash_v2,
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

    /// Listen for torrent lifecycle events until the receiver is dropped.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Pause everything before the disk fills, if the setting asks for it.
    ///
    /// This exists because running out of space is not a recoverable error
    /// part-way through: librqbit logs a write failure per file per torrent and
    /// keeps going, so a full disk produces hundreds of log lines, a torrent
    /// whose data is now wrong, and - if the session index happens to be
    /// rewritten at the wrong moment - a profile that will not start at all.
    /// Stopping early is the only cheap defence.
    ///
    /// The limit is a percentage of the volume holding the default save path,
    /// which is what PicoTorrent's setting meant. Torrents saved elsewhere are
    /// not checked; per-torrent volumes would mean one stat per torrent per
    /// tick for a case nobody has asked for.
    ///
    /// ponytail: polls every 30s. A filesystem watch would be prompter but is
    /// three platform implementations for a threshold nobody sits exactly on;
    /// 30s is well inside the time it takes to write a torrent's worth of data.
    /// Keep the live rate limits in step with the alternative-limits switch
    /// and the schedule.
    ///
    /// librqbit takes new limits on the running session, so this changes speed
    /// without the rebuild that `apply_settings` does - which matters because a
    /// schedule flips twice a day and rebuilding would drop every connection
    /// each time.
    ///
    /// Settings are re-read every tick rather than captured at spawn: the
    /// toolbar toggle writes the setting and expects the effect immediately,
    /// and a rebuild is exactly what this exists to avoid.
    fn spawn_speed_scheduler(&self) {
        let inner = self.inner.clone();
        let db = self.db.clone();

        self.rt.spawn(async move {
            let cfg = Configuration::new(db);
            // What was last pushed into the session, so an unchanged answer
            // costs one comparison instead of a write per second.
            let mut applied: Option<(Option<u32>, Option<u32>)> = None;

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                let wanted = crate::bittorrent::limits::current_rates(&cfg, Local::now());
                if applied == Some(wanted) {
                    continue;
                }
                applied = Some(wanted);

                let (down, up) = wanted;
                let rq = inner.read().unwrap().clone();
                rq.ratelimits.set_download_bps(down.and_then(NonZeroU32::new));
                rq.ratelimits.set_upload_bps(up.and_then(NonZeroU32::new));
                tracing::info!(
                    "rate limits now {} down / {} up",
                    down.map_or(String::from("unlimited"), |b| format!("{} B/s", b)),
                    up.map_or(String::from("unlimited"), |b| format!("{} B/s", b))
                );
            }
        });
    }

    /// Stop seeding torrents that have met their share limit.
    ///
    /// Every ten seconds rather than every second: a ratio does not move fast
    /// enough to care, and the action taken can delete files, so the cheaper
    /// mistake is to act a moment late.
    ///
    /// Only torrents that have finished are considered. A torrent still
    /// downloading can already be over its ratio - a re-added one starts with
    /// its old upload total - and stopping it before it has the data would be
    /// the opposite of what the setting asks for.
    fn spawn_share_limit_guard(&self) {
        let inner = self.inner.clone();
        let db = self.db.clone();
        let meta = self.meta.clone();
        let events = self.events.clone();

        self.rt.spawn(async move {
            let cfg = Configuration::new(db.clone());

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;

                let global = crate::bittorrent::limits::ShareLimits::global(&cfg);
                // Nothing configured globally is the common case, and reading
                // every torrent's overrides to discover that would be waste -
                // but a torrent may still carry its own limit, so this only
                // skips the work when the table is empty too.
                let overrides = read_share_overrides(&db);
                if global.ratio.is_none() && global.seed_minutes.is_none() && overrides.is_empty() {
                    continue;
                }

                let rq = inner.read().unwrap().clone();
                let now = Local::now();

                // Collected before acting: removing a torrent while iterating
                // the session's own list is asking for trouble.
                // A cell because `with_torrents` hands out a `Fn`, so the
                // closure cannot own a mutable borrow of the list it fills.
                let done: std::cell::RefCell<Vec<(String, crate::bittorrent::limits::ShareAction)>> =
                    std::cell::RefCell::new(Vec::new());
                rq.with_torrents(|torrents| {
                    for (_, handle) in torrents {
                        let stats = handle.stats();
                        if !stats.finished
                            || matches!(stats.state, TorrentStatsState::Paused)
                        {
                            continue;
                        }
                        let hash = handle.info_hash().as_string();

                        let ratio = match stats.progress_bytes {
                            0 => 0.0,
                            downloaded => stats.uploaded_bytes as f64 / downloaded as f64,
                        };
                        // Seeding time is measured from completion, which is
                        // what the column records. A torrent that has never
                        // completed in this profile has none, and is left alone
                        // rather than treated as having seeded forever.
                        let seeded = meta
                            .lock()
                            .unwrap()
                            .get(&hash)
                            .and_then(|m| m.completed_on)
                            .map(|at| (now - at).num_minutes());

                        let limits = match overrides.get(&hash) {
                            Some((ratio_limit, time_limit)) => {
                                global.with_overrides(*ratio_limit, *time_limit)
                            }
                            None => global,
                        };
                        if limits.reached(ratio, seeded.unwrap_or(0)) {
                            done.borrow_mut().push((hash, limits.action));
                        }
                    }
                });

                for (hash, action) in done.into_inner() {
                    apply_share_action(&rq, &db, &meta, &events, &hash, action).await;
                }
            }
        });
    }

    fn spawn_low_disk_guard(&self, cfg: &Configuration) {
        let enabled = cfg.get_bool("pause_on_low_disk_space");
        if !enabled {
            return;
        }
        // Below 0 or above 100 can never trigger or always would; treat a
        // nonsense value as the default rather than acting on it.
        let limit = cfg
            .get_int("pause_on_low_disk_space_limit")
            .filter(|p| (0..=100).contains(p))
            .unwrap_or(5) as f64;

        let path = cfg
            .get_string("default_save_path")
            .map(PathBuf::from)
            .unwrap_or_else(Environment::get_downloads_path);

        let inner = self.inner.clone();
        let events = self.events.clone();

        self.rt.spawn(async move {
            // Edge-triggered: pause once on the way down, and only arm again
            // once there is room. Without this it would re-pause and re-toast
            // every 30 seconds for as long as the disk stayed full.
            let mut tripped = false;

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                // None is "cannot tell" - a removable drive, a path that has
                // gone away. Never pause on that.
                let Some(free) = crate::core::utils::free_space_percent(&path) else {
                    continue;
                };

                if free >= limit {
                    tripped = false;
                    continue;
                }
                if tripped {
                    continue;
                }
                tripped = true;

                let rq = inner.read().unwrap().clone();
                let running = running_hashes(&rq);
                if running.is_empty() {
                    continue;
                }

                for hash in &running {
                    if let Ok(id) = librqbit::api::TorrentIdOrHash::parse(hash)
                        && let Some(handle) = rq.get(id)
                    {
                        let _ = rq.pause(&handle).await;
                    }
                }

                // Same channel as every other background failure, so it
                // reaches the UI as a toast and the web API's /errors.
                report_error(
                    &events,
                    format!(
                        "Only {free:.1}% free on {} (limit {limit:.0}%) - paused {} torrent(s).",
                        path.display(),
                        running.len(),
                    ),
                );
            }
        });
    }

    /// Poll the live torrent set once a second and emit what changed.
    ///
    /// Diffing rather than hooking each mutation: `add_torrent`, `remove`, the
    /// web API and a plugin calling in all end up here, and a torrent that
    /// librqbit finishes on its own has no call site to hook in the first
    /// place.
    ///
    /// ponytail: a 1s poll over every handle. Fine for the hundreds of torrents
    /// a desktop client holds; if someone runs thousands headless, have
    /// librqbit push state changes instead of walking the set.
    fn spawn_scan_task(&self) {
        let inner = self.inner.clone();
        let events = self.events.clone();
        let meta = self.meta.clone();

        self.rt.spawn(async move {
            // Owned by the task, not by `meta`: this is "what the previous tick
            // saw", which is nobody else's business and must not be confused
            // with the persisted per-torrent metadata.
            let mut live: HashMap<String, String> = HashMap::new();
            let mut finished: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Torrents that had already completed before this run started.
            //
            // `primed` alone is not enough. It skips the FIRST tick, but a
            // torrent restored from the previous run reports finished=false
            // while librqbit verifies it, and flips to true a tick or two
            // later - which is indistinguishable from a real completion. On a
            // client holding a shelf of finished torrents that is a burst of
            // "download complete" notifications on every launch.
            //
            // Read once, here, rather than consulted per tick: the stats loop
            // writes `completed_on` the moment it sees a torrent finish, so a
            // genuine first completion would race it and be swallowed too.
            //
            // Each entry suppresses exactly ONE completion - the one that ends
            // startup verification - and is then dropped, so a later recheck
            // still announces when it finishes again.
            let mut preexisting: std::collections::HashSet<String> = meta
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, m)| m.completed_on.is_some())
                .map(|(hash, _)| hash.clone())
                .collect();
            let mut primed = false;

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                // Re-read every tick: applying preferences swaps the whole
                // librqbit session out from under us.
                let rq = inner.read().unwrap().clone();
                let snapshot: Vec<(String, String, bool)> = rq.with_torrents(|torrents| {
                    torrents
                        .map(|(_, handle)| {
                            let hash = handle.info_hash().as_string();
                            let name = handle.name().unwrap_or_else(|| hash.clone());
                            (hash, name, handle.stats().finished)
                        })
                        .collect()
                });

                let mut seen: HashMap<String, String> = HashMap::new();
                for (hash, name, is_finished) in snapshot {
                    // The first tick establishes the baseline. Without this
                    // every torrent restored from the previous run would look
                    // newly added, and anything already complete would raise a
                    // completion on every launch.
                    if primed {
                        if !live.contains_key(&hash) {
                            events.emit(SessionEvent::TorrentAdded {
                                hash: hash.clone(),
                                name: name.clone(),
                            });
                        }
                        if is_finished && !finished.contains(&hash) {
                            if preexisting.remove(&hash) {
                                tracing::debug!(
                                    "{name} finished verifying; it was already complete"
                                );
                            } else {
                                tracing::info!("torrent finished: {name}");
                                events.emit(SessionEvent::TorrentCompleted {
                                    hash: hash.clone(),
                                    name: name.clone(),
                                });
                            }
                        }
                    }

                    if is_finished {
                        finished.insert(hash.clone());
                    } else {
                        // A recheck can un-finish a torrent; let it complete again.
                        finished.remove(&hash);
                    }
                    seen.insert(hash, name);
                }

                if primed {
                    for (hash, name) in &live {
                        if !seen.contains_key(hash) {
                            events.emit(SessionEvent::TorrentRemoved {
                                hash: hash.clone(),
                                name: name.clone(),
                            });
                            finished.remove(hash);
                        }
                    }
                }

                live = seen;
                primed = true;
            }
        });
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

        // BEP 47 padding files are alignment, not content: a run of zeros so
        // the next real file starts on a piece boundary. Hybrid torrents carry
        // them by construction and the v2-only path inserts them, so they turn
        // up in perfectly ordinary torrents. Hidden unless someone has asked
        // to see them - nothing is ever downloaded for one either way.
        let show_padding = crate::core::configuration::Configuration::new(self.db.clone())
            .get_bool("ui.show_padding_files");

        metadata
            .file_infos
            .iter()
            .enumerate()
            .filter(|(_, fi)| show_padding || !fi.attrs.padding)
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

        // Connected peers grouped by what found them (patch 0019). A paused
        // torrent has no live state and therefore no counts, which is right:
        // the numbers describe connections, and it has none.
        let total_pieces = handle
            .metadata
            .load_full()
            .map(|m| m.lengths().total_pieces() as u64)
            .unwrap_or(0);
        let by_source = match handle.live() {
            Some(live) if total_pieces > 0 => live.peer_counts_by_source(total_pieces),
            _ => Vec::new(),
        };
        // (seeds, leeches) for one source, or None when nothing is connected
        // through it.
        let counts = |want: librqbit::PeerSource| {
            by_source
                .iter()
                .find(|(source, _, _)| *source == want)
                .map(|(_, connected, seeds)| (*seeds, connected.saturating_sub(*seeds)))
        };

        // Peer-discovery sources.
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
        rows.push(TrackerRow::source("DHT", dht_status).with_counts(counts(librqbit::PeerSource::Dht)));
        let lsd_on = crate::core::configuration::Configuration::new(self.db.clone())
            .get_bool("libtorrent.enable_lsd");
        let lsd_status = if paused {
            tr.i18n("tracker_paused")
        } else if lsd_on {
            tr.i18n("tracker_enabled")
        } else {
            tr.i18n("tracker_disabled")
        };
        rows.push(TrackerRow::source("LSD", lsd_status).with_counts(counts(librqbit::PeerSource::Lsd)));
        let pex_on = crate::core::configuration::Configuration::new(self.db.clone())
            .get_bool("libtorrent.enable_pex");
        let pex_status = if paused {
            tr.i18n("tracker_paused")
        } else if pex_on {
            tr.i18n("tracker_enabled")
        } else {
            tr.i18n("tracker_disabled")
        };
        rows.push(TrackerRow::source("PeX", pex_status).with_counts(counts(librqbit::PeerSource::Pex)));

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
                        label: url.to_string(),
                        status: tr.i18n("tracker_paused"),
                        seeders: None,
                        leechers: None,
                        fails: s.map(|s| s.fails).unwrap_or(0),
                        next_announce: None,
                    }
                } else {
                    TrackerRow {
                        kind: TrackerRowKind::Tracker,
                        label: url.to_string(),
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
        format!(
            "magnet:?xt=urn:btih:{hash}&dn={}",
            crate::core::utils::percent_encode(name.as_bytes(), b"")
        )
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

#[cfg(test)]
mod tests {

    use super::{QueueMove, queue_target};

    use super::{PRIORITY_HIGH, PRIORITY_MAX, PRIORITY_NORMAL, PRIORITY_SKIP, priority_order};
    use std::collections::HashMap;

    /// Nothing prioritised must come out exactly as it went in.
    ///
    /// Upstream sorts by filename here. Once this function is supplying the
    /// order, a torrent nobody has touched has to keep its own file order
    /// rather than being quietly resorted.
    #[test]
    fn an_untouched_torrent_keeps_its_file_order() {
        assert_eq!(priority_order(4, &HashMap::new()), vec![0, 1, 2, 3]);
    }

    /// Higher levels first; equal levels keep their original order.
    #[test]
    fn higher_priority_files_come_first() {
        let mut stored = HashMap::new();
        stored.insert(3, PRIORITY_MAX);
        stored.insert(1, PRIORITY_HIGH);
        assert_eq!(priority_order(5, &stored), vec![3, 1, 0, 2, 4]);
    }

    /// A skipped file stays on the list. It has been taken out of `only_files`
    /// already, and leaving it off here would just make the engine invent a
    /// place for it.
    #[test]
    fn skipped_files_are_still_ordered_last() {
        let mut stored = HashMap::new();
        stored.insert(0, PRIORITY_SKIP);
        stored.insert(2, PRIORITY_MAX);

        let order = priority_order(3, &stored);
        assert_eq!(order, vec![2, 1, 0]);
        assert_eq!(order.len(), 3, "a file went missing from the ordering");
    }

    /// Every file appears exactly once, whatever the levels - the picker walks
    /// this list assuming it is a complete permutation.
    #[test]
    fn the_ordering_is_always_a_permutation() {
        let mut stored = HashMap::new();
        stored.insert(0, PRIORITY_MAX);
        stored.insert(1, PRIORITY_SKIP);
        stored.insert(2, PRIORITY_NORMAL);
        // An index past the end of the torrent, which a stale row could be.
        stored.insert(99, PRIORITY_MAX);

        let mut order = priority_order(4, &stored);
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2, 3]);
    }

    /// Reordering saturates at both ends and never wraps.
    #[test]
    fn moving_within_the_queue_stops_at_the_ends() {
        // A five-row queue, moving the middle row.
        assert_eq!(queue_target(2, 5, QueueMove::Top), 0);
        assert_eq!(queue_target(2, 5, QueueMove::Up), 1);
        assert_eq!(queue_target(2, 5, QueueMove::Down), 3);
        assert_eq!(queue_target(2, 5, QueueMove::Bottom), 4);

        // At the top, up is a no-op rather than an underflow.
        assert_eq!(queue_target(0, 5, QueueMove::Up), 0);
        assert_eq!(queue_target(0, 5, QueueMove::Top), 0);

        // At the bottom, down stays put rather than wrapping to the top.
        assert_eq!(queue_target(4, 5, QueueMove::Down), 4);
        assert_eq!(queue_target(4, 5, QueueMove::Bottom), 4);
    }

    /// One row, and none at all: neither may panic on the subtraction.
    #[test]
    fn a_short_queue_has_nowhere_to_move() {
        for to in [QueueMove::Top, QueueMove::Up, QueueMove::Down, QueueMove::Bottom] {
            assert_eq!(queue_target(0, 1, to), 0, "{to:?} in a one-row queue");
            assert_eq!(queue_target(0, 0, to), 0, "{to:?} in an empty queue");
        }
    }

    /// Adding several torrents at once must add all of them.
    ///
    /// `add_torrent` spawns one task per call, so a batch fires N concurrent
    /// adds at librqbit. This drives that exact shape against a real session:
    /// if concurrent adds drop torrents, the batch Add dialog silently adds a
    /// subset and there is nothing in the log to say so.
    #[test]
    fn concurrent_adds_all_land() {
        use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse};

        let dir = std::env::temp_dir().join(format!("nt-concurrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Distinct names give distinct info hashes, which is the whole point.
        let torrents: Vec<Vec<u8>> = (0..5).map(single_file_torrent).collect();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let added = rt.block_on(async {
            // Persistence ON. This is the whole point of the test: the
            // non-persistent path hands out ids with an atomic fetch_add and
            // never had the bug, so `persistence: None` passed happily while
            // real batch adds collapsed into one. See patch 0006.
            let opts = librqbit::SessionOptions {
                dht: None,
                persistence: Some(librqbit::SessionPersistenceConfig::Json {
                    folder: Some(dir.join("state")),
                }),
                listen: None,
                ..Default::default()
            };
            let session = librqbit::Session::new_with_opts(dir.clone(), opts)
                .await
                .expect("session");

            // The shape add_torrent uses: one spawned task each, all at once.
            let mut handles = Vec::new();
            for bytes in torrents {
                let s = session.clone();
                let out = dir.clone();
                handles.push(tokio::spawn(async move {
                    let opts = AddTorrentOptions {
                        // paused:false and a shared output folder, exactly as
                        // the Add dialog sends them with "start immediately"
                        // ticked. paused:true took a different path inside
                        // librqbit and hid this.
                        paused: false,
                        output_folder: Some(out.to_string_lossy().into_owned()),
                        overwrite: true,
                        ..Default::default()
                    };
                    match s.add_torrent(AddTorrent::from_bytes(bytes), Some(opts)).await {
                        Ok(AddTorrentResponse::Added(..))
                        | Ok(AddTorrentResponse::AlreadyManaged(..)) => Ok(()),
                        Ok(AddTorrentResponse::ListOnly(_)) => Err("list only".to_string()),
                        Err(e) => Err(format!("{e:#}")),
                    }
                }));
            }

            let mut failures = Vec::new();
            for h in handles {
                if let Ok(Err(e)) = h.await {
                    failures.push(e);
                }
            }
            let count = session.with_torrents(|t| t.count());
            (count, failures)
        });

        let _ = std::fs::remove_dir_all(&dir);
        let (count, failures) = added;
        assert!(failures.is_empty(), "adds failed: {failures:?}");
        assert_eq!(count, 5, "only {count} of 5 concurrent adds landed");
    }

    /// A paused torrent must leave the disk alone.
    ///
    /// Restoring a session adds every torrent paused and resumes the ones that
    /// were running, so anything this path touches, it touches for the whole
    /// list on every start - including torrents whose data the user has since
    /// deleted or moved to another drive.
    #[test]
    fn a_paused_add_creates_no_files() {
        use librqbit::{AddTorrent, AddTorrentOptions};

        let dir = std::env::temp_dir().join(format!("nt-paused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bytes = single_file_torrent(0);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let opts = librqbit::SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                ..Default::default()
            };
            let session = librqbit::Session::new_with_opts(dir.clone(), opts)
                .await
                .expect("session");
            let add = AddTorrentOptions {
                paused: true,
                output_folder: Some(dir.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            };
            session
                .add_torrent(AddTorrent::from_bytes(bytes), Some(add))
                .await
                .expect("add");
            // The initial check runs on a spawned task, so give it its turn.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        });

        let created = dir.join("file-0.bin").exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !created,
            "a paused torrent created its files - nothing should be written until it starts"
        );
    }

    /// ...and starting it must create them after all.
    ///
    /// The other half of the deferral: a torrent whose storage was never
    /// initialized has to initialize it when it starts, or it would run with no
    /// files to write into.
    #[test]
    fn starting_a_paused_torrent_creates_its_files() {
        use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse};

        let dir = std::env::temp_dir().join(format!("nt-unpause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bytes = single_file_torrent(0);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let created = rt.block_on(async {
            let opts = librqbit::SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                ..Default::default()
            };
            let session = librqbit::Session::new_with_opts(dir.clone(), opts)
                .await
                .expect("session");
            let add = AddTorrentOptions {
                paused: true,
                output_folder: Some(dir.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            };
            let handle = match session
                .add_torrent(AddTorrent::from_bytes(bytes), Some(add))
                .await
                .expect("add")
            {
                AddTorrentResponse::Added(_, h) => h,
                _ => panic!("the torrent was not added"),
            };
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            assert!(
                !dir.join("file-0.bin").exists(),
                "the file was created while still paused"
            );

            session.unpause(&handle).await.expect("unpause");
            // Initialization and the check it skipped both run on a spawned
            // task, so give them their turn.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            dir.join("file-0.bin").exists()
        });

        let _ = std::fs::remove_dir_all(&dir);
        assert!(created, "starting the torrent did not create its files");
    }

    /// A single-file torrent whose name (and therefore info hash) varies with
    /// `n`, so a batch of them is a batch of genuinely different torrents.
    fn single_file_torrent(n: usize) -> Vec<u8> {
        let name = format!("file-{n}.bin");
        let mut out = Vec::new();
        out.extend_from_slice(b"d4:infod6:lengthi4096e4:name");
        out.extend_from_slice(name.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b"12:piece lengthi262144e6:pieces20:");
        out.extend_from_slice(&[0u8; 20]);
        out.extend_from_slice(b"ee");
        out
    }

    /// A 0-byte session.json used to make the app unstartable. It must be
    /// moved aside, a healthy one must be left strictly alone, and the
    /// .torrent files beside it must survive either way.
    #[test]
    fn corrupt_session_index_is_quarantined_not_deleted() {
        use super::quarantine_unreadable_session_index as quarantine;

        // std::env::temp_dir, not the tempfile crate: the other tests here do
        // the same and it is not worth a dependency.
        let dir = std::env::temp_dir().join(format!("nt-session-idx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.as_path();
        let index = p.join("session.json");
        let torrent = p.join("abc123.torrent");
        std::fs::write(&torrent, b"d4:infod4:name3:isoee").unwrap();

        // Nothing there at all: librqbit handles that, so do not interfere.
        quarantine(p);
        assert!(!index.exists());

        // The reported failure: an empty file.
        std::fs::write(&index, b"").unwrap();
        quarantine(p);
        assert!(!index.exists(), "empty index should be moved aside");
        let saved: Vec<_> = std::fs::read_dir(p)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt-"))
            .collect();
        assert_eq!(saved.len(), 1, "kept as evidence, not deleted");
        assert!(torrent.exists(), "torrent files must be untouched");

        // Truncated mid-write is the same class of failure.
        std::fs::write(&index, b"{\"torrents\":{\"1\":").unwrap();
        quarantine(p);
        assert!(!index.exists(), "unparseable index should be moved aside");

        // A healthy index must survive untouched - this runs on every start.
        let good = b"{\"torrents\":{}}";
        std::fs::write(&index, good).unwrap();
        quarantine(p);
        assert_eq!(std::fs::read(&index).unwrap(), good, "valid index kept");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two ways this client names itself on the wire must agree on the
    /// version, and neither may panic on a version the project could actually
    /// reach. `-NT0200-` and `NanoTorrent 0.2.0` are the same identity in the
    /// peer id's encoding and BEP 10's.
    #[test]
    fn client_identifies_itself_consistently() {
        use super::azureus_peer_id;

        let id = azureus_peer_id("0.2.0");
        assert_eq!(&id.0[..8], b"-NT0200-", "0.2.0 peer id prefix");

        // One character per component from a 64-entry alphabet, so 10 is 'A'.
        assert_eq!(&azureus_peer_id("0.2.10").0[..8], b"-NT02A0-");
        assert_eq!(&azureus_peer_id("1.0.0").0[..8], b"-NT1000-");

        // Past the alphabet librqbit unwraps a None. Clamped, not panicking.
        assert_eq!(&azureus_peer_id("0.2.64").0[..8], b"-NT02-0-");
        azureus_peer_id("0.2.255");
        azureus_peer_id("not.a.version");

        // The BEP 10 string carries the same version the peer id encodes.
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(crate::buildinfo::client_id(), format!("NanoTorrent {v}"));
        assert_eq!(&azureus_peer_id(v).0[..3], b"-NT");
    }

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

    /// End-to-end: build each of the three shapes with our own writer, then
    /// feed them to the reader the Add dialog uses.
    ///
    /// This used to assert that v2-only was *refused* with a readable message.
    /// It is now supported, so the assertion is the opposite one - and the
    /// dialog reading it matters as much as the session accepting it, because
    /// a dialog that refuses what the session would take is the same bug seen
    /// from the other side.
    #[test]
    fn every_torrent_shape_reaches_the_add_dialog() {
        let dir = std::env::temp_dir().join("nanotorrent-v2-msg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let payload = dir.join("payload.bin");
        std::fs::write(&payload, vec![7u8; 300 * 1024]).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        use crate::bittorrent::torrent_create::TorrentVersion as V;

        let build = |v| {
            let out = dir.join("t.torrent");
            let outcome = rt
                .block_on(super::build_torrent(super::CreateTorrentParams {
                    source: payload.clone(),
                    trackers: vec![],
                    comment: String::new(),
                    private: false,
                    piece_length: Some(256 * 1024),
                    version: v,
                    output: out,
                    add_to_session: false,
                }))
                .unwrap();
            let super::CreateTorrentOutcome::Created { bytes, .. } = outcome else {
                panic!("expected Created")
            };
            bytes
        };

        // All three read, and all three agree on what is in the torrent -
        // the same file, the same size - whichever half the reader used.
        for (label, version) in [("v1", V::V1), ("hybrid", V::Hybrid), ("v2", V::V2)] {
            let parsed = crate::ui::torrentfile::parse(&build(version))
                .unwrap_or_else(|e| panic!("{label} did not reach the dialog: {e}"));
            assert_eq!(parsed.name, "payload.bin", "{label} name");
            assert_eq!(parsed.total_size, 300 * 1024, "{label} size");
            assert_eq!(parsed.files.len(), 1, "{label} file count");
        }

        // And each shape is prepared the right way round.
        use crate::bittorrent::v2::V2Prep;

        let v2_bytes = build(V::V2);
        match crate::bittorrent::v2::prepare(&v2_bytes).unwrap() {
            V2Prep::V2Only(p) => {
                assert_eq!(p.files, 1);
                assert_eq!(p.pieces, 2, "300 KiB over 256 KiB pieces");
            }
            _ => panic!("v2-only was not recognised"),
        }

        // A hybrid keeps its v1 half - but must also carry the second hash, or
        // it only ever joins half of its swarm.
        match crate::bittorrent::v2::prepare(&build(V::Hybrid)).unwrap() {
            V2Prep::Hybrid { secondary, .. } => {
                let meta = crate::bittorrent::v2::parse(&build(V::Hybrid))
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    secondary.0,
                    meta.truncated_info_hash(),
                    "the secondary hash is not the truncated v2 one"
                );
            }
            _ => panic!("a hybrid was not recognised as one"),
        }

        assert!(
            matches!(
                crate::bittorrent::v2::prepare(&build(V::V1)).unwrap(),
                V2Prep::V1Only
            ),
            "a v1 torrent was treated as something else"
        );
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

        let parsed = librqbit::torrent_from_bytes(&written).unwrap();
        assert_eq!(
            parsed.announce.as_ref().map(|a| a.as_ref()),
            Some(&b"http://tracker.example.com/announce"[..])
        );
        assert_eq!(parsed.announce_list.len(), 2);
        assert!(parsed.info.data.private);
        assert_eq!(
            parsed.comment.as_ref().map(|c| c.as_ref()),
            Some(&b"test comment"[..])
        );
        assert_eq!(parsed.info.data.piece_length, 256 * 1024);

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

        // Inside the async block, not outside it: BlockingSpawner::new reads
        // the current runtime handle and panics if there isn't one.
        let torrent = rt
            .block_on(async {
                librqbit::create_torrent(
                    &payload,
                    librqbit::CreateTorrentOptions {
                        name: Some("nanotorrent-test"),
                        ..Default::default()
                    },
                    &librqbit::spawn_utils::BlockingSpawner::new(1),
                )
                .await
            })
            .unwrap();

        std::fs::write(
            dir.join("nanotorrent-test.torrent"),
            torrent.as_bytes().unwrap(),
        )
        .unwrap();
    }

    /// The bus has no unsubscribe: a subscriber leaves by dropping its
    /// receiver, and the next emit is what notices. Worth pinning, because the
    /// alternative failure is a sender kept forever for a receiver that is gone.
    #[test]
    fn events_reach_every_subscriber_and_dropped_ones_unregister() {
        use super::{EventBus, SessionEvent};

        let bus = EventBus::new();
        let first = bus.subscribe();
        let second = bus.subscribe();

        bus.emit(SessionEvent::Error("one".into()));
        assert!(matches!(first.try_recv(), Ok(SessionEvent::Error(m)) if m == "one"));
        assert!(matches!(second.try_recv(), Ok(SessionEvent::Error(m)) if m == "one"));

        drop(second);
        bus.emit(SessionEvent::Error("two".into()));

        assert!(matches!(first.try_recv(), Ok(SessionEvent::Error(m)) if m == "two"));
        assert_eq!(
            bus.0.lock().unwrap().len(),
            1,
            "the dropped subscriber should have been pruned on emit"
        );
    }
}

#[cfg(test)]
mod rarest_first_tests {
    use librqbit::pick_rarest;

    /// The queue as the engine yields it: (file rank, piece), rank ascending.
    fn queue(pieces: &[(usize, u32)]) -> impl Iterator<Item = (usize, u32)> + '_ {
        pieces.iter().copied()
    }

    /// The whole point. Everything else here guards a way of getting it wrong.
    #[test]
    fn the_rarest_piece_the_peer_can_serve_is_chosen() {
        let q = [(0, 0u32), (0, 1), (0, 2), (0, 3)];
        // Piece 2 is held by one peer, the rest by many.
        let avail = |p: u32| match p {
            2 => 1,
            _ => 9,
        };
        assert_eq!(pick_rarest(queue(&q), |_| true, avail), Some(2));
    }

    /// A rare piece the peer does not have is not a candidate.
    #[test]
    fn rarity_never_overrides_what_the_peer_actually_has() {
        let q = [(0, 0u32), (0, 1), (0, 2)];
        let avail = |p: u32| match p {
            2 => 1,
            1 => 4,
            _ => 9,
        };
        // The peer has everything except the rarest.
        assert_eq!(pick_rarest(queue(&q), |p| p != 2, avail), Some(1));
        // And when it has nothing, there is no candidate at all.
        assert_eq!(pick_rarest(queue(&q), |_| false, avail), None);
    }

    /// File priority outranks rarity: a rare piece in a less wanted file must
    /// not jump ahead of a common one in a more wanted file. This is the
    /// property that makes "download this file first" mean anything.
    #[test]
    fn file_priority_beats_rarity() {
        let q = [(0, 0u32), (0, 1), (1, 2), (1, 3)];
        let avail = |p: u32| match p {
            3 => 1, // rarest in the whole torrent, but rank 1
            0 => 8,
            1 => 5,
            _ => 7,
        };
        // Rank 0 wins outright; within it, the rarer of the two.
        assert_eq!(pick_rarest(queue(&q), |_| true, avail), Some(1));
    }

    /// ...but rarity still decides inside the lower rank once the higher one
    /// has nothing to give.
    #[test]
    fn rarity_decides_within_the_first_rank_that_has_a_candidate() {
        let q = [(0, 0u32), (0, 1), (1, 2), (1, 3)];
        let avail = |p: u32| match p {
            3 => 2,
            2 => 6,
            _ => 1,
        };
        // The peer has nothing from rank 0.
        assert_eq!(pick_rarest(queue(&q), |p| p >= 2, avail), Some(3));
    }

    /// Equal rarity keeps the order the engine produced - which is first
    /// piece, last piece, then the middle. Streaming depends on it, and it is
    /// the common case: in a healthy swarm every piece is equally available.
    #[test]
    fn ties_keep_the_engines_own_order() {
        // 0..5 comes out of the engine as 0, 4, 1, 2, 3.
        let q = [(0, 0u32), (0, 4), (0, 1), (0, 2), (0, 3)];
        assert_eq!(pick_rarest(queue(&q), |_| true, |_| 5), Some(0));
        // With the first piece unavailable, the last is next - not piece 1.
        assert_eq!(pick_rarest(queue(&q), |p| p != 0, |_| 5), Some(4));
    }

    /// No availability data yet - a torrent that has just started, before the
    /// first refresh - must behave exactly as the engine did before this
    /// existed: the first candidate in queue order.
    #[test]
    fn no_availability_data_reproduces_the_old_order() {
        let q = [(0, 0u32), (0, 4), (0, 1), (0, 2), (0, 3)];
        assert_eq!(pick_rarest(queue(&q), |_| true, |_| 0), Some(0));
        assert_eq!(pick_rarest(queue(&q), |p| p >= 2, |_| 0), Some(4));
    }

    /// An empty queue is not a panic.
    #[test]
    fn an_empty_queue_yields_nothing() {
        let q: [(usize, u32); 0] = [];
        assert_eq!(pick_rarest(queue(&q), |_| true, |_| 1), None);
    }

    /// The early exit must not change the answer.
    ///
    /// Scanning stops at the first piece only one peer holds, and stops
    /// looking at later ranks once any candidate is found. Both are
    /// optimisations, so both have to agree with an exhaustive search.
    #[test]
    fn the_early_exits_agree_with_an_exhaustive_search() {
        // A deterministic pseudo-random spread, so this covers a lot of shapes
        // without a dependency on a random generator.
        for seed in 0..200u32 {
            let mut q: Vec<(usize, u32)> = Vec::new();
            for i in 0..12u32 {
                let rank = ((seed / (i + 1)) % 3) as usize;
                q.push((rank, i));
            }
            // Ranks must ascend, which is what the engine guarantees.
            q.sort_by_key(|(rank, _)| *rank);

            let avail = |p: u32| ((seed.wrapping_mul(7) + p * 13) % 5) + 1;
            let has = |p: u32| (seed.wrapping_add(p)) % 4 != 0;

            let got = pick_rarest(q.iter().copied(), has, avail);

            // The exhaustive answer: lowest rank with a candidate, then lowest
            // availability, then earliest in the queue.
            let want = q
                .iter()
                .copied()
                .filter(|(_, p)| has(*p))
                .min_by_key(|(rank, p)| (*rank, avail(*p)))
                .map(|(_, p)| p);
            assert_eq!(got, want, "seed {seed} disagreed");
        }
    }
}

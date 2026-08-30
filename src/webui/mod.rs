//! Optional HTTP interface to the session.
//!
//! Off by default. When enabled it is the *only* interface on Linux and macOS
//! until the cross-platform UI lands, and a remote control alongside the Win32
//! window elsewhere.
//!
//! # Why Actix rather than axum
//!
//! Not routing taste - the connection layer. `actix-http` enforces a
//! `client_request_timeout` by default (slowloris on the header phase) and
//! `actix-server` caps concurrent connections; `axum::serve` wires up neither,
//! and hyper's equivalents are opt-in and assembled by hand in your own accept
//! loop. Those protections live inside actix-http's dispatcher, which owns the
//! socket, codec and timers together, so they cannot be lifted out and bolted
//! onto hyper - it is an either/or, and this is the safer default.
//!
//! # Threading
//!
//! Actix wants its own `System` (current-thread runtimes per worker), and the
//! session already owns a multi-threaded tokio runtime. They do not merge, so
//! the server gets a dedicated thread.
//!
//! That matters for every handler: `Session`'s API is synchronous with
//! `Runtime::block_on` inside it, and calling that from any async runtime
//! thread panics. **Every** call into `Session` therefore goes through
//! `web::block`, which hands it to a blocking pool thread that is not inside a
//! runtime. Call `state.session` directly from a handler and it will panic at
//! the first request.

mod auth;
pub mod cli;
mod fs;
pub mod tls;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix_web::dev::ServerHandle;
use actix_web::error::{ErrorBadRequest, ErrorNotFound};
use actix_web::middleware::from_fn;
use actix_web::{App, HttpResponse, HttpServer, Responder, web};
use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::bittorrent::session::{AddParams, AddTorrentSource, Session, SessionEvent};
use crate::bittorrent::torrentstatus::{State, TorrentStatus};
use crate::core::configuration::Configuration;
use crate::core::environment::Environment;

pub use auth::Credentials;

/// How the listener is secured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsMode {
    Off,
    SelfSigned,
    Custom { cert: PathBuf, key: PathBuf },
}

#[derive(Clone, Debug)]
pub struct WebConfig {
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub username: String,
    pub password_hash: String,
    pub tls: TlsMode,
    pub advanced: Advanced,
}

/// The actix knobs behind the Preferences "Advanced" toggle.
///
/// These used to be literals in `build`. They are configurable because the
/// right value depends on where the server sits - a slow link needs a longer
/// request timeout than the slowloris guard wants to allow, and a machine
/// serving one browser needs nothing like the connection ceiling a shared one
/// does. The defaults are the old literals, so leaving them alone changes
/// nothing.
///
/// Every field is clamped by `Advanced::load`, never taken raw: a zero worker
/// count or a zero connection limit is a server that binds and then answers
/// nothing, which looks like a crash and is far harder to diagnose than a
/// value that quietly refused to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    /// Seconds a client may take to send its request headers.
    pub client_request_timeout: u64,
    /// Seconds a client that stopped reading may hold its worker slot.
    pub client_disconnect_timeout: u64,
    /// Seconds an idle connection is kept open. Zero disables keep-alive.
    pub keep_alive: u64,
    pub max_connections: usize,
    /// TLS handshakes in flight - the expensive half of a connection flood.
    pub max_connection_rate: usize,
    pub workers: usize,
    pub shutdown_timeout: u64,
    /// Request body ceiling in MEGABYTES, as typed; `build` converts.
    pub max_body_size: usize,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced {
            client_request_timeout: 5,
            client_disconnect_timeout: 5,
            keep_alive: 30,
            max_connections: 256,
            max_connection_rate: 64,
            workers: 2,
            shutdown_timeout: 5,
            max_body_size: 8,
        }
    }
}

impl Advanced {
    /// Read the tuning settings, clamping each into a range that still yields a
    /// working server.
    ///
    /// Out-of-range is clamped rather than rejected, and missing falls back to
    /// the default: this runs on the startup path, and no value typed into a
    /// preferences field should be able to stop the interface coming up.
    pub fn load(cfg: &Configuration) -> Advanced {
        let d = Advanced::default();
        // Named closures over `cfg` so each line below reads as the range it
        // allows rather than as three lines of Option plumbing.
        let secs = |key: &str, lo: u64, hi: u64, fallback: u64| -> u64 {
            cfg.get_int(key)
                .map_or(fallback, |v| (v.max(0) as u64).clamp(lo, hi))
        };
        let count = |key: &str, lo: usize, hi: usize, fallback: usize| -> usize {
            cfg.get_int(key)
                .map_or(fallback, |v| (v.max(0) as usize).clamp(lo, hi))
        };

        Advanced {
            // At least a second: a zero timeout would cut off every request
            // before it arrived. The hour ceiling is arbitrary but finite -
            // "no timeout" is the one setting that must not be reachable.
            client_request_timeout: secs("webui.client_request_timeout", 1, 3600, d.client_request_timeout),
            client_disconnect_timeout: secs(
                "webui.client_disconnect_timeout",
                1,
                3600,
                d.client_disconnect_timeout,
            ),
            // Zero is meaningful here, and only here: actix reads it as
            // "close after every response".
            keep_alive: secs("webui.keep_alive", 0, 86400, d.keep_alive),
            max_connections: count("webui.max_connections", 1, 100_000, d.max_connections),
            max_connection_rate: count("webui.max_connection_rate", 1, 100_000, d.max_connection_rate),
            // Capped well under any real core count: each worker is a thread,
            // and this serves one person.
            workers: count("webui.workers", 1, 64, d.workers),
            // Zero means "drop connections at once on shutdown", which is a
            // legitimate choice for a desktop app being closed.
            shutdown_timeout: secs("webui.shutdown_timeout", 0, 3600, d.shutdown_timeout),
            // Below 1 MB would reject ordinary .torrent uploads; the ceiling
            // keeps an unbounded body from being free memory for anyone
            // holding the password.
            max_body_size: count("webui.max_body_size", 1, 1024, d.max_body_size),
        }
    }
}

impl WebConfig {
    /// Read the `webui.*` settings, substituting a working default for
    /// anything missing or out of range.
    ///
    /// Every fallback here is the safe one: loopback rather than any-address,
    /// and TLS on rather than off. A corrupt or half-written setting must not
    /// be the thing that puts a plaintext listener on the LAN.
    pub fn load(cfg: &Configuration) -> WebConfig {
        let tls = match cfg.get_string("webui.tls_mode").unwrap_or_default().as_str() {
            "off" => TlsMode::Off,
            "custom" => TlsMode::Custom {
                cert: PathBuf::from(cfg.get_string("webui.tls_cert_path").unwrap_or_default()),
                key: PathBuf::from(cfg.get_string("webui.tls_key_path").unwrap_or_default()),
            },
            // Unknown values fall back to the secure option rather than to
            // plaintext - a typo in the setting must not silently downgrade.
            _ => TlsMode::SelfSigned,
        };

        WebConfig {
            enabled: cfg.get_bool("webui.enabled"),
            bind_address: cfg
                .get_string("webui.bind_address")
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| String::from("127.0.0.1")),
            port: cfg
                .get_int("webui.port")
                .filter(|p| (1..=65535).contains(p))
                .unwrap_or(8443) as u16,
            username: cfg
                .get_string("webui.username")
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| String::from("nanotorrent")),
            password_hash: cfg.get_string("webui.password_hash").unwrap_or_default(),
            tls,
            advanced: Advanced::load(cfg),
        }
    }

    /// The `host:port` string to bind the listener to.
    fn socket_addr(&self) -> String {
        format!("{}:{}", self.bind_address, self.port)
    }

    /// True when the listener would accept connections from beyond this machine.
    fn is_exposed(&self) -> bool {
        !matches!(self.bind_address.as_str(), "127.0.0.1" | "::1" | "localhost")
    }
}

struct AppState {
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    /// Needed to apply settings to the running session, and to let a `lang/`
    /// folder next to the executable override the compiled-in translations.
    env: Arc<Environment>,
    /// This layer's own subscription to session events, so /errors no longer
    /// competes with the desktop window for them. Both used to drain one
    /// shared queue, which meant whichever polled first won and the other
    /// silently lost the error.
    errors: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<SessionEvent>>>,
}

impl AppState {
    /// The configured language, for the setting descriptions and the strings
    /// the page itself renders.
    fn translator(&self) -> crate::ui::translator::Translator {
        crate::load_translator(&self.env, &self.cfg)
    }
}

// --- wire types -------------------------------------------------------------
// Deliberately separate from TorrentStatus: the internal struct is free to be
// renamed and reshaped, and a client should not break when it is.

#[derive(Serialize)]
struct SessionInfo {
    version: &'static str,
    listen_port: Option<u16>,
    dht_nodes: Option<i64>,
    download_rate: i64,
    upload_rate: i64,
    torrents: usize,
}

#[derive(Serialize)]
struct TorrentDto {
    info_hash: String,
    name: String,
    state: &'static str,
    paused: bool,
    progress: f32,
    size: i64,
    remaining: i64,
    download_rate: i64,
    upload_rate: i64,
    peers: i64,
    peers_total: i64,
    seeds: i64,
    seeds_total: i64,
    ratio: f32,
    /// Seconds, or null when not transferring - a duration is not a number
    /// everyone agrees on, so say which unit this is.
    eta_seconds: Option<u64>,
    availability: f32,
    save_path: String,
    label: Option<String>,
    queue_position: i64,
    added_on: String,
    completed_on: Option<String>,
    error: Option<String>,
}

/// Stable wire names for the state enum. Not `Debug`, which would change the
/// moment a variant is renamed.
fn state_name(state: State) -> &'static str {
    match state {
        State::Unknown => "unknown",
        State::Error => "error",
        State::CheckingFiles => "checking_files",
        State::CheckingResumeData => "checking_resume_data",
        State::Downloading => "downloading",
        State::DownloadingChecking => "downloading_checking",
        State::DownloadingMetadata => "downloading_metadata",
        State::DownloadingPaused => "downloading_paused",
        State::DownloadingQueued => "downloading_queued",
        State::Uploading => "uploading",
        State::UploadingPaused => "uploading_paused",
        State::UploadingQueued => "uploading_queued",
    }
}

impl From<TorrentStatus> for TorrentDto {
    fn from(t: TorrentStatus) -> TorrentDto {
        TorrentDto {
            state: state_name(t.state),
            info_hash: t.info_hash,
            name: t.name,
            paused: t.paused,
            progress: t.progress,
            size: t.total_wanted,
            remaining: t.total_wanted_remaining,
            download_rate: t.download_payload_rate,
            upload_rate: t.upload_payload_rate,
            peers: t.peers_current,
            peers_total: t.peers_total,
            seeds: t.seeds_current,
            seeds_total: t.seeds_total,
            ratio: t.ratio,
            eta_seconds: t.eta.map(|d| d.as_secs()),
            // The UI dashes this when negative; negative zero is not negative,
            // which is how "-0.00" reaches the screen. Normalise it here so the
            // wire format never carries a signed zero.
            availability: if t.availability <= 0.0 { 0.0 } else { t.availability },
            save_path: t.save_path,
            label: (!t.label_name.is_empty()).then_some(t.label_name),
            queue_position: t.queue_position,
            added_on: t.added_on.to_rfc3339(),
            completed_on: t.completed_on.map(|d| d.to_rfc3339()),
            error: (!t.error.is_empty()).then_some(t.error),
        }
    }
}

// --- handlers ---------------------------------------------------------------

/// `GET /api/health` - a liveness probe that touches nothing.
///
/// Deliberately does not consult the session: this answers "is the HTTP layer
/// up", and a probe that blocks on a busy session would report the wrong thing.
async fn h_health() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({ "status": "ok" }))
}

/// The web remote itself - one file, compiled in.
///
/// Embedded rather than served from disk for the same reason `lang/*.json` is:
/// a single executable with nothing beside it to lose, and no path to get
/// wrong on three platforms.
/// Fill the page's `{{key}}` placeholders from the configured language.
///
/// Substituted here rather than fetched by the page: a second round trip would
/// show English first and then repaint, and this costs one pass over a page
/// that is already in memory.
///
/// `{{__T__}}` is special - it becomes a JSON object of every key the page
/// asked for, which the script uses for the strings it builds at runtime.
/// Serialising it as JSON is what keeps an apostrophe in a French translation
/// from ending a JavaScript string literal.
fn render_page(tr: &crate::ui::translator::Translator) -> String {
    let html = include_str!("index.html");

    let keys: Vec<&str> = {
        let mut keys = Vec::new();
        let mut rest = html;
        while let Some(start) = rest.find("{{") {
            let Some(end) = rest[start..].find("}}") else { break };
            let key = &rest[start + 2..start + end];
            if key != "__T__" && !keys.contains(&key) {
                keys.push(key);
            }
            rest = &rest[start + end + 2..];
        }

        // The script also reaches for strings the markup never names, as
        // `T.some_key`. Those have to be in the table or they arrive as
        // `undefined` - which is exactly how the first version shipped a toast
        // reading "undefined: Failed to fetch". `T` is only ever the string
        // table in this file, so matching on it is unambiguous.
        let mut rest = html;
        while let Some(at) = rest.find("T.") {
            let tail = &rest[at + 2..];
            let len = tail
                .find(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_')
                .unwrap_or(tail.len());
            let key = &tail[..len];
            if !key.is_empty() && !keys.contains(&key) {
                keys.push(key);
            }
            rest = &rest[at + 2 + len..];
        }
        keys
    };

    let table: std::collections::BTreeMap<&str, String> =
        keys.iter().map(|k| (*k, tr.i18n(k))).collect();
    let json = serde_json::to_string(&table).unwrap_or_else(|_| String::from("{}"));

    let mut out = String::with_capacity(html.len() + json.len() + 512);
    let mut rest = html;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start..].find("}}") else { break };
        out.push_str(&rest[..start]);
        let key = &rest[start + 2..start + end];
        if key == "__T__" {
            out.push_str(&json);
        } else {
            // Escaped: a translation is data, and one containing < or & would
            // otherwise change the shape of the page.
            for c in tr.i18n(key).chars() {
                match c {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '"' => out.push_str("&quot;"),
                    _ => out.push(c),
                }
            }
        }
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    out
}

async fn h_index(state: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        // Everything is inline, so 'unsafe-inline' is unavoidable - but the
        // rest still matters: default-src 'self' blocks loading or exfiltrating
        // to any other origin, which is the half of XSS that hurts. The page
        // itself never innerHTMLs server data; torrent names are attacker-
        // controlled and go in via textContent.
        .insert_header((
            "Content-Security-Policy",
            "default-src 'self'; style-src 'self' 'unsafe-inline'; \
             script-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             form-action 'none'; frame-ancestors 'none'; base-uri 'none'",
        ))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        // The remote shows filesystem paths and torrent names; keep them out
        // of any Referer sent to a site someone clicks through to.
        .insert_header(("Referrer-Policy", "no-referrer"))
        .body(render_page(&state.translator()))
}

/// `GET /api/session` - version, listen port, DHT node count, current rates
/// and how many torrents there are.
async fn h_session(state: web::Data<AppState>) -> actix_web::Result<impl Responder> {
    let st = state.clone();
    // web::block, not a direct call: see the threading note at the top.
    let info = web::block(move || {
        let (down, up) = st.session.session_rates();
        SessionInfo {
            version: crate::buildinfo::version(),
            listen_port: st.session.listen_port(),
            dht_nodes: st.session.dht_nodes(),
            download_rate: down,
            upload_rate: up,
            torrents: st.session.torrents(&HashMap::new()).len(),
        }
    })
    .await?;

    Ok(web::Json(info))
}

/// `GET /api/torrents` - one row per torrent, the same fields the main
/// window's list shows, with label ids already resolved to names.
async fn h_torrents(state: web::Data<AppState>) -> actix_web::Result<impl Responder> {
    let st = state.clone();
    let rows = web::block(move || {
        let labels: HashMap<i32, String> = st
            .cfg
            .get_labels()
            .into_iter()
            .map(|l| (l.id, l.name))
            .collect();
        st.session
            .torrents(&labels)
            .into_iter()
            .map(TorrentDto::from)
            .collect::<Vec<_>>()
    })
    .await?;

    Ok(web::Json(rows))
}

/// Drains this layer's error queue.
///
/// Draining, not peeking, and that is the contract: adds are fire-and-forget,
/// so this is where a failed magnet or a rejected .torrent surfaces, and two
/// HTTP clients polling it will each see a subset. One poller.
///
/// The desktop window is no longer one of them - it holds its own subscription
/// and sees every error regardless of what any web client does.
async fn h_errors(state: web::Data<AppState>) -> actix_web::Result<impl Responder> {
    let st = state.clone();
    let errors = web::block(move || {
        let rx = st.errors.lock().unwrap();
        let mut drained = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::Error(message) = event {
                drained.push(message);
            }
        }
        drained
    })
    .await?;
    Ok(web::Json(serde_json::json!({ "errors": errors })))
}

// --- mutating handlers ------------------------------------------------------

/// serde default for `AddRequest::start`: a torrent added without saying
/// otherwise starts, which is what every client does.
fn default_start() -> bool {
    true
}

#[derive(Deserialize)]
struct AddRequest {
    magnet: Option<String>,
    /// base64-encoded `.torrent` contents. Base64 inside JSON rather than
    /// multipart: torrent files are kilobytes, and it saves a dependency and a
    /// second request shape.
    torrent_file: Option<String>,
    save_path: Option<String>,
    #[serde(default = "default_start")]
    start: bool,
    label_id: Option<i32>,
    only_files: Option<Vec<usize>>,
}

/// One torrent, or a batch of them.
///
/// Untagged so the original single-object body still works and a JSON array is
/// simply the new form. A separate endpoint or a version bump would be a lot of
/// ceremony for "the same thing, n times".
#[derive(Deserialize)]
#[serde(untagged)]
enum AddBody {
    // Boxed: AddRequest carries a base64 torrent, so the Many variant would
    // otherwise be far smaller than One and clippy rightly objects.
    One(Box<AddRequest>),
    Many(Vec<AddRequest>),
}

/// Validate one request and turn it into something the session can add.
///
/// Split out from the handler so a batch validates every entry the same way a
/// single add does - the checks are the interesting part and there is now more
/// than one caller.
fn add_source(req: &mut AddRequest) -> actix_web::Result<AddTorrentSource> {
    match (req.magnet.take(), req.torrent_file.take()) {
        (Some(magnet), None) => {
            // Magnet scheme only. librqbit's from_url happily fetches http(s)
            // too, which would turn "add a torrent" into "make the server
            // issue a request to any URL I name" - including hosts only it can
            // reach. Support that deliberately or not at all.
            if !magnet.starts_with("magnet:") {
                return Err(ErrorBadRequest("magnet must start with 'magnet:'"));
            }
            Ok(AddTorrentSource::MagnetUri(magnet))
        }
        (None, Some(encoded)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|e| ErrorBadRequest(format!("torrent_file is not valid base64: {e}")))?;
            if bytes.is_empty() {
                return Err(ErrorBadRequest("torrent_file decoded to zero bytes"));
            }
            Ok(AddTorrentSource::TorrentFileBytes(bytes))
        }
        _ => Err(ErrorBadRequest(
            "provide exactly one of 'magnet' or 'torrent_file'",
        )),
    }
}

/// One `.torrent` to look inside, base64 as everywhere else on this endpoint.
#[derive(Deserialize)]
struct InspectRequest {
    torrent_file: String,
}

#[derive(Serialize)]
struct InspectedFile {
    path: String,
    size: u64,
}

/// What a `.torrent` turns out to contain.
#[derive(Serialize)]
struct Inspected {
    name: String,
    total_size: i64,
    /// In metainfo order, which is the order `only_files` indexes by - so the
    /// caller can hand positions from this list straight back to the add.
    files: Vec<InspectedFile>,
}

/// One or many, matching [`AddBody`] so a batch is one request.
#[derive(Deserialize)]
#[serde(untagged)]
enum InspectBody {
    One(Box<InspectRequest>),
    Many(Vec<InspectRequest>),
}

/// `POST /api/torrents/inspect` - read `.torrent` files without adding them.
///
/// This is what lets the web remote show the same name, size and file tree the
/// desktop Add dialog does, and tick files off before committing. Parsing here
/// rather than in the browser reuses [`crate::ui::torrentfile::parse`] - the
/// same function the desktop dialog uses, so the two cannot disagree about
/// what a torrent contains.
///
/// Adds nothing and touches no session state; it is a pure read of the bytes
/// posted to it. Magnets are not accepted: there is nothing to inspect until
/// their metadata resolves, which is why they skip this step entirely.
async fn h_inspect(body: web::Json<InspectBody>) -> actix_web::Result<impl Responder> {
    let reqs = match body.into_inner() {
        InspectBody::One(r) => vec![*r],
        InspectBody::Many(r) => r,
    };
    if reqs.is_empty() {
        return Err(ErrorBadRequest("no torrents given"));
    }

    let parsed = web::block(move || {
        reqs.into_iter()
            .enumerate()
            .map(|(i, req)| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(req.torrent_file.trim())
                    .map_err(|e| format!("torrent {}: not valid base64: {e}", i + 1))?;
                let t = crate::ui::torrentfile::parse(&bytes)
                    .map_err(|e| format!("torrent {}: {e}", i + 1))?;
                Ok(Inspected {
                    name: t.name,
                    total_size: t.total_size,
                    files: t
                        .files
                        .into_iter()
                        .map(|(path, size)| InspectedFile { path, size })
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .await?
    .map_err(ErrorBadRequest)?;

    Ok(web::Json(parsed))
}

/// `POST /api/torrents` - add magnet links or uploaded `.torrent` files.
///
/// Takes either one request object or an array of them. Each entry names
/// exactly one of `magnet` / `torrent_file` and carries its own save path,
/// label and file selection, so a batch is not forced to share settings.
///
/// All-or-nothing on validation: every entry is checked before any is added,
/// so a typo in the eighth magnet does not leave seven added and the request
/// reported as failed. Nothing is partially applied.
async fn h_add(
    state: web::Data<AppState>,
    body: web::Json<AddBody>,
) -> actix_web::Result<impl Responder> {
    let mut reqs = match body.into_inner() {
        AddBody::One(req) => vec![*req],
        AddBody::Many(reqs) => reqs,
    };
    if reqs.is_empty() {
        return Err(ErrorBadRequest("no torrents given"));
    }

    let mut batch = Vec::with_capacity(reqs.len());
    for (i, req) in reqs.iter_mut().enumerate() {
        // Say which one, or a batch of twenty reports an unlocatable error.
        let source = add_source(req)
            .map_err(|e| ErrorBadRequest(format!("torrent {}: {e}", i + 1)))?;
        batch.push((
            source,
            AddParams {
                save_path: req.save_path.take(),
                start_torrent: req.start,
                only_files: req.only_files.take(),
                label_id: req.label_id,
            },
        ));
    }

    let count = batch.len();
    let st = state.clone();
    web::block(move || {
        for (source, params) in batch {
            st.session.add_torrent(source, params);
        }
    })
    .await?;

    // 202, not 200: add_torrent returns before the torrent exists. Resolving a
    // magnet's metadata can take minutes, or never finish. Poll /api/torrents
    // for arrival and /api/errors for failure.
    Ok(HttpResponse::Accepted().json(serde_json::json!({
        "status": "accepted",
        "count": count,
    })))
}

/// Run `op` against a torrent, 404ing if that hash is not in the session.
///
/// The `Session` mutators silently do nothing for an unknown hash - fine for
/// the UI, which can only pass hashes it just listed, but over HTTP a typo
/// would look exactly like success.
async fn with_torrent<F>(
    state: &web::Data<AppState>,
    hash: String,
    op: F,
) -> actix_web::Result<HttpResponse>
where
    F: FnOnce(&Session, &str) + Send + 'static,
{
    let st = state.clone();
    let found = web::block(move || {
        if !st.session.exists(&hash) {
            return false;
        }
        op(&st.session, &hash);
        true
    })
    .await?;

    if found {
        Ok(HttpResponse::NoContent().finish())
    } else {
        Err(ErrorNotFound("no torrent with that info hash"))
    }
}

/// `POST /api/torrents/{hash}/pause`
async fn h_pause(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    with_torrent(&state, hash.into_inner(), |s, h| s.pause(h)).await
}

/// `POST /api/torrents/{hash}/resume`
async fn h_resume(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    with_torrent(&state, hash.into_inner(), |s, h| s.resume(h)).await
}

/// `POST /api/torrents/{hash}/recheck` - re-hash what is on disk.
async fn h_recheck(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    with_torrent(&state, hash.into_inner(), |s, h| s.recheck(h)).await
}

#[derive(Deserialize)]
struct RemoveQuery {
    /// Defaults to false. Deleting data is the destructive option, so it has
    /// to be asked for by name rather than being the default for a DELETE.
    #[serde(default)]
    delete_files: bool,
}

/// `DELETE /api/torrents/{hash}` - remove a torrent, and its data only when
/// `?delete_files=true` says so explicitly.
async fn h_remove(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    query: web::Query<RemoveQuery>,
) -> actix_web::Result<HttpResponse> {
    let delete_files = query.delete_files;
    with_torrent(&state, hash.into_inner(), move |s, h| {
        s.remove(h, delete_files)
    })
    .await
}

#[derive(Deserialize)]
struct MoveRequest {
    path: String,
}

/// `POST /api/torrents/{hash}/move` - move a torrent's storage.
///
/// The path must be absolute: a relative one would resolve against whatever
/// directory the process happens to be running in, which is not something the
/// caller can see.
async fn h_move(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<MoveRequest>,
) -> actix_web::Result<HttpResponse> {
    let path = body.into_inner().path;
    if !std::path::Path::new(&path).is_absolute() {
        return Err(ErrorBadRequest("path must be absolute"));
    }
    with_torrent(&state, hash.into_inner(), move |s, h| {
        s.move_storage(h, &path)
    })
    .await
}

/// `POST /api/torrents/{hash}/location` - point a torrent at data that has
/// already been moved.
///
/// The counterpart to `/move`: that one relocates the files, this one relocates
/// the torrent and leaves the data alone. Same absolute-path rule.
async fn h_set_location(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<MoveRequest>,
) -> actix_web::Result<HttpResponse> {
    let path = body.into_inner().path;
    if !std::path::Path::new(&path).is_absolute() {
        return Err(ErrorBadRequest("path must be absolute"));
    }
    with_torrent(&state, hash.into_inner(), move |s, h| {
        s.set_location(h, &path)
    })
    .await
}

// --- settings ---------------------------------------------------------------

#[derive(Serialize)]
struct SettingDto {
    name: String,
    /// Current value, rendered exactly as `--set` would accept it back.
    value: String,
    /// bool | int | text | dir | choice
    kind: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    options: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<i64>,
    #[serde(skip_serializing_if = "str::is_empty")]
    unit: &'static str,
    description: String,
}

#[derive(Serialize)]
struct SectionDto {
    #[serde(skip)]
    key: &'static str,
    name: String,
    settings: Vec<SettingDto>,
}

/// `GET /api/settings` - every preference, grouped the way Preferences groups
/// them.
///
/// Built from the same registry the command line uses, so the three surfaces
/// cannot drift: adding a setting there makes it appear here with the right
/// control and the right validation, with nothing to change in this file.
async fn h_settings(state: web::Data<AppState>) -> actix_web::Result<HttpResponse> {
    let tr = state.translator();
    let mut sections: Vec<SectionDto> = Vec::new();

    for s in crate::cli::SETTINGS {
        let f = crate::cli::field(s);
        let dto = SettingDto {
            name: String::from(s.name),
            value: crate::cli::show(&state.cfg, s),
            kind: f.kind,
            options: f.options,
            labels: f.labels,
            min: f.min,
            max: f.max,
            unit: f.unit,
            description: crate::cli::description(s, &tr),
        };
        // Grouped by walking the registry in order, which is already grouped -
        // so the drawer's sections match the Preferences tabs without a second
        // list to keep in step.
        match sections.last_mut() {
            Some(last) if last.key == s.section => last.settings.push(dto),
            _ => sections.push(SectionDto {
                key: s.section,
                name: tr.i18n(s.section),
                settings: vec![dto],
            }),
        }
    }

    Ok(HttpResponse::Ok().json(sections))
}

#[derive(Deserialize)]
struct SettingRequest {
    name: String,
    value: String,
}

/// `POST /api/settings` - change one preference.
///
/// One at a time rather than a whole document: each value is validated on its
/// own terms and a rejected one has to name itself, which a bulk write cannot
/// do without inventing a per-field error shape.
///
/// Note that `web-*` changes are stored but NOT applied to the running server -
/// restarting the interface out from under the request that changed it would
/// answer with a dropped connection. They take effect the next time it starts.
async fn h_set_setting(
    state: web::Data<AppState>,
    body: web::Json<SettingRequest>,
) -> actix_web::Result<HttpResponse> {
    let body = body.into_inner();
    let setting = crate::cli::find(&body.name)
        .ok_or_else(|| ErrorBadRequest(format!("unknown setting '{}'", body.name)))?;

    let tr = state.translator();
    crate::cli::set(&state.cfg, setting, &body.value, &tr)
        .map_err(|e| ErrorBadRequest(format!("{e:#}")))?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "name": setting.name,
        "value": crate::cli::show(&state.cfg, setting),
    })))
}

/// `POST /api/settings/apply` - make the stored settings take effect now.
///
/// The same rebuild the desktop does when Preferences is accepted: the librqbit
/// session is torn down and recreated from the settings, so rate limits, DHT,
/// PeX, encryption and the proxy change without restarting the application.
///
/// The `webui.*` settings are the exception and are NOT applied here. Restarting
/// the HTTP server from inside one of its own requests would answer that request
/// by dropping the connection; they take effect when NanoTorrent next starts.
async fn h_apply_settings(state: web::Data<AppState>) -> actix_web::Result<HttpResponse> {
    state.session.apply_settings(&state.env, &state.cfg);
    Ok(HttpResponse::Ok().json(serde_json::json!({ "applied": true })))
}

#[derive(Deserialize)]
struct LabelRequest {
    /// null clears the label.
    label_id: Option<i32>,
}

/// `POST /api/torrents/{hash}/label` - assign a label, or clear it with a
/// null `label_id`.
async fn h_label(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<LabelRequest>,
) -> actix_web::Result<HttpResponse> {
    let label_id = body.into_inner().label_id;
    with_torrent(&state, hash.into_inner(), move |s, h| s.set_label(h, label_id)).await
}

// --- filesystem handlers ----------------------------------------------------

/// `GET /api/fs/roots` - the drives (Windows) or mount points (Unix) the save
/// path browser starts from.
async fn h_fs_roots() -> actix_web::Result<impl Responder> {
    Ok(web::Json(web::block(fs::roots).await?))
}

/// `GET /api/fs/list?path=...` - directories under one path, for picking a
/// save location. Files are not listed; only somewhere to put them.
async fn h_fs_list(query: web::Query<fs::PathRequest>) -> actix_web::Result<impl Responder> {
    let path = query.into_inner().path;
    let listing = web::block(move || fs::list(&path)).await?.map_err(ErrorBadRequest)?;
    Ok(web::Json(listing))
}

/// `POST /api/fs/mkdir` - create a directory so a torrent can be pointed at
/// somewhere that does not exist yet.
async fn h_fs_mkdir(body: web::Json<fs::PathRequest>) -> actix_web::Result<impl Responder> {
    let path = body.into_inner().path;
    let listing = web::block(move || fs::mkdir(&path)).await?.map_err(ErrorBadRequest)?;
    Ok(HttpResponse::Created().json(listing))
}

// --- server -----------------------------------------------------------------

/// Start the web interface if it is enabled and configured.
///
/// `Ok(None)` means "switched off", which is not an error. An `Err` means it
/// was asked for and could not be started - the caller decides how loudly to
/// say so, because that is fatal headless and merely bad on Windows.
/// Stop a running web interface.
///
/// `ServerHandle::stop` is async and the caller is the UI thread, which has no
/// runtime - so this drives the future on a throwaway one. Graceful, so a
/// request in flight when someone presses Ok in Preferences finishes rather
/// than being cut.
pub fn stop(handle: ServerHandle) {
    std::thread::Builder::new()
        .name(String::from("nt-webui-stop"))
        .spawn(move || {
            actix_web::rt::System::new().block_on(handle.stop(true));
        })
        .map(|t| {
            // Joined so the port is free before the caller rebinds it -
            // respawning on the same port otherwise races the old listener.
            let _ = t.join();
        })
        .unwrap_or_else(|err| tracing::error!("could not stop the web interface: {err}"));
}

/// Stop whatever is running and start again from the current settings.
///
/// Returns the new handle, or `None` when the interface is now disabled.
pub fn restart(
    current: Option<ServerHandle>,
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    env: Arc<Environment>,
) -> Result<Option<ServerHandle>> {
    if let Some(handle) = current {
        stop(handle);
    }
    spawn(session, cfg, env)
}

pub fn spawn(
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    env: Arc<Environment>,
) -> Result<Option<ServerHandle>> {
    let wc = WebConfig::load(&cfg);
    if !wc.enabled {
        return Ok(None);
    }

    let creds = Credentials {
        username: wc.username.clone(),
        password_hash: wc.password_hash.clone(),
    };
    anyhow::ensure!(
        creds.is_configured(),
        "the web interface is enabled but has no password set (webui.password_hash is empty). \
         Set one before it will listen - an open port here is a remote file manager."
    );

    // Plaintext on anything but loopback would put those credentials on the
    // wire in the clear. Refuse rather than warn: the setting combination is
    // almost certainly a mistake, and it is one that cannot be taken back once
    // someone has sniffed it.
    anyhow::ensure!(
        !(wc.tls == TlsMode::Off && wc.is_exposed()),
        "refusing to serve the web interface without TLS on {} - Basic auth credentials \
         would be sent in clear text. Use webui.tls_mode = self-signed, or bind to 127.0.0.1.",
        wc.bind_address
    );

    let data_dir = env.get_application_data_path();
    let tls_config = match &wc.tls {
        TlsMode::Off => None,
        TlsMode::SelfSigned => Some(tls::self_signed(&data_dir)?),
        TlsMode::Custom { cert, key } => {
            anyhow::ensure!(
                !cert.as_os_str().is_empty() && !key.as_os_str().is_empty(),
                "webui.tls_mode is 'custom' but webui.tls_cert_path / webui.tls_key_path are empty"
            );
            Some(tls::from_pem(cert, key)?)
        }
    };

    // Hand the handle back to the caller synchronously, so a bind failure is
    // reported at startup rather than vanishing into a detached thread.
    let (tx, rx) = std::sync::mpsc::channel::<Result<ServerHandle>>();
    let addr = wc.socket_addr();
    let scheme = if tls_config.is_some() { "https" } else { "http" };
    // Cloned out of `wc` because the thread below outlives this scope.
    let advanced = wc.advanced.clone();

    std::thread::Builder::new()
        .name(String::from("nt-webui"))
        .spawn(move || {
            let system = actix_web::rt::System::new();
            system.block_on(async move {
                match build(&addr, session, cfg, env, creds, tls_config, advanced) {
                    Ok(server) => {
                        let _ = tx.send(Ok(server.handle()));
                        if let Err(err) = server.await {
                            tracing::error!("web interface stopped: {err}");
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err));
                    }
                }
            });
        })
        .context("could not start the web interface thread")?;

    let handle = rx
        .recv()
        .context("the web interface thread died before reporting readiness")??;

    tracing::info!("web interface listening on {scheme}://{}", wc.socket_addr());
    if wc.tls == TlsMode::SelfSigned
        && let Some(fp) = tls::fingerprint(&tls::cert_path(&data_dir))
    {
        // So the browser's warning can be checked rather than clicked through.
        tracing::info!("web interface certificate SHA-256: {fp}");
    }

    Ok(Some(handle))
}

/// Assemble the router, bind the socket and return the unstarted server.
///
/// Split out from [`spawn`] so binding fails here, on the caller's thread,
/// with a real error - inside the server thread it would only reach a log line
/// nobody is watching.
fn build(
    addr: &str,
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    env: Arc<Environment>,
    creds: Credentials,
    tls_config: Option<rustls::ServerConfig>,
    advanced: Advanced,
) -> Result<actix_web::dev::Server> {
    // Subscribed once for the lifetime of the server, not per request: a
    // per-request subscription would only ever see events raised while that
    // one request was in flight.
    let errors = Arc::new(std::sync::Mutex::new(session.subscribe()));
    let state = web::Data::new(AppState {
        session,
        cfg,
        env,
        errors,
    });
    let creds = web::Data::new(creds);
    // Built out here, not in the factory closure: the closure runs once per
    // worker, so constructing it there would give each worker its own counter
    // and multiply the real attempt limit by the worker count.
    let attempts = web::Data::new(auth::Attempts::default());

    // Megabytes in the setting, bytes here. Computed outside the factory
    // closure, which runs per worker and cannot borrow `advanced` - it is
    // moved into the builder calls below.
    let body_limit = advanced.max_body_size * 1024 * 1024;
    // Zero is not "no keep-alive" to actix's Duration form, it is a zero-length
    // one; KeepAlive::Disabled is the setting the field actually offers.
    let keep_alive = match advanced.keep_alive {
        0 => actix_web::http::KeepAlive::Disabled,
        secs => actix_web::http::KeepAlive::Timeout(Duration::from_secs(secs)),
    };

    let server = HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .app_data(creds.clone())
            .app_data(attempts.clone())
            .wrap(from_fn(auth::require_auth))
            // A .torrent with thousands of files runs to a few MB once
            // base64'd; actix's 2 KB default would reject them. Still a cap,
            // because an unbounded body is free memory for anyone with the
            // password.
            .app_data(web::JsonConfig::default().limit(body_limit))
            .route("/", web::get().to(h_index))
            .service(
                web::scope("/api")
                    .route("/health", web::get().to(h_health))
                    .route("/session", web::get().to(h_session))
                    .route("/errors", web::get().to(h_errors))
                    .route("/torrents", web::get().to(h_torrents))
                    .route("/torrents", web::post().to(h_add))
                    .route("/torrents/inspect", web::post().to(h_inspect))
                    .route("/torrents/{hash}", web::delete().to(h_remove))
                    .route("/torrents/{hash}/pause", web::post().to(h_pause))
                    .route("/torrents/{hash}/resume", web::post().to(h_resume))
                    .route("/torrents/{hash}/recheck", web::post().to(h_recheck))
                    .route("/torrents/{hash}/move", web::post().to(h_move))
                    .route("/torrents/{hash}/location", web::post().to(h_set_location))
                    .route("/settings", web::get().to(h_settings))
                    .route("/settings", web::post().to(h_set_setting))
                    .route("/settings/apply", web::post().to(h_apply_settings))
                    .route("/torrents/{hash}/label", web::post().to(h_label))
                    .route("/fs/roots", web::get().to(h_fs_roots))
                    .route("/fs/list", web::get().to(h_fs_list))
                    .route("/fs/mkdir", web::post().to(h_fs_mkdir)),
            )
    })
    // Actix's defaults are tuned for a public server; these are for a personal
    // client, and every one of them is a cheap bound on a misbehaving or
    // hostile peer. All of them come from Preferences > Web interface >
    // Advanced, defaulting to the values that used to be written here.
    //
    // client_request_timeout defaults to 5s already and is the slowloris
    // guard - restated so it is visible rather than inherited silently.
    .client_request_timeout(Duration::from_secs(advanced.client_request_timeout))
    // Defaults to ZERO, i.e. disabled: a client that stops reading mid-response
    // would otherwise hold its worker slot indefinitely.
    .client_disconnect_timeout(Duration::from_secs(advanced.client_disconnect_timeout))
    .keep_alive(keep_alive)
    // 25600 per worker by default. A handful of browser tabs need double
    // digits; this is the cheapest bound on connection flooding.
    .max_connections(advanced.max_connections)
    // Caps TLS handshakes in flight. Handshakes are the expensive half, so
    // this is what stops a flood costing far more CPU than bandwidth.
    .max_connection_rate(advanced.max_connection_rate)
    // One per core by default. This serves one person, not a load test.
    .workers(advanced.workers)
    .shutdown_timeout(advanced.shutdown_timeout);

    let server = match tls_config {
        Some(config) => server
            .bind_rustls_0_23(addr, config)
            .with_context(|| format!("cannot bind the web interface to {addr} (TLS)"))?,
        None => server
            .bind(addr)
            .with_context(|| format!("cannot bind the web interface to {addr}"))?,
    };

    Ok(server.run())
}

#[cfg(test)]
mod tests {
    /// The tuning fields decide whether the server comes up at all, so the
    /// clamp is the thing under test: nothing typed into a preferences field
    /// may produce a listener that binds and then answers nothing.
    ///
    /// Migration defaults are checked in the same test on purpose - a default
    /// that disagrees with `Advanced::default` would mean the Preferences
    /// fields show one thing on a fresh install and the server does another.
    #[test]
    fn advanced_clamps_and_defaults_match_the_migration() {
        use crate::core::configuration::Configuration;
        use crate::core::database::Database;
        use std::sync::Arc;

        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db.clone());

        assert_eq!(super::Advanced::load(&cfg), super::Advanced::default());

        // Zero workers or zero connections is the dangerous case: actix accepts
        // both and the result is a server that listens and serves nothing.
        cfg.set("webui.workers", &0i64);
        cfg.set("webui.max_connections", &0i64);
        cfg.set("webui.client_request_timeout", &0i64);
        // Negative reaches the same floor rather than wrapping to a huge usize.
        cfg.set("webui.max_body_size", &-5i64);
        let adv = super::Advanced::load(&cfg);
        assert_eq!(adv.workers, 1);
        assert_eq!(adv.max_connections, 1);
        assert_eq!(adv.client_request_timeout, 1);
        assert_eq!(adv.max_body_size, 1);

        // Absurdly large is capped, not accepted: 64 threads is already far
        // more than this serves.
        cfg.set("webui.workers", &10_000i64);
        assert_eq!(super::Advanced::load(&cfg).workers, 64);

        // Zero is legitimate for exactly these two and must survive the clamp.
        cfg.set("webui.keep_alive", &0i64);
        cfg.set("webui.shutdown_timeout", &0i64);
        let adv = super::Advanced::load(&cfg);
        assert_eq!(adv.keep_alive, 0);
        assert_eq!(adv.shutdown_timeout, 0);
    }

    /// The web remote shows a file list only if inspection returns the same
    /// thing the desktop dialog sees, in the same order - only_files indexes
    /// by that order, so a mismatch would untick the wrong file.
    #[test]
    fn inspect_reports_what_the_desktop_dialog_sees() {
        use super::{InspectBody, InspectRequest};
        use base64::Engine as _;

        // Minimal single-file v1 metainfo, same shape ui::torrentfile tests use.
        let mut t = Vec::new();
        t.extend_from_slice(b"d4:infod6:lengthi4096e4:name13:Some.File.mkv");
        t.extend_from_slice(b"12:piece lengthi262144e6:pieces20:");
        t.extend_from_slice(&[0u8; 20]);
        t.extend_from_slice(b"ee");

        let encoded = base64::engine::general_purpose::STANDARD.encode(&t);
        let body = format!(r#"{{"torrent_file":"{encoded}"}}"#);

        // Single object and array must both parse, as with AddBody.
        assert!(matches!(
            serde_json::from_str::<InspectBody>(&body).unwrap(),
            InspectBody::One(_)
        ));
        let many: InspectBody = serde_json::from_str(&format!("[{body},{body}]")).unwrap();
        let InspectBody::Many(reqs) = many else {
            panic!("array should parse as Many");
        };
        assert_eq!(reqs.len(), 2);

        // And the bytes really do decode to what the desktop parser reports.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&reqs[0].torrent_file)
            .unwrap();
        let parsed = crate::ui::torrentfile::parse(&decoded).unwrap();
        assert_eq!(parsed.name, "Some.File.mkv");
        assert_eq!(parsed.total_size, 4096);
        assert_eq!(parsed.files, vec![(String::from("Some.File.mkv"), 4096)]);

        // Garbage must be rejected, not silently shown as an empty torrent.
        let junk = InspectRequest {
            torrent_file: base64::engine::general_purpose::STANDARD.encode(b"not a torrent"),
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(junk.torrent_file.trim())
            .unwrap();
        assert!(crate::ui::torrentfile::parse(&bytes).is_err());
    }

    /// The untagged AddBody must keep accepting the old single-object body
    /// while also taking an array - that back-compat is the whole reason it is
    /// untagged, and it is the kind of thing a serde attribute change breaks
    /// silently.
    #[test]
    fn add_body_takes_one_or_many() {
        use super::{AddBody, add_source};

        let one: AddBody = serde_json::from_str(r#"{"magnet":"magnet:?xt=1"}"#).unwrap();
        assert!(matches!(one, AddBody::One(_)));

        let many: AddBody =
            serde_json::from_str(r#"[{"magnet":"magnet:?xt=1"},{"magnet":"magnet:?xt=2"}]"#)
                .unwrap();
        let AddBody::Many(reqs) = many else {
            panic!("array should parse as Many");
        };
        assert_eq!(reqs.len(), 2);

        // `start` defaults to true whichever form it arrives in.
        assert!(reqs[0].start);

        // Validation is per entry, and rejects the same shapes it always has.
        let mut bad = match serde_json::from_str::<AddBody>(r#"{"magnet":"http://x/y"}"#).unwrap() {
            AddBody::One(r) => *r,
            AddBody::Many(_) => unreachable!(),
        };
        assert!(add_source(&mut bad).is_err(), "http:// must not be fetched");

        let mut neither = match serde_json::from_str::<AddBody>("{}").unwrap() {
            AddBody::One(r) => *r,
            AddBody::Many(_) => unreachable!(),
        };
        assert!(add_source(&mut neither).is_err(), "needs one source");
    }

    use super::*;

    #[test]
    fn loopback_is_not_treated_as_exposed() {
        let mut wc = WebConfig {
            enabled: true,
            bind_address: String::from("127.0.0.1"),
            port: 8443,
            username: String::from("u"),
            password_hash: String::new(),
            tls: TlsMode::Off,
            advanced: Advanced::default(),
        };
        assert!(!wc.is_exposed());
        wc.bind_address = String::from("::1");
        assert!(!wc.is_exposed());

        // 0.0.0.0 is the one that catches people out: it is every interface,
        // so plaintext there puts Basic auth on the LAN in clear text.
        wc.bind_address = String::from("0.0.0.0");
        assert!(wc.is_exposed());
        wc.bind_address = String::from("192.168.1.10");
        assert!(wc.is_exposed());
    }

    #[test]
    fn availability_never_serialises_as_negative_zero() {
        // Guards the "-0.00" the Win32 list still shows.
        let mut status = sample_status();
        status.availability = -0.0;
        assert_eq!(TorrentDto::from(status).availability, 0.0_f32);
        assert!(!TorrentDto::from(sample_status()).availability.is_sign_negative());
    }

    #[test]
    fn state_names_are_stable_and_distinct() {
        let all = [
            State::Unknown, State::Error, State::CheckingFiles, State::CheckingResumeData,
            State::Downloading, State::DownloadingChecking, State::DownloadingMetadata,
            State::DownloadingPaused, State::DownloadingQueued, State::Uploading,
            State::UploadingPaused, State::UploadingQueued,
        ];
        let mut names: Vec<&str> = all.iter().copied().map(state_name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two states share a wire name");
    }

    fn sample_status() -> TorrentStatus {
        TorrentStatus {
            added_on: chrono::Local::now(),
            all_time_download: 0,
            all_time_upload: 0,
            availability: -0.0,
            completed_on: None,
            download_payload_rate: 0,
            error: String::new(),
            eta: None,
            info_hash_v1: None,
            info_hash_v2: None,
            info_hash: String::from("abc"),
            label_id: None,
            label_name: String::new(),
            name: String::from("t"),
            paused: false,
            peers_current: 0,
            peers_total: 0,
            progress: 0.0,
            queue_position: 0,
            ratio: 0.0,
            save_path: String::new(),
            seeds_current: 0,
            seeds_total: 0,
            state: State::Downloading,
            total_wanted: 0,
            total_wanted_remaining: 0,
            upload_payload_rate: 0,
        }
    }
}

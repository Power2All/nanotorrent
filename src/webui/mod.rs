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
pub mod streamtoken;
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
/// Request body ceiling, in megabytes.
///
/// Actix's default is 2 KB, which rejects any real `.torrent` upload - one with
/// thousands of files runs to a few MB once base64'd. Still a cap, because an
/// unbounded body is free memory for anyone holding the password.
const MAX_BODY_MB: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    /// Failed logins from one address that trip the lockout. Zero disables it.
    pub auth_max_failures: u32,
    /// Seconds over which those failures are counted.
    pub auth_window: u64,
    /// Seconds an address is refused once it has tripped.
    pub auth_block: u64,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced {
            // Five tries a minute, then an hour out. Deliberately strict: this
            // guards one password on a machine its owner can always reach by
            // other means, so the cost of being wrong is small and the cost of
            // being too permissive is someone else's.
            auth_max_failures: 5,
            auth_window: 60,
            auth_block: 3600,
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
        let secs = |key: &str, lo: u64, hi: u64, fallback: u64| -> u64 {
            cfg.get_int(key)
                .map_or(fallback, |v| (v.max(0) as u64).clamp(lo, hi))
        };
        let count = |key: &str, lo: usize, hi: usize, fallback: usize| -> usize {
            cfg.get_int(key)
                .map_or(fallback, |v| (v.max(0) as usize).clamp(lo, hi))
        };

        Advanced {
            // Zero is meaningful: it switches the lockout off. Anything above
            // it is clamped to something a person could plausibly mean.
            auth_max_failures: count("webui.auth_max_failures", 0, 1000, d.auth_max_failures as usize)
                as u32,
            // At least a second of window - a zero window would count every
            // failure in its own window and never trip.
            auth_window: secs("webui.auth_window", 1, 86400, d.auth_window),
            // A week's ceiling. No "forever": a lockout the owner cannot wait
            // out is a way to lock yourself out of your own client.
            auth_block: secs("webui.auth_block", 1, 604_800, d.auth_block),
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
    /// Capability tokens for the streaming endpoint - the one way in that is
    /// not the web interface's password. See [`streamtoken`].
    stream_tokens: Arc<streamtoken::StreamTokens>,
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
    /// Browsers holding an event stream open, this one included.
    clients: usize,
    /// Whether the alternative speed limits are on, so the turtle in the
    /// toolbar comes back lit after a reload rather than resetting to off.
    alt_speed: bool,
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
/// Every string key the page asks for, through either channel.
///
/// Shared with the test that checks they all resolve: if the test scanned
/// separately it could agree with itself while disagreeing with what actually
/// gets substituted, which is the one thing worth ruling out.
fn template_keys(html: &str) -> Vec<&str> {
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
}

fn render_page(tr: &crate::ui::translator::Translator) -> String {
    let html = include_str!("index.html");

    let keys = template_keys(html);

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

/// `GET /favicon.ico` - the application icon.
///
/// Browsers ask for this unprompted on every first load, so without it the
/// server answers its own page with a 404 in the console. The same PNG the
/// desktop window uses, so the tab matches the app.
async fn h_favicon() -> impl Responder {
    HttpResponse::Ok()
        .content_type("image/png")
        .insert_header(("Cache-Control", "public, max-age=86400"))
        .body(&include_bytes!("../../res/app-256.png")[..])
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
            clients: connected_clients(),
            alt_speed: st.cfg.get_bool("speed.alt_enabled"),
        }
    })
    .await?;

    Ok(web::Json(info))
}

/// Browsers currently holding an event stream open.
///
/// One stream is one viewer, which makes this the honest count of who is
/// watching - better than counting requests, which cannot tell a browser that
/// is still there from one that closed its tab an hour ago.
///
/// A global rather than something threaded through AppState: the desktop
/// window wants to read it, and it has no handle on the web server - the
/// server is rebuilt whenever the web settings change, and this outlives that.
static WEB_CLIENTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many browsers are watching right now.
pub fn connected_clients() -> usize {
    WEB_CLIENTS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Counts one viewer for as long as it is alive.
///
/// A guard rather than a decrement at the end of the loop: the loop can end by
/// break, by the channel closing, or by the task being dropped when the server
/// restarts, and only a Drop covers all three. Getting this wrong leaks a
/// viewer that never disconnects, and the count only ever climbs.
struct Viewer;

impl Viewer {
    fn new() -> Self {
        WEB_CLIENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Viewer
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        WEB_CLIENTS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How often a connected browser is sent a fresh snapshot.
///
/// One second, matching the desktop window's own tick. Polling could not
/// afford this - every request re-ran Argon2 - but a stream authenticates once,
/// so the frequency is now a question of bandwidth rather than CPU, and a few
/// kilobytes of JSON a second over a LAN is nothing.
const EVENT_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// Everything the page redraws itself from, in one object.
///
/// Built on the blocking pool: every session call in here blocks, which is the
/// same reason the individual handlers use `web::block`.
fn snapshot(state: &AppState) -> serde_json::Value {
    let labels: HashMap<i32, String> = state
        .cfg
        .get_labels()
        .into_iter()
        .map(|l| (l.id, l.name))
        .collect();
    let rows: Vec<TorrentDto> = state
        .session
        .torrents(&labels)
        .into_iter()
        .map(TorrentDto::from)
        .collect();
    let (down, up) = state.session.session_rates();

    let mut errors: Vec<String> = Vec::new();
    if let Ok(rx) = state.errors.lock() {
        // The translator is built only when there is something to say with it:
        // this runs on every poll and loading a locale is a file read.
        let mut tr = None;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::Error(err) = event {
                let tr = tr.get_or_insert_with(|| state.translator());
                errors.push(err.text(tr));
            }
        }
    }

    serde_json::json!({
        "session": SessionInfo {
            version: crate::buildinfo::version(),
            listen_port: state.session.listen_port(),
            dht_nodes: state.session.dht_nodes(),
            download_rate: down,
            upload_rate: up,
            torrents: rows.len(),
            clients: connected_clients(),
            alt_speed: state.cfg.get_bool("speed.alt_enabled"),
        },
        "torrents": rows,
        "errors": errors,
    })
}

/// `GET /api/events` - a Server-Sent Events stream of snapshots.
///
/// This replaces polling `/api/session`, `/api/torrents` and `/api/errors` on a
/// timer. The three endpoints remain: they are the documented API, they are
/// what a script would use, and they are the fallback for a browser that
/// cannot open a stream.
///
/// The reason is not the JSON, which is tiny. It is that HTTP Basic auth
/// verifies the password with Argon2 on EVERY request - deliberately expensive,
/// measured here at ~21ms against ~0.8ms for a request that never reaches it.
/// Three of those every two seconds is about 3% of a core, continuously, for
/// each open tab. A stream pays it once, when it connects.
///
/// SSE rather than a WebSocket: this is one-way traffic - every action the page
/// takes is already a POST - and a plain GET inherits the Basic auth that a
/// WebSocket handshake cannot carry in a browser. It also needs no dependency
/// and reconnects by itself.
///
/// ponytail: one sampler per connection, so two open tabs sample twice. Fine
/// for a personal client; a shared broadcast is the upgrade if it ever matters.
async fn h_events(state: web::Data<AppState>) -> impl Responder {
    use futures::SinkExt;

    let (mut tx, rx) =
        futures::channel::mpsc::channel::<Result<web::Bytes, actix_web::Error>>(4);

    actix_web::rt::spawn(async move {
        // Dropped when this task ends, however it ends.
        let _viewer = Viewer::new();
        loop {
            let st = state.clone();
            let Ok(payload) = web::block(move || snapshot(&st)).await else {
                break;
            };
            let frame = format!("data: {payload}\n\n");
            // Fails once the browser has gone, which is how this task ends -
            // there is no other owner to tell it to stop.
            if tx.send(Ok(web::Bytes::from(frame))).await.is_err() {
                break;
            }
            tokio::time::sleep(EVENT_TICK).await;
        }
    });

    HttpResponse::Ok()
        .content_type("text/event-stream")
        // A cached or buffered event stream is not an event stream. The last
        // header is for reverse proxies, which buffer by default.
        .insert_header(("Cache-Control", "no-store"))
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(rx)
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
        let mut drained: Vec<String> = Vec::new();
        let mut tr = None;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::Error(err) = event {
                let tr = tr.get_or_insert_with(|| st.translator());
                drained.push(err.text(tr));
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
    /// The file's index in the TORRENT, which is what `only_files` means -
    /// not its position in this list. Padding files are filtered out, so the
    /// two stopped being the same thing.
    index: usize,
    path: String,
    size: u64,
}

/// What a `.torrent` turns out to contain.
#[derive(Serialize)]
struct Inspected {
    name: String,
    total_size: i64,
    /// In metainfo order, minus padding files. Hand back each file's `index`
    /// in `only_files`, NOT its position here.
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
                        // Padding is an alignment artifact, not a file. Hidden
                        // here always: unlike the desktop list this is a
                        // picker, and there is nothing to pick.
                        .filter(|f| !f.padding)
                        .map(|f| InspectedFile {
                            index: f.index,
                            path: f.path,
                            size: f.size,
                        })
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


// --- the remote's half of the desktop's per-torrent controls -----------------
//
// Everything below mirrors something the desktop already does, and calls the
// same `Session` method it calls. Nothing here decides policy: a limit written
// from a phone has to behave exactly like the same limit set from the window,
// or one of the two is lying.

#[derive(Deserialize)]
struct QueueRequest {
    /// "top", "up", "down" or "bottom".
    to: String,
}

/// `POST /api/torrents/{hash}/queue` - move one torrent in the queue.
async fn h_queue(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<QueueRequest>,
) -> actix_web::Result<HttpResponse> {
    use crate::bittorrent::session::QueueMove;
    let to = match body.into_inner().to.as_str() {
        "top" => QueueMove::Top,
        "up" => QueueMove::Up,
        "down" => QueueMove::Down,
        "bottom" => QueueMove::Bottom,
        other => {
            return Err(ErrorBadRequest(format!(
                "unknown queue move {other:?}, expected top/up/down/bottom"
            )));
        }
    };
    with_torrent(&state, hash.into_inner(), move |s, h| {
        s.move_in_queue(h, to)
    })
    .await
}

/// `POST /api/torrents/{hash}/reannounce` - ask the trackers again now.
async fn h_reannounce(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    with_torrent(&state, hash.into_inner(), |s, h| s.reannounce(h)).await
}

/// One peer, for `GET /api/torrents/{hash}/peers`.
#[derive(Serialize)]
struct PeerRow {
    addr: String,
    state: String,
    fetched_bytes: u64,
    pieces: u32,
}

#[derive(Serialize)]
struct FileRow {
    index: usize,
    name: String,
    length: u64,
    progress: f32,
    /// 0 skip, 1 normal, 2 high, 3 maximum - the same scale the database and
    /// the desktop's Files tab use.
    priority: i64,
}

/// A `Range: bytes=...` header as an inclusive (start, end), given the length.
///
/// Only the single-range forms a media player actually sends: `bytes=a-b`,
/// `bytes=a-` and the suffix form `bytes=-n`. A multi-range request would need a
/// multipart reply, which no player asks for, so it is refused rather than
/// half-answered. `None` means the header was absent or unusable and the whole
/// file should be sent.
/// The whole file as an inclusive byte range, or `None` if there is no byte in
/// it to name.
///
/// `(0, len - 1)` written out, because `len - 1` on a u64 is not a small
/// mistake: at `len == 0` it panics in a debug build and wraps to 18 exabytes
/// in a release one, and a zero-length file is an ordinary thing for a torrent
/// to contain. `parse_range` already refuses every range against an empty file
/// through the same `checked_sub`; this is the path that had no header to
/// parse.
fn whole_file_range(len: u64) -> Option<(u64, u64)> {
    Some((0, len.checked_sub(1)?))
}

fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (from, to) = spec.split_once('-')?;
    let last = len.checked_sub(1)?;

    if from.is_empty() {
        // bytes=-n - the final n bytes.
        let n: u64 = to.parse().ok()?;
        if n == 0 {
            return None;
        }
        return Some((len.saturating_sub(n), last));
    }

    let start: u64 = from.parse().ok()?;
    if start > last {
        return None;
    }
    let end = if to.is_empty() {
        last
    } else {
        to.parse::<u64>().ok()?.min(last)
    };
    if start > end { None } else { Some((start, end)) }
}

/// A torrent filename, safe to put in a header or a playlist.
///
/// Two separate problems, one answer:
///
/// * A header value may not contain a control character. `insert_header`
///   does not panic on one - it stores the error and the response becomes a
///   500 - so a torrent with a `\x01` in a filename turned the whole stream
///   into a server error instead of a download.
/// * `"` and `\` end the quoting in `Content-Disposition`, and CR/LF would
///   split the header outright.
///
/// Everything below `0x20`, plus DEL, plus the three quoting characters,
/// becomes `_`. Bytes above 0x7F are left alone: they are legal in a header
/// value as obs-text, and stripping them would mangle every non-Latin
/// filename there is.
fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\\' | '\x7f' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// A content type a media player will accept, from the file's extension.
///
/// Hand-rolled rather than a mime database: the list that matters for streaming
/// is short, and `application/octet-stream` is a fine answer for the rest -
/// players sniff the container anyway. `video/x-matroska` is the one worth
/// getting right, because browsers use it to decide they cannot play a file.
fn stream_content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("mov") => "video/quicktime",
        Some("wmv") => "video/x-ms-wmv",
        Some("flv") => "video/x-flv",
        Some("mpg") | Some("mpeg") => "video/mpeg",
        Some("ts") | Some("m2ts") | Some("mts") => "video/mp2t",
        Some("ogv") => "video/ogg",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("aac") => "audio/aac",
        Some("m4a") => "audio/mp4",
        Some("opus") => "audio/opus",
        Some("ogg") | Some("oga") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("srt") => "application/x-subrip",
        Some("vtt") => "text/vtt",
        _ => "application/octet-stream",
    }
}

/// `GET /api/torrents/{hash}/files/{index}/playlist.m3u` - a playlist the
/// operating system will open in a media player.
///
/// The browser downloads this; the OS opens it. NanoTorrent launches nothing,
/// which is why this works from the web interface at all, and why it works the
/// same from another machine.
async fn h_playlist(
    req: actix_web::HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<(String, usize)>,
) -> actix_web::Result<HttpResponse> {
    let (hash, index) = path.into_inner();

    // Opened only to learn the name and to be sure the file is really there -
    // a playlist pointing at a 404 is worse than a refusal.
    let probe = state
        .session
        .stream_file(&hash, index, None)
        .ok_or_else(|| ErrorNotFound("no such torrent, file, or metadata not resolved yet"))?;
    let name = probe.name.clone();
    drop(probe);

    let token = state
        .stream_tokens
        .issue(&hash, index)
        .ok_or_else(|| ErrorBadRequest("that is not an info hash"))?;

    // The client's own view of how it reached us. Host is client-supplied, but
    // this URL is going straight back to that same client, so a spoofed one
    // only misdirects the spoofer.
    let info = req.connection_info();
    let base = format!("{}://{}", info.scheme(), info.host());
    let url = format!("{base}/api/torrents/{hash}/files/{index}/stream?token={token}");

    // Sanitised for the BODY as much as for the header. An .m3u is
    // line-oriented, so a newline in a torrent's filename appends a line of the
    // torrent author's choosing to the playlist - including another URL, which
    // the media player would then go and fetch. The header was already being
    // cleaned; the body was not, which is the half that mattered.
    let safe = safe_filename(&name);

    // #EXTINF gives the player something to show instead of the URL. -1 because
    // the duration is not knowable from here, which players accept.
    let body = format!("#EXTM3U\n#EXTINF:-1,{safe}\n{url}\n");
    Ok(HttpResponse::Ok()
        .content_type("audio/x-mpegurl")
        .insert_header((
            "Content-Disposition",
            format!("attachment; filename=\"{safe}.m3u\""),
        ))
        // A playlist carrying a token has no business in a shared cache, or in
        // the browser's back/forward cache after the token dies.
        .insert_header(("Cache-Control", "no-store"))
        .body(body))
}

/// `GET|HEAD /api/torrents/{hash}/files/{index}/stream` - the file's bytes,
/// while it is still downloading.
///
/// This is what makes "watch it now" possible without NanoTorrent launching
/// anything: point VLC, mpv or a browser at this URL and the player does the
/// playing. librqbit's reader blocks on a piece it does not have yet and asks
/// for it, so seeking works on an incomplete file.
///
/// `Accept-Ranges` and 206 are not optional here. Players open the URL, read the
/// container header, and immediately seek - usually to the end, for the index of
/// an MP4 written that way. Answering 200-and-the-whole-file to every request
/// makes a player appear to hang while it downloads a film to reach the part you
/// asked for.
async fn h_stream(
    req: actix_web::HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<(String, usize)>,
) -> actix_web::Result<HttpResponse> {
    let (hash, index) = path.into_inner();

    let range_header = req
        .headers()
        .get(actix_web::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Opened before the range is known to be satisfiable, because the file
    // length comes from the torrent. A HEAD gets the same open and drops it.
    let probe = state
        .session
        .stream_file(&hash, index, None)
        .ok_or_else(|| ErrorNotFound("no such torrent, file, or metadata not resolved yet"))?;
    let file_len = probe.file_len;
    let name = probe.name.clone();
    drop(probe);

    let range = match range_header.as_deref() {
        Some(h) => match parse_range(h, file_len) {
            Some(r) => Some(r),
            // A range header we understood but cannot satisfy. 416 with the real
            // length is what tells a player to ask again sensibly.
            None => {
                return Ok(HttpResponse::RangeNotSatisfiable()
                    .insert_header(("Content-Range", format!("bytes */{file_len}")))
                    .insert_header(("Accept-Ranges", "bytes"))
                    .finish());
            }
        },
        None => None,
    };

    let content_type = stream_content_type(&name);

    // See `whole_file_range`: an empty file has no last byte, and answering
    // with nothing is the honest reply rather than an underflow.
    if whole_file_range(file_len).is_none() {
        return Ok(HttpResponse::Ok()
            .insert_header(("Accept-Ranges", "bytes"))
            .insert_header((
                "Content-Disposition",
                format!("inline; filename=\"{}\"", safe_filename(&name)),
            ))
            .content_type(content_type)
            .body(actix_web::body::SizedStream::new(
                0,
                futures::stream::empty::<Result<actix_web::web::Bytes, std::io::Error>>(),
            )));
    }

    let (start, end) = match range.or_else(|| whole_file_range(file_len)) {
        Some(r) => r,
        None => return Ok(HttpResponse::NoContent().finish()),
    };
    let length = end - start + 1;

    let mut res = if range.is_some() {
        let mut r = HttpResponse::PartialContent();
        r.insert_header(("Content-Range", format!("bytes {start}-{end}/{file_len}")));
        r
    } else {
        HttpResponse::Ok()
    };
    res.insert_header(("Accept-Ranges", "bytes"))
        .insert_header(("Content-Length", length.to_string()))
        // inline, so a browser plays it rather than offering to save it. The
        // name is quoted and its own quotes stripped - torrent filenames are
        // hostile input and this one ends up in a header.
        .insert_header((
            "Content-Disposition",
            format!("inline; filename=\"{}\"", safe_filename(&name)),
        ))
        .content_type(content_type);

    if req.method() == actix_web::http::Method::HEAD {
        // SizedStream, not finish(): actix derives Content-Length from the body,
        // so an empty HEAD body answers `content-length: 0` and a player that
        // probes with HEAD first reads that as an empty file. SizedStream
        // declares the length the matching GET would send, and sends nothing.
        return Ok(res.body(actix_web::body::SizedStream::new(
            length,
            futures::stream::empty::<Result<actix_web::web::Bytes, std::io::Error>>(),
        )));
    }

    let open = state
        .session
        .stream_file(&hash, index, range)
        .ok_or_else(|| ErrorNotFound("the torrent went away while opening the stream"))?;

    let body = futures::stream::unfold(open.chunks, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (chunk.map(actix_web::web::Bytes::from), rx))
    });
    Ok(res.streaming(body))
}

/// `GET /api/torrents/{hash}/peers` - who this torrent is talking to.
///
/// The desktop's Peers tab has always had this; the web API could describe a
/// torrent but not its swarm, which left a third-party dashboard unable to show
/// the one thing that explains a slow download.
async fn h_peers(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let st = state.clone();
    let hash = hash.into_inner();
    let rows = web::block(move || {
        if !st.session.exists(&hash) {
            return None;
        }
        Some(
            st.session
                .peers(&hash)
                .into_iter()
                .map(|p| PeerRow {
                    addr: p.addr,
                    state: p.state,
                    fetched_bytes: p.fetched_bytes,
                    pieces: p.pieces,
                })
                .collect::<Vec<_>>(),
        )
    })
    .await?
    .ok_or_else(|| ErrorNotFound("no torrent with that info hash"))?;

    Ok(HttpResponse::Ok().json(rows))
}

/// `GET /api/torrents/{hash}/magnet` - a magnet link for a torrent already here.
///
/// For exporting, or handing the same torrent to something else. Built from the
/// info hash and display name, which is all a magnet needs to be resolvable.
async fn h_magnet(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let st = state.clone();
    let hash = hash.into_inner();
    let uri = web::block(move || {
        st.session
            .torrents(&std::collections::HashMap::new())
            .into_iter()
            .find(|t| t.info_hash == hash)
            .map(|t| st.session.magnet_uri(&hash, &t.name))
    })
    .await?
    .ok_or_else(|| ErrorNotFound("no torrent with that info hash"))?;

    Ok(HttpResponse::Ok().json(serde_json::json!({ "magnet": uri })))
}

/// `GET /api/torrents/{hash}/files` - the file list with its priorities.
async fn h_files(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let st = state.clone();
    let hash = hash.into_inner();
    let rows = web::block(move || {
        if !st.session.exists(&hash) {
            return None;
        }
        let stored = st.session.file_priorities(&hash);
        Some(
            st.session
                .files(&hash)
                .into_iter()
                .enumerate()
                .map(|(index, f)| FileRow {
                    index,
                    name: f.name,
                    length: f.length,
                    progress: f.progress,
                    // Absent means nobody has touched it, which is Normal -
                    // only rows that differ are stored.
                    priority: stored
                        .get(&index)
                        .copied()
                        .unwrap_or(crate::bittorrent::session::PRIORITY_NORMAL),
                })
                .collect::<Vec<_>>(),
        )
    })
    .await?
    .ok_or_else(|| ErrorNotFound("no torrent with that info hash"))?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Deserialize)]
struct FilePriorityRequest {
    index: usize,
    priority: i64,
}

/// `POST /api/torrents/{hash}/files` - set one file's priority.
async fn h_set_file_priority(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<FilePriorityRequest>,
) -> actix_web::Result<HttpResponse> {
    use crate::bittorrent::session::{PRIORITY_MAX, PRIORITY_SKIP};
    let req = body.into_inner();
    if !(PRIORITY_SKIP..=PRIORITY_MAX).contains(&req.priority) {
        return Err(ErrorBadRequest(
            "priority must be 0 (skip), 1 (normal), 2 (high) or 3 (maximum)",
        ));
    }
    with_torrent(&state, hash.into_inner(), move |s, h| {
        s.set_file_priority(h, req.index, req.priority);
    })
    .await
}

#[derive(Serialize)]
struct TrackerRowJson {
    /// "source" (DHT/LSD/PeX), "tier" (a group heading) or "tracker".
    kind: &'static str,
    label: String,
    status: String,
    seeders: Option<u32>,
    leechers: Option<u32>,
    fails: u32,
}

/// `GET /api/torrents/{hash}/trackers` - the Trackers tab, as data.
async fn h_trackers(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    use crate::bittorrent::session::TrackerRowKind;
    let st = state.clone();
    let hash = hash.into_inner();
    let rows = web::block(move || {
        if !st.session.exists(&hash) {
            return None;
        }
        let tr = st.translator();
        Some(
            st.session
                .tracker_rows(&hash, &tr)
                .into_iter()
                .map(|r| TrackerRowJson {
                    kind: match r.kind {
                        TrackerRowKind::Source => "source",
                        TrackerRowKind::Tier => "tier",
                        TrackerRowKind::Tracker => "tracker",
                    },
                    label: r.label,
                    status: r.status,
                    seeders: r.seeders,
                    leechers: r.leechers,
                    fails: r.fails,
                })
                .collect::<Vec<_>>(),
        )
    })
    .await?
    .ok_or_else(|| ErrorNotFound("no torrent with that info hash"))?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Deserialize)]
struct TrackerRequest {
    /// Add: the new URL. Edit: the replacement. Remove: the one to drop.
    url: String,
    /// Add only. Past the last tier means a new one, which is how "add a tier"
    /// is spelled - the same rule the desktop's picker follows.
    #[serde(default)]
    tier: usize,
    /// Edit only: the URL being replaced.
    #[serde(default)]
    from: Option<String>,
}

/// `POST /api/torrents/{hash}/trackers` - add one.
async fn h_add_tracker(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<TrackerRequest>,
) -> actix_web::Result<HttpResponse> {
    let req = body.into_inner();
    let st = state.clone();
    let hash = hash.into_inner();
    let ok = web::block(move || {
        st.session.exists(&hash) && st.session.add_tracker(&hash, req.url.trim(), req.tier)
    })
    .await?;
    if ok {
        Ok(HttpResponse::NoContent().finish())
    } else {
        // One message for both causes: a URL that is not a tracker address,
        // and a torrent that is no longer there. The caller can tell which by
        // whether the torrent is still in its list.
        Err(ErrorBadRequest("not a usable tracker URL for that torrent"))
    }
}

/// `POST /api/torrents/{hash}/trackers/edit` - replace one URL, keeping its
/// tier. Deliberately not a DELETE-then-POST: the tier would be lost.
async fn h_edit_tracker(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<TrackerRequest>,
) -> actix_web::Result<HttpResponse> {
    let req = body.into_inner();
    let Some(from) = req.from else {
        return Err(ErrorBadRequest("`from` is required when editing a tracker"));
    };
    let st = state.clone();
    let hash = hash.into_inner();
    let ok = web::block(move || {
        st.session.exists(&hash) && st.session.edit_tracker(&hash, &from, req.url.trim())
    })
    .await?;
    if ok {
        Ok(HttpResponse::NoContent().finish())
    } else {
        Err(ErrorBadRequest("not a usable tracker URL for that torrent"))
    }
}

/// `POST /api/torrents/{hash}/trackers/remove` - stop using one.
///
/// A POST rather than a DELETE because the URL travels in the body: it is long,
/// contains its own query string, and does not want percent-encoding twice.
async fn h_remove_tracker(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<TrackerRequest>,
) -> actix_web::Result<HttpResponse> {
    let url = body.into_inner().url;
    with_torrent(&state, hash.into_inner(), move |s, h| {
        s.remove_tracker(h, &url);
    })
    .await
}

#[derive(Serialize)]
struct TagsResponse {
    /// Every tag that exists, so the page can offer them all.
    all: Vec<String>,
    /// The ones on this torrent.
    on: Vec<String>,
    /// This torrent's own ratio limit. `null` follows the global setting.
    ratio_limit: Option<f64>,
    /// This torrent's own seeding time limit, in minutes. `null` follows the
    /// global setting.
    seed_time_limit: Option<i64>,
}

/// `GET /api/torrents/{hash}/tags` - tags and share limits.
///
/// One read for the whole panel rather than two: they are shown together and
/// change together, and a second round trip buys nothing.
async fn h_tags(
    state: web::Data<AppState>,
    hash: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let st = state.clone();
    let hash = hash.into_inner();
    let out = web::block(move || {
        if !st.session.exists(&hash) {
            return None;
        }
        let (ratio_limit, seed_time_limit) = st.session.share_overrides(&hash);
        Some(TagsResponse {
            all: st.cfg.get_tags().into_iter().map(|t| t.name).collect(),
            on: st.cfg.tags_for(&hash).into_iter().map(|t| t.name).collect(),
            ratio_limit,
            seed_time_limit,
        })
    })
    .await?
    .ok_or_else(|| ErrorNotFound("no torrent with that info hash"))?;
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Deserialize)]
struct TagsRequest {
    /// The complete set this torrent should carry. Names that do not exist yet
    /// are created, the same as typing a new one in Preferences.
    tags: Vec<String>,
}

/// `POST /api/torrents/{hash}/tags` - replace the torrent's tags.
async fn h_set_tags(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<TagsRequest>,
) -> actix_web::Result<HttpResponse> {
    let wanted = body.into_inner().tags;
    let st = state.clone();
    let hash = hash.into_inner();
    let found = web::block(move || {
        if !st.session.exists(&hash) {
            return false;
        }
        let wanted: Vec<String> = wanted
            .into_iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        // Remove first, then add: a tag in both lists is left alone rather
        // than being taken off and put back.
        for tag in st.cfg.tags_for(&hash) {
            if !wanted.contains(&tag.name) {
                st.cfg.remove_tag(&hash, tag.id);
            }
        }
        for name in &wanted {
            if let Some(id) = st.cfg.ensure_tag(name) {
                st.cfg.add_tag(&hash, id);
            }
        }
        true
    })
    .await?;
    if found {
        Ok(HttpResponse::NoContent().finish())
    } else {
        Err(ErrorNotFound("no torrent with that info hash"))
    }
}

#[derive(Deserialize)]
struct LimitsRequest {
    /// Stop seeding at this ratio. `null` follows the global setting, 0 means
    /// no limit at all - the same three-way the nullable column carries.
    #[serde(default, deserialize_with = "double_option")]
    ratio: Option<Option<f64>>,
    /// Stop seeding after this many minutes. Same three-way.
    #[serde(default, deserialize_with = "double_option")]
    seed_minutes: Option<Option<i64>>,
}

/// Tells "absent" from "present and null", which is what lets one endpoint set
/// either limit without disturbing the other.
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(d).map(Some)
}

/// `POST /api/torrents/{hash}/limits` - this torrent's own share limits.
async fn h_limits(
    state: web::Data<AppState>,
    hash: web::Path<String>,
    body: web::Json<LimitsRequest>,
) -> actix_web::Result<HttpResponse> {
    let req = body.into_inner();
    with_torrent(&state, hash.into_inner(), move |s, h| {
        if let Some(ratio) = req.ratio {
            s.set_ratio_limit(h, ratio);
        }
        if let Some(minutes) = req.seed_minutes {
            s.set_seed_time_limit(h, minutes);
        }
    })
    .await
}

#[derive(Deserialize)]
struct AltSpeedRequest {
    enabled: bool,
}

/// `POST /api/speed/alt` - the turtle button.
///
/// Writes the manual switch only. The schedule can also turn the alternative
/// limits on, and this must not silently turn that off - the scheduler would
/// put them straight back and the button would look broken.
async fn h_alt_speed(
    state: web::Data<AppState>,
    body: web::Json<AltSpeedRequest>,
) -> actix_web::Result<HttpResponse> {
    let on = body.into_inner().enabled;
    let st = state.clone();
    web::block(move || st.cfg.set("speed.alt_enabled", &on)).await?;
    Ok(HttpResponse::NoContent().finish())
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

// --- column widths ---------------------------------------------------------
//

// --- plugins ---------------------------------------------------------------
//
// A plugin's surface is held by the plugin host (`plugins::ui`), not by the
// desktop window - the window reads a snapshot and posts events back. These
// three routes do exactly the same thing, so a `.rhai` plugin written for the
// desktop shows up in the browser with no changes and no new permission: it is
// still `ui`, still the same list-and-buttons shape.

#[derive(Serialize)]
struct PluginRowDto {
    id: String,
    title: String,
    subtitle: String,
    selected: bool,
}

impl From<&crate::plugins::ui::Row> for PluginRowDto {
    fn from(r: &crate::plugins::ui::Row) -> Self {
        PluginRowDto {
            id: r.id.clone(),
            title: r.title.clone(),
            subtitle: r.subtitle.clone(),
            selected: r.selected,
        }
    }
}

#[derive(Serialize)]
struct PluginListDto {
    name: String,
    /// The plugin's own window title, empty when it has declared no window.
    title: String,
    configurable: bool,
    /// The plugin's own icon as SVG path data, empty when it declared none.
    /// Drawn by the web interface at 16x16; see `ui_icon` in plugins/api.rs,
    /// which is also what guarantees this cannot carry markup.
    icon: String,
    /// The user's switch. Independent of whether it compiles: a broken plugin
    /// stays on and shows its error, because switching it off would hide the
    /// problem.
    enabled: bool,
    /// True once it is running and has declared a window - only then is there
    /// anything to open.
    has_window: bool,
    /// Why it will not run, if it will not.
    error: Option<String>,
    /// Waiting for its permissions to be approved. The web interface can
    /// switch a plugin on, but deliberately cannot approve one: consent to
    /// what a script may reach is asked for at the machine it runs on.
    needs_approval: bool,
    /// What its header asks for, in words.
    permissions: Vec<String>,
}

#[derive(Serialize)]
struct PluginSurfaceDto {
    name: String,
    title: String,
    status: String,
    placeholder: String,
    configurable: bool,
    /// `[id, label]` pairs, drawn left to right.
    buttons: Vec<[String; 2]>,
    groups: Vec<PluginRowDto>,
    rows: Vec<PluginRowDto>,
    /// Non-empty while the plugin has a form up, in which case the browser
    /// draws that instead of the lists - the same swap the window makes.
    form_id: String,
    form_title: String,
    fields: Vec<PluginFieldDto>,
}

#[derive(Serialize)]
struct PluginFieldDto {
    id: String,
    label: String,
    /// "text", "check", "choice" or "number".
    kind: String,
    value: String,
    options: Vec<String>,
    hint: String,
}

impl From<&crate::plugins::ui::Field> for PluginFieldDto {
    fn from(f: &crate::plugins::ui::Field) -> Self {
        PluginFieldDto {
            id: f.id.clone(),
            label: f.label.clone(),
            kind: f.kind.clone(),
            value: f.value.clone(),
            options: f.options.clone(),
            hint: f.hint.clone(),
        }
    }
}

/// What the browser is allowed to send back. Named rather than free-form so a
/// request cannot invent an event the desktop window could not raise.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum PluginEventBody {
    /// The plugin's chance to fill the surface before it is looked at - the
    /// browser raises this when the panel is opened, as the window does.
    Opened,
    Row { id: String },
    Group { id: String },
    Button {
        id: String,
        #[serde(default)]
        input: String,
    },
    Menu { id: String },
    /// A form saved. `values` is field id to value, the same shape the window
    /// reads back out of its own controls.
    Form {
        id: String,
        #[serde(default)]
        values: Vec<(String, String)>,
    },
    /// A form dismissed without saving.
    FormCancel { id: String },
    /// One of the plugin's items on a file's context menu. The file is named
    /// in the request because the browser knows which row was clicked and the
    /// host does not - there is no "current torrent" on this side.
    FileMenu {
        id: String,
        hash: String,
        index: i64,
        #[serde(default)]
        name: String,
    },
    Configure,
}

/// Refuse anything the plugin host does not already know about.
///
/// Without this the name from the URL would reach `ui::post` unchecked, and a
/// plugin that is disabled - or was never installed - would look to the host
/// like one that simply had nothing on its surface.
fn known_plugin(name: &str) -> bool {
    crate::plugins::ui::windows().iter().any(|w| w.name == name)
        // A plugin may offer items on a file's context menu and have no window
        // at all - handing a file to a media player needs no window. Its
        // clicks still have to reach it.
        || crate::plugins::ui::file_menu_items().iter().any(|(n, _, _)| n == name)
}

/// `GET /api/plugins` - every plugin in the folder, running or not.
///
/// The whole folder rather than only the ones with a window: "which plugins do
/// I have, and are they on" is the question the button is answering, and a
/// plugin that is switched off has no window by definition - listing only
/// windows would make a disabled plugin look like one that does not exist.
async fn h_plugins(state: web::Data<AppState>) -> actix_web::Result<HttpResponse> {
    let dir = crate::plugins::plugin_dir(&state.env);
    let windows = crate::plugins::ui::windows();

    let list: Vec<PluginListDto> = crate::plugins::scan(&dir, &state.cfg)
        .into_iter()
        .map(|p| {
            let window = windows.iter().find(|w| w.name == p.name);
            PluginListDto {
                title: window.map(|w| w.title.clone()).unwrap_or_default(),
                configurable: window.is_some_and(|w| w.configurable),
                icon: window.map(|w| w.icon.clone()).unwrap_or_default(),
                has_window: window.is_some(),
                enabled: p.enabled,
                error: p.error,
                needs_approval: !p.granted && !p.requested.is_empty(),
                permissions: p.requested.iter().map(|x| x.tag().to_owned()).collect(),
                name: p.name,
            }
        })
        .collect();
    Ok(HttpResponse::Ok().json(list))
}

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

/// `GET /api/plugins/file-menu` - what plugins offer on a file's context menu.
///
/// Flat, across every plugin, because that is how it is drawn: one menu on one
/// file, not a menu per plugin. Empty for a session with no plugins, which is
/// the usual one, and the browser then draws no menu at all.
async fn h_plugin_file_menu() -> actix_web::Result<HttpResponse> {
    let items: Vec<serde_json::Value> = crate::plugins::ui::file_menu_items()
        .into_iter()
        .map(|(plugin, id, label)| {
            serde_json::json!({
                // `plugin` addresses it, `title` is the same name as it should
                // be read - computed here rather than in the browser so both
                // surfaces spell a plugin's name the same way.
                "title": crate::plugins::ui::display_name(&plugin),
                "plugin": plugin,
                "id": id,
                "label": label,
            })
        })
        .collect();
    Ok(HttpResponse::Ok().json(items))
}

/// `POST /api/plugins/{name}/enabled` - switch one plugin on or off.
///
/// Reloads the host, so the change takes effect now rather than at the next
/// start - the same thing the Preferences checkbox does. Approval is NOT
/// granted here: switching a plugin on that has never been approved leaves it
/// waiting, which is the point.
async fn h_plugin_enabled(
    state: web::Data<AppState>,
    name: web::Path<String>,
    body: web::Json<EnabledBody>,
) -> actix_web::Result<HttpResponse> {
    let name = name.into_inner();
    let dir = crate::plugins::plugin_dir(&state.env);

    // Only a plugin that is actually in the folder. Without this the name from
    // the URL would be written straight into the disabled list, which would
    // then carry entries for files that never existed.
    if !crate::plugins::scan(&dir, &state.cfg).iter().any(|p| p.name == name) {
        return Ok(HttpResponse::NotFound().json(serde_json::json!({
            "error": "no such plugin"
        })));
    }

    crate::plugins::set_enabled(&state.cfg, &name, body.enabled);
    crate::plugins::reload(state.session.clone(), state.cfg.clone(), state.env.clone());
    Ok(HttpResponse::NoContent().finish())
}

/// `GET /api/plugins/{name}` - one plugin's surface as it stands now.
async fn h_plugin(name: web::Path<String>) -> actix_web::Result<HttpResponse> {
    let name = name.into_inner();
    let Some(ui) = crate::plugins::ui::snapshot(&name).filter(|u| u.has_window()) else {
        return Ok(HttpResponse::NotFound().json(serde_json::json!({
            "error": "no such plugin, or it has no window"
        })));
    };

    Ok(HttpResponse::Ok().json(PluginSurfaceDto {
        name,
        title: ui.title.clone(),
        status: ui.status.clone(),
        placeholder: ui.placeholder.clone(),
        configurable: ui.configurable,
        buttons: ui
            .buttons
            .iter()
            .map(|(id, label)| [id.clone(), label.clone()])
            .collect(),
        groups: ui.groups.iter().map(PluginRowDto::from).collect(),
        rows: ui.rows.iter().map(PluginRowDto::from).collect(),
        form_id: ui.form_id.clone(),
        form_title: ui.form_title.clone(),
        fields: ui.fields.iter().map(PluginFieldDto::from).collect(),
    }))
}

/// `POST /api/plugins/{name}/event` - a click, in the plugin's own terms.
///
/// Fire and forget, exactly as the desktop window does: the plugin runs on the
/// host's own thread and reports by updating its surface, which the next GET
/// picks up. Waiting for it here would tie a browser request to however long
/// someone's script decides to take.
async fn h_plugin_event(
    name: web::Path<String>,
    body: web::Json<PluginEventBody>,
) -> actix_web::Result<HttpResponse> {
    use crate::plugins::ui::UiEvent;

    let plugin = name.into_inner();
    if !known_plugin(&plugin) {
        return Ok(HttpResponse::NotFound().json(serde_json::json!({
            "error": "no such plugin, or it has no window"
        })));
    }

    // From a browser, so `ui_show()` inside the plugin's handler stays a
    // no-op: the browser opens its own panel, and nobody asked for a window on
    // the machine running the session.
    crate::plugins::ui::post_from(crate::plugins::ui::Origin::Web, match body.into_inner() {
        PluginEventBody::Opened => UiEvent::Opened { plugin },
        PluginEventBody::Row { id } => UiEvent::Row { plugin, id },
        PluginEventBody::Group { id } => UiEvent::Group { plugin, id },
        PluginEventBody::Button { id, input } => UiEvent::Button { plugin, id, input },
        PluginEventBody::Menu { id } => UiEvent::Menu { plugin, id },
        PluginEventBody::Form { id, values } => UiEvent::Form { plugin, id, values },
        PluginEventBody::FormCancel { id } => UiEvent::FormCancelled { plugin, id },
        PluginEventBody::Configure => UiEvent::Configure { plugin },
        PluginEventBody::FileMenu {
            id,
            hash,
            index,
            name,
        } => UiEvent::FileMenu {
            plugin,
            id,
            hash,
            index,
            name,
        },
    });
    Ok(HttpResponse::Accepted().finish())
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
    // No server, no tokens: a plugin asking for a stream URL after this gets
    // "" back rather than a URL into a closed port.
    streamtoken::unpublish();
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
    // One store for every worker, like `attempts` below: a per-worker map would
    // mean a token minted on one connection and used on the next was unknown.
    let stream_tokens = Arc::new(streamtoken::StreamTokens::default());
    // Published so a plugin can mint a token without going through HTTP - see
    // `stream_url` in the plugin API. Replaced on every start, so a token from
    // a previous run of the server is not honoured by this one.
    streamtoken::publish(stream_tokens.clone());
    let state = web::Data::new(AppState {
        session,
        cfg,
        env,
        errors,
        stream_tokens: stream_tokens.clone(),
    });
    let creds = web::Data::new(creds);
    // Built out here, not in the factory closure: the closure runs once per
    // worker, so constructing it there would give each worker its own counter
    // and multiply the real attempt limit by the worker count.
    let attempts = web::Data::new(auth::Attempts::new(auth::Limits {
        max_failures: advanced.auth_max_failures,
        window: std::time::Duration::from_secs(advanced.auth_window),
        block: std::time::Duration::from_secs(advanced.auth_block),
    }));

    let body_limit = MAX_BODY_MB * 1024 * 1024;

    let server = HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .app_data(creds.clone())
            .app_data(attempts.clone())
            // On every response, not just the page: the stream endpoint serves
            // bytes out of a stranger's torrent under a content type this
            // server chose, and "the browser will not sniff past it" should not
            // depend on which handler answered.
            .wrap(actix_web::middleware::DefaultHeaders::new()
                .add(("X-Content-Type-Options", "nosniff")))
            .wrap(from_fn(auth::require_auth))
            // A .torrent with thousands of files runs to a few MB once
            // base64'd; actix's 2 KB default would reject them. Still a cap,
            // because an unbounded body is free memory for anyone with the
            // password.
            .app_data(web::JsonConfig::default().limit(body_limit))
            .route("/", web::get().to(h_index))
            .route("/favicon.ico", web::get().to(h_favicon))
            .service(
                web::scope("/api")
                    .route("/health", web::get().to(h_health))
                    .route("/events", web::get().to(h_events))
                    .route("/session", web::get().to(h_session))
                    .route("/errors", web::get().to(h_errors))
                    .route("/torrents", web::get().to(h_torrents))
                    .route("/torrents", web::post().to(h_add))
                    .route("/torrents/inspect", web::post().to(h_inspect))
                    .route("/torrents/{hash}", web::delete().to(h_remove))
                    .route("/torrents/{hash}/pause", web::post().to(h_pause))
                    .route("/torrents/{hash}/resume", web::post().to(h_resume))
                    .route("/torrents/{hash}/recheck", web::post().to(h_recheck))
                    .route("/torrents/{hash}/queue", web::post().to(h_queue))
                    .route("/torrents/{hash}/reannounce", web::post().to(h_reannounce))
                    .route("/torrents/{hash}/peers", web::get().to(h_peers))
                    .route("/torrents/{hash}/magnet", web::get().to(h_magnet))
                    .route("/torrents/{hash}/files", web::get().to(h_files))
                    .route(
                        "/torrents/{hash}/files/{index}/playlist.m3u",
                        web::get().to(h_playlist),
                    )
                    .route(
                        "/torrents/{hash}/files/{index}/stream",
                        web::get().to(h_stream),
                    )
                    .route(
                        "/torrents/{hash}/files/{index}/stream",
                        web::head().to(h_stream),
                    )
                    .route("/torrents/{hash}/files", web::post().to(h_set_file_priority))
                    .route("/torrents/{hash}/trackers", web::get().to(h_trackers))
                    .route("/torrents/{hash}/trackers", web::post().to(h_add_tracker))
                    .route("/torrents/{hash}/trackers/edit", web::post().to(h_edit_tracker))
                    .route("/torrents/{hash}/trackers/remove", web::post().to(h_remove_tracker))
                    .route("/torrents/{hash}/tags", web::get().to(h_tags))
                    .route("/torrents/{hash}/tags", web::post().to(h_set_tags))
                    .route("/torrents/{hash}/limits", web::post().to(h_limits))
                    .route("/speed/alt", web::post().to(h_alt_speed))
                    .route("/plugins", web::get().to(h_plugins))
                    // Before `/plugins/{name}`, or the literal is swallowed
                    // by the parameter - actix matches in registration order.
                    .route("/plugins/file-menu", web::get().to(h_plugin_file_menu))
                    .route("/plugins/{name}/enabled", web::post().to(h_plugin_enabled))
                    .route("/plugins/{name}", web::get().to(h_plugin))
                    .route("/plugins/{name}/event", web::post().to(h_plugin_event))
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
    // hostile peer.
    //
    // Constants, not settings. They were settings for one release and nobody
    // has a reason to move them: a worker count and a handshake rate are not
    // decisions a person using a torrent client makes, and offering them cost
    // a field in Preferences, a row in the web drawer, a CLI flag and a
    // description in 76 languages each.
    //
    // client_request_timeout is actix's own default and is the slowloris
    // guard - restated so it is visible rather than inherited silently.
    .client_request_timeout(Duration::from_secs(5))
    // Defaults to ZERO, i.e. disabled: a client that stops reading mid-response
    // would otherwise hold its worker slot indefinitely.
    .client_disconnect_timeout(Duration::from_secs(5))
    .keep_alive(Duration::from_secs(30))
    // 25600 per worker by default. A handful of browser tabs need double
    // digits; this is the cheapest bound on connection flooding.
    .max_connections(256)
    // Caps TLS handshakes in flight. Handshakes are the expensive half, so
    // this is what stops a flood costing far more CPU than bandwidth.
    .max_connection_rate(64)
    // One per core by default. This serves one person, not a load test.
    .workers(2)
    .shutdown_timeout(5);

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

        // Zero is legitimate for the failure count and only for that: it is how
        // the lockout is switched off. Negative reaches the same floor rather
        // than wrapping to a huge usize.
        cfg.set("webui.auth_max_failures", &0i64);
        cfg.set("webui.auth_window", &-5i64);
        let adv = super::Advanced::load(&cfg);
        assert_eq!(adv.auth_max_failures, 0, "zero switches the lockout off");
        assert_eq!(adv.auth_window, 1, "but a zero window would never trip");

        // Absurdly large is capped rather than accepted. A block nobody can
        // wait out is a way to lock yourself out of your own client.
        cfg.set("webui.auth_block", &10_000_000i64);
        assert_eq!(super::Advanced::load(&cfg).auth_block, 604_800);
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
        assert_eq!(
            parsed.files,
            vec![crate::ui::torrentfile::ParsedFile {
                index: 0,
                path: String::from("Some.File.mkv"),
                size: 4096,
                padding: false,
            }]
        );

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

    /// A zero-length file is legal in a torrent and used to underflow: the
    /// whole-file range was written `(0, file_len - 1)`, which panics on a
    /// debug build and wraps to 18 exabytes on a release one.
    #[test]
    fn an_empty_file_has_no_range_rather_than_a_huge_one() {
        use super::{parse_range, whole_file_range};

        assert_eq!(whole_file_range(0), None, "nothing to name");
        assert_eq!(whole_file_range(1), Some((0, 0)), "one byte is byte zero");
        assert_eq!(whole_file_range(4096), Some((0, 4095)));
        assert_eq!(whole_file_range(u64::MAX), Some((0, u64::MAX - 1)));

        // The header path already refused these; the point is that both paths
        // now agree rather than one of them wrapping.
        for header in ["bytes=0-", "bytes=-1", "bytes=0-0"] {
            assert_eq!(parse_range(header, 0), None, "{header} against an empty file");
        }
    }

    /// A torrent filename reaches a header AND the body of a playlist, and
    /// both were wrong in different ways: the header turned a control
    /// character into a 500, and the playlist let a newline append a line of
    /// the torrent author's choosing - with a URL on it, which a media player
    /// would then fetch.
    #[test]
    fn a_filename_is_made_safe_for_a_header_and_for_a_playlist() {
        use super::safe_filename;

        // The playlist injection. Without the newline gone, the m3u below
        // gains an #EXTINF and a URL nobody here wrote.
        let hostile = "ep01.mkv\n#EXTINF:-1,pwned\nhttp://attacker.example/x";
        let safe = safe_filename(hostile);
        assert!(!safe.contains('\n'), "no newline survives: {safe:?}");
        assert!(!safe.contains('\r'));
        assert_eq!(safe.matches('_').count(), 2, "one per newline, nothing else");

        // The 500. These are legal in a torrent path and illegal in a header
        // value, and actix answers an illegal one with a server error.
        for bad in ['\u{1}', '\u{8}', '\u{b}', '\u{c}', '\u{1f}', '\u{7f}'] {
            let out = safe_filename(&format!("a{bad}b"));
            assert_eq!(out, "a_b", "control char {:#x} must go", bad as u32);
        }

        // Header quoting.
        assert_eq!(safe_filename(r#"a"b\c"#), "a_b_c");

        // Left alone: anything a real filename is made of. Non-ASCII is legal
        // in a header value as obs-text, and stripping it would mangle every
        // filename that is not English.
        for ok in ["Ubuntu 24.04.iso", "Сезон 1.mkv", "日本語.mp4", "a b (1) [x].bin"] {
            assert_eq!(safe_filename(ok), ok, "{ok} must survive untouched");
        }
    }

    /// `nosniff` used to be set by the page handler alone, so every API
    /// response - including the stream, which serves a stranger's bytes under
    /// a content type this server picked - went out without it.
    ///
    /// Through `init_service`, which builds the real middleware stack in
    /// process: no listener, no port, nothing left running.
    #[actix_web::test]
    async fn every_response_carries_nosniff_not_just_the_page() {
        use actix_web::{App, HttpResponse, middleware::DefaultHeaders, test, web};

        let app = test::init_service(
            App::new()
                .wrap(DefaultHeaders::new().add(("X-Content-Type-Options", "nosniff")))
                .route("/api/health", web::get().to(|| async { HttpResponse::Ok().finish() }))
                // Stands in for the stream: a handler that sets its own content
                // type and its own headers, which is where a DefaultHeaders
                // that only applied "if absent" could have been fooled.
                .route(
                    "/api/stream",
                    web::get().to(|| async {
                        HttpResponse::Ok()
                            .content_type("video/x-matroska")
                            .insert_header(("Accept-Ranges", "bytes"))
                            .body("bytes")
                    }),
                ),
        )
        .await;

        for path in ["/api/health", "/api/stream"] {
            let res = test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
            assert_eq!(
                res.headers().get("X-Content-Type-Options").map(|v| v.to_str().unwrap()),
                Some("nosniff"),
                "{path} must carry it"
            );
        }
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

#[cfg(test)]
mod stream_tests {
    use super::{parse_range, stream_content_type};

    /// 1000 bytes, so byte indices and the length never look interchangeable.
    const LEN: u64 = 1000;

    #[test]
    fn the_forms_players_actually_send() {
        // An opening probe: the first bytes, for the container header.
        assert_eq!(parse_range("bytes=0-1023", LEN), Some((0, 999)), "clamped to the file");
        assert_eq!(parse_range("bytes=0-99", LEN), Some((0, 99)));
        // A seek: from here to the end.
        assert_eq!(parse_range("bytes=500-", LEN), Some((500, 999)));
        // The MP4 index at the end of the file, which is why suffix ranges exist.
        assert_eq!(parse_range("bytes=-200", LEN), Some((800, 999)));
        assert_eq!(parse_range("bytes=-5000", LEN), Some((0, 999)), "longer than the file");
        // One byte, the degenerate case a few players use to test for ranges.
        assert_eq!(parse_range("bytes=999-999", LEN), Some((999, 999)));
        // Whitespace and casing of the unit are not worth rejecting over.
        assert_eq!(parse_range("  bytes=10-20  ", LEN), Some((10, 20)));
    }

    #[test]
    fn what_has_to_be_refused() {
        // Past the end: 416, not a clamp. A player asking beyond the file has
        // stale length information and should be told.
        assert_eq!(parse_range("bytes=1000-1001", LEN), None);
        assert_eq!(parse_range("bytes=1000-", LEN), None);
        // Backwards.
        assert_eq!(parse_range("bytes=300-200", LEN), None);
        // Multi-range needs a multipart reply, which nothing here writes.
        assert_eq!(parse_range("bytes=0-99,200-299", LEN), None);
        // Not bytes, not a range, not a number.
        assert_eq!(parse_range("items=0-99", LEN), None);
        assert_eq!(parse_range("bytes=abc-def", LEN), None);
        assert_eq!(parse_range("bytes=", LEN), None);
        assert_eq!(parse_range("bytes=-0", LEN), None);
        // A zero-length file has no satisfiable range at all.
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn containers_browsers_judge_by_type() {
        assert_eq!(stream_content_type("Show.S01E01.mp4"), "video/mp4");
        assert_eq!(stream_content_type("Show.S01E01.MKV"), "video/x-matroska");
        assert_eq!(stream_content_type("clip.webm"), "video/webm");
        assert_eq!(stream_content_type("track.flac"), "audio/flac");
        assert_eq!(stream_content_type("subs.srt"), "application/x-subrip");
        // No extension, or one nobody streams.
        assert_eq!(stream_content_type("README"), "application/octet-stream");
        assert_eq!(stream_content_type("disk.iso"), "application/octet-stream");
        // A dot in a directory name must not be read as an extension.
        assert_eq!(stream_content_type("Show.S01.1080p.mkv"), "video/x-matroska");
    }
}

#[cfg(test)]
mod page_script {
    /// The page is one `<script>`, so a duplicate top-level `let`/`const` is a
    /// SyntaxError that takes the entire interface down - not one feature, all
    /// of them. That shipped once (`selected`, declared by both the add dialog
    /// and the toolbar), and a blank page is a bad way to find out.
    ///
    /// Only top-level declarations count, which here means column zero: every
    /// nested one in this file is indented.
    #[test]
    fn no_duplicate_top_level_declarations() {
        let html = include_str!("index.html");
        let script = html
            .split_once("<script>")
            .expect("the page has a script block")
            .1;

        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (n, line) in script.lines().enumerate() {
            let Some(rest) = line
                .strip_prefix("let ")
                .or_else(|| line.strip_prefix("const "))
                .or_else(|| line.strip_prefix("var "))
            else {
                continue;
            };
            // `let a = 1, b = 2;` is one statement declaring two names.
            for part in rest.split(',') {
                let name = part
                    .split(['=', ';', ' ', ':'])
                    .next()
                    .unwrap_or("")
                    .trim();
                if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                if let Some(first) = seen.insert(name, n + 1) {
                    panic!(
                        "`{name}` is declared twice at the top level of index.html \
                         (lines {first} and {}) - that is a SyntaxError and blanks \
                         the whole page",
                        n + 1
                    );
                }
            }
        }
    }

    /// Every `{{key}}` the markup asks for has to exist in en-US, or it renders
    /// as an empty string and a control ends up with no label at all.
    #[test]
    fn every_template_key_is_translated() {
        let html = include_str!("index.html");
        let english: serde_json::Value = serde_json::from_str(
            crate::ui::translator::EMBEDDED_LANGS
                .iter()
                .find(|(l, _)| *l == crate::DEFAULT_LOCALE)
                .expect("en-US is embedded")
                .1,
        )
        .expect("en-US parses");

        // Both channels, via the same scan render_page substitutes with: a
        // `{{key}}` that en-US lacks renders empty, and a `T.key` it lacks is
        // silently humanised ("Confirm remove torrents" where a sentence
        // belongs), which is the quieter and therefore worse failure.
        let missing: Vec<&str> = super::template_keys(html)
            .into_iter()
            .filter(|k| english.get(k).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "index.html asks for keys en-US does not have: {missing:?}"
        );
    }
}

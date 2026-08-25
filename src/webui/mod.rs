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

use crate::bittorrent::session::{AddParams, AddTorrentSource, Session};
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
async fn h_index() -> impl Responder {
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
        .body(include_str!("index.html"))
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

/// Drains the session's error queue.
///
/// Draining, not peeking, and that is the contract: adds are fire-and-forget,
/// so this is where a failed magnet or a rejected .torrent surfaces, and two
/// clients polling it will each see a subset. One poller.
async fn h_errors(state: web::Data<AppState>) -> actix_web::Result<impl Responder> {
    let st = state.clone();
    let errors = web::block(move || st.session.take_errors()).await?;
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

/// `POST /api/torrents` - add a magnet link or an uploaded `.torrent`.
///
/// Exactly one of `magnet` / `torrent_file` must be given. Both are validated
/// before reaching the engine: the magnet for its scheme, because librqbit
/// would otherwise fetch arbitrary URLs on request (see the note inside), and
/// the file for being decodable and non-empty.
async fn h_add(
    state: web::Data<AppState>,
    body: web::Json<AddRequest>,
) -> actix_web::Result<impl Responder> {
    let req = body.into_inner();

    let source = match (req.magnet, req.torrent_file) {
        (Some(magnet), None) => {
            // Magnet scheme only. librqbit's from_url happily fetches http(s)
            // too, which would turn "add a torrent" into "make the server
            // issue a request to any URL I name" - including hosts only it can
            // reach. Support that deliberately or not at all.
            if !magnet.starts_with("magnet:") {
                return Err(ErrorBadRequest("magnet must start with 'magnet:'"));
            }
            AddTorrentSource::MagnetUri(magnet)
        }
        (None, Some(encoded)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|e| ErrorBadRequest(format!("torrent_file is not valid base64: {e}")))?;
            if bytes.is_empty() {
                return Err(ErrorBadRequest("torrent_file decoded to zero bytes"));
            }
            AddTorrentSource::TorrentFileBytes(bytes)
        }
        _ => {
            return Err(ErrorBadRequest(
                "provide exactly one of 'magnet' or 'torrent_file'",
            ));
        }
    };

    let params = AddParams {
        save_path: req.save_path,
        start_torrent: req.start,
        only_files: req.only_files,
        label_id: req.label_id,
    };

    let st = state.clone();
    web::block(move || st.session.add_torrent(source, params)).await?;

    // 202, not 200: add_torrent returns before the torrent exists. Resolving a
    // magnet's metadata can take minutes, or never finish. Poll /api/torrents
    // for arrival and /api/errors for failure.
    Ok(HttpResponse::Accepted().json(serde_json::json!({ "status": "accepted" })))
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

    std::thread::Builder::new()
        .name(String::from("nt-webui"))
        .spawn(move || {
            let system = actix_web::rt::System::new();
            system.block_on(async move {
                match build(&addr, session, cfg, creds, tls_config) {
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
    creds: Credentials,
    tls_config: Option<rustls::ServerConfig>,
) -> Result<actix_web::dev::Server> {
    let state = web::Data::new(AppState { session, cfg });
    let creds = web::Data::new(creds);

    let server = HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .app_data(creds.clone())
            .wrap(from_fn(auth::require_auth))
            // A .torrent with thousands of files runs to a few MB once
            // base64'd; actix's 2 KB default would reject them. Still a cap,
            // because an unbounded body is free memory for anyone with the
            // password.
            .app_data(web::JsonConfig::default().limit(8 * 1024 * 1024))
            .route("/", web::get().to(h_index))
            .service(
                web::scope("/api")
                    .route("/health", web::get().to(h_health))
                    .route("/session", web::get().to(h_session))
                    .route("/errors", web::get().to(h_errors))
                    .route("/torrents", web::get().to(h_torrents))
                    .route("/torrents", web::post().to(h_add))
                    .route("/torrents/{hash}", web::delete().to(h_remove))
                    .route("/torrents/{hash}/pause", web::post().to(h_pause))
                    .route("/torrents/{hash}/resume", web::post().to(h_resume))
                    .route("/torrents/{hash}/recheck", web::post().to(h_recheck))
                    .route("/torrents/{hash}/move", web::post().to(h_move))
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
    // client_request_timeout defaults to 5s already and is the slowloris
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

//! A qBittorrent-compatible Web API, under `/api/v2`.
//!
//! Off unless `webui.qbittorrent_api` is on. It exists so the tools written
//! for qBittorrent - Sonarr, Radarr, Prowlarr, Lidarr, autobrr, phone remotes -
//! can drive NanoTorrent by being told it is a qBittorrent. NanoTorrent's own
//! API under `/api` is untouched by it: different paths, its own handlers, and
//! nothing here is called from there.
//!
//! The shape copied is qBittorrent 5.0's (Web API 2.11.2): the names, the
//! units, the `Ok.` / `Fails.` bodies, the `SID` cookie, 403 for a request
//! with no session. Where NanoTorrent has no such thing the field is still
//! there, holding what qBittorrent itself reports for "unknown" or "off", so a
//! client that reads it does not fail on a missing key.
//!
//! What it deliberately does not do:
//! - fetch `http(s)` URLs handed to `torrents/add`. Only magnets are taken from
//!   `urls`; a .torrent goes in as an upload. The server fetching any URL it is
//!   given is a way to make it request hosts only it can reach, which is why
//!   NanoTorrent's own API refuses the same thing.
//! - change preferences. `app/setPreferences` answers 200 and changes nothing:
//!   settings belong to Preferences, the web drawer and `--set`, which validate
//!   them. The read side is there because clients check it.
//!
//! Authentication is qBittorrent's: `auth/login` with a form, then the `SID`
//! cookie. The cookie is checked in [`super::auth::require_auth`], next to the
//! Basic auth every other route uses - and Basic works here too.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use actix_web::http::header::HeaderMap;
use actix_web::http::{Method, StatusCode};
use actix_web::{HttpRequest, HttpResponse, web};
use rand::RngExt;
use serde_json::{Value, json};

use super::AppState;
use super::auth::{self, Attempts, Credentials};
use crate::bittorrent::session::{
    AddParams, AddTorrentSource, FileEntry, PRIORITY_HIGH, PRIORITY_MAX, PRIORITY_NORMAL,
    PRIORITY_SKIP, QueueMove,
};
use crate::bittorrent::torrentstatus::{State, TorrentStatus};

/// What `app/webapiVersion` reports: qBittorrent 5.0's. Clients pick their
/// calls by it - from 2.11 on, `torrents/stop` and `torrents/start` rather
/// than `pause` and `resume` (both spellings are answered here anyway).
pub const WEBAPI_VERSION: &str = "2.11.2";
/// What `app/version` reports. Some clients refuse a version they do not
/// recognise as qBittorrent's; `app/buildInfo` says what this really is.
pub const APP_VERSION: &str = "v5.0.2";

/// qBittorrent's default `web_ui_session_timeout`.
const SESSION_TIMEOUT: Duration = Duration::from_secs(3600);
/// A cap, so logging in over and over cannot grow the table without bound.
const MAX_SESSIONS: usize = 256;
/// What qBittorrent sends for an ETA it cannot give: 100 days.
const NO_ETA: u64 = 8_640_000;

const BANNED: &str = "Your IP address has been banned after too many failed authentication attempts.";

// --- sessions ---------------------------------------------------------------

/// Logged-in `SID` cookies and when each was last used.
///
/// In memory only, and per server start: restarting the web interface logs
/// every client out, which they handle by logging in again.
#[derive(Default)]
pub struct Sids(Mutex<HashMap<String, Instant>>);

impl Sids {
    fn start(&self) -> String {
        let mut raw = [0u8; 32];
        rand::rng().fill(&mut raw[..]);
        let sid: String = raw.iter().map(|b| format!("{b:02x}")).collect();

        let mut map = self.0.lock().unwrap();
        map.retain(|_, seen| seen.elapsed() < SESSION_TIMEOUT);
        if map.len() >= MAX_SESSIONS
            && let Some(oldest) = map.iter().min_by_key(|(_, seen)| **seen).map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(sid.clone(), Instant::now());
        sid
    }

    /// Whether this cookie is a live session. Using it keeps it alive, the
    /// way qBittorrent's timeout counts from the last request, not the login.
    pub(super) fn check(&self, sid: &str) -> bool {
        let mut map = self.0.lock().unwrap();
        match map.get_mut(sid) {
            Some(seen) if seen.elapsed() < SESSION_TIMEOUT => {
                *seen = Instant::now();
                true
            }
            Some(_) => {
                map.remove(sid);
                false
            }
            None => false,
        }
    }

    fn end(&self, sid: &str) {
        self.0.lock().unwrap().remove(sid);
    }
}

/// The `SID` cookie's value, if the request carries one.
pub(super) fn sid(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all("Cookie")
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix("SID="))
}

// --- request parameters -----------------------------------------------------

/// A request's parameters, from the query string and the body together -
/// qBittorrent clients send either, and `torrents/add` sends a multipart body
/// with the .torrent files in it.
#[derive(Default)]
struct Params {
    fields: HashMap<String, String>,
    files: Vec<Vec<u8>>,
}

impl Params {
    fn read(req: &HttpRequest, body: &[u8]) -> Option<Params> {
        let mut p = Params::default();
        p.form(req.query_string().as_bytes());
        let content_type = req
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        match boundary(content_type) {
            Some(boundary) => p.multipart(body, &boundary)?,
            None => p.form(body),
        }
        Some(p)
    }

    fn form(&mut self, raw: &[u8]) {
        for (k, v) in url::form_urlencoded::parse(raw) {
            self.fields.insert(k.into_owned(), v.into_owned());
        }
    }

    /// `multipart/form-data`, already whole in memory (the body limit bounds
    /// it). Parts with a filename are uploads, whatever their field name -
    /// qBittorrent reads every uploaded file as a torrent the same way.
    fn multipart(&mut self, body: &[u8], boundary: &str) -> Option<()> {
        let delim = format!("--{boundary}");
        let next_delim = format!("\r\n{delim}");
        let mut at = find(body, delim.as_bytes(), 0)? + delim.len();
        loop {
            match body.get(at..at + 2)? {
                b"--" => return Some(()),
                b"\r\n" => at += 2,
                _ => return None,
            }
            let head_end = find(body, b"\r\n\r\n", at)?;
            let head = std::str::from_utf8(&body[at..head_end]).ok()?;
            let start = head_end + 4;
            let end = find(body, next_delim.as_bytes(), start)?;
            let content = &body[start..end];
            let (name, filename) = disposition(head)?;
            if filename {
                self.files.push(content.to_vec());
            } else {
                self.fields
                    .insert(name, String::from_utf8_lossy(content).into_owned());
            }
            at = end + next_delim.len();
        }
    }

    fn get(&self, key: &str) -> &str {
        self.fields.get(key).map_or("", String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        self.get(key).eq_ignore_ascii_case("true")
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// The boundary of a `multipart/form-data` content type.
fn boundary(content_type: &str) -> Option<String> {
    let (mime, rest) = content_type.split_once(';')?;
    if !mime.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    rest.split(';')
        .find_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            k.eq_ignore_ascii_case("boundary")
                .then(|| v.trim_matches('"').to_owned())
        })
        .filter(|b| !b.is_empty())
}

/// A part's field name, and whether it is a file upload.
fn disposition(head: &str) -> Option<(String, bool)> {
    let line = head.lines().find(|l| {
        l.get(..20)
            .is_some_and(|p| p.eq_ignore_ascii_case("content-disposition:"))
    })?;
    let mut name = None;
    let mut file = false;
    for param in line.split(';').skip(1).map(str::trim) {
        if let Some(v) = param.strip_prefix("name=") {
            name = Some(v.trim_matches('"').to_owned());
        } else if param.starts_with("filename") {
            file = true;
        }
    }
    Some((name?, file))
}

// --- replies ----------------------------------------------------------------

enum Reply {
    Text(StatusCode, String),
    Json(Value),
}

fn ok() -> Reply {
    Reply::Text(StatusCode::OK, String::new())
}

fn text(s: impl Into<String>) -> Reply {
    Reply::Text(StatusCode::OK, s.into())
}

fn fail(code: StatusCode, msg: &str) -> Reply {
    Reply::Text(code, msg.to_owned())
}

fn not_found() -> Reply {
    fail(StatusCode::NOT_FOUND, "Not Found")
}

impl Reply {
    fn into_response(self) -> HttpResponse {
        match self {
            Reply::Text(code, body) => HttpResponse::build(code)
                .content_type("text/plain; charset=UTF-8")
                .body(body),
            Reply::Json(v) => HttpResponse::Ok().json(v),
        }
    }
}

// --- routes -----------------------------------------------------------------

/// Mount `/api/v2`. Registered ahead of NanoTorrent's own `/api` scope, which
/// would otherwise take these paths by prefix and answer 404.
pub(super) fn configure(cfg: &mut web::ServiceConfig, sids: web::Data<Sids>, body_limit: usize) {
    cfg.app_data(sids).service(
        web::scope("/api/v2")
            .app_data(web::PayloadConfig::new(body_limit))
            .route("/auth/login", web::post().to(h_login))
            .route("/auth/logout", web::post().to(h_logout))
            .route("/{scope}/{action}", web::route().to(h_api)),
    );
}

/// `POST /api/v2/auth/login` - the one path here the middleware lets through
/// without a session, so it does its own lockout and its own check.
async fn h_login(
    req: HttpRequest,
    body: web::Bytes,
    creds: web::Data<Credentials>,
    attempts: web::Data<Attempts>,
    sids: web::Data<Sids>,
) -> HttpResponse {
    // Already logged in: qBittorrent says Ok. and issues nothing new.
    if sid(req.headers()).is_some_and(|s| sids.check(s)) {
        return text("Ok.").into_response();
    }
    let Some(p) = Params::read(&req, &body) else {
        return fail(StatusCode::BAD_REQUEST, "Bad Request").into_response();
    };
    let (user, pass) = (p.get("username").to_owned(), p.get("password").to_owned());
    // The peer, never a forwarding header - see `require_auth`.
    let who = req
        .peer_addr()
        .map_or_else(|| String::from("unknown"), |a| a.ip().to_string());

    // Argon2 is the expensive part; off the async worker.
    match web::block(move || auth::login(&creds, &attempts, &who, &user, &pass)).await {
        Ok(auth::Login::Ok) => HttpResponse::Ok()
            .insert_header((
                "Set-Cookie",
                format!("SID={}; HttpOnly; Path=/; SameSite=Strict", sids.start()),
            ))
            .content_type("text/plain; charset=UTF-8")
            .body("Ok."),
        Ok(auth::Login::Fails) => text("Fails.").into_response(),
        Ok(auth::Login::Banned) => fail(StatusCode::FORBIDDEN, BANNED).into_response(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// `POST /api/v2/auth/logout`
async fn h_logout(req: HttpRequest, sids: web::Data<Sids>) -> HttpResponse {
    if let Some(s) = sid(req.headers()) {
        sids.end(s);
    }
    HttpResponse::Ok()
        .insert_header(("Set-Cookie", "SID=; HttpOnly; Path=/; Max-Age=0"))
        .finish()
}

/// Every other `/api/v2/{scope}/{action}`.
///
/// GET reaches the reads only. Anything that changes something takes POST,
/// which is also what puts it behind the cross-site check in `require_auth` -
/// that check exempts GET because nothing behind a GET changes state.
async fn h_api(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Bytes,
) -> HttpResponse {
    let (scope, action) = path.into_inner();
    let post = match *req.method() {
        Method::POST => true,
        Method::GET | Method::HEAD => false,
        _ => return fail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response(),
    };
    let Some(p) = Params::read(&req, &body) else {
        return fail(StatusCode::BAD_REQUEST, "Bad Request").into_response();
    };
    // Every session call blocks - see the module notes in mod.rs.
    let st = state.clone();
    let reply = web::block(move || {
        read(&st, &scope, &action, &p).or_else(|| {
            if post {
                Some(write(&st, &scope, &action, p).unwrap_or_else(not_found))
            } else {
                None
            }
        })
    })
    .await;
    match reply {
        Ok(Some(reply)) => reply.into_response(),
        Ok(None) => fail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

// --- reads ------------------------------------------------------------------

fn read(st: &AppState, scope: &str, action: &str, p: &Params) -> Option<Reply> {
    Some(match (scope, action) {
        ("app", "version") => text(APP_VERSION),
        ("app", "webapiVersion") => text(WEBAPI_VERSION),
        ("app", "buildInfo") => Reply::Json(json!({
            "qt": "",
            "libtorrent": format!("librqbit (NanoTorrent {})", crate::buildinfo::version()),
            "boost": "",
            "openssl": "",
            "zlib": "",
            "bitness": usize::BITS,
            "platform": std::env::consts::OS,
        })),
        ("app", "preferences") => Reply::Json(preferences(st)),
        ("app", "defaultSavePath") => text(default_save_path(st)),

        ("transfer", "info") => Reply::Json(transfer_info(st, &all(st))),
        ("transfer", "speedLimitsMode") => {
            text(if st.cfg.get_bool("speed.alt_enabled") { "1" } else { "0" })
        }
        ("transfer", "downloadLimit") => text(rate_limit(st, "download").to_string()),
        ("transfer", "uploadLimit") => text(rate_limit(st, "upload").to_string()),

        ("torrents", "info") => Reply::Json(Value::Array(torrents_info(st, p))),
        ("torrents", "properties") => one(st, p, properties)?,
        ("torrents", "trackers") => one(st, p, trackers)?,
        ("torrents", "files") => one(st, p, files)?,
        ("torrents", "categories") => Reply::Json(categories(st)),
        ("torrents", "tags") => Reply::Json(json!(tag_names(st))),

        ("sync", "maindata") => Reply::Json(maindata(st, p)),
        _ => return None,
    })
}

/// A read about the torrent named by `hash`, or 404.
fn one(st: &AppState, p: &Params, f: fn(&AppState, &TorrentStatus) -> Value) -> Option<Reply> {
    let hash = p.get("hash").to_ascii_lowercase();
    Some(match st.session.torrent(&hash) {
        Some(mut t) => {
            t.label_name = label_name(st, t.label_id);
            Reply::Json(f(st, &t))
        }
        None => not_found(),
    })
}

/// Every torrent, label names filled in.
fn all(st: &AppState) -> Vec<TorrentStatus> {
    st.session.torrents(&st.session.label_names())
}

fn label_name(st: &AppState, id: Option<i32>) -> String {
    id.and_then(|id| st.session.label_names().remove(&id))
        .unwrap_or_default()
}

fn default_save_path(st: &AppState) -> String {
    crate::bittorrent::session::default_save_path(&st.cfg)
        .to_string_lossy()
        .into_owned()
}

/// A global rate limit in bytes per second, 0 for none - the settings hold
/// KB/s behind an on/off switch.
fn rate_limit(st: &AppState, which: &str) -> i64 {
    if st.cfg.get_bool(&format!("libtorrent.enable_{which}_rate_limit")) {
        st.cfg
            .get_int(&format!("libtorrent.{which}_rate_limit"))
            .unwrap_or(0)
            .max(0)
            * 1024
    } else {
        0
    }
}

/// qBittorrent's state names, from NanoTorrent's state and what is moving.
fn state(t: &TorrentStatus) -> &'static str {
    match t.state {
        State::Error => "error",
        State::CheckingFiles if t.progress >= 1.0 => "checkingUP",
        State::CheckingFiles => "checkingDL",
        State::Downloading if t.download_payload_rate > 0 => "downloading",
        State::Downloading => "stalledDL",
        State::DownloadingMetadata => "metaDL",
        State::DownloadingPaused => "stoppedDL",
        State::Uploading if t.upload_payload_rate > 0 => "uploading",
        State::Uploading => "stalledUP",
        State::UploadingPaused => "stoppedUP",
        State::Unknown => "unknown",
    }
}

/// `torrents/info`'s `filter`, over the state names above.
fn filter_matches(filter: &str, state: &str, t: &TorrentStatus) -> bool {
    let complete = state.ends_with("UP") || state == "uploading";
    let stopped = state.starts_with("stopped");
    let active = t.download_payload_rate > 0 || t.upload_payload_rate > 0;
    match filter {
        "downloading" => !complete && state != "error",
        "seeding" => complete && !stopped,
        "completed" => complete,
        "stopped" | "paused" => stopped,
        "running" | "resumed" => !stopped,
        "active" => active,
        "inactive" => !active,
        "stalled" => state.starts_with("stalled"),
        "stalled_uploading" => state == "stalledUP",
        "stalled_downloading" => state == "stalledDL",
        "errored" => state == "error",
        // "all", absent, or a name this does not know: everything.
        _ => true,
    }
}

/// qBittorrent's save path and content path, from NanoTorrent's output folder.
///
/// NanoTorrent's `save_path` is the folder the files sit in - for a torrent of
/// several files, the directory named after it. qBittorrent's is the folder
/// that directory is in, with `content_path` naming the directory itself, or
/// the one file. File names in `torrents/files` are relative to the former.
struct Layout {
    save_path: String,
    content_path: String,
    /// Prefix for file names, "" for a single file.
    root: String,
}

fn layout(t: &TorrentStatus, files: &[FileEntry]) -> Layout {
    let folder = std::path::Path::new(&t.save_path);
    let single = files.len() == 1 && !files[0].name.contains(['/', '\\']);
    if single || files.is_empty() {
        return Layout {
            save_path: t.save_path.clone(),
            content_path: match files.first() {
                Some(f) if single => folder.join(&f.name).to_string_lossy().into_owned(),
                _ => t.save_path.clone(),
            },
            root: String::new(),
        };
    }
    Layout {
        save_path: folder
            .parent()
            .map_or_else(|| t.save_path.clone(), |p| p.to_string_lossy().into_owned()),
        content_path: t.save_path.clone(),
        root: folder
            .file_name()
            .map(|n| format!("{}/", n.to_string_lossy()))
            .unwrap_or_default(),
    }
}

/// A per-torrent share limit as qBittorrent writes it: -2 follows the global
/// setting, -1 is no limit. NanoTorrent stores `None` and 0 for those.
fn ratio_limit(v: Option<f64>) -> f64 {
    match v {
        None => -2.0,
        Some(r) if r <= 0.0 => -1.0,
        Some(r) => r,
    }
}

fn seed_limit(v: Option<i64>) -> i64 {
    match v {
        None => -2,
        Some(m) if m <= 0 => -1,
        Some(m) => m,
    }
}

/// The global limits, -1 when off.
fn global_limits(st: &AppState) -> (f64, i64) {
    if !st.cfg.get_bool("queue.share_limit_enabled") {
        return (-1.0, -1);
    }
    let ratio = st
        .cfg
        .get_int("libtorrent.share_ratio_limit")
        .filter(|r| *r > 0)
        .map_or(-1.0, |r| r as f64 / 100.0);
    let minutes = st.cfg.get_int("queue.seed_time_limit").filter(|m| *m >= 0).unwrap_or(-1);
    (ratio, minutes)
}

/// One torrent as `torrents/info` and `sync/maindata` show it.
fn torrent_info(st: &AppState, t: &TorrentStatus, global: (f64, i64)) -> Value {
    let hash = &t.info_hash;
    let files = st.session.files(hash);
    let lay = layout(t, &files);
    let tiers = st.session.tracker_tiers(hash);
    let (ratio_override, seed_override) = st.session.share_overrides(hash);
    let state = state(t);
    let size = t.total_wanted;
    let mut info = json!({
        "hash": hash,
        "infohash_v1": t.info_hash_v1.clone().unwrap_or_default(),
        "infohash_v2": t.info_hash_v2.clone().unwrap_or_default(),
        "name": t.name,
        "magnet_uri": st.session.magnet_uri(hash, &t.name),
        "size": size,
        "total_size": files.iter().map(|f| f.length).sum::<u64>(),
        "progress": t.progress,
        "dlspeed": t.download_payload_rate,
        "upspeed": t.upload_payload_rate,
        // 0 when not queued, else the position counting from 1.
        "priority": if t.queue_position < 0 { 0 } else { t.queue_position + 1 },
        "num_seeds": t.seeds_current,
        "num_complete": t.seeds_total,
        "num_leechs": t.peers_current,
        "num_incomplete": t.peers_total,
        "state": state,
        "eta": t.eta.map_or(NO_ETA, |d| d.as_secs().min(NO_ETA)),
    });
    // Two halves: one `json!` this size runs past the macro recursion limit.
    let rest = json!({
        "seq_dl": false,
        "f_l_piece_prio": false,
        "category": t.label_name,
        "tags": st.cfg.tags_for(hash).into_iter().map(|t| t.name).collect::<Vec<_>>().join(", "),
        "super_seeding": false,
        "force_start": false,
        "save_path": lay.save_path,
        "download_path": "",
        "content_path": lay.content_path,
        "root_path": if lay.root.is_empty() { "" } else { lay.content_path.as_str() },
        "added_on": t.added_on.timestamp(),
        "completion_on": t.completed_on.map_or(-1, |d| d.timestamp()),
        "tracker": tiers.iter().flatten().next().cloned().unwrap_or_default(),
        "trackers_count": tiers.iter().flatten().count(),
        "dl_limit": -1,
        "up_limit": -1,
        "downloaded": t.all_time_download,
        "uploaded": t.all_time_upload,
        "downloaded_session": t.all_time_download,
        "uploaded_session": t.all_time_upload,
        "amount_left": t.total_wanted_remaining,
        "completed": size - t.total_wanted_remaining,
        "max_ratio": ratio_override.map_or(global.0, |r| ratio_limit(Some(r)).max(-1.0)),
        "max_seeding_time": seed_override.map_or(global.1, |m| seed_limit(Some(m)).max(-1)),
        "max_inactive_seeding_time": -1,
        "ratio": t.ratio,
        "ratio_limit": ratio_limit(ratio_override),
        "seeding_time_limit": seed_limit(seed_override),
        "inactive_seeding_time_limit": -2,
        "popularity": 0,
        "seen_complete": -1,
        "auto_tmm": false,
        "time_active": (chrono::Local::now() - t.added_on).num_seconds().max(0),
        "seeding_time": t.completed_on.map_or(0, |d| (chrono::Local::now() - d).num_seconds().max(0)),
        "last_activity": 0,
        "availability": if t.availability > 0.0 { t.availability } else { -1.0 },
        "reannounce": 0,
        "comment": "",
        "private": false,
        "has_metadata": t.state != State::DownloadingMetadata,
    });
    if let (Value::Object(a), Value::Object(b)) = (&mut info, rest) {
        a.extend(b);
    }
    info
}

fn torrents_info(st: &AppState, p: &Params) -> Vec<Value> {
    let global = global_limits(st);
    let wanted: Option<Vec<String>> = p
        .fields
        .get("hashes")
        .map(|h| h.split('|').map(str::to_ascii_lowercase).collect());
    let category = p.fields.get("category");
    let tag = p.fields.get("tag");
    let filter = p.get("filter");

    let mut rows: Vec<Value> = all(st)
        .iter()
        .filter(|t| wanted.as_ref().is_none_or(|w| w.contains(&t.info_hash)))
        // Absent means any; empty means none.
        .filter(|t| category.is_none_or(|c| *c == t.label_name))
        .filter(|t| filter_matches(filter, state(t), t))
        .map(|t| torrent_info(st, t, global))
        .filter(|v| {
            tag.is_none_or(|tag| {
                let tags = v["tags"].as_str().unwrap_or_default();
                if tag.is_empty() {
                    tags.is_empty()
                } else {
                    tags.split(", ").any(|t| t == tag)
                }
            })
        })
        .collect();

    let sort = p.get("sort");
    if !sort.is_empty() {
        rows.sort_by(|a, b| compare(&a[sort], &b[sort]));
    }
    if p.flag("reverse") {
        rows.reverse();
    }
    let offset: i64 = p.get("offset").parse().unwrap_or(0);
    let skip = if offset < 0 {
        rows.len().saturating_sub(offset.unsigned_abs() as usize)
    } else {
        offset as usize
    };
    let limit = p.get("limit").parse::<usize>().ok().filter(|l| *l > 0).unwrap_or(usize::MAX);
    rows.into_iter().skip(skip).take(limit).collect()
}

fn compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&y.as_f64().unwrap_or(0.0)),
        (Value::String(x), Value::String(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

fn properties(st: &AppState, t: &TorrentStatus) -> Value {
    let info = torrent_info(st, t, global_limits(st));
    let pairs: [(&str, Value); 43] = [
        ("hash", json!(t.info_hash)),
        ("infohash_v1", info["infohash_v1"].clone()),
        ("infohash_v2", info["infohash_v2"].clone()),
        ("name", json!(t.name)),
        ("save_path", info["save_path"].clone()),
        ("download_path", json!("")),
        ("creation_date", json!(-1)),
        ("piece_size", json!(-1)),
        ("comment", json!("")),
        ("total_wasted", json!(0)),
        ("total_uploaded", json!(t.all_time_upload)),
        ("total_uploaded_session", json!(t.all_time_upload)),
        ("total_downloaded", json!(t.all_time_download)),
        ("total_downloaded_session", json!(t.all_time_download)),
        ("up_limit", json!(-1)),
        ("dl_limit", json!(-1)),
        ("time_elapsed", info["time_active"].clone()),
        ("seeding_time", info["seeding_time"].clone()),
        ("nb_connections", json!(t.peers_current + t.seeds_current)),
        ("nb_connections_limit", json!(-1)),
        ("share_ratio", json!(t.ratio)),
        ("addition_date", json!(t.added_on.timestamp())),
        ("completion_date", info["completion_on"].clone()),
        ("created_by", json!("")),
        ("dl_speed_avg", json!(-1)),
        ("dl_speed", json!(t.download_payload_rate)),
        ("eta", info["eta"].clone()),
        ("last_seen", json!(-1)),
        ("peers", json!(t.peers_current)),
        ("peers_total", json!(t.peers_total)),
        ("pieces_have", json!(-1)),
        ("pieces_num", json!(-1)),
        ("reannounce", json!(0)),
        ("seeds", json!(t.seeds_current)),
        ("seeds_total", json!(t.seeds_total)),
        ("total_size", info["total_size"].clone()),
        ("up_speed_avg", json!(-1)),
        ("up_speed", json!(t.upload_payload_rate)),
        ("popularity", json!(0)),
        ("private", json!(false)),
        ("is_private", json!(false)),
        ("has_metadata", info["has_metadata"].clone()),
        ("isPrivate", json!(false)),
    ];
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// The real trackers, by tier. qBittorrent's status: 1 not contacted yet,
/// 2 working, 4 not working.
fn trackers(st: &AppState, t: &TorrentStatus) -> Value {
    use crate::bittorrent::session::TrackerRowKind;
    let rows = st.session.tracker_rows(&t.info_hash, &st.translator());
    let stats = |url: &str| {
        rows.iter()
            .find(|r| r.kind == TrackerRowKind::Tracker && r.label.trim() == url)
    };
    let list: Vec<Value> = st
        .session
        .tracker_tiers(&t.info_hash)
        .iter()
        .enumerate()
        .flat_map(|(tier, urls)| urls.iter().map(move |url| (tier, url)))
        .map(|(tier, url)| {
            let row = stats(url);
            let seeds = row.and_then(|r| r.seeders);
            let status = match row {
                _ if seeds.is_some() => 2,
                Some(r) if r.fails > 0 => 4,
                _ => 1,
            };
            json!({
                "url": url,
                "status": status,
                "tier": tier,
                "num_peers": -1,
                "num_seeds": seeds.map_or(-1, i64::from),
                "num_leeches": row.and_then(|r| r.leechers).map_or(-1, i64::from),
                "num_downloaded": -1,
                "msg": if status == 4 { row.map_or("", |r| r.status.as_str()) } else { "" },
            })
        })
        .collect();
    Value::Array(list)
}

/// NanoTorrent's priority scale to qBittorrent's: 0 skip, 1 normal, 6 high,
/// 7 maximum.
fn to_qbt_priority(p: i64) -> i64 {
    match p {
        PRIORITY_SKIP => 0,
        PRIORITY_HIGH => 6,
        PRIORITY_MAX => 7,
        _ => 1,
    }
}

fn from_qbt_priority(p: i64) -> Option<i64> {
    Some(match p {
        0 => PRIORITY_SKIP,
        1 => PRIORITY_NORMAL,
        // The only four qBittorrent accepts; anything else is its 400 too.
        6 => PRIORITY_HIGH,
        7 => PRIORITY_MAX,
        _ => return None,
    })
}

fn files(st: &AppState, t: &TorrentStatus) -> Value {
    let files = st.session.files(&t.info_hash);
    let lay = layout(t, &files);
    let stored = st.session.file_priorities(&t.info_hash);
    let list: Vec<Value> = files
        .iter()
        .map(|f| {
            let level = if f.included {
                stored.get(&f.index).copied().unwrap_or(PRIORITY_NORMAL)
            } else {
                PRIORITY_SKIP
            };
            json!({
                "index": f.index,
                "name": format!("{}{}", lay.root, f.name.replace('\\', "/")),
                "size": f.length,
                "progress": f.progress,
                "priority": to_qbt_priority(level),
                "is_seed": f.progress >= 1.0,
                "piece_range": [0, 0],
                "availability": -1,
            })
        })
        .collect();
    Value::Array(list)
}

fn categories(st: &AppState) -> Value {
    let map: serde_json::Map<String, Value> = st
        .cfg
        .get_labels()
        .into_iter()
        .map(|l| {
            let path = if l.save_path_enabled { l.save_path } else { String::new() };
            (l.name.clone(), json!({ "name": l.name, "savePath": path }))
        })
        .collect();
    Value::Object(map)
}

fn tag_names(st: &AppState) -> Vec<String> {
    st.cfg.get_tags().into_iter().map(|t| t.name).collect()
}

fn transfer_info(st: &AppState, rows: &[TorrentStatus]) -> Value {
    let (down, up) = st.session.session_rates();
    // NanoTorrent keeps all-time totals per torrent, not a session total;
    // the sum is the closest thing it has.
    let downloaded: i64 = rows.iter().map(|t| t.all_time_download).sum();
    let uploaded: i64 = rows.iter().map(|t| t.all_time_upload).sum();
    json!({
        "dl_info_speed": down,
        "dl_info_data": downloaded,
        "up_info_speed": up,
        "up_info_data": uploaded,
        "dl_rate_limit": rate_limit(st, "download"),
        "up_rate_limit": rate_limit(st, "upload"),
        "dht_nodes": st.session.dht_nodes().unwrap_or(0),
        "connection_status": if st.session.listen_port().is_some() { "connected" } else { "disconnected" },
    })
}

/// `sync/maindata`, always as a full update.
///
/// qBittorrent sends only what changed since the `rid` a client last saw; a
/// full update is the case every client must already handle (it is what the
/// first request gets), so answering every request with one is correct, only
/// larger. For one person's torrent list that is not worth a diff engine.
fn maindata(st: &AppState, p: &Params) -> Value {
    let rows = all(st);
    let global = global_limits(st);
    let mut trackers: serde_json::Map<String, Value> = serde_json::Map::new();
    let torrents: serde_json::Map<String, Value> = rows
        .iter()
        .map(|t| {
            for url in st.session.tracker_tiers(&t.info_hash).into_iter().flatten() {
                if let Value::Array(list) = trackers.entry(url).or_insert_with(|| json!([])) {
                    list.push(json!(t.info_hash));
                }
            }
            (t.info_hash.clone(), torrent_info(st, t, global))
        })
        .collect();

    let mut server_state = transfer_info(st, &rows);
    let (down, up) = (
        server_state["dl_info_data"].as_i64().unwrap_or(0),
        server_state["up_info_data"].as_i64().unwrap_or(0),
    );
    if let Value::Object(s) = &mut server_state {
        s.insert("alltime_dl".into(), json!(down));
        s.insert("alltime_ul".into(), json!(up));
        s.insert(
            "global_ratio".into(),
            json!(if down > 0 { format!("{:.2}", up as f64 / down as f64) } else { String::from("-") }),
        );
        s.insert(
            "total_peer_connections".into(),
            json!(rows.iter().map(|t| t.peers_current + t.seeds_current).sum::<i64>()),
        );
        s.insert("queueing".into(), json!(true));
        s.insert("use_alt_speed_limits".into(), json!(st.cfg.get_bool("speed.alt_enabled")));
        s.insert("refresh_interval".into(), json!(1500));
        s.insert("free_space_on_disk".into(), json!(0));
        s.insert("use_subcategories".into(), json!(false));
    }

    let rid = p.get("rid").parse::<u64>().unwrap_or(0) % 1_000_000 + 1;
    json!({
        "rid": rid,
        "full_update": true,
        "torrents": torrents,
        "torrents_removed": [],
        "categories": categories(st),
        "categories_removed": [],
        "tags": tag_names(st),
        "tags_removed": [],
        "trackers": trackers,
        "trackers_removed": [],
        "server_state": server_state,
    })
}

/// The read side of qBittorrent's preferences, from NanoTorrent's settings.
fn preferences(st: &AppState) -> Value {
    let cfg = &st.cfg;
    let int = |key: &str| cfg.get_int(key).unwrap_or(0);
    let (ratio, minutes) = global_limits(st);
    let kb = |which: &str| {
        if cfg.get_bool(&format!("libtorrent.enable_{which}_rate_limit")) {
            int(&format!("libtorrent.{which}_rate_limit"))
        } else {
            -1
        }
    };
    let web = super::WebConfig::load(cfg);
    json!({
        "locale": cfg.get_string("locale_name").unwrap_or_default(),
        "save_path": default_save_path(st),
        "temp_path_enabled": cfg.get_bool("downloads.incomplete_enabled"),
        "temp_path": cfg.get_string("downloads.incomplete_path").unwrap_or_default(),
        "auto_tmm_enabled": false,
        "create_subfolder_enabled": true,
        "start_paused_enabled": false,
        "queueing_enabled": true,
        "max_active_downloads": int("libtorrent.active_downloads"),
        "max_active_torrents": int("libtorrent.active_limit"),
        "max_active_uploads": int("libtorrent.active_seeds"),
        "dont_count_slow_torrents": false,
        "max_ratio_enabled": ratio >= 0.0,
        "max_ratio": ratio,
        "max_seeding_time_enabled": minutes >= 0,
        "max_seeding_time": minutes,
        // 0 stop, 1 remove, 3 remove with its files.
        "max_ratio_act": match cfg.get_string("queue.share_limit_action").as_deref() {
            Some("remove") => 1,
            Some("remove_with_data") => 3,
            _ => 0,
        },
        "dht": cfg.get_bool("libtorrent.enable_dht"),
        "pex": cfg.get_bool("libtorrent.enable_pex"),
        "lsd": cfg.get_bool("libtorrent.enable_lsd"),
        // 1 is "require encryption", 0 "prefer it".
        "encryption": i32::from(
            cfg.get_bool("libtorrent.require_incoming_encryption")
                && cfg.get_bool("libtorrent.require_outgoing_encryption"),
        ),
        "anonymous_mode": cfg.get_bool("libtorrent.anonymous_mode"),
        "listen_port": st.session.listen_port().unwrap_or(0),
        "random_port": false,
        "upnp": false,
        "dl_limit": kb("download"),
        "up_limit": kb("upload"),
        "alt_dl_limit": int("speed.alt_download_rate"),
        "alt_up_limit": int("speed.alt_upload_rate"),
        "scheduler_enabled": cfg.get_bool("speed.schedule_enabled"),
        "ip_filter_enabled": cfg.get_bool("ipfilter.enabled"),
        "web_ui_address": web.bind_address,
        "web_ui_port": web.port,
        "web_ui_username": web.username,
        "use_https": web.tls != super::TlsMode::Off,
        "web_ui_max_auth_fail_count": web.advanced.max_failures,
        "web_ui_ban_duration": web.advanced.block.as_secs(),
        "web_ui_session_timeout": SESSION_TIMEOUT.as_secs(),
        "bypass_local_auth": false,
        "web_ui_csrf_protection_enabled": true,
    })
}

// --- writes -----------------------------------------------------------------

/// The torrents `hashes` names that are in the session, or all of them for
/// `all`.
fn hashes(st: &AppState, p: &Params) -> Vec<String> {
    let raw = p.get("hashes");
    if raw.eq_ignore_ascii_case("all") {
        return st
            .session
            .torrents(&HashMap::new())
            .into_iter()
            .map(|t| t.info_hash)
            .collect();
    }
    raw.split('|')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty() && st.session.exists(h))
        .collect()
}

fn each(st: &AppState, p: &Params, f: impl Fn(&crate::bittorrent::session::Session, &str)) -> Reply {
    for h in hashes(st, p) {
        f(&st.session, &h);
    }
    ok()
}

fn names(list: &str, sep: char) -> Vec<String> {
    list.split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn write(st: &web::Data<AppState>, scope: &str, action: &str, p: Params) -> Option<Reply> {
    use crate::bittorrent::session::Session;
    Some(match (scope, action) {
        ("app", "setPreferences") => ok(),

        ("transfer", "toggleSpeedLimitsMode") => {
            let on = st.cfg.get_bool("speed.alt_enabled");
            st.cfg.set("speed.alt_enabled", &!on);
            ok()
        }
        ("transfer", "setSpeedLimitsMode") => {
            st.cfg.set("speed.alt_enabled", &(p.get("mode") == "1"));
            ok()
        }

        ("torrents", "add") => add(st, p),
        ("torrents", "delete") => {
            st.session.remove_many(&hashes(st, &p), p.flag("deleteFiles"));
            ok()
        }
        // 5.0 renamed pause and resume; clients older than 2.11 still use them.
        ("torrents", "stop" | "pause") => each(st, &p, Session::pause),
        ("torrents", "start" | "resume") => each(st, &p, Session::resume),
        ("torrents", "setForceStart") => {
            if p.flag("value") {
                each(st, &p, Session::resume)
            } else {
                ok()
            }
        }
        ("torrents", "recheck") => each(st, &p, Session::recheck),
        ("torrents", "reannounce") => each(st, &p, Session::reannounce),
        ("torrents", "topPrio") => each(st, &p, |s, h| s.move_in_queue(h, QueueMove::Top)),
        ("torrents", "bottomPrio") => each(st, &p, |s, h| s.move_in_queue(h, QueueMove::Bottom)),
        ("torrents", "increasePrio") => each(st, &p, |s, h| s.move_in_queue(h, QueueMove::Up)),
        ("torrents", "decreasePrio") => each(st, &p, |s, h| s.move_in_queue(h, QueueMove::Down)),
        ("torrents", "setLocation") => {
            let to = p.get("location").trim().to_owned();
            if to.is_empty() {
                return Some(fail(StatusCode::BAD_REQUEST, "Save path cannot be empty"));
            }
            if !std::path::Path::new(&to).is_absolute() {
                return Some(fail(StatusCode::BAD_REQUEST, "Save path must be absolute"));
            }
            each(st, &p, |s, h| s.move_storage(h, &to))
        }
        ("torrents", "setShareLimits") => {
            let (Ok(ratio), Ok(minutes)) = (
                p.get("ratioLimit").parse::<f64>(),
                p.get("seedingTimeLimit").parse::<i64>(),
            ) else {
                return Some(fail(StatusCode::BAD_REQUEST, "Bad Request"));
            };
            let ratio = match ratio {
                r if r <= -2.0 => None,
                r if r < 0.0 => Some(0.0),
                r => Some(r),
            };
            let minutes = match minutes {
                m if m <= -2 => None,
                m if m < 0 => Some(0),
                m => Some(m),
            };
            each(st, &p, |s, h| {
                s.set_ratio_limit(h, ratio);
                s.set_seed_time_limit(h, minutes);
            })
        }
        ("torrents", "filePrio") => {
            let hash = p.get("hash").to_ascii_lowercase();
            let Some(level) = p.get("priority").parse().ok().and_then(from_qbt_priority) else {
                return Some(fail(StatusCode::BAD_REQUEST, "Priority is not valid"));
            };
            let Ok(ids) = p.get("id").split('|').map(str::parse::<usize>).collect::<Result<Vec<_>, _>>()
            else {
                return Some(fail(StatusCode::BAD_REQUEST, "File IDs must be integers"));
            };
            if !st.session.exists(&hash) {
                return Some(not_found());
            }
            let known: Vec<usize> = st.session.files(&hash).iter().map(|f| f.index).collect();
            if known.is_empty() || ids.iter().any(|i| !known.contains(i)) {
                return Some(fail(StatusCode::CONFLICT, "File ID not found"));
            }
            for id in ids {
                st.session.set_file_priority(&hash, id, level);
            }
            ok()
        }

        ("torrents", "setCategory") => {
            let category = p.get("category");
            let id = if category.is_empty() {
                None
            } else {
                match st.session.label_id(category) {
                    Some(id) => Some(id),
                    None => return Some(fail(StatusCode::CONFLICT, "Category does not exist")),
                }
            };
            each(st, &p, |s, h| s.set_label(h, id))
        }
        ("torrents", "createCategory") => {
            let name = p.get("category").trim();
            if name.is_empty() {
                return Some(fail(StatusCode::BAD_REQUEST, "Category cannot be empty"));
            }
            if st.session.label_id(name).is_some() {
                return Some(fail(StatusCode::CONFLICT, "Category name is invalid"));
            }
            let path = p.get("savePath").trim();
            st.cfg.upsert_label(&crate::core::configuration::Label {
                name: name.to_owned(),
                save_path: path.to_owned(),
                save_path_enabled: !path.is_empty(),
                ..Default::default()
            });
            ok()
        }
        ("torrents", "editCategory") => {
            let name = p.get("category").trim();
            if name.is_empty() {
                return Some(fail(StatusCode::BAD_REQUEST, "Category cannot be empty"));
            }
            let Some(mut label) = st.cfg.get_labels().into_iter().find(|l| l.name.eq_ignore_ascii_case(name))
            else {
                return Some(fail(StatusCode::CONFLICT, "Category editing failed"));
            };
            let path = p.get("savePath").trim();
            label.save_path = path.to_owned();
            label.save_path_enabled = !path.is_empty();
            st.cfg.upsert_label(&label);
            ok()
        }
        ("torrents", "removeCategories") => {
            for name in names(p.get("categories"), '\n') {
                if let Some(id) = st.session.label_id(&name) {
                    st.cfg.delete_label(id);
                }
            }
            ok()
        }

        ("torrents", "createTags") => {
            for name in names(p.get("tags"), ',') {
                st.cfg.ensure_tag(&name);
            }
            ok()
        }
        ("torrents", "deleteTags") => {
            let gone = names(p.get("tags"), ',');
            for tag in st.cfg.get_tags().into_iter().filter(|t| gone.contains(&t.name)) {
                st.cfg.delete_tag(tag.id);
            }
            ok()
        }
        ("torrents", "addTags") => {
            let ids: Vec<i32> = names(p.get("tags"), ',')
                .iter()
                .filter_map(|n| st.cfg.ensure_tag(n))
                .collect();
            for h in hashes(st, &p) {
                for id in &ids {
                    st.cfg.add_tag(&h, *id);
                }
            }
            ok()
        }
        ("torrents", "removeTags") => {
            // No tags named means every tag, as in qBittorrent.
            let gone = names(p.get("tags"), ',');
            for h in hashes(st, &p) {
                for tag in st.cfg.tags_for(&h) {
                    if gone.is_empty() || gone.contains(&tag.name) {
                        st.cfg.remove_tag(&h, tag.id);
                    }
                }
            }
            ok()
        }

        ("torrents", "addTrackers") => {
            let hash = p.get("hash").to_ascii_lowercase();
            if !st.session.exists(&hash) {
                return Some(not_found());
            }
            // A new tier for each, after the last: the desktop's "add a tier".
            let tier = st.session.tracker_tiers(&hash).len();
            for (i, url) in names(p.get("urls"), '\n').iter().enumerate() {
                st.session.add_tracker(&hash, url, tier + i);
            }
            ok()
        }
        ("torrents", "editTracker") => {
            let hash = p.get("hash").to_ascii_lowercase();
            if !st.session.exists(&hash) {
                return Some(not_found());
            }
            if st.session.edit_tracker(&hash, p.get("url"), p.get("newUrl").trim()) {
                ok()
            } else {
                fail(StatusCode::CONFLICT, "Tracker not found or new URL invalid")
            }
        }
        ("torrents", "removeTrackers") => {
            let hash = p.get("hash").to_ascii_lowercase();
            if !st.session.exists(&hash) {
                return Some(not_found());
            }
            for url in names(p.get("urls"), '|') {
                st.session.remove_tracker(&hash, &url);
            }
            ok()
        }
        _ => return None,
    })
}

/// `torrents/add`: magnets from `urls`, .torrent files from the uploads.
///
/// "Ok." when anything was accepted, "Fails." when nothing was - the same
/// rule as qBittorrent. An upload that is not a torrent is a 415, also as
/// there.
fn add(st: &web::Data<AppState>, p: Params) -> Reply {
    let mut sources = Vec::new();
    for bytes in p.files.iter() {
        if crate::ui::torrentfile::parse(bytes).is_err() {
            return fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Error: an uploaded file is not a valid torrent file.");
        }
        sources.push((info_hash_of_file(bytes), AddTorrentSource::TorrentFileBytes(bytes.clone())));
    }
    for url in names(p.get("urls"), '\n') {
        if url.starts_with("magnet:") {
            let hash = crate::bittorrent::v2::magnet_hashes(&url).v1.map(|h| hex(&h));
            sources.push((hash, AddTorrentSource::MagnetUri(url)));
        } else {
            tracing::warn!("qBittorrent API: not fetching {url} - only magnet links are taken from urls");
        }
    }
    if sources.is_empty() {
        return text("Fails.");
    }

    // A category that does not exist yet is created, as qBittorrent does.
    let category = p.get("category").trim();
    let label = if category.is_empty() {
        None
    } else {
        if st.session.label_id(category).is_none() {
            st.cfg.upsert_label(&crate::core::configuration::Label {
                name: category.to_owned(),
                ..Default::default()
            });
        }
        st.cfg.get_labels().into_iter().find(|l| l.name.eq_ignore_ascii_case(category))
    };
    let save_path = Some(p.get("savepath").trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| label.as_ref().filter(|l| l.save_path_enabled).map(|l| l.save_path.clone()))
        .filter(|s| !s.is_empty());
    let params = AddParams {
        save_path,
        start_torrent: !(p.flag("stopped") || p.flag("paused")),
        only_files: None,
        label_id: label.map(|l| l.id),
    };
    let tags = names(p.get("tags"), ',');

    let mut accepted = false;
    for (hash, source) in sources {
        if st.session.add_torrent(source, params.clone()) {
            accepted = true;
            if let Some(hash) = hash.filter(|_| !tags.is_empty()) {
                tag_when_added(st.clone(), hash, tags.clone());
            }
        }
    }
    text(if accepted { "Ok." } else { "Fails." })
}

/// Tag a torrent once it is in the database. An add returns before the
/// torrent exists, and a tag row needs it there; a magnet can take minutes to
/// arrive. Tries for ten minutes, then gives up quietly - an add that never
/// arrived has already been reported as an error by the session.
fn tag_when_added(st: web::Data<AppState>, hash: String, tags: Vec<String>) {
    let _ = std::thread::Builder::new()
        .name(String::from("nt-qbt-tags"))
        .spawn(move || {
            let ids: Vec<i32> = tags.iter().filter_map(|n| st.cfg.ensure_tag(n)).collect();
            for _ in 0..300 {
                if st.session.exists(&hash) {
                    for id in &ids {
                        st.cfg.add_tag(&hash, *id);
                    }
                    let on: Vec<i32> = st.cfg.tags_for(&hash).into_iter().map(|t| t.id).collect();
                    if ids.iter().all(|id| on.contains(id)) {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

/// The v1 info hash of a .torrent: SHA-1 over its `info` dictionary. `None`
/// for a v2-only torrent, whose identity is not that.
fn info_hash_of_file(bytes: &[u8]) -> Option<String> {
    use sha1::{Digest, Sha1};
    let info = crate::core::bencode::dict_get(bytes, b"info")?;
    crate::core::bencode::dict_get(info, b"pieces")?;
    Some(hex(&Sha1::digest(info)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The door this module adds to `require_auth`, through the real
    /// middleware: 403 without a session, "Fails." for a wrong password, and a
    /// cookie from a right one that then opens the API. And with the API off
    /// - no session table - `/api/v2` is plain Basic auth like everything else.
    #[actix_web::test]
    async fn only_a_login_cookie_or_basic_auth_opens_the_api() {
        use actix_web::{App, middleware::from_fn, test};

        let hash = Credentials::hash_password("pw").unwrap();
        let app = |with_api: bool| {
            let creds = Credentials { username: String::from("u"), password_hash: hash.clone() };
            let mut app = App::new()
                .app_data(web::Data::new(creds))
                .app_data(web::Data::new(Attempts::new(auth::Limits::default())));
            if with_api {
                app = app.app_data(web::Data::new(Sids::default()));
            }
            app.wrap(from_fn(auth::require_auth))
                .route("/api/v2/auth/login", web::post().to(h_login))
                .route(
                    "/api/v2/app/version",
                    web::get().to(|| async { HttpResponse::Ok().body(APP_VERSION) }),
                )
        };
        let login = |pass: &str| {
            test::TestRequest::post()
                .uri("/api/v2/auth/login")
                .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
                .set_payload(format!("username=u&password={pass}"))
                .to_request()
        };
        let version = || test::TestRequest::get().uri("/api/v2/app/version");

        let on = test::init_service(app(true)).await;
        let res = test::call_service(&on, version().to_request()).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "no session, no header");

        let res = test::call_service(&on, login("wrong")).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("Set-Cookie").is_none());
        assert_eq!(test::read_body(res).await, "Fails.");

        let res = test::call_service(&on, login("pw")).await;
        let cookie = res.headers().get("Set-Cookie").unwrap().to_str().unwrap().to_owned();
        assert_eq!(test::read_body(res).await, "Ok.");
        let sid = cookie.strip_prefix("SID=").unwrap().split(';').next().unwrap();

        let res = test::call_service(
            &on,
            version().insert_header(("Cookie", format!("SID={sid}"))).to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "the cookie opens it");
        let res = test::call_service(
            &on,
            version().insert_header(("Cookie", "SID=forged")).to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "a made-up cookie does not");

        let off = test::init_service(app(false)).await;
        let res = test::call_service(&off, login("pw")).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "API off: login is not a way in");
        let res = test::call_service(&off, version().to_request()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn a_multipart_body_yields_its_fields_and_uploads() {
        let body = b"--XyZ\r\n\
            Content-Disposition: form-data; name=\"urls\"\r\n\r\n\
            magnet:?xt=urn:btih:abc\r\n\
            --XyZ\r\n\
            Content-Disposition: form-data; name=\"torrents\"; filename=\"a.torrent\"\r\n\
            Content-Type: application/x-bittorrent\r\n\r\n\
            d4:infod\r\n--notit\r\ne\r\n\
            --XyZ\r\n\
            content-disposition: form-data; name=\"category\"\r\n\r\n\
            tv\r\n\
            --XyZ--\r\n";
        let mut p = Params::default();
        p.multipart(body, "XyZ").expect("well-formed");
        assert_eq!(p.get("urls"), "magnet:?xt=urn:btih:abc");
        assert_eq!(p.get("category"), "tv");
        // A CRLF inside the upload that is not followed by the boundary is
        // content, not a delimiter.
        assert_eq!(p.files, vec![b"d4:infod\r\n--notit\r\ne".to_vec()]);
    }

    #[test]
    fn a_truncated_multipart_body_is_refused() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"urls\"\r\n\r\nmagnet:";
        assert!(Params::default().multipart(body, "XyZ").is_none());
        assert!(Params::default().multipart(b"no boundary here", "XyZ").is_none());
    }

    #[test]
    fn the_boundary_is_read_from_the_content_type() {
        assert_eq!(
            boundary("multipart/form-data; boundary=----abc").as_deref(),
            Some("----abc")
        );
        assert_eq!(
            boundary("Multipart/Form-Data; charset=utf-8; boundary=\"q t\"").as_deref(),
            Some("q t")
        );
        assert_eq!(boundary("application/x-www-form-urlencoded"), None);
        assert_eq!(boundary("multipart/form-data; boundary="), None);
    }

    #[test]
    fn the_sid_cookie_is_found_among_others() {
        let mut h = HeaderMap::new();
        h.insert(
            actix_web::http::header::COOKIE,
            "theme=dark; SID=abc123; x=y".parse().unwrap(),
        );
        assert_eq!(sid(&h), Some("abc123"));
        assert_eq!(sid(&HeaderMap::new()), None);
    }

    #[test]
    fn a_session_lives_until_it_ends() {
        let sids = Sids::default();
        let s = sids.start();
        assert_eq!(s.len(), 64);
        assert!(sids.check(&s));
        assert!(!sids.check("not-a-session"));
        sids.end(&s);
        assert!(!sids.check(&s));
    }

    #[test]
    fn states_use_qbittorrent_5_names() {
        let t = |state, dl, ul, progress| TorrentStatus {
            state,
            download_payload_rate: dl,
            upload_payload_rate: ul,
            progress,
            ..Default::default()
        };
        assert_eq!(state(&t(State::Downloading, 10, 0, 0.5)), "downloading");
        assert_eq!(state(&t(State::Downloading, 0, 0, 0.5)), "stalledDL");
        assert_eq!(state(&t(State::DownloadingPaused, 0, 0, 0.5)), "stoppedDL");
        assert_eq!(state(&t(State::Uploading, 0, 5, 1.0)), "uploading");
        assert_eq!(state(&t(State::Uploading, 0, 0, 1.0)), "stalledUP");
        assert_eq!(state(&t(State::UploadingPaused, 0, 0, 1.0)), "stoppedUP");
        assert_eq!(state(&t(State::CheckingFiles, 0, 0, 1.0)), "checkingUP");
        assert_eq!(state(&t(State::DownloadingMetadata, 0, 0, 0.0)), "metaDL");

        let seeding = t(State::Uploading, 0, 0, 1.0);
        assert!(filter_matches("completed", "stalledUP", &seeding));
        assert!(filter_matches("seeding", "stalledUP", &seeding));
        assert!(!filter_matches("downloading", "stalledUP", &seeding));
        assert!(!filter_matches("seeding", "stoppedUP", &seeding));
        assert!(filter_matches("stopped", "stoppedDL", &seeding));
    }

    #[test]
    fn priorities_and_limits_map_both_ways() {
        for level in [PRIORITY_SKIP, PRIORITY_NORMAL, PRIORITY_HIGH, PRIORITY_MAX] {
            assert_eq!(from_qbt_priority(to_qbt_priority(level)), Some(level));
        }
        for invalid in [-1, 2, 3, 5, 8] {
            assert_eq!(from_qbt_priority(invalid), None, "{invalid}");
        }
        assert_eq!(ratio_limit(None), -2.0);
        assert_eq!(ratio_limit(Some(0.0)), -1.0);
        assert_eq!(ratio_limit(Some(1.5)), 1.5);
        assert_eq!(seed_limit(None), -2);
        assert_eq!(seed_limit(Some(0)), -1);
        assert_eq!(seed_limit(Some(90)), 90);
    }

    #[test]
    fn a_multi_file_torrent_is_reported_from_its_parent_folder() {
        let entry = |name: &str| FileEntry {
            index: 0,
            name: name.to_owned(),
            length: 1,
            included: true,
            progress: 0.0,
        };
        let t = TorrentStatus {
            save_path: String::from("/data/Show"),
            ..Default::default()
        };
        let multi = layout(&t, &[entry("a.mkv"), entry("Subs/a.srt")]);
        assert_eq!(multi.save_path, "/data");
        assert_eq!(multi.content_path, "/data/Show");
        assert_eq!(multi.root, "Show/");

        let t = TorrentStatus {
            save_path: String::from("/data"),
            ..Default::default()
        };
        let single = layout(&t, &[entry("film.mkv")]);
        assert_eq!(single.save_path, "/data");
        assert_eq!(single.content_path, std::path::Path::new("/data").join("film.mkv").to_string_lossy());
        assert_eq!(single.root, "");
    }

    #[test]
    fn a_torrent_files_info_hash_is_the_sha1_of_its_info_dict() {
        // d4:infod6:lengthi1e4:name1:a12:piece lengthi1e6:pieces0:ee
        let info = b"d6:lengthi1e4:name1:a12:piece lengthi1e6:pieces0:e";
        let mut file = b"d4:info".to_vec();
        file.extend_from_slice(info);
        file.push(b'e');
        use sha1::{Digest, Sha1};
        assert_eq!(info_hash_of_file(&file), Some(hex(&Sha1::digest(info))));
        assert_eq!(info_hash_of_file(b"d4:infod4:name1:aee"), None, "no pieces: v2-only");
    }
}

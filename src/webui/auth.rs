//! HTTP Basic authentication for the web interface.
//!
//! Applied as middleware rather than as a per-handler extractor on purpose: a
//! forgotten extractor is a silently unauthenticated endpoint, whereas a
//! forgotten middleware fails closed by never being wired at all. Everything
//! under the server is behind this, `/api/fs` most of all - it browses the
//! filesystem, and `add_torrent` / `move_storage` write to it.

use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{Error, HttpResponse, web};
use argon2::Argon2;
// `phc::PasswordHash`, not the root re-export: argon2 0.6 deprecated the
// latter. Same type, same PHC string format, so hashes written by earlier
// versions still parse and verify.
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine;
use subtle::ConstantTimeEq;

/// How many failures from one address before it is refused, and for how long.
///
/// Argon2 already makes each guess expensive, which is most of the defence.
/// This exists because nothing previously stopped a client simply trying
/// forever - and `bind_address` can be set to 0.0.0.0.
/// How the lockout is tuned, from Preferences or the web drawer.
///
/// The window and the block are separate on purpose. They used to be one
/// number, which forced a choice nobody should have to make: a long lockout
/// meant a long memory for stray typos, and a short memory meant a short
/// lockout. Counting over a minute and then blocking for an hour is the shape
/// people actually want.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Failures within `window` that trip the lockout. Zero disables it.
    pub max_failures: u32,
    /// How long failures are remembered while counting.
    pub window: std::time::Duration,
    /// How long an address is refused once it has tripped.
    pub block: std::time::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_failures: 5,
            window: std::time::Duration::from_secs(60),
            block: std::time::Duration::from_secs(3600),
        }
    }
}

/// Failed attempts per client address.
///
/// Keyed by address, not by username: there is only one account, so counting
/// per user would be one global counter that any passer-by could use to lock
/// the owner out.
#[derive(Default)]
pub struct Attempts {
    limits: Limits,
    state: std::sync::Mutex<std::collections::HashMap<String, Record>>,
}

/// One address's history: how many failures in the current window, when that
/// window opened, and - once it has tripped - when it may try again.
#[derive(Clone, Copy)]
struct Record {
    count: u32,
    window_start: std::time::Instant,
    blocked_until: Option<std::time::Instant>,
}

impl Attempts {
    pub fn new(limits: Limits) -> Self {
        Attempts {
            limits,
            state: Default::default(),
        }
    }

    /// How long this address must wait, or `None` if it may try now.
    ///
    /// A lapsed window resets the count on read, so the map does not need a
    /// sweeper task - an address that stops trying is forgotten the next time
    /// it appears.
    fn locked_for(&self, who: &str) -> Option<std::time::Duration> {
        if self.limits.max_failures == 0 {
            return None;
        }
        let mut map = self.state.lock().unwrap();
        let record = *map.get(who)?;

        if let Some(until) = record.blocked_until {
            return match until.checked_duration_since(std::time::Instant::now()) {
                // Still serving it out.
                Some(left) if !left.is_zero() => Some(left),
                // Served. Forget the address entirely rather than leaving it
                // one failure from another block.
                _ => {
                    map.remove(who);
                    None
                }
            };
        }

        // Not blocked, and the counting window has lapsed: drop it, so the map
        // needs no sweeper task and a quiet address is forgotten on sight.
        if record.window_start.elapsed() >= self.limits.window {
            map.remove(who);
        }
        None
    }

    fn record_failure(&self, who: &str) {
        if self.limits.max_failures == 0 {
            return;
        }
        let mut map = self.state.lock().unwrap();
        let now = std::time::Instant::now();
        let entry = map.entry(who.to_string()).or_insert(Record {
            count: 0,
            window_start: now,
            blocked_until: None,
        });

        // Restart the window if the last one lapsed, so occasional typos
        // spread over an afternoon never accumulate into a lockout.
        if entry.blocked_until.is_none() && entry.window_start.elapsed() >= self.limits.window {
            *entry = Record {
                count: 0,
                window_start: now,
                blocked_until: None,
            };
        }
        entry.count += 1;
        if entry.count >= self.limits.max_failures {
            entry.blocked_until = Some(now + self.limits.block);
        }

        // Unbounded growth is the obvious way to turn a rate limiter into the
        // denial of service it was meant to prevent. Only entries that have
        // gone quiet are dropped, so an active attacker cannot flush their own.
        if map.len() > 1024 {
            let horizon = self.limits.window.max(self.limits.block);
            map.retain(|_, r| {
                r.blocked_until.is_some_and(|u| u > now) || r.window_start.elapsed() < horizon
            });
        }
    }

    fn record_success(&self, who: &str) {
        self.state.lock().unwrap().remove(who);
    }
}

/// The single configured account. There is one user; this is a personal
/// client, not a multi-tenant service.
pub struct Credentials {
    pub username: String,
    /// PHC-format Argon2 string. Empty means "not configured", and the server
    /// refuses to start rather than listening without a password.
    pub password_hash: String,
}

impl Credentials {
    /// Whether a password has been set. Without one the server refuses to
    /// listen at all, rather than listening with no way in.
    pub fn is_configured(&self) -> bool {
        !self.password_hash.is_empty() && PasswordHash::new(&self.password_hash).is_ok()
    }

    /// Hash a new password for storage. Argon2id with the crate defaults.
    ///
    /// The hasher generates the salt itself now - sixteen random bytes, the
    /// same length `SaltString::generate` used to produce - so there is no
    /// salt to pass in and none to get wrong.
    pub fn hash_password(password: &str) -> anyhow::Result<String> {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash: PasswordHash| hash.to_string())
            .map_err(|e| anyhow::anyhow!("failed to hash password: {e}"))
    }

    /// Check one set of credentials.
    ///
    /// Neither half returns early, so a wrong username and a wrong password
    /// cost the same Argon2 hash and take the same time - which is what stops
    /// the reply saying which half was wrong.
    ///
    /// `ct_eq` on the username is content-constant-time but NOT
    /// length-constant-time: subtle's slice impl documents that it
    /// short-circuits when the lengths differ. So the LENGTH of the configured
    /// username is observable. That is accepted rather than fixed: it is one
    /// small integer about a name that is `nanotorrent` unless someone changed
    /// it, and hiding it would mean hashing the username too - real cost for a
    /// secret nobody is keeping.
    fn verify(&self, username: &str, password: &str) -> bool {
        // Both checks always run, and only then are combined. Returning early
        // on a bad username would make a wrong-user request measurably faster
        // than a wrong-password one, which tells an attacker when they have
        // guessed the username.
        let user_ok: bool = username
            .as_bytes()
            .ct_eq(self.username.as_bytes())
            .into();

        let pass_ok = match PasswordHash::new(&self.password_hash) {
            Ok(parsed) => Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        };

        user_ok & pass_ok
    }
}

/// Split a `Basic <base64>` header into its user and password halves.
fn parse_basic(header: &str) -> Option<(String, String)> {
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    // The password half may itself contain ':', so split once from the left.
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// The 401 every failed or missing authentication gets.
///
/// One shape for all of them - wrong password, wrong username, no header at
/// all - so the reply never says which part was wrong.
/// Refuse an address that has failed too often, telling it when to come back.
///
/// 429 rather than another 401: the credentials were not even looked at, and
/// a client that keeps seeing 401 has no way to tell it is being throttled.
fn too_many_requests(req: ServiceRequest, wait: std::time::Duration) -> ServiceResponse<BoxBody> {
    let res = HttpResponse::TooManyRequests()
        .insert_header(("Retry-After", wait.as_secs().max(1).to_string()))
        .body("too many failed attempts");
    req.into_response(res)
}

/// A cross-site write. 403, not 401: the credentials were fine, the request
/// had no business being made, and a 401 would make the browser re-prompt for
/// a password that would not have helped.
fn forbidden(req: ServiceRequest) -> ServiceResponse<BoxBody> {
    req.into_response(
        HttpResponse::Forbidden().body("cross-site requests cannot change anything here"),
    )
}

fn unauthorized(req: ServiceRequest) -> ServiceResponse<BoxBody> {
    // The realm makes browsers show their own credential prompt, which is all
    // the login UI a personal client needs.
    req.into_response(
        HttpResponse::Unauthorized()
            .insert_header(("WWW-Authenticate", "Basic realm=\"NanoTorrent\""))
            .finish(),
    )
}
/// The `(hash, index)` of `/api/torrents/{hash}/files/{index}/stream`, if that
/// is what this path is.
///
/// Written out rather than reached for through actix's path extractors because
/// this runs in middleware, before a route has been matched - there is nothing
/// to extract from yet. Matching the shape by hand also means no other route
/// can start accepting tokens by accident: a path that is one segment off
/// simply is not this one.
fn stream_path(path: &str) -> Option<(&str, usize)> {
    let rest = path.strip_prefix("/api/torrents/")?;
    let (hash, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("files/")?;
    let (index, tail) = rest.split_once('/')?;
    if tail != "stream" {
        return None;
    }
    Some((hash, index.parse().ok()?))
}

/// Is this a cross-site request trying to change something?
///
/// Browsers send Basic credentials on ANY request to an origin they hold them
/// for, including a form on somebody else's page posting to this one. Most of
/// the API is accidentally safe from that - `web::Json` demands
/// `application/json`, and a form cannot send it - but the handlers that take
/// no body at all (pause, resume, recheck, reannounce, apply settings) took a
/// cross-site form POST and did as they were told.
///
/// `Origin` is the check because the browser sets it and script cannot: it is
/// on every POST from a modern browser, and on every cross-origin fetch. A
/// request with no `Origin` is not from a browser form - curl, a plugin, a
/// script - and is left alone, which is what keeps the API usable from
/// anything that is not a browser.
///
/// GET and HEAD are exempt: nothing behind them changes state, which is
/// checked by the route table rather than assumed here.
fn cross_site_write(req: &ServiceRequest) -> bool {
    use actix_web::http::Method;
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return false;
    }

    let Some(origin) = req.headers().get("Origin").and_then(|v| v.to_str().ok()) else {
        // No Origin at all: not a browser form. Left alone deliberately.
        return false;
    };
    // "null" is what a sandboxed iframe or a file:// page sends. It is never
    // this server, and treating it as unknown would let exactly the page that
    // hid its origin through.
    if origin.eq_ignore_ascii_case("null") {
        return true;
    }

    // Compared against the host asked for, not against a configured URL: the
    // interface has no canonical address - it is reached by loopback, by LAN
    // address and by hostname, and every one of those is the right answer to
    // whoever typed it.
    //
    // Two places to look, because HTTP/1.1 and HTTP/2 disagree about where the
    // host lives. h2 has no `Host` header at all - it carries `:authority`,
    // which actix puts in the request URI - and TLS is on by default with h2 in
    // actix's ALPN list, so a browser here is usually speaking h2. Reading only
    // `Host` compared the page's own origin against "" and refused every write
    // the page attempted.
    //
    // Deliberately NOT `connection_info().host()`, which would consult
    // `Forwarded` and `X-Forwarded-Host` as well: those are set by the caller,
    // so either would let a hostile origin vouch for itself and walk straight
    // through this check.
    let host = req
        .headers()
        .get("Host")
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
        .unwrap_or_default();
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    !origin_host.eq_ignore_ascii_case(host)
}

/// Does this request carry a capability token good for the file it is asking
/// for? See [`crate::webui::streamtoken`].
fn stream_token_ok(req: &ServiceRequest) -> bool {
    // Reads only. A token is a capability to read one file; it is not a
    // session, and nothing about it should let a request change anything.
    if req.method() != actix_web::http::Method::GET
        && req.method() != actix_web::http::Method::HEAD
    {
        return false;
    }
    let Some((hash, index)) = stream_path(req.path()) else {
        return false;
    };
    let Some(state) = req.app_data::<web::Data<super::AppState>>() else {
        return false;
    };
    // `token` out of the query string, without pulling in a parser: it is one
    // parameter and the value is hex.
    let Some(token) = req.query_string().split('&').find_map(|pair| {
        pair.strip_prefix("token=")
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_hexdigit()))
    }) else {
        return false;
    };
    state.stream_tokens.verify(token, hash, index)
}


/// Middleware demanding HTTP Basic credentials on every request it wraps.
///
/// Applied to the whole app rather than per-route, so a route added later is
/// protected by default instead of by remembering to say so.
pub async fn require_auth<B>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<BoxBody>, Error>
where
    B: MessageBody + 'static,
{
    let Some(creds) = req.app_data::<web::Data<Credentials>>().cloned() else {
        // Misconfiguration, not a client error: fail closed rather than
        // waving the request through because state is missing.
        tracing::error!("auth middleware has no credentials in app data - denying");
        return Ok(unauthorized(req));
    };

    // The one door that is not the password: a short-lived token, good for a
    // single file, so a media player can be handed a URL without also being
    // handed the credentials to the whole interface.
    //
    // FIRST, ahead of the lockout below, and that ordering is load-bearing. The
    // lockout exists because verifying a password costs an Argon2 hash, so
    // guessing has to be made expensive to serve; a token is a hashmap lookup
    // against a 256-bit value, with nothing to guess and nothing expensive to
    // provoke. Checking it after the lockout meant a locked-out address - a
    // mistyped password minutes earlier - killed playback that was already
    // running, which is how this was found.
    if stream_token_ok(&req) {
        return next.call(req).await.map(|res| res.map_into_boxed_body());
    }

    // Before the password, because this is not about who is asking - the
    // credentials are genuinely theirs, attached by their own browser to
    // somebody else's page's request.
    if cross_site_write(&req) {
        tracing::warn!("refused a cross-site {} to {}", req.method(), req.path());
        return Ok(forbidden(req));
    }

    // peer_addr, NOT connection_info().realip_remote_addr(): that one trusts
    // Forwarded / X-Forwarded-For, which the client sends. Keying on a header
    // the attacker controls means a fresh counter per request and no limit at
    // all. Behind a reverse proxy this collapses to the proxy's address, which
    // is the honest answer - the proxy is the peer.
    //
    // .ip() and not the SocketAddr: the port differs on every connection, so
    // keying on the pair would also be a counter that never counts past one.
    let who = req
        .peer_addr()
        .map_or_else(|| String::from("unknown"), |a| a.ip().to_string());

    // Checked BEFORE the password is verified: an Argon2 hash per attempt is
    // the expensive part, so a locked-out address must not be able to make the
    // server do that work at all.
    let attempts = req.app_data::<web::Data<Attempts>>().cloned();
    if let Some(a) = attempts.as_ref()
        && let Some(wait) = a.locked_for(&who)
    {
        tracing::warn!("locked out {who} for another {}s", wait.as_secs());
        return Ok(too_many_requests(req, wait));
    }

    let header = req.headers().get("Authorization").and_then(|h| h.to_str().ok());
    // Whether there was an attempt at all, separately from whether it parsed.
    // A request with no Authorization header is not a guess: it is a browser
    // asking what this is so it can show its prompt, a bookmark, a port scan.
    // Counting those as failures meant five of them - which one page load can
    // produce on its own - locked the owner out for an hour. A real guess
    // always carries the header, so the brute-force defence is unchanged.
    let attempted = header.is_some();
    let supplied = header.and_then(parse_basic);

    match supplied {
        Some((user, pass)) if creds.verify(&user, &pass) => {
            if let Some(a) = attempts.as_ref() {
                a.record_success(&who);
            }
            next.call(req).await.map(|res| res.map_into_boxed_body())
        }
        _ => {
            if attempted && let Some(a) = attempts.as_ref() {
                a.record_failure(&who);
            }
            tracing::warn!("rejected web request to {} from {who}", req.path());
            Ok(unauthorized(req))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HTTP/2 carries the host as the `:authority` pseudo-header and sends no
    /// `Host` header at all - and actix serves h2 to any modern browser as soon
    /// as TLS is on, which is the default. Reading only `Host` therefore
    /// compared the page's own origin against an empty string and refused every
    /// state-changing request the page made: "Open" on a plugin came back 403,
    /// and so did the settings cog.
    ///
    /// The fix has to keep failing closed, so the authority is read from the
    /// request URI - which actix fills from `:authority` - and never from
    /// `Forwarded` or `X-Forwarded-Host`, which the caller controls and could
    /// simply set to match its own Origin.
    #[test]
    fn a_same_origin_write_over_http2_is_allowed() {
        use actix_web::test::TestRequest;

        // No Host header; the authority rides in the URI, as h2 delivers it.
        let h2 = |origin: &str| {
            cross_site_write(
                &TestRequest::post()
                    .uri("https://127.0.0.1:8443/api/plugins/player/event")
                    .insert_header(("Origin", origin))
                    .to_srv_request(),
            )
        };

        assert!(!h2("https://127.0.0.1:8443"), "the page itself, over h2");
        assert!(h2("https://evil.example"), "another origin, over h2");
        assert!(h2("https://127.0.0.1:9999"), "same host, another port, over h2");

        // A forwarding header must not be able to vouch for the request: if it
        // could, any origin could name itself and walk through.
        assert!(
            cross_site_write(
                &TestRequest::post()
                    .uri("/api/torrents/abc/pause")
                    .insert_header(("Origin", "https://evil.example"))
                    .insert_header(("X-Forwarded-Host", "evil.example"))
                    .insert_header(("Host", "127.0.0.1:8443"))
                    .to_srv_request()
            ),
            "X-Forwarded-Host must not be trusted"
        );
    }

    /// A browser attaches Basic credentials to a form on somebody else's page
    /// posting here. Most of the API is saved by `web::Json` demanding a
    /// content type a form cannot send; the handlers that take no body at all
    /// were not, and did as they were told.
    #[test]
    fn a_cross_site_write_is_refused_and_everything_else_is_not() {
        use actix_web::test::TestRequest;

        let post = |origin: Option<&str>| {
            let mut r = TestRequest::post()
                .uri("/api/torrents/abc/pause")
                .insert_header(("Host", "127.0.0.1:8443"));
            if let Some(o) = origin {
                r = r.insert_header(("Origin", o));
            }
            cross_site_write(&r.to_srv_request())
        };

        assert!(post(Some("https://evil.example")), "another origin");
        assert!(post(Some("http://127.0.0.1:9999")), "same host, another port");
        assert!(post(Some("null")), "a sandboxed frame hides its origin");

        assert!(!post(Some("https://127.0.0.1:8443")), "the page itself");
        assert!(!post(Some("http://127.0.0.1:8443")), "scheme is not the check");
        // curl, a script, the plugin host: no Origin, and left alone
        // deliberately - the API has to stay usable from something that is not
        // a browser.
        assert!(!post(None), "no Origin at all");

        // Reads change nothing, so they are not this check's business - and
        // refusing them would break every <img> and every link.
        assert!(
            !cross_site_write(
                &TestRequest::get()
                    .uri("/api/torrents")
                    .insert_header(("Host", "127.0.0.1:8443"))
                    .insert_header(("Origin", "https://evil.example"))
                    .to_srv_request()
            ),
            "GET is exempt"
        );
    }


    fn creds() -> Credentials {
        Credentials {
            username: String::from("nanotorrent"),
            password_hash: Credentials::hash_password("correct horse battery").unwrap(),
        }
    }

    #[test]
    fn only_the_stream_route_is_shaped_like_a_stream_route() {
        assert_eq!(
            stream_path("/api/torrents/abc123/files/0/stream"),
            Some(("abc123", 0))
        );
        assert_eq!(
            stream_path("/api/torrents/abc123/files/12/stream"),
            Some(("abc123", 12))
        );

        // Everything a token must never reach.
        assert_eq!(stream_path("/api/settings"), None);
        assert_eq!(stream_path("/api/torrents"), None);
        assert_eq!(stream_path("/api/torrents/abc123"), None);
        assert_eq!(stream_path("/api/torrents/abc123/files"), None);
        assert_eq!(stream_path("/api/torrents/abc123/files/0"), None);
        assert_eq!(stream_path("/api/torrents/abc123/files/0/stream/extra"), None);
        assert_eq!(stream_path("/api/torrents/abc123/files/x/stream"), None, "index must be a number");
        assert_eq!(stream_path("/api/torrents/abc123/peers"), None);
        // Not the API at all.
        assert_eq!(stream_path("/torrents/abc123/files/0/stream"), None);
        assert_eq!(stream_path(""), None);
    }

    #[test]
    fn accepts_only_the_right_pair() {
        let c = creds();
        assert!(c.verify("nanotorrent", "correct horse battery"));
        assert!(!c.verify("nanotorrent", "wrong"));
        assert!(!c.verify("someone-else", "correct horse battery"));
        assert!(!c.verify("", ""));
    }

    #[test]
    fn an_unconfigured_account_never_authenticates() {
        // The empty hash is what a fresh database ships with. It must not be
        // treatable as "any password matches".
        let c = Credentials {
            username: String::from("nanotorrent"),
            password_hash: String::new(),
        };
        assert!(!c.is_configured());
        assert!(!c.verify("nanotorrent", ""));
        assert!(!c.verify("nanotorrent", "anything"));
    }

    #[test]
    fn basic_header_parsing() {
        // "nanotorrent:pw:with:colons" - the password keeps its colons.
        let enc = base64::engine::general_purpose::STANDARD.encode("nanotorrent:pw:with:colons");
        let (u, p) = parse_basic(&format!("Basic {enc}")).unwrap();
        assert_eq!(u, "nanotorrent");
        assert_eq!(p, "pw:with:colons");

        assert!(parse_basic("Bearer abc").is_none());
        assert!(parse_basic("Basic !!!not-base64!!!").is_none());
        // No colon at all is not a credential pair.
        let enc = base64::engine::general_purpose::STANDARD.encode("nocolon");
        assert!(parse_basic(&format!("Basic {enc}")).is_none());
    }


    /// The limiter must actually latch, and a success must clear it - a
    /// counter that never trips, or one that keeps the owner out after they
    /// finally type the right password, are both worse than no limiter.
    #[test]
    fn lockout_latches_and_clears() {
        let limits = super::Limits::default();
        let a = super::Attempts::new(limits);

        for _ in 0..limits.max_failures - 1 {
            a.record_failure("10.0.0.1");
        }
        assert!(a.locked_for("10.0.0.1").is_none(), "tripped one attempt early");

        a.record_failure("10.0.0.1");
        assert!(a.locked_for("10.0.0.1").is_some(), "did not trip at the limit");

        // Per address, or one attacker locks the owner out of their own client.
        assert!(a.locked_for("10.0.0.2").is_none());

        a.record_success("10.0.0.1");
        assert!(a.locked_for("10.0.0.1").is_none(), "success did not clear it");
    }

    /// The block is served for the *block* duration, not the counting window.
    /// Conflating the two was the old behaviour and is the bug this setting
    /// exists to make impossible: five tries a minute, blocked for an hour.
    #[test]
    fn the_block_outlives_the_counting_window() {
        let a = super::Attempts::new(super::Limits {
            max_failures: 2,
            window: std::time::Duration::from_millis(30),
            block: std::time::Duration::from_secs(3600),
        });

        a.record_failure("10.0.0.1");
        a.record_failure("10.0.0.1");
        let left = a.locked_for("10.0.0.1").expect("should be blocked");
        assert!(left > std::time::Duration::from_secs(3000), "{left:?}");

        // Well past the counting window, and still blocked.
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(
            a.locked_for("10.0.0.1").is_some(),
            "the window lapsing released the block"
        );
    }

    /// Failures spread wider than the window must never accumulate: someone
    /// who mistypes once a day is not an attacker.
    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let a = super::Attempts::new(super::Limits {
            max_failures: 3,
            window: std::time::Duration::from_millis(20),
            block: std::time::Duration::from_secs(60),
        });

        for _ in 0..6 {
            a.record_failure("10.0.0.1");
            std::thread::sleep(std::time::Duration::from_millis(25));
            assert!(
                a.locked_for("10.0.0.1").is_none(),
                "spread-out typos tripped the lockout"
            );
        }
    }

    /// Zero attempts means the feature is off - and off must mean nothing is
    /// counted or blocked, not "blocks on the first try".
    #[test]
    fn zero_disables_the_limiter() {
        let a = super::Attempts::new(super::Limits {
            max_failures: 0,
            ..super::Limits::default()
        });
        for _ in 0..50 {
            a.record_failure("10.0.0.1");
        }
        assert!(a.locked_for("10.0.0.1").is_none(), "disabled limiter tripped");
    }
    #[test]
    fn hashes_are_salted() {
        // Two hashes of the same password must differ, or the stored value
        // leaks that two accounts share a password.
        let a = Credentials::hash_password("same").unwrap();
        let b = Credentials::hash_password("same").unwrap();
        assert_ne!(a, b);
    }

    /// Hashes written by argon2 0.5 must still verify under 0.6.
    ///
    /// These two strings were generated by argon2 0.5.3 - the version this
    /// upgraded from - and are pasted in verbatim. Nothing in the test suite
    /// would otherwise notice the stored format changing: every other test
    /// hashes and verifies with the same library, which agrees with itself no
    /// matter what it writes. If this fails, everyone with a web password set
    /// is locked out of their own client by an upgrade.
    #[test]
    fn hashes_from_the_previous_argon2_still_verify() {
        const VECTORS: &[(&str, &str)] = &[
            (
                "correct horse battery staple",
                "$argon2id$v=19$m=19456,t=2,p=1$wRgDwfcPAnXYxl5AGSIFSg$\
                 C6ridpGHOUr/m1fOT5gTxTxhS0s+Itj7KkVEJ8Hb9yA",
            ),
            (
                "hunter2",
                "$argon2id$v=19$m=19456,t=2,p=1$+O45XDaQ77pez7kgs25BUA$\
                 OVdMN5bWar485qsMR1GUdP07m5kEwVvjL76yRaibVTc",
            ),
        ];

        for (password, stored) in VECTORS {
            let creds = Credentials {
                username: String::from("nanotorrent"),
                password_hash: (*stored).to_owned(),
            };
            assert!(
                creds.is_configured(),
                "a 0.5-era hash should still parse: {stored}"
            );
            assert!(
                creds.verify("nanotorrent", password),
                "a 0.5-era hash should still verify its own password: {stored}"
            );
            assert!(
                !creds.verify("nanotorrent", "not the password"),
                "and should still reject a wrong one"
            );
        }
    }

    /// And what 0.6 writes is the same shape, so a downgrade or a third-party
    /// reader is not surprised either.
    #[test]
    fn a_freshly_written_hash_keeps_the_same_phc_shape() {
        let hash = Credentials::hash_password("correct horse battery staple").unwrap();
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "unexpected format: {hash}"
        );
        // 16 random bytes of salt, base64 without padding, is 22 characters -
        // the same as the vectors above.
        let salt = hash.split('$').nth(4).expect("a salt field");
        assert_eq!(salt.len(), 22, "salt length changed: {hash}");
    }
}

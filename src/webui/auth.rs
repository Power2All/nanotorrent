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
    /// Both halves are compared in constant time: Argon2 already gives that
    /// for the password, and the username needs the same treatment or the
    /// difference in reply timing leaks whether it was right.
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

fn unauthorized(req: ServiceRequest) -> ServiceResponse<BoxBody> {
    // The realm makes browsers show their own credential prompt, which is all
    // the login UI a personal client needs.
    req.into_response(
        HttpResponse::Unauthorized()
            .insert_header(("WWW-Authenticate", "Basic realm=\"NanoTorrent\""))
            .finish(),
    )
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

    let supplied = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(parse_basic);

    match supplied {
        Some((user, pass)) if creds.verify(&user, &pass) => {
            if let Some(a) = attempts.as_ref() {
                a.record_success(&who);
            }
            next.call(req).await.map(|res| res.map_into_boxed_body())
        }
        _ => {
            if let Some(a) = attempts.as_ref() {
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

    fn creds() -> Credentials {
        Credentials {
            username: String::from("nanotorrent"),
            password_hash: Credentials::hash_password("correct horse battery").unwrap(),
        }
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

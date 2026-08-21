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
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, password_hash::rand_core::OsRng};
use base64::Engine;
use subtle::ConstantTimeEq;

/// The single configured account. There is one user; this is a personal
/// client, not a multi-tenant service.
pub struct Credentials {
    pub username: String,
    /// PHC-format Argon2 string. Empty means "not configured", and the server
    /// refuses to start rather than listening without a password.
    pub password_hash: String,
}

impl Credentials {
    pub fn is_configured(&self) -> bool {
        !self.password_hash.is_empty() && PasswordHash::new(&self.password_hash).is_ok()
    }

    /// Hash a new password for storage. Argon2id with the crate defaults.
    pub fn hash_password(password: &str) -> anyhow::Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("failed to hash password: {e}"))
    }

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

fn unauthorized(req: ServiceRequest) -> ServiceResponse<BoxBody> {
    // The realm makes browsers show their own credential prompt, which is all
    // the login UI a personal client needs.
    req.into_response(
        HttpResponse::Unauthorized()
            .insert_header(("WWW-Authenticate", "Basic realm=\"NanoTorrent\""))
            .finish(),
    )
}

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

    let supplied = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(parse_basic);

    match supplied {
        Some((user, pass)) if creds.verify(&user, &pass) => {
            next.call(req).await.map(|res| res.map_into_boxed_body())
        }
        _ => {
            tracing::warn!(
                "rejected web request to {} from {}",
                req.path(),
                req.connection_info()
                    .realip_remote_addr()
                    .unwrap_or("unknown")
                    .to_string()
            );
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

    #[test]
    fn hashes_are_salted() {
        // Two hashes of the same password must differ, or the stored value
        // leaks that two accounts share a password.
        let a = Credentials::hash_password("same").unwrap();
        let b = Credentials::hash_password("same").unwrap();
        assert_ne!(a, b);
    }
}

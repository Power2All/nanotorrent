//! TLS for the web interface.
//!
//! Three modes, matching `webui.tls_mode`:
//!
//! - `off`      - plain HTTP. Only sane bound to loopback; the caller warns.
//! - `self-signed` - generate once, cache in the data folder, reuse thereafter.
//!   Browsers will warn on first visit; the fingerprint is logged so the
//!   warning can actually be checked rather than clicked through blindly.
//! - `custom`   - PEM paths the user supplies, for anyone already terminating
//!   TLS with their own certificate.
//!
//! Let's Encrypt is deliberately absent: it needs a public DNS name and inbound
//! 443, which a torrent client on a home LAN generally does not have. It lands
//! later, behind an explicit opt-in.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Certificate lifetime is rcgen's default. Self-signed certs are trusted by
/// fingerprint here, not by expiry, and a torrent box may run for a year
/// without anyone looking at it.
const CERT_FILE: &str = "webui-cert.pem";
const KEY_FILE: &str = "webui-key.pem";

/// Build the rustls config for a supplied certificate/key pair.
pub fn from_pem(cert_path: &Path, key_path: &Path) -> Result<ServerConfig> {
    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("cannot read certificate {}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .with_context(|| format!("cannot read private key {}", key_path.display()))?;
    build(&cert_pem, &key_pem)
}

/// Load the cached self-signed certificate, generating it on first use.
pub fn self_signed(data_dir: &Path) -> Result<ServerConfig> {
    let cert_path = data_dir.join(CERT_FILE);
    let key_path = data_dir.join(KEY_FILE);

    if !cert_path.exists() || !key_path.exists() {
        generate(&cert_path, &key_path)?;
    }

    // A cached pair that no longer parses (truncated by a bad shutdown, say)
    // should not take the whole interface down - regenerate and carry on.
    match from_pem(&cert_path, &key_path) {
        Ok(config) => Ok(config),
        Err(err) => {
            tracing::warn!("cached web certificate unusable ({err:#}) - regenerating");
            generate(&cert_path, &key_path)?;
            from_pem(&cert_path, &key_path)
        }
    }
}

/// Write a fresh self-signed certificate and key.
///
/// Called once, when the configured pair is missing. Browsers will warn about
/// it - that is inherent to self-signing - but the traffic is still encrypted,
/// which is what stops Basic credentials crossing the network in clear text.
fn generate(cert_path: &PathBuf, key_path: &PathBuf) -> Result<()> {
    // SANs cover the names this is actually reached by. The LAN address still
    // cannot be predicted - it changes - so reaching the interface by its LAN IP
    // warns even after the certificate is trusted, and the hostname is the
    // answer there. The LOOPBACK addresses are predictable, though, and they are
    // the ones a media player on this machine is handed: without them VLC
    // refuses a stream URL with "the name in the certificate does not match the
    // expected" on top of the usual untrusted-issuer complaint, and accepting
    // the certificate permanently does not clear a name mismatch.
    //
    // rcgen turns an entry that parses as an IP address into an IP SAN and the
    // rest into DNS SANs, so this list needs no further ceremony.
    let names = vec![
        String::from("localhost"),
        String::from("127.0.0.1"),
        String::from("::1"),
        hostname().unwrap_or_else(|| String::from("nanotorrent")),
    ];
    let key = rcgen::generate_simple_self_signed(names)
        .context("failed to generate a self-signed certificate")?;

    std::fs::write(cert_path, key.cert.pem()).with_context(|| {
        format!("cannot write certificate to {}", cert_path.display())
    })?;
    // `signing_key`, not `key_pair`: rcgen 0.14 renamed the field. Same PEM,
    // same PKCS#8 encoding, so certificates written by earlier versions still
    // load - this only changes how the pair is spelled on the way out.
    std::fs::write(key_path, key.signing_key.serialize_pem())
        .with_context(|| format!("cannot write private key to {}", key_path.display()))?;
    restrict_permissions(key_path);

    tracing::info!(
        "generated a self-signed certificate for the web interface at {}",
        cert_path.display()
    );
    Ok(())
}

/// Make the private key readable only by its owner.
///
/// The data folder is per-user on every supported platform, so this is defence
/// in depth rather than the only barrier - but a world-readable 0644 key file
/// on a shared Linux box would be a real one.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!("could not restrict permissions on {}: {err}", path.display());
    }
}

/// Windows inherits the per-user ACL of `%LOCALAPPDATA%`, so there is no mode
/// bit to set. Named rather than cfg'd away at the call site so the Unix branch
/// does not look optional.
#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// This machine's hostname, for the certificate's subject alternative names,
/// so reaching the interface by name rather than by IP still matches.
fn hostname() -> Option<String> {
    // Only used as a certificate SAN, so an env var is enough - not worth a
    // dependency or a syscall wrapper for a nicety.
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|h| !h.is_empty())
}

/// Select the rustls crypto provider for the whole process.
///
/// rustls 0.23 refuses to guess when both its `aws-lc-rs` and `ring` features
/// are enabled, and cargo's feature unification turns both on here - librqbit
/// pulls reqwest 0.12 and this crate pulls reqwest 0.13, and they do not agree.
/// Without this, the first TLS handshake panics rather than returning an error.
///
/// aws-lc-rs is the choice because it is already linked: librqbit's SHA-1 piece
/// hashing runs through it. Idempotent, and safe to call from anywhere.
pub fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_err()
        {
            // Someone else got there first, which is fine - the point is that
            // exactly one provider is installed, not that we installed it.
            tracing::debug!("a rustls crypto provider was already installed");
        }
    });
}

/// Turn a PEM certificate and key into a rustls server config, failing with a
/// readable message rather than a parse error when the pair does not load.
fn build(cert_pem: &[u8], key_pem: &[u8]) -> Result<ServerConfig> {
    ensure_crypto_provider();

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(cert_pem))
        .collect::<Result<_, _>>()
        .context("certificate file contains no valid PEM certificate")?;
    anyhow::ensure!(!certs.is_empty(), "certificate file contains no certificate");

    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(key_pem))
            .context("private key file is not valid PEM")?
            .context("private key file contains no private key")?;

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate and private key do not match")
}

/// SHA-256 fingerprint of the leaf certificate, formatted like a browser shows
/// it. Logged at startup so a self-signed warning can be verified instead of
/// dismissed on faith.
pub fn fingerprint(cert_path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let pem = std::fs::read(cert_path).ok()?;
    let cert = rustls_pemfile::certs(&mut BufReader::new(&pem[..]))
        .next()?
        .ok()?;
    let digest = Sha256::digest(&cert);
    Some(
        digest
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// Where the self-signed certificate lives - beside the settings database,
/// so it travels with the profile rather than the installation.
pub fn cert_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CERT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_then_reuses_a_certificate() {
        let dir = std::env::temp_dir().join(format!("nt-tls-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        assert!(self_signed(&dir).is_ok(), "first call should generate");
        let first = std::fs::read(dir.join(CERT_FILE)).unwrap();

        assert!(self_signed(&dir).is_ok(), "second call should reuse");
        let second = std::fs::read(dir.join(CERT_FILE)).unwrap();
        assert_eq!(first, second, "certificate was regenerated instead of reused");

        assert!(fingerprint(&cert_path(&dir)).is_some_and(|f| f.contains(':')));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The certificate has to name the loopback addresses, not just `localhost`.
    ///
    /// A media player handed a stream URL is the reason. Without an IP SAN, VLC
    /// refuses `https://127.0.0.1/...` with "the name in the certificate does not
    /// match the expected" *on top of* the untrusted-issuer complaint - and
    /// accepting the certificate permanently clears the issuer, never the name.
    /// The LAN address genuinely cannot be predicted here; the loopback ones can.
    #[test]
    fn the_certificate_covers_the_loopback_addresses() {
        let dir = std::env::temp_dir().join(format!("nt-tls-san-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        self_signed(&dir).expect("generate");

        let pem = std::fs::read_to_string(dir.join(CERT_FILE)).unwrap();
        let der = pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<String>();
        let der = base64_decode(&der).expect("the certificate is base64 PEM");

        // Read the SANs out of the DER rather than adding an X.509 parser: an
        // IP SAN is a 4-byte (or 16-byte) octet string, and 127.0.0.1 is the
        // exact byte sequence 7F 00 00 01 sitting behind tag 0x87 (context 7).
        let v4 = [0x87u8, 4, 127, 0, 0, 1];
        assert!(
            der.windows(v4.len()).any(|w| w == v4),
            "no IP SAN for 127.0.0.1 - VLC will reject a stream URL by IP"
        );
        assert!(
            String::from_utf8_lossy(&der).contains("localhost"),
            "no DNS SAN for localhost"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal base64 for the test above, so this does not reach for a crate.
    fn base64_decode(s: &str) -> Option<Vec<u8>> {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0u32;
        for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
            let v = A.iter().position(|&a| a == c)? as u32;
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        Some(out)
    }

    #[test]
    fn a_corrupt_cached_certificate_is_regenerated() {
        let dir = std::env::temp_dir().join(format!("nt-tls-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(CERT_FILE), b"not a certificate").unwrap();
        std::fs::write(dir.join(KEY_FILE), b"not a key").unwrap();

        assert!(
            self_signed(&dir).is_ok(),
            "a damaged cached pair must self-heal, not kill the interface"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

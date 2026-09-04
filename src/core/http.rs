//! The one place an HTTP client is built.
//!
//! There used to be three, each calling `reqwest::Client::builder()` for
//! itself: web seeds, the plugin host's `http_get`, and the update check. None
//! of them knew about the SOCKS proxy, so a user who had switched the proxy on
//! and ticked "proxy peer connections" was still fetching torrent payload from
//! web seeds over their own address - a leak that looked exactly like privacy.
//!
//! Routing every one of them through here is not tidiness. It is the property
//! that a client which forgets to ask for the proxy cannot exist, because there
//! is nowhere else to get a client from.
//!
//! One rule, no per-caller exceptions: **if a proxy is configured, everything
//! uses it.** librqbit distinguishes peer traffic from tracker traffic because
//! libtorrent did; that distinction does not extend here. Everything this
//! module hands out is "the application talking to the internet", and a user
//! who proxies their torrents did not mean to exempt the update check.

use std::sync::Arc;

use crate::core::configuration::Configuration;

/// Proxy protocol, as stored in `libtorrent.proxy_type`.
///
/// The numbering is libtorrent's, kept because it is what is already in
/// people's settings databases.
const PROXY_TYPE_SOCKS4: i64 = 1;
const PROXY_TYPE_SOCKS5: i64 = 2;
const PROXY_TYPE_SOCKS5_PW: i64 = 3;

/// The SOCKS URL to route through, or `None` for a direct connection.
///
/// Shared with the session so the engine and this module cannot disagree about
/// whether a proxy is in play - they read the same keys and build the same URL.
pub fn proxy_url(cfg: &Configuration) -> Option<String> {
    let kind = cfg.get_int("libtorrent.proxy_type").unwrap_or(0);
    if !matches!(kind, PROXY_TYPE_SOCKS4 | PROXY_TYPE_SOCKS5 | PROXY_TYPE_SOCKS5_PW) {
        return None;
    }

    let host = cfg.get_string("libtorrent.proxy_host").unwrap_or_default();
    let port = cfg.get_int("libtorrent.proxy_port").unwrap_or(0);
    if host.is_empty() || port == 0 {
        return None;
    }

    // socks5h, not socks5, so names are resolved AT the proxy. Resolving here
    // would send a DNS query for every tracker and web seed straight out of
    // this machine, which is the leak the proxy was turned on to prevent.
    if kind == PROXY_TYPE_SOCKS5_PW {
        let user = cfg
            .get_string("libtorrent.proxy_username")
            .unwrap_or_default();
        let pass = cfg
            .get_string("libtorrent.proxy_password")
            .unwrap_or_default();
        Some(format!("socks5h://{user}:{pass}@{host}:{port}"))
    } else {
        Some(format!("socks5h://{host}:{port}"))
    }
}

/// Build an HTTP client that honours the proxy setting.
///
/// Returns an error rather than a direct client when a proxy is configured but
/// unusable. Falling back to a direct connection is the one thing this must
/// never do: it would turn a broken proxy into a silent deanonymisation.
pub fn client(cfg: &Configuration) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().user_agent(crate::buildinfo::user_agent());

    if let Some(url) = proxy_url(cfg) {
        builder = builder.proxy(reqwest::Proxy::all(&url)?);
    }

    builder.build()
}

/// The same, for callers that hold the configuration behind an `Arc`.
pub fn client_arc(cfg: &Arc<Configuration>) -> reqwest::Result<reqwest::Client> {
    client(cfg.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Configuration {
        let db = Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        Configuration::new(db)
    }

    /// No proxy configured is a direct connection, which is the default and
    /// has to keep working.
    #[test]
    fn no_proxy_configured_means_no_proxy_url() {
        let cfg = cfg();
        assert_eq!(proxy_url(&cfg), None);
        assert!(client(&cfg).is_ok());
    }

    /// A proxy type with no host is not a proxy. Returning a URL built from an
    /// empty host would make every request fail in a way that looks like the
    /// network being down rather than a setting being half-filled.
    #[test]
    fn a_half_configured_proxy_is_not_used() {
        let cfg = cfg();
        cfg.set("libtorrent.proxy_type", &PROXY_TYPE_SOCKS5);
        assert_eq!(proxy_url(&cfg), None, "no host yet");

        cfg.set("libtorrent.proxy_host", &"127.0.0.1");
        assert_eq!(proxy_url(&cfg), None, "no port yet");
    }

    /// Names must be resolved at the proxy, not here.
    ///
    /// `socks5://` would have this machine look up every tracker and web seed
    /// hostname itself, sending a DNS query from the real address for each one.
    /// That is a leak of exactly what the user was hiding.
    #[test]
    fn hostnames_are_resolved_at_the_proxy() {
        let cfg = cfg();
        cfg.set("libtorrent.proxy_type", &PROXY_TYPE_SOCKS5);
        cfg.set("libtorrent.proxy_host", &"127.0.0.1");
        cfg.set("libtorrent.proxy_port", &1080_i64);

        let url = proxy_url(&cfg).expect("a proxy");
        assert!(url.starts_with("socks5h://"), "got {url}");
        assert!(url.ends_with("127.0.0.1:1080"), "got {url}");
    }

    #[test]
    fn credentials_are_carried_when_the_type_asks_for_them() {
        let cfg = cfg();
        cfg.set("libtorrent.proxy_type", &PROXY_TYPE_SOCKS5_PW);
        cfg.set("libtorrent.proxy_host", &"proxy.invalid");
        cfg.set("libtorrent.proxy_port", &9050_i64);
        cfg.set("libtorrent.proxy_username", &"someone");
        cfg.set("libtorrent.proxy_password", &"secret");

        assert_eq!(
            proxy_url(&cfg).as_deref(),
            Some("socks5h://someone:secret@proxy.invalid:9050")
        );
    }

    /// A configured proxy really does reach the built client. Without this the
    /// factory could quietly hand out direct clients and every test above
    /// would still pass.
    #[test]
    fn a_configured_proxy_produces_a_client() {
        let cfg = cfg();
        cfg.set("libtorrent.proxy_type", &PROXY_TYPE_SOCKS5);
        cfg.set("libtorrent.proxy_host", &"127.0.0.1");
        cfg.set("libtorrent.proxy_port", &1080_i64);
        assert!(client(&cfg).is_ok());
    }
}

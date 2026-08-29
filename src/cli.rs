//! Reading and writing preferences from the command line.
//!
//! The Preferences dialog is the only other way to reach most of these, which
//! leaves a headless build (`--no-default-features`) with settings it can read
//! but never change, and a remote box with no way to turn a rate limit down
//! without an X session.
//!
//! Handled before the single-instance IPC check in `main`, for the reason
//! given in [`crate::webui::cli`]: argv is otherwise forwarded to the running
//! window as though it were a torrent to open.
//!
//! The names here are the CLI's own, not the database keys. Half the stored
//! keys still carry PicoTorrent's `libtorrent.` prefix - this build runs
//! librqbit - and none of that is worth exposing to someone writing a script.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::core::configuration::Configuration;
use crate::core::database::Database;
use crate::core::environment::Environment;
use crate::ui::translator::Translator;

/// What a setting accepts, and how it is stored.
pub enum Kind {
    /// `true` or `false`.
    Bool,
    /// A whole number in an inclusive range. `unit` is for the listing only.
    Int {
        lo: i64,
        hi: i64,
        unit: &'static str,
    },
    /// Free text. An empty value clears it.
    Text,
    /// A filesystem path that has to exist as a directory.
    Dir,
    /// One of a fixed set, stored as the string itself.
    Choice(&'static [&'static str]),
    /// One of a fixed set, stored as its **index** - which is how the original
    /// wrote `proxy_type`, and what the session still reads.
    Index(&'static [&'static str]),
    /// One of a fixed set, in `persistent_object` rather than `setting`.
    Persist(&'static [&'static str]),
    /// A locale, checked against the ones this build embeds.
    Locale,
    /// Held in the `listen_interface` table rather than in `setting`.
    ListenAddress,
    ListenPort,
    /// Delegated to [`crate::webui::cli::set_setting`] under the name given,
    /// so the web settings keep their one validation path - including the
    /// warnings it prints about non-loopback binds and TLS being off.
    Web(&'static str),
}

pub struct Setting {
    pub name: &'static str,
    /// The `setting` table key. Empty for the kinds that live elsewhere.
    pub key: &'static str,
    pub kind: Kind,
}

const THEMES: &[&str] = &["system", "light", "dark"];
const CLOSE_ACTIONS: &[&str] = &["ask", "minimize", "exit"];
const TLS_MODES: &[&str] = &["self-signed", "custom", "off"];
/// Index order is the stored value - see `ConnectionProxyType::from_i64`.
const PROXY_TYPES: &[&str] = &[
    "none",
    "socks4",
    "socks5",
    "socks5-password",
    "http",
    "http-password",
];

/// Every preference reachable from the command line.
///
/// Deliberately a whitelist rather than a passthrough to the settings table:
/// two thirds of the keys in there are inherited PicoTorrent leftovers that
/// nothing reads, and a generic setter would accept a typo, report success and
/// change nothing.
pub const SETTINGS: &[Setting] = &[
    // --- General ---------------------------------------------------------
    Setting { name: "language", key: "locale_name", kind: Kind::Locale },
    Setting { name: "theme", key: "theme_id", kind: Kind::Choice(THEMES) },
    Setting { name: "close-action", key: "ui.close_action", kind: Kind::Persist(CLOSE_ACTIONS) },
    Setting { name: "skip-add-dialog", key: "skip_add_torrent_dialog", kind: Kind::Bool },
    Setting { name: "tray-icon", key: "show_in_notification_area", kind: Kind::Bool },
    Setting { name: "minimize-to-tray", key: "minimize_to_notification_area", kind: Kind::Bool },
    Setting { name: "notify-complete", key: crate::core::toast::ENABLED_KEY, kind: Kind::Bool },
    Setting { name: "check-updates", key: "update_checks.enabled", kind: Kind::Bool },
    Setting { name: "update-url", key: "update_checks.url", kind: Kind::Text },

    // --- Downloads -------------------------------------------------------
    Setting { name: "save-path", key: "default_save_path", kind: Kind::Dir },
    Setting { name: "pause-on-low-disk", key: "pause_on_low_disk_space", kind: Kind::Bool },
    Setting { name: "low-disk-limit", key: "pause_on_low_disk_space_limit", kind: Kind::Int { lo: 0, hi: 100, unit: "%" } },
    Setting { name: "active-limit", key: "libtorrent.active_limit", kind: Kind::Int { lo: 0, hi: 100_000, unit: "torrents" } },
    Setting { name: "active-downloads", key: "libtorrent.active_downloads", kind: Kind::Int { lo: 0, hi: 100_000, unit: "torrents" } },
    Setting { name: "active-seeds", key: "libtorrent.active_seeds", kind: Kind::Int { lo: 0, hi: 100_000, unit: "torrents" } },
    Setting { name: "limit-download", key: "libtorrent.enable_download_rate_limit", kind: Kind::Bool },
    Setting { name: "download-rate-limit", key: "libtorrent.download_rate_limit", kind: Kind::Int { lo: 0, hi: 10_000_000, unit: "KB/s" } },
    Setting { name: "limit-upload", key: "libtorrent.enable_upload_rate_limit", kind: Kind::Bool },
    Setting { name: "upload-rate-limit", key: "libtorrent.upload_rate_limit", kind: Kind::Int { lo: 0, hi: 10_000_000, unit: "KB/s" } },

    // --- Connection ------------------------------------------------------
    Setting { name: "listen-address", key: "", kind: Kind::ListenAddress },
    Setting { name: "listen-port", key: "", kind: Kind::ListenPort },
    Setting { name: "dht", key: "libtorrent.enable_dht", kind: Kind::Bool },
    Setting { name: "lsd", key: "libtorrent.enable_lsd", kind: Kind::Bool },
    Setting { name: "pex", key: "libtorrent.enable_pex", kind: Kind::Bool },
    Setting { name: "geoip", key: "geoip.enabled", kind: Kind::Bool },
    Setting { name: "ipfilter", key: "ipfilter.enabled", kind: Kind::Bool },
    Setting { name: "ipfilter-path", key: "ipfilter.file_path", kind: Kind::Text },
    Setting { name: "encrypt-incoming", key: "libtorrent.require_incoming_encryption", kind: Kind::Bool },
    Setting { name: "encrypt-outgoing", key: "libtorrent.require_outgoing_encryption", kind: Kind::Bool },
    Setting { name: "anonymous-mode", key: "libtorrent.anonymous_mode", kind: Kind::Bool },

    // --- Proxy -----------------------------------------------------------
    Setting { name: "proxy-type", key: "libtorrent.proxy_type", kind: Kind::Index(PROXY_TYPES) },
    Setting { name: "proxy-host", key: "libtorrent.proxy_host", kind: Kind::Text },
    Setting { name: "proxy-port", key: "libtorrent.proxy_port", kind: Kind::Int { lo: 1, hi: 65535, unit: "" } },
    Setting { name: "proxy-hostnames", key: "libtorrent.proxy_hostnames", kind: Kind::Bool },
    Setting { name: "proxy-peers", key: "libtorrent.proxy_peers", kind: Kind::Bool },
    Setting { name: "proxy-trackers", key: "libtorrent.proxy_trackers", kind: Kind::Bool },

    // --- Web interface ---------------------------------------------------
    // The first six delegate, so `--set web-port` and `--webui-set port` are
    // the same code and cannot validate differently.
    Setting { name: "web-enabled", key: "webui.enabled", kind: Kind::Bool },
    Setting { name: "web-bind", key: "webui.bind_address", kind: Kind::Web("bind_address") },
    Setting { name: "web-port", key: "webui.port", kind: Kind::Web("port") },
    Setting { name: "web-username", key: "webui.username", kind: Kind::Web("username") },
    Setting { name: "web-tls-mode", key: "webui.tls_mode", kind: Kind::Web("tls_mode") },
    Setting { name: "web-cert", key: "webui.tls_cert_path", kind: Kind::Web("tls_cert_path") },
    Setting { name: "web-key", key: "webui.tls_key_path", kind: Kind::Web("tls_key_path") },

    // Advanced. Ranges match Advanced::load, which clamps on the way out too.
    Setting { name: "web-request-timeout", key: "webui.client_request_timeout", kind: Kind::Int { lo: 1, hi: 3600, unit: "s" } },
    Setting { name: "web-disconnect-timeout", key: "webui.client_disconnect_timeout", kind: Kind::Int { lo: 1, hi: 3600, unit: "s" } },
    Setting { name: "web-keep-alive", key: "webui.keep_alive", kind: Kind::Int { lo: 0, hi: 86400, unit: "s" } },
    Setting { name: "web-max-connections", key: "webui.max_connections", kind: Kind::Int { lo: 1, hi: 100_000, unit: "" } },
    Setting { name: "web-max-connection-rate", key: "webui.max_connection_rate", kind: Kind::Int { lo: 1, hi: 100_000, unit: "/s" } },
    Setting { name: "web-workers", key: "webui.workers", kind: Kind::Int { lo: 1, hi: 64, unit: "threads" } },
    Setting { name: "web-shutdown-timeout", key: "webui.shutdown_timeout", kind: Kind::Int { lo: 0, hi: 3600, unit: "s" } },
    Setting { name: "web-max-body", key: "webui.max_body_size", kind: Kind::Int { lo: 1, hi: 1024, unit: "MB" } },
];

/// The `--set` / `--get` half of `--help`, in the configured language.
///
/// Flag and setting NAMES stay English - they are what you type. Only the
/// prose around them is translated.
pub fn usage(tr: &Translator) -> String {
    format!(
        concat!(
            "{}\n",
            "\n",
            "  nanotorrent --list-settings       {}\n",
            "  nanotorrent --get NAME            {}\n",
            "  nanotorrent --set NAME VALUE      {}\n",
            "\n",
            "{}\n",
            "\n",
            "{}",
        ),
        tr.i18n("cli_prefs_header"),
        tr.i18n("cli_flag_list_settings"),
        tr.i18n("cli_flag_get"),
        tr.i18n("cli_flag_set"),
        tr.i18n("cli_bool_note"),
        tr.i18n("cli_applies_note"),
    )
}

/// Every settable name, for `--help`.
///
/// Generated from [`SETTINGS`] rather than written out, so a setting cannot be
/// added without appearing here. It reads no values, because `--help` runs
/// before the database is opened - `--list-settings` is the one that shows
/// what each is currently set to.
pub fn settings_help(tr: &Translator) -> String {
    let width = SETTINGS.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let mut out = format!("{}\n\n", tr.i18n("cli_settings_header"));
    for s in SETTINGS {
        out.push_str(&format!(
            "  {:width$}  {} ({})\n",
            s.name,
            tr.i18n(&help_key(s)),
            accepts(s, tr),
            width = width
        ));
    }
    out
}

/// The locale key holding a setting's description.
///
/// Derived from the name rather than stored, so adding a setting cannot leave
/// its description behind - `i18n` humanises a key it has never seen, which is
/// visible enough in `--help` to get noticed.
fn help_key(s: &Setting) -> String {
    format!("cli_set_{}", s.name.replace('-', "_"))
}

pub fn find(name: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.name == name)
}

/// The current value, rendered the way `--set` would accept it back.
pub fn show(cfg: &Configuration, s: &Setting) -> String {
    match &s.kind {
        Kind::Bool => cfg.get_bool(s.key).to_string(),
        Kind::Int { .. } => cfg
            .get_int(s.key)
            .map(|v| v.to_string())
            .unwrap_or_else(|| String::from("(unset)")),
        // The one delegated setting that is stored as a number. Read as a
        // string it parses as nothing and reported as "(unset)", while
        // --webui-status showed the real port.
        Kind::Web("port") => cfg
            .get_int(s.key)
            .map(|v| v.to_string())
            .unwrap_or_else(|| String::from("(unset)")),
        Kind::Text | Kind::Dir | Kind::Locale | Kind::Choice(_) | Kind::Web(_) => {
            let v = cfg.get_string(s.key).unwrap_or_default();
            if v.is_empty() { String::from("(unset)") } else { v }
        }
        Kind::Index(names) => {
            let i = cfg.get_int(s.key).unwrap_or(0).max(0) as usize;
            names.get(i).map(|n| String::from(*n)).unwrap_or_else(|| String::from("(unknown)"))
        }
        Kind::Persist(_) => {
            let v = cfg.get_persistent(s.key).unwrap_or_default();
            if v.is_empty() { String::from("(unset)") } else { v }
        }
        Kind::ListenAddress => cfg
            .get_listen_interfaces()
            .first()
            .map(|i| i.address.clone())
            .unwrap_or_else(|| String::from("(unset)")),
        Kind::ListenPort => cfg
            .get_listen_interfaces()
            .first()
            .map(|i| i.port.to_string())
            .unwrap_or_else(|| String::from("(unset)")),
    }
}

/// What this setting accepts, for the listing.
fn accepts(s: &Setting, tr: &Translator) -> String {
    match &s.kind {
        Kind::Bool => String::from("true|false"),
        Kind::Int { lo, hi, unit } => {
            if unit.is_empty() {
                format!("{lo}-{hi}")
            } else {
                format!("{lo}-{hi} {unit}")
            }
        }
        Kind::Text => tr.i18n("cli_accepts_text"),
        Kind::Dir => tr.i18n("cli_accepts_dir"),
        Kind::Locale => tr.i18n("cli_accepts_locale"),
        Kind::Choice(v) | Kind::Index(v) | Kind::Persist(v) => v.join("|"),
        Kind::ListenAddress => tr.i18n("cli_accepts_address"),
        Kind::ListenPort => String::from("1-65535"),
        Kind::Web("port") => String::from("1-65535"),
        Kind::Web("tls_mode") => TLS_MODES.join("|"),
        Kind::Web(_) => tr.i18n("cli_accepts_text"),
    }
}

pub fn set(cfg: &Configuration, s: &Setting, value: &str, tr: &Translator) -> Result<()> {
    match &s.kind {
        Kind::Bool => {
            let on = match value {
                "true" | "on" | "1" | "yes" => true,
                "false" | "off" | "0" | "no" => false,
                _ => anyhow::bail!("{} takes true or false", s.name),
            };
            cfg.set(s.key, &on);
        }
        Kind::Int { lo, hi, .. } => {
            let v: i64 = value
                .trim()
                .parse()
                .ok()
                .filter(|v| (*lo..=*hi).contains(v))
                .with_context(|| format!("{} must be a number between {lo} and {hi}", s.name))?;
            cfg.set(s.key, &v);
        }
        Kind::Text => cfg.set(s.key, &value),
        Kind::Dir => {
            // Checked now rather than at startup, where a typo would show up as
            // torrents landing somewhere unexpected.
            anyhow::ensure!(
                std::path::Path::new(value).is_dir(),
                "{value} is not an existing directory"
            );
            cfg.set(s.key, &value);
        }
        Kind::Locale => {
            anyhow::ensure!(
                crate::ui::translator::EMBEDDED_LANGS
                    .iter()
                    .any(|(l, _)| l.eq_ignore_ascii_case(value)),
                "{value} is not a language this build ships"
            );
            cfg.set(s.key, &value);
        }
        Kind::Choice(names) => {
            anyhow::ensure!(
                names.contains(&value),
                "{} must be one of: {}",
                s.name,
                names.join(", ")
            );
            cfg.set(s.key, &value);
        }
        Kind::Index(names) => {
            let i = names
                .iter()
                .position(|n| *n == value)
                .with_context(|| format!("{} must be one of: {}", s.name, names.join(", ")))?;
            cfg.set(s.key, &(i as i64));
        }
        Kind::Persist(names) => {
            anyhow::ensure!(
                names.contains(&value),
                "{} must be one of: {}",
                s.name,
                names.join(", ")
            );
            cfg.set_persistent(s.key, value);
        }
        Kind::ListenAddress | Kind::ListenPort => {
            let mut iface = cfg
                .get_listen_interfaces()
                .into_iter()
                .next()
                .context("no listen interface is configured")?;
            if matches!(s.kind, Kind::ListenPort) {
                iface.port = value
                    .trim()
                    .parse()
                    .ok()
                    .filter(|p| (1..=65535).contains(p))
                    .context("listen-port must be a number between 1 and 65535")?;
            } else {
                anyhow::ensure!(!value.trim().is_empty(), "listen-address must not be empty");
                iface.address = String::from(value);
            }
            cfg.upsert_listen_interface(&iface);
        }
        Kind::Web(short) => crate::webui::cli::set_setting(cfg, short, value, tr)?,
    }
    Ok(())
}

fn list(cfg: &Configuration, tr: &Translator) -> String {
    let width = SETTINGS.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let mut out = String::from("Setting");
    out.push_str(&" ".repeat(width.saturating_sub(7) + 2));
    out.push_str("Value             What it does (accepted values)\n");
    for s in SETTINGS {
        let value = show(cfg, s);
        out.push_str(&format!(
            "{:width$}  {:<17} {} ({})\n",
            s.name,
            value,
            tr.i18n(&help_key(s)),
            accepts(s, tr),
            width = width
        ));
    }
    out.push_str("\n--get NAME shows one of these; --set NAME VALUE changes it.\n");
    out
}

/// Returns `Ok(true)` when a flag was handled and the process should exit.
pub fn handle(args: &[String]) -> Result<bool> {
    let Some(flag) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    if !matches!(flag, "--list-settings" | "--get" | "--set") {
        return Ok(false);
    }

    let env = Environment::create();
    let db = Arc::new(Database::open(&env).context("cannot open the settings database")?);
    db.migrate().context("cannot migrate the settings database")?;
    let cfg = Configuration::new(db);
    let tr = crate::load_translator(&env, &cfg);

    match flag {
        "--list-settings" => print!("{}", list(&cfg, &tr)),
        "--get" => {
            let name = args.get(1).map(String::as_str).context(usage(&tr))?;
            let s = find(name)
                .with_context(|| format!("unknown setting '{name}'\n\n{}", usage(&tr)))?;
            println!("{}", show(&cfg, s));
        }
        "--set" => {
            let (name, value) = match (args.get(1), args.get(2)) {
                (Some(n), Some(v)) => (n.as_str(), v.as_str()),
                _ => anyhow::bail!("{}", usage(&tr)),
            };
            let s = find(name)
                .with_context(|| format!("unknown setting '{name}'\n\n{}", usage(&tr)))?;
            set(&cfg, s, value, &tr)?;
            println!("{name} = {}", show(&cfg, s));
            println!("Applies the next time NanoTorrent starts.");
        }
        _ => unreachable!("guarded by the matches! above"),
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A setting whose description was never added to en-US shows up in
    /// `--help` as a humanised key ("Cli set web max body"), which is ugly but
    /// easy to miss. Catch it here instead.
    #[test]
    fn every_setting_has_an_english_description() {
        let english: serde_json::Value = serde_json::from_str(
            crate::ui::translator::EMBEDDED_LANGS
                .iter()
                .find(|(l, _)| *l == crate::DEFAULT_LOCALE)
                .expect("en-US is embedded")
                .1,
        )
        .expect("en-US parses");
        for s in SETTINGS {
            let key = help_key(s);
            assert!(
                english.get(&key).is_some(),
                "{} has no {key} string in en-US",
                s.name
            );
        }
    }

    #[test]
    fn every_name_is_unique_and_lookupable() {
        let mut names: Vec<&str> = SETTINGS.iter().map(|s| s.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate setting name");

        for s in SETTINGS {
            assert!(find(s.name).is_some(), "{} is not findable", s.name);
            // A name with an underscore would be a database key that leaked
            // into the CLI; the flags are all hyphenated.
            assert!(!s.name.contains('_'), "{} should use hyphens", s.name);
        }
    }

    #[test]
    fn only_the_table_less_kinds_may_have_an_empty_key() {
        for s in SETTINGS {
            let elsewhere = matches!(s.kind, Kind::ListenAddress | Kind::ListenPort);
            assert_eq!(
                s.key.is_empty(),
                elsewhere,
                "{} has the wrong key/kind pairing",
                s.name
            );
        }
    }

    #[test]
    fn the_proxy_names_are_in_the_stored_order() {
        // The stored value is the index, so reordering PROXY_TYPES would
        // silently repoint every existing configuration at another protocol.
        use crate::core::configuration::ConnectionProxyType as P;
        for (i, _) in PROXY_TYPES.iter().enumerate() {
            let expected = match i {
                0 => P::None,
                1 => P::Socks4,
                2 => P::Socks5,
                3 => P::Socks5Password,
                4 => P::Http,
                5 => P::HttpPassword,
                _ => unreachable!(),
            };
            assert_eq!(P::from_i64(i as i64), expected);
        }
    }
}

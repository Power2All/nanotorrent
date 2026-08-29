//! Maintenance flags for the web interface.
//!
//! These exist because on Linux and macOS there is no Preferences dialog to
//! turn the interface on or set its password, and a client nobody can reach is
//! not much use.
//!
//! They are handled *before* the single-instance IPC check in `main`. That
//! ordering is load-bearing: `ipc::init` forwards a second instance's argv to
//! the running one and exits, so a `--set-web-password` invocation would
//! otherwise be posted to the running window as if it were a torrent to open.

use std::io::{IsTerminal, Read};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::core::configuration::Configuration;
use crate::core::database::Database;
use crate::core::environment::Environment;
use crate::ui::translator::Translator;

/// The web-interface half of `--help`, in the configured language.
pub fn usage(tr: &Translator) -> String {
    format!(
        concat!(
            "{}\n",
            "\n",
            "  nanotorrent --webui on|off        {}\n",
            "  nanotorrent --set-web-password    {}\n",
            "  nanotorrent --webui-status        {}\n",
            "  nanotorrent --webui-set KEY VALUE {}\n",
            "\n",
            "{}\n",
            "\n",
            "  bind_address    {}\n",
            "  port            {}\n",
            "  username        {}\n",
            "  tls_mode        {}\n",
            "  tls_cert_path   {}\n",
            "  tls_key_path    {}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "\n",
            "  echo -n 'your password' | nanotorrent --set-web-password\n",
            "\n",
            "{}",
        ),
        tr.i18n("cli_web_header"),
        tr.i18n("cli_web_flag_onoff"),
        tr.i18n("cli_web_flag_password"),
        tr.i18n("cli_web_flag_status"),
        tr.i18n("cli_web_flag_set"),
        tr.i18n("cli_web_settings_header"),
        tr.i18n("cli_web_bind_address"),
        tr.i18n("cli_web_port"),
        tr.i18n("cli_web_username"),
        tr.i18n("cli_web_tls_mode"),
        tr.i18n("cli_web_cert"),
        tr.i18n("cli_web_key"),
        tr.i18n("cli_web_tls_off_note"),
        tr.i18n("cli_web_password_note"),
        tr.i18n("cli_applies_note"),
    )
}

/// Returns `Ok(true)` when a flag was handled and the process should exit.
pub fn handle(args: &[String]) -> Result<bool> {
    let Some(flag) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    if !matches!(
        flag,
        "--webui" | "--set-web-password" | "--webui-status" | "--webui-set"
    ) {
        return Ok(false);
    }

    let env = Environment::create();
    let db = Arc::new(Database::open(&env).context("cannot open the settings database")?);
    db.migrate().context("cannot migrate the settings database")?;
    let cfg = Configuration::new(db);
    let tr = crate::load_translator(&env, &cfg);

    match flag {
        "--webui" => {
            let state = args.get(1).map(String::as_str);
            let on = match state {
                Some("on") | Some("true") | Some("1") => true,
                Some("off") | Some("false") | Some("0") => false,
                _ => anyhow::bail!("{}", usage(&tr)),
            };
            cfg.set("webui.enabled", &on);

            if on && cfg.get_string("webui.password_hash").unwrap_or_default().is_empty() {
                // Enabling without a password produces a server that refuses to
                // listen, which looks like a bug unless you say so here.
                println!(
                    "Web interface enabled, but no password is set - it will not listen yet.\n\
                     Set one with:  nanotorrent --set-web-password"
                );
            } else {
                println!(
                    "Web interface {}. Restart NanoTorrent for it to take effect.",
                    if on { "enabled" } else { "disabled" }
                );
            }
        }

        "--set-web-password" => {
            let password = read_password()?;
            anyhow::ensure!(!password.is_empty(), "refusing to set an empty password");
            // No maximum, no character-class rules: length is what matters and
            // arbitrary rules only push people towards weaker, memorable ones.
            anyhow::ensure!(
                password.chars().count() >= 8,
                "password must be at least 8 characters"
            );

            let hash = super::Credentials::hash_password(&password)?;
            cfg.set("webui.password_hash", &hash);
            println!("Web interface password updated. Restart NanoTorrent for it to take effect.");
        }

        "--webui-set" => {
            let (Some(key), Some(value)) = (args.get(1), args.get(2)) else {
                anyhow::bail!("{}", usage(&tr));
            };
            set_setting(&cfg, key, value, &tr)?;
            println!("webui.{key} = {value}. Restart NanoTorrent for it to take effect.");
        }

        "--webui-status" => {
            let wc = super::WebConfig::load(&cfg);
            println!("enabled      : {}", wc.enabled);
            println!("bind address : {}", wc.bind_address);
            println!("port         : {}", wc.port);
            println!("username     : {}", wc.username);
            println!(
                "password     : {}",
                if wc.password_hash.is_empty() { "NOT SET" } else { "set" }
            );
            println!("tls mode     : {:?}", wc.tls);
        }

        _ => unreachable!("guarded by the matches! above"),
    }

    Ok(true)
}

/// Whitelisted rather than a generic key/value setter over the settings table.
/// A typo would otherwise write a key nothing ever reads and report success,
/// which is the worst possible outcome for a security-relevant setting.
pub(crate) fn set_setting(
    cfg: &Configuration,
    key: &str,
    value: &str,
    tr: &Translator,
) -> Result<()> {
    match key {
        "port" => {
            let port: i64 = value
                .parse()
                .ok()
                .filter(|p| (1..=65535).contains(p))
                .context("port must be a number between 1 and 65535")?;
            cfg.set("webui.port", &port);
        }
        "tls_mode" => {
            anyhow::ensure!(
                matches!(value, "self-signed" | "custom" | "off"),
                "tls_mode must be one of: self-signed, custom, off"
            );
            if value == "off" {
                // Not refused here - it is legitimate on loopback, and startup
                // is where the bind address is known. Say so now rather than
                // letting it fail confusingly later.
                println!(
                    "Note: with tls_mode=off the interface will only start on 127.0.0.1."
                );
            }
            cfg.set("webui.tls_mode", &value);
        }
        "bind_address" => {
            anyhow::ensure!(!value.trim().is_empty(), "bind_address must not be empty");
            if value != "127.0.0.1" && value != "::1" {
                println!(
                    "Note: {value} is reachable from outside this machine. \
                     Make sure the password is one you are happy exposing."
                );
            }
            cfg.set("webui.bind_address", &value);
        }
        "username" => {
            anyhow::ensure!(!value.trim().is_empty(), "username must not be empty");
            cfg.set("webui.username", &value);
        }
        "tls_cert_path" | "tls_key_path" => {
            // Checked now rather than at startup, where a typo would only show
            // up as the interface silently not coming back after a restart.
            anyhow::ensure!(
                std::path::Path::new(value).is_file(),
                "{value} is not an existing file"
            );
            cfg.set(&format!("webui.{key}"), &value);
        }
        _ => anyhow::bail!("unknown setting '{key}'\n\n{}", usage(tr)),
    }
    Ok(())
}

/// Read a password from stdin.
///
/// From stdin rather than an argument so it never lands in shell history or
/// in another user's `ps` output. Works piped as well as typed, which is what
/// makes the `echo -n ... |` form in the usage text possible.
fn read_password() -> Result<String> {
    let mut stdin = std::io::stdin();

    if stdin.is_terminal() {
        // Suppressing terminal echo means termios on Unix and SetConsoleMode on
        // Windows - a dependency's worth of code for a path that piping avoids
        // entirely. Say so instead of pretending the input is hidden.
        println!("Enter a new web interface password (it will be visible as you type):");
        println!("To avoid that, pipe it instead:  echo -n 'password' | nanotorrent --set-web-password");
    }

    let mut buf = String::new();
    stdin
        .read_to_string(&mut buf)
        .context("could not read the password from stdin")?;

    // Trim only the line ending, not meaningful leading/trailing spaces - a
    // password is whatever the user typed.
    Ok(buf
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string())
}

#[cfg(test)]
mod tests {
    /// The flag list here and the one `handle` matches on must not drift, or a
    /// documented flag silently falls through to being treated as a torrent
    /// path by the normal startup route.
    #[test]
    fn every_documented_flag_is_handled() {
        // Rendered in English: the flags are English in every language, and an
        // embedded locale needs no files on disk.
        let tr = crate::ui::translator::Translator::load(
            std::path::Path::new(""),
            crate::DEFAULT_LOCALE,
        );
        let usage = super::usage(&tr);
        for line in usage.lines() {
            for word in line.split_whitespace() {
                if let Some(flag) = word.strip_prefix("--") {
                    let flag = format!("--{}", flag.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-'));
                    assert!(
                        matches!(
                            flag.as_str(),
                            "--webui"
                                | "--set-web-password"
                                | "--webui-status"
                                | "--webui-set"
                        ),
                        "the usage text mentions {flag} but handle() does not match it"
                    );
                }
            }
        }
    }
}

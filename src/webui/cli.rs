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
            "  nanotorrent --plugins on|off      {}\n",
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
        tr.i18n("cli_plugins_header"),
        tr.i18n("cli_plugins_flag_onoff"),
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
        "--webui" | "--set-web-password" | "--webui-status" | "--webui-set" | "--plugins"
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
                println!("{}", tr.i18n("cli_web_needs_password"));
            } else {
                let key = if on { "cli_web_turned_on" } else { "cli_web_turned_off" };
                println!("{}", tr.i18n(key));
                println!("{}", tr.i18n("cli_applies_note"));
            }
        }

        "--plugins" => {
            let on = match args.get(1).map(String::as_str) {
                Some("on") | Some("true") | Some("1") => true,
                Some("off") | Some("false") | Some("0") => false,
                _ => anyhow::bail!("{}", usage(&tr)),
            };
            cfg.set(crate::plugins::ENABLED_KEY, &on);

            if on {
                // Enabling with an empty folder is silent otherwise, which
                // looks identical to the host failing to start.
                let dir = crate::plugins::plugin_dir(&env);
                println!(
                    "{}",
                    tr.i18n1("cli_plugins_turned_on", &dir.display().to_string())
                );
                println!("{}", tr.i18n("cli_applies_note"));
            } else {
                println!("{}", tr.i18n("cli_plugins_turned_off"));
                println!("{}", tr.i18n("cli_applies_note"));
            }
        }

        "--set-web-password" => {
            let password = read_password(&tr)?;
            anyhow::ensure!(!password.is_empty(), "refusing to set an empty password");
            // No maximum, no character-class rules: length is what matters and
            // arbitrary rules only push people towards weaker, memorable ones.
            anyhow::ensure!(
                password.chars().count() >= 8,
                "password must be at least 8 characters"
            );

            let hash = super::Credentials::hash_password(&password)?;
            cfg.set("webui.password_hash", &hash);
            println!("{}", tr.i18n("cli_password_updated"));
            println!("{}", tr.i18n("cli_applies_note"));
        }

        "--webui-set" => {
            let (Some(key), Some(value)) = (args.get(1), args.get(2)) else {
                anyhow::bail!("{}", usage(&tr));
            };
            set_setting(&cfg, key, value, &tr)?;
            println!("webui.{key} = {value}");
            println!("{}", tr.i18n("cli_applies_note"));
        }

        "--webui-status" => {
            let wc = super::WebConfig::load(&cfg);
            // Deliberately unaligned. The labels used to be fixed English, so
            // padding them into a column was free; translated they are not, and
            // there is no correct way to pad without display widths - counting
            // chars puts the colon in the wrong place for CJK, where one
            // character occupies two columns. unicode-width would fix it and is
            // only in the tree via slint-build, so it would be a new dependency
            // for the headless build, which is a lot to pay for a straight edge.
            let rows = [
                (tr.i18n("enabled"), wc.enabled.to_string()),
                (tr.i18n("bind_address"), wc.bind_address.clone()),
                (tr.i18n("port"), wc.port.to_string()),
                (tr.i18n("username"), wc.username.clone()),
                (
                    tr.i18n("password"),
                    tr.i18n(if wc.password_hash.is_empty() { "cli_not_set" } else { "cli_is_set" }),
                ),
                (tr.i18n("tls_mode"), format!("{:?}", wc.tls)),
            ];
            for (label, value) in &rows {
                println!("{label}: {value}");
            }
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
        _ => anyhow::bail!("{}\n\n{}", tr.i18n1("cli_unknown_setting", key), usage(tr)),
    }
    Ok(())
}

/// Read a password from stdin.
///
/// From stdin rather than an argument so it never lands in shell history or
/// in another user's `ps` output. Works piped as well as typed, which is what
/// makes the `echo -n ... |` form in the usage text possible.
fn read_password(tr: &Translator) -> Result<String> {
    let mut stdin = std::io::stdin();

    if stdin.is_terminal() {
        // Suppressing terminal echo means termios on Unix and SetConsoleMode on
        // Windows - a dependency's worth of code for a path that piping avoids
        // entirely. Say so instead of pretending the input is hidden.
        println!("{}", tr.i18n("cli_password_prompt"));
        println!("{}", tr.i18n("cli_password_pipe_hint"));
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
                                | "--plugins"
                        ),
                        "the usage text mentions {flag} but handle() does not match it"
                    );
                }
            }
        }
    }
}

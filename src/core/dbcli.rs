//! Database maintenance from the command line: encryption, and settings in
//! and out as JSON.
//!
//! These are maintenance flags rather than settings, for the same reason the
//! web interface's are (see [`crate::webui::cli`]): each one rewrites a file on
//! disk, so it is an action taken once, not a value that sits in the settings
//! table. There is no `database.encryption` key to set - and there could not
//! be, since it would live inside the database it describes.
//!
//! They are handled *before* the single-instance IPC check in `main`, like the
//! web ones, or a `--encrypt-database` would be posted to the running window as
//! if it were a torrent to open.
//!
//! That ordering is also why the flags that write have to check for a running
//! instance themselves. Running before the IPC hand-off means nothing else is
//! going to stop them, and a NanoTorrent that is already open holds its own
//! copy of the database - in memory, when it is encrypted - which it will write
//! back over anything done here.

use std::sync::Arc;

use anyhow::{Context, Result};

use super::configuration::Configuration;
use crate::ui::translator::Translator;

use super::database::Database;
use super::dbkey;
use super::environment::Environment;

pub fn usage(tr: &Translator) -> String {
    format!(
        "{}\n  \
         nanotorrent --database-status      {}\n  \
         nanotorrent --encrypt-database     {}\n  \
         nanotorrent --decrypt-database     {}\n  \
         nanotorrent --export-settings FILE {}\n  \
         nanotorrent --import-settings FILE {}\n\n\
         {}\n\n\
         {}\n\n\
         {}\n",
        tr.i18n("cli_db_header"),
        tr.i18n("cli_db_flag_status"),
        tr.i18n("cli_db_flag_encrypt"),
        tr.i18n("cli_db_flag_decrypt"),
        tr.i18n("cli_db_flag_export"),
        tr.i18n("cli_db_flag_import"),
        tr.i18n("database_explain"),
        tr.i18n("cli_db_export_note"),
        tr.i18n("cli_db_writes_note"),
    )
}

/// Returns `true` when a flag was handled and the process should stop.
pub fn handle(args: &[String]) -> Result<bool> {
    let Some(flag) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    if !matches!(
        flag,
        "--database-status"
            | "--encrypt-database"
            | "--decrypt-database"
            | "--export-settings"
            | "--import-settings"
    ) {
        return Ok(false);
    }

    // Everything below except the two read-only flags rewrites the database. A
    // running NanoTorrent holds its own copy - in memory, when it is encrypted -
    // and would write it back over whatever happened here, so the change would
    // vanish without anything having gone visibly wrong.
    let writes = !matches!(flag, "--database-status" | "--export-settings");
    if writes && crate::ipc::another_instance_running() {
        anyhow::bail!("NanoTorrent is running. Close it first - this rewrites the database it has open.");
    }

    let env = Environment::create();
    let path = env.get_database_file_path();
    let key_file = dbkey::key_path(&env);
    // Asked of the open database rather than of the key file, so the answer is
    // about the database that exists rather than about a key beside it.
    let db = open(&env)?;
    let encrypted = db.is_encrypted();
    let tr = crate::load_translator(&env, &Configuration::new(db.clone()));

    match flag {
        "--database-status" => {
            println!("{}: {}", tr.i18n("database"), path.display());
            if encrypted {
                // The cipher name is not translated - it is the algorithm's name.
                println!(
                    "{}: {} (XChaCha20-Poly1305)",
                    tr.i18n("database_status"),
                    tr.i18n("database_on")
                );
                println!("{}: {}", tr.i18n("database_key_file"), key_file.display());
                #[cfg(windows)]
                println!("{}", tr.i18n("cli_db_key_windows"));
                #[cfg(not(windows))]
                println!("{}", tr.i18n("cli_db_key_unix"));
            } else {
                println!("{}: {}", tr.i18n("database_status"), tr.i18n("database_off"));
                println!("{}", tr.i18n("cli_db_turn_on_hint"));
            }
        }

        "--encrypt-database" => {
            if encrypted {
                println!("{}", tr.i18n("cli_db_already_encrypted"));
                return Ok(true);
            }
            db.set_encryption(&env, true)
                .context("the database was left unchanged")?;
            remember_asked(&db);
            println!("{}", tr.i18n("cli_db_encrypted"));
            println!("{}: {}", tr.i18n("database_key_file"), key_file.display());
            println!("\n{}", tr.i18n("database_backup_key"));
        }

        "--decrypt-database" => {
            if !encrypted {
                println!("{}", tr.i18n("cli_db_not_encrypted"));
                return Ok(true);
            }
            db.set_encryption(&env, false)
                .context("the database was left encrypted")?;
            remember_asked(&db);
            println!("{}", tr.i18n("cli_db_decrypted"));
        }

        "--export-settings" => {
            let target = file_arg(args, flag)?;
            let json = super::dbexport::export(&db)?;
            std::fs::write(&target, json)
                .with_context(|| format!("writing {}", target.display()))?;
            println!(
                "{}",
                tr.i18n1("cli_db_settings_written", &target.display().to_string())
            );
            println!("{}", tr.i18n("cli_db_export_omits"));
        }

        "--import-settings" => {
            let source = file_arg(args, flag)?;
            let json = std::fs::read_to_string(&source)
                .with_context(|| format!("reading {}", source.display()))?;
            let report = super::dbexport::import(&db, &json)?;
            println!("{}", report.summary(&tr));
        }

        _ => unreachable!("flag list and match arms disagree"),
    }

    Ok(true)
}

/// The path after a flag that takes one.
fn file_arg(args: &[String], flag: &str) -> Result<std::path::PathBuf> {
    match args.get(1) {
        Some(path) if !path.starts_with('-') => Ok(std::path::PathBuf::from(path)),
        _ => anyhow::bail!("{flag} needs a file name"),
    }
}

fn open(env: &Environment) -> Result<Arc<Database>> {
    let db = Arc::new(Database::open(env).context("cannot open the settings database")?);
    db.migrate().context("cannot migrate the settings database")?;
    Ok(db)
}

/// Someone who has answered this from the command line should not then be
/// asked again by the window.
fn remember_asked(db: &Arc<Database>) {
    Configuration::new(db.clone()).set("database.encryption_prompted", &true);
}

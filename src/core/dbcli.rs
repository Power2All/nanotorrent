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
use super::database::Database;
use super::dbkey;
use super::environment::Environment;

pub fn usage() -> String {
    String::from(
        "Usage:\n  \
         nanotorrent --database-status      show whether the settings database is encrypted\n  \
         nanotorrent --encrypt-database     encrypt it, unlocking automatically from now on\n  \
         nanotorrent --decrypt-database     write it back out in the clear\n  \
         nanotorrent --export-settings FILE write the settings to a JSON file\n  \
         nanotorrent --import-settings FILE read them back from one\n\n\
         Encryption protects the database if the file leaves this machine - a backup, a\n\
         copied profile, another account on the same computer - and any change made\n\
         without the key is detected rather than merely hidden. It cannot protect against\n\
         a program already running as you, which can read the key exactly as NanoTorrent\n\
         does.\n\n\
         An export carries the settings you have changed, plus your labels and filters.\n\
         It deliberately leaves out the proxy password, the web interface password and\n\
         the permissions you granted each plugin: a plain file is the wrong home for the\n\
         first two, and a settings file that could grant plugin permissions would undo\n\
         the reason the database can be encrypted at all.\n\n\
         Everything except --database-status and --export-settings writes to the\n\
         database, so close NanoTorrent first.\n",
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

    match flag {
        "--database-status" => {
            println!("Database: {}", path.display());
            if encrypted {
                println!("Encrypted: yes (XChaCha20-Poly1305, unlocked automatically)");
                println!("Key file:  {}", key_file.display());
                #[cfg(windows)]
                println!(
                    "The key is sealed to this Windows account, so the database cannot be\n\
                     opened by another user or on another machine."
                );
                #[cfg(not(windows))]
                println!(
                    "The key is stored with owner-only permissions. Copying the profile\n\
                     directory copies the key with it, so it guards against other accounts\n\
                     rather than against someone taking the whole folder."
                );
            } else {
                println!("Encrypted: no");
                println!("Turn it on with:  nanotorrent --encrypt-database");
            }
        }

        "--encrypt-database" => {
            if encrypted {
                println!("The database is already encrypted. Nothing to do.");
                return Ok(true);
            }
            db.set_encryption(&env, true)
                .context("the database was left unchanged")?;
            remember_asked(&db);
            println!("Database encrypted. It unlocks automatically from now on.");
            println!("Key file: {}", key_file.display());
            println!(
                "\nBack this file up with the database, not separately from it: without the\n\
                 key the database cannot be recovered by anyone, including you."
            );
        }

        "--decrypt-database" => {
            if !encrypted {
                println!("The database is not encrypted. Nothing to do.");
                return Ok(true);
            }
            db.set_encryption(&env, false)
                .context("the database was left encrypted")?;
            remember_asked(&db);
            println!("Database decrypted. It is now a plain SQLite file.");
        }

        "--export-settings" => {
            let target = file_arg(args, flag)?;
            let json = super::dbexport::export(&db)?;
            std::fs::write(&target, json)
                .with_context(|| format!("writing {}", target.display()))?;
            println!("Settings written to {}", target.display());
            println!(
                "The proxy password, the web interface password and your plugin \
                 permissions are not in this file."
            );
        }

        "--import-settings" => {
            let source = file_arg(args, flag)?;
            let json = std::fs::read_to_string(&source)
                .with_context(|| format!("reading {}", source.display()))?;
            let report = super::dbexport::import(&db, &json)?;
            println!("{}", report.summary());
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

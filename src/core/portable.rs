//! Portable mode: the profile kept beside the program instead of in the user's
//! profile folder.
//!
//! [`Environment`] decides whether a copy IS portable, from a marker file beside
//! the program. This is the rest of it: `--portable`, which writes that marker,
//! and the one-time offer to bring an existing profile into a new portable copy.
//!
//! The marker is the switch, not the flag. A flag that only applied to the run
//! it was given on would be lost on every launch that does not come from the
//! user's own shortcut - a magnet clicked in a browser, a .torrent opened from
//! Explorer, the autostart entry, a second instance handing over its arguments -
//! and each of those would quietly open the ordinary profile instead. A file
//! beside the program is seen by all of them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::core::dbkey;
use crate::core::environment::{Environment, PORTABLE_MARKERS, package_family_name};

/// The marker `--portable` writes. Plain text, so whoever finds it in the
/// folder can tell what it is for and how to undo it.
const MARKER_TEXT: &str = "This file makes NanoTorrent portable: its settings, torrents and logs are\r\n\
kept in this folder instead of in your user profile.\r\n\
\r\n\
Delete this file to go back. The profile in this folder is left as it is.\r\n";

/// What `--portable` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Switched {
    /// This copy was not portable and is now. Carries where the profile goes.
    Now(PathBuf),
    /// It already was; nothing was written.
    Already(PathBuf),
}

/// `--portable`: make this copy portable from now on, by writing the marker.
///
/// Refuses, with a reason, rather than falling back: a copy that was asked to
/// be portable and silently kept using the user profile is worse than one that
/// says it cannot.
pub fn enable() -> Result<Switched> {
    let root = Environment::portable_root();
    if PORTABLE_MARKERS.iter().any(|m| root.join(m).exists()) {
        return Ok(Switched::Already(root));
    }

    // Checked before trying to write, so the reason given is the real one: a
    // Store install's folder is read-only by design, and "access denied" would
    // send someone looking for a permissions problem that is not theirs.
    if package_family_name().is_some() {
        bail!(
            "the Microsoft Store version of NanoTorrent cannot be portable: Windows installs \
             it in a folder no program may write to, and keeps its settings for it.\n\n\
             For a portable copy, use the installer or the release archive from \
             https://www.nanotorrent.org and put it in a folder you can write to."
        );
    }

    write_marker(&root)?;
    Ok(Switched::Now(root))
}

/// Write the marker into `root`, explaining a failure in terms of what to do.
fn write_marker(root: &Path) -> Result<()> {
    // One message rather than a context chain: this ends up in a message box,
    // where "reason: explanation: os error" reads backwards.
    const NOT_HERE: &str = if cfg!(windows) {
        "A folder under Program Files is not one."
    } else {
        "A system folder such as /usr or /opt is not one."
    };
    let marker = root.join(PORTABLE_MARKERS[0]);
    std::fs::write(&marker, MARKER_TEXT).map_err(|err| {
        anyhow::anyhow!(
            "portable mode could not be switched on: writing {} failed ({err}).\n\n\
             A portable copy keeps its settings, torrents and logs in its own folder, so \
             that has to be a folder it can write to. {NOT_HERE} Copy NanoTorrent \
             somewhere that is - a USB stick, or a folder in your documents - and run it \
             with --portable from there.",
            marker.display(),
        )
    })
}

/// The ordinary profile worth offering to a new portable copy, if there is
/// one: this copy is portable, has no profile of its own yet, and a profile
/// exists in the per-user folder.
///
/// "No profile of its own yet" is also what keeps the question from coming
/// back. Either answer ends with a database here - a copied one, or the fresh
/// one the first start creates - so it is asked exactly once.
pub fn copy_candidate(env: &Environment) -> Option<PathBuf> {
    if !env.is_portable() || env.get_database_file_path().exists() {
        return None;
    }
    let from = Environment::user_data_dir()?;
    if !from.join(DATABASE_FILE).exists() || same_folder(&from, &env.get_application_data_path()) {
        return None;
    }
    Some(from)
}

/// The settings database's name, in any profile.
const DATABASE_FILE: &str = "NanoTorrent.sqlite";

/// Not copied: the database and its key are handled apart (see [`Staged`]),
/// logs belong to the copy that wrote them, and the markers and SQLite's own
/// side files would be wrong or stale in a new place.
fn skipped(name: &str) -> bool {
    name == "logs"
        || name == DATABASE_FILE
        || name == dbkey::KEY_FILE
        || name.starts_with("NanoTorrent.sqlite-")
        || name.ends_with(".tmp")
        || PORTABLE_MARKERS.contains(&name)
}

fn same_folder(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The ordinary profile's database, read out as a plain SQLite file and kept
/// in the temp folder until the user has answered.
///
/// Read before asking, for two reasons: the question should be in the user's
/// own language, which is a setting in that very database; and a profile that
/// cannot be read at all should be found out before anyone is offered it.
///
/// Always plain, even when the original is encrypted. An encrypted profile's
/// key is bound to this Windows account (DPAPI), so a copy that stayed
/// encrypted would open here and nowhere else - the opposite of portable.
pub struct Staged {
    source: PathBuf,
    database: PathBuf,
}

impl Staged {
    pub fn new(source: &Path) -> Result<Staged> {
        let from = source.join(DATABASE_FILE);
        let database = std::env::temp_dir().join(format!(
            "nanotorrent-portable-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&database);
        let staged = Staged {
            source: source.to_path_buf(),
            database,
        };

        if crate::core::database::file_is_sealed(&from) {
            let key = dbkey::load_from(source)?
                .context("that profile is encrypted and its key file is missing")?;
            let sealed =
                std::fs::read(&from).with_context(|| format!("reading {}", from.display()))?;
            let plain = dbkey::unseal(&key, &sealed)?;
            std::fs::write(&staged.database, plain)
                .with_context(|| format!("writing {}", staged.database.display()))?;
        } else {
            // Read-only, and a copy made BY SQLite rather than of the file's
            // bytes: another NanoTorrent may have this database open, and
            // VACUUM INTO reads one consistent snapshot of it where a file copy
            // could catch a write halfway.
            let conn = rusqlite::Connection::open_with_flags(
                &from,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .with_context(|| format!("opening {}", from.display()))?;
            conn.execute(
                "VACUUM INTO ?1",
                [staged.database.to_string_lossy().as_ref()],
            )
            .with_context(|| format!("copying {}", from.display()))?;
        }
        Ok(staged)
    }

    /// The folder the profile is being copied from.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// The language that profile was using, so the question can be asked in
    /// it. `None` means English, as it does everywhere else.
    pub fn locale(&self) -> Option<String> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        let raw: String = conn
            .query_row(
                "SELECT IFNULL(value, default_value) FROM setting WHERE key = 'locale_name'",
                [],
                |row| row.get(0),
            )
            .ok()?;
        // Stored as JSON, like every setting.
        serde_json::from_str::<String>(&raw)
            .ok()
            .filter(|l| !l.is_empty())
    }

    /// Copy the profile into this portable copy's folder.
    ///
    /// Everything else in the profile first - session state, DHT table,
    /// plugins, certificates - and the database last, because the database is
    /// what [`copy_candidate`] looks for: a copy that stopped partway leaves no
    /// database, so the next start offers the copy again instead of starting
    /// on half a profile. Nothing already in the destination is overwritten.
    pub fn install(self, env: &Environment) -> Result<usize> {
        let dest = env.get_application_data_path();
        let mut copied = 0;
        copy_tree(&self.source, &dest, true, &mut copied)?;
        std::fs::copy(&self.database, env.get_database_file_path())
            .with_context(|| format!("writing {}", env.get_database_file_path().display()))?;
        Ok(copied + 1)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.database);
    }
}

/// Copy `from` into `to`, leaving anything already in `to` alone. `top` is
/// true for the profile folder itself, the only level [`skipped`] applies to.
fn copy_tree(from: &Path, to: &Path, top: bool, copied: &mut usize) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let entries = std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if top && skipped(&name.to_string_lossy()) {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target, false, copied)?;
        } else if kind.is_file() && !target.exists() {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nanotorrent-portable-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The marker is written where Environment looks for it, and a folder that
    /// cannot take it is reported as that - naming the folder - rather than
    /// being left to fail somewhere later.
    #[test]
    fn the_marker_lands_where_it_is_looked_for() {
        let dir = scratch("marker");
        write_marker(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join(PORTABLE_MARKERS[0])).unwrap();
        assert!(text.contains("Delete this file"));

        let missing = dir.join("does-not-exist");
        let err = write_marker(&missing).unwrap_err().to_string();
        assert!(err.contains("does-not-exist"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only the profile folder's own top level is filtered: a `logs` folder
    /// inside a plugin's folder is that plugin's business.
    #[test]
    fn the_copy_skips_what_belongs_to_the_old_copy() {
        let from = scratch("from");
        let to = scratch("to");
        for (path, body) in [
            ("NanoTorrent.sqlite", "db"),
            ("NanoTorrent.key", "key"),
            ("NanoTorrent.sqlite-journal", "j"),
            ("portable.txt", "m"),
            ("logs/NanoTorrent.1.log", "log"),
            ("dht.json", "dht"),
            ("session/session.json", "s"),
            ("plugins/feed/logs/keep.txt", "k"),
        ] {
            let p = from.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        // Already in the destination: must survive.
        std::fs::write(to.join("dht.json"), "mine").unwrap();

        let mut copied = 0;
        copy_tree(&from, &to, true, &mut copied).unwrap();

        assert!(to.join("session/session.json").exists());
        assert!(to.join("plugins/feed/logs/keep.txt").exists());
        for gone in ["NanoTorrent.sqlite", "NanoTorrent.key", "NanoTorrent.sqlite-journal", "portable.txt", "logs"] {
            assert!(!to.join(gone).exists(), "{gone} was copied");
        }
        assert_eq!(std::fs::read_to_string(to.join("dht.json")).unwrap(), "mine");
        assert_eq!(copied, 2);
        let _ = std::fs::remove_dir_all(&from);
        let _ = std::fs::remove_dir_all(&to);
    }

    /// A plain profile is staged by SQLite into a real database, and its
    /// language read back out of it - the setting is stored as JSON.
    #[test]
    fn a_plain_profile_stages_with_its_language() {
        let from = scratch("stage");
        {
            let conn = rusqlite::Connection::open(from.join(DATABASE_FILE)).unwrap();
            conn.execute_batch(
                "CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT, default_value TEXT);
                 INSERT INTO setting VALUES ('locale_name', '\"de-DE\"', NULL);",
            )
            .unwrap();
        }
        let staged = Staged::new(&from).unwrap();
        assert_eq!(staged.locale().as_deref(), Some("de-DE"));
        let path = staged.database.clone();
        assert!(path.exists());
        drop(staged);
        assert!(!path.exists(), "the staged copy outlived its use");
        let _ = std::fs::remove_dir_all(&from);
    }
}

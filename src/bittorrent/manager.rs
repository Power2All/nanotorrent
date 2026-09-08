//! Housekeeping that runs on its own thread: the watched folder, and moving a
//! download once it has finished.
//!
//! # Why a thread rather than a task
//!
//! Both jobs use [`Session`]'s synchronous API (`add_torrent`, `move_storage`)
//! and those use `block_on` internally. Calling them from a task on the
//! session's own tokio runtime would panic, so this runs beside it instead.
//!
//! The upside is that this module needs nothing private: it holds an
//! `Arc<Session>` and a `Configuration` and calls the same methods the UI does,
//! which is why it can live outside `session.rs` at all.
//!
//! It holds a [`Weak`] reference, so shutting the session down ends the thread
//! rather than keeping it alive to poll a corpse.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use crate::core::configuration::Configuration;

use super::session::{AddParams, AddTorrentSource, Session, SessionEvent};

/// How often the watched folder is looked at. Slow on purpose: this is a
/// directory listing, and nobody notices two seconds when dropping a file in.
const TICK: std::time::Duration = std::time::Duration::from_secs(2);

/// What a handled `.torrent` is renamed to, so the next scan ignores it.
///
/// Renamed rather than deleted. The file is the user's, and a scanner that eats
/// its input is one wrong path away from clearing out a folder somebody was
/// keeping - see [`aside`].
const HANDLED: &str = "added";

/// Start the housekeeping thread.
pub fn spawn(session: &Arc<Session>, cfg: Arc<Configuration>) {
    let weak = Arc::downgrade(session);
    let events = session.subscribe();

    let started = std::thread::Builder::new()
        .name(String::from("nt-manager"))
        .spawn(move || run(weak, cfg, events));

    if let Err(err) = started {
        // Not fatal: the client works, it just will not watch a folder or move
        // finished downloads. Saying so beats both failing to start and going
        // quiet about a feature that is switched on.
        tracing::error!("could not start the housekeeping thread: {err}");
    }
}

fn run(
    session: Weak<Session>,
    cfg: Arc<Configuration>,
    events: std::sync::mpsc::Receiver<SessionEvent>,
) {
    loop {
        std::thread::sleep(TICK);
        let Some(session) = session.upgrade() else {
            return;
        };

        // Drained every tick whether or not anything is configured: an
        // undrained channel grows for the life of the process.
        for event in events.try_iter() {
            if let SessionEvent::TorrentCompleted { hash, name } = event {
                move_finished(&session, &cfg, &hash, &name);
            }
        }

        scan(&session, &cfg);
    }
}

/// Move a finished torrent to wherever it belongs now.
///
/// Two settings can ask for a move and they collapse into one destination:
/// "move completed downloads" names a folder outright, and the incomplete
/// folder means anything sitting in it has finished its stay. A torrent already
/// in the right place is left alone, which is also what makes this safe to run
/// on every completion.
fn move_finished(session: &Arc<Session>, cfg: &Configuration, hash: &str, name: &str) {
    let Some(current) = session
        .torrents(&Default::default())
        .into_iter()
        .find(|t| t.info_hash == hash)
        .map(|t| t.save_path)
    else {
        return;
    };

    let Some(destination) = destination(cfg, &current) else {
        return;
    };
    if same_folder(&current, &destination) {
        return;
    }

    tracing::info!("{name} finished; moving it to {destination}");
    session.move_storage(hash, &destination);
}

/// Where a finished torrent currently sitting in `current` should end up, or
/// `None` to leave it be.
fn destination(cfg: &Configuration, current: &str) -> Option<String> {
    if cfg.get_bool("move_completed_downloads") {
        let target = folder(cfg, "move_completed_downloads_path");
        if target.is_some() {
            return target;
        }
        // Switched on with nowhere to put them. Better to do nothing and say so
        // than to invent a destination.
        tracing::warn!("move completed downloads is on but no folder is set");
    }

    // Otherwise the only reason to move is that it is still in the incomplete
    // folder, which is a staging area rather than a home.
    let incomplete = cfg
        .get_bool("downloads.incomplete_enabled")
        .then(|| folder(cfg, "downloads.incomplete_path"))
        .flatten()?;
    same_folder(current, &incomplete)
        .then(|| folder(cfg, "default_save_path"))
        .flatten()
}

/// A configured folder, or `None` when it is unset or blank.
fn folder(cfg: &Configuration, key: &str) -> Option<String> {
    cfg.get_string(key).filter(|p| !p.is_empty())
}

/// Are these the same folder?
///
/// Compared through the filesystem where possible, so `D:\dl` and `D:\dl\` -
/// and on Windows `d:\DL` - are one place rather than three. Falls back to a
/// string compare for paths that do not exist yet, which is the honest answer
/// when there is nothing to canonicalise.
fn same_folder(a: &str, b: &str) -> bool {
    match (
        std::fs::canonicalize(Path::new(a)),
        std::fs::canonicalize(Path::new(b)),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Add anything new in the watched folder.
fn scan(session: &Arc<Session>, cfg: &Configuration) {
    if !cfg.get_bool("watch.enabled") {
        return;
    }
    let Some(dir) = folder(cfg, "watch.path") else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // A folder that has gone away (an unplugged drive, a typo) is not worth
        // a message every two seconds.
        return;
    };

    let start = cfg.get_bool("watch.start");
    let label = cfg.get_int("watch.label_id").filter(|id| *id >= 0);

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_torrent(&path) {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // Still being written, most likely. Left for the next pass rather
            // than renamed, so a half-copied file gets another chance.
            Err(err) => {
                tracing::debug!("cannot read {} yet: {err}", path.display());
                continue;
            }
        };

        // Renamed BEFORE the add, not after: adding is asynchronous, so waiting
        // for it to report back would mean the next tick - two seconds later -
        // finding the same file and adding it again.
        if !aside(&path) {
            continue;
        }

        tracing::info!("adding {} from the watched folder", path.display());
        session.add_torrent(
            AddTorrentSource::TorrentFileBytes(bytes),
            AddParams {
                save_path: None,
                start_torrent: start,
                only_files: None,
                label_id: label.map(|id| id as i32),
            },
        );
    }
}

fn is_torrent(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}

/// Rename a handled file out of the way, returning whether it worked.
///
/// A failure here means the add is skipped: adding a torrent this loop would
/// find again on the next pass is worse than not adding it at all.
fn aside(path: &Path) -> bool {
    let mut target = path.to_path_buf();
    target.as_mut_os_string().push(".");
    target.as_mut_os_string().push(HANDLED);

    // Somebody dropped the same file in twice. Number it rather than clobbering
    // the earlier one, which might not be identical.
    let target = unique(target);

    match std::fs::rename(path, &target) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("cannot set {} aside, skipping it: {err}", path.display());
            false
        }
    }
}

fn unique(candidate: PathBuf) -> PathBuf {
    if !candidate.exists() {
        return candidate;
    }
    let mut taken = HashSet::new();
    for n in 2.. {
        let numbered = PathBuf::from(format!("{}.{n}", candidate.display()));
        if !numbered.exists() && taken.insert(numbered.clone()) {
            return numbered;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn cfg() -> Configuration {
        let db = Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        Configuration::new(db)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nt-mgr-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn nothing_configured_means_no_move() {
        assert_eq!(destination(&cfg(), "D:\\downloads"), None);
    }

    #[test]
    fn move_completed_wins_wherever_the_torrent_is() {
        let cfg = cfg();
        cfg.set("move_completed_downloads", &true);
        cfg.set("move_completed_downloads_path", &"D:\\done");
        assert_eq!(
            destination(&cfg, "D:\\downloads"),
            Some(String::from("D:\\done"))
        );
    }

    /// Switched on with no folder must not invent one.
    #[test]
    fn move_completed_without_a_folder_does_nothing() {
        let cfg = cfg();
        cfg.set("move_completed_downloads", &true);
        cfg.set("move_completed_downloads_path", &"");
        assert_eq!(destination(&cfg, "D:\\downloads"), None);
    }

    /// A torrent in the incomplete folder graduates to the save path; one that
    /// is already elsewhere is left where it is.
    #[test]
    fn the_incomplete_folder_only_releases_what_is_in_it() {
        let cfg = cfg();
        cfg.set("downloads.incomplete_enabled", &true);
        cfg.set("downloads.incomplete_path", &"D:\\part");
        cfg.set("default_save_path", &"D:\\done");

        assert_eq!(
            destination(&cfg, "D:\\part"),
            Some(String::from("D:\\done"))
        );
        assert_eq!(destination(&cfg, "D:\\somewhere-else"), None);
    }

    #[test]
    fn a_handled_file_is_renamed_rather_than_deleted() {
        let dir = scratch("aside");
        let file = dir.join("a.torrent");
        std::fs::write(&file, b"d4:infod4:name1:aee").unwrap();

        assert!(aside(&file));
        assert!(!file.exists(), "the original was left in place");
        assert!(
            dir.join("a.torrent.added").exists(),
            "the file was not renamed aside"
        );

        // The same name again gets a number rather than overwriting.
        std::fs::write(&file, b"d4:infod4:name1:bee").unwrap();
        assert!(aside(&file));
        assert!(dir.join("a.torrent.added.2").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_torrent_files_are_picked_up() {
        let dir = scratch("ext");
        for name in ["a.torrent", "b.TORRENT", "c.txt", "d.torrent.added"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let mut found: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_torrent(p))
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, vec!["a.torrent", "b.TORRENT"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

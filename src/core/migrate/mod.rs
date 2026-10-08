//! Migrating from another BitTorrent client.
//!
//! Every client here reduces to the same shape the PicoTorrent import already
//! produced - a [`Scan`] of [`ImportEntry`] - so the driver that adds them
//! ([`crate::bittorrent::session::Session::migrate_from`]) is shared and only
//! the reading differs per client.
//!
//! None of these carry progress across. librqbit cannot read libtorrent's
//! resume data (nor BitComet's, nor Transmission's), so what comes over is the
//! torrent, its save path and its label; the engine rechecks the files already
//! on disk and recovers the progress from them. That is also why nothing here
//! copies or moves a single byte of downloaded data - it is already where the
//! other client left it, and the migration only learns where that is.

pub mod bitcomet;
pub mod qbittorrent;
pub mod transmission;
pub mod utorrent;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::Result;

pub use crate::core::pico_import::Scan;

/// A client NanoTorrent can migrate from.
///
/// An enum rather than a trait: there are five of them, they are known at
/// compile time, and a trait object would buy nothing but indirection.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Source {
    PicoTorrent,
    QBittorrent,
    UTorrent,
    BitComet,
    Transmission,
}

impl Source {
    /// Menu order: the one this program is a port of first, then by how likely
    /// someone is to be coming from it.
    pub const ALL: [Source; 5] = [
        Source::PicoTorrent,
        Source::QBittorrent,
        Source::UTorrent,
        Source::BitComet,
        Source::Transmission,
    ];

    /// The client's own name. Never translated - these are products, and
    /// "µTorrent" is spelled the same in every language.
    pub fn label(self) -> &'static str {
        match self {
            Source::PicoTorrent => "PicoTorrent",
            Source::QBittorrent => "qBittorrent",
            Source::UTorrent => "µTorrent",
            Source::BitComet => "BitComet",
            Source::Transmission => "Transmission",
        }
    }

    /// Where this client keeps its profile, if it is installed.
    ///
    /// `None` means "not found here", which is what greys the entry out. It is
    /// never a refusal: a profile somewhere unusual is still reachable by
    /// pointing the migration at it by hand.
    pub fn detect(self) -> Option<PathBuf> {
        match self {
            // The only one whose "profile" is a single file rather than a
            // directory, because it is the only one already supported and its
            // reader takes the database path.
            Source::PicoTorrent => crate::core::environment::Environment::create()
                .get_picotorrent_db_path(),
            Source::QBittorrent => qbittorrent::detect(),
            Source::UTorrent => utorrent::detect(),
            Source::BitComet => bitcomet::detect(),
            Source::Transmission => transmission::detect(),
        }
    }

    /// Everything this client has at `root`.
    ///
    /// `cancel` is checked between files. Reading a big profile is thousands
    /// of file reads and can take a minute; without this the Cancel button
    /// sat there doing nothing until the scan finished on its own. A
    /// cancelled scan returns what it had, and the caller throws it away.
    pub fn scan(self, root: &Path, cancel: &AtomicBool) -> Result<Scan> {
        match self {
            Source::PicoTorrent => crate::core::pico_import::read_torrents(root, cancel),
            Source::QBittorrent => qbittorrent::scan(root, cancel),
            Source::UTorrent => utorrent::scan(root, cancel),
            Source::BitComet => bitcomet::scan(root, cancel),
            Source::Transmission => transmission::scan(root, cancel),
        }
    }

    /// The file or folder inside a profile that identifies this client.
    ///
    /// `detect` looks for it in the usual place; `resolve` looks for it
    /// wherever someone points.
    fn marker(self) -> &'static str {
        match self {
            Source::PicoTorrent => "PicoTorrent.sqlite",
            Source::QBittorrent => "BT_backup",
            Source::UTorrent => "resume.dat",
            Source::BitComet => "torrents",
            Source::Transmission => "torrents",
        }
    }

    /// The name the profile folder usually has, for someone who picked the
    /// folder one level above it.
    fn folder(self) -> &'static str {
        match self {
            Source::PicoTorrent => "PicoTorrent",
            Source::QBittorrent => "qBittorrent",
            Source::UTorrent => "uTorrent",
            Source::BitComet => "BitComet",
            Source::Transmission => "transmission",
        }
    }

    /// Whether `root` is something [`Self::scan`] could read.
    ///
    /// Cheap on purpose - it looks for the one marker rather than parsing
    /// anything, so a folder picked by hand can be accepted or rejected
    /// before the confirmation dialog rather than after it.
    pub fn accepts(self, root: &Path) -> bool {
        // PicoTorrent's "root" is the database file itself, not a folder.
        if self == Source::PicoTorrent {
            return root.is_file();
        }
        root.join(self.marker()).exists()
    }

    /// Make sense of a folder someone chose by hand.
    ///
    /// People point at the folder they can see, and a backup can be laid out
    /// any way at all. Four readings are tried, in the order they are likely:
    /// the folder itself, the folder it sits in (someone picked `BT_backup`
    /// rather than the profile above it), the client's own folder inside it
    /// (someone picked the folder their backups live in), and - for
    /// PicoTorrent, whose profile is a single file - the database inside it.
    ///
    /// `None` means none of those held the marker, which is worth saying so
    /// about rather than failing later with something obscure.
    pub fn resolve(self, picked: &Path) -> Option<PathBuf> {
        let mut tries: Vec<PathBuf> = vec![
            picked.to_path_buf(),
            picked.join(self.folder()),
        ];
        if let Some(parent) = picked.parent() {
            tries.push(parent.to_path_buf());
        }
        if self == Source::PicoTorrent {
            // A folder was picked but the profile is the file inside it.
            tries.insert(0, picked.join(self.marker()));
            tries.push(picked.join(self.folder()).join(self.marker()));
        }
        tries.into_iter().find(|p| self.accepts(p))
    }

    /// The client's settings as `(key, JSON value)`, for the ones that can be
    /// read into this build's schema.
    ///
    /// Only PicoTorrent can: NanoTorrent inherited its `setting` table
    /// verbatim, so a value copied across is already in the right shape. The
    /// others store their preferences in formats whose keys mean different
    /// things (qBittorrent's INI, Transmission's JSON), and guessing at a
    /// mapping would silently change settings nobody asked to change.
    pub fn settings(self, root: &Path) -> Result<Vec<(String, String)>> {
        match self {
            Source::PicoTorrent => crate::core::pico_import::read_settings(root),
            _ => Ok(Vec::new()),
        }
    }

    /// Whether [`Self::settings`] can return anything, so the UI can hide a
    /// tick box that would do nothing.
    pub fn carries_settings(self) -> bool {
        matches!(self, Source::PicoTorrent)
    }
}

/// Where a migration has got to, for the progress window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    /// Torrents dealt with so far, out of `total`.
    pub done: usize,
    pub total: usize,
    /// The torrent being worked on, for the line under the bar.
    pub current: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Reading the other client's files. No total is known yet.
    Scanning,
    /// Emptying the current list, when that was asked for. Its own phase
    /// because it happens between the scan and the first torrent, and a
    /// window still showing "Reading ..." through it looks like a hang.
    Purging,
    Adding,
    /// Cancelled or failed, and undoing what was added.
    Reverting,
    Done,
}

/// Read every `*.ext` in `dir` through `read_one`.
///
/// The loop the file-per-torrent importers share. A file that will not read is
/// counted, never fatal: one unreadable torrent out of hundreds is a line in
/// the report, not a failed migration. Cancel is checked per file, because the
/// read is the expensive part and a profile with thousands of them is exactly
/// when someone wants to stop.
pub(crate) fn scan_dir(
    dir: &Path,
    ext: &str,
    cancel: &AtomicBool,
    mut read_one: impl FnMut(&Path) -> Option<crate::core::pico_import::ImportEntry>,
) -> Result<Scan> {
    use anyhow::Context;

    let listing = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let paths = listing
        .flatten()
        .map(|item| item.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some(ext));
    Ok(collect(paths, cancel, |path| read_one(&path)))
}

/// Read each candidate into an entry, counting the ones that will not read.
///
/// Cancel is checked before every read. The result is sorted by info hash:
/// `read_dir` and resume-file order are arbitrary, and a migration run twice
/// should add torrents in the same order both times.
pub(crate) fn collect<T>(
    candidates: impl IntoIterator<Item = T>,
    cancel: &AtomicBool,
    mut read_one: impl FnMut(T) -> Option<crate::core::pico_import::ImportEntry>,
) -> Scan {
    use std::sync::atomic::Ordering;

    let mut entries = Vec::new();
    let mut unreadable = 0usize;
    for candidate in candidates {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match read_one(candidate) {
            Some(entry) => entries.push(entry),
            None => unreadable += 1,
        }
    }
    entries.sort_by(|a, b| a.info_hash.cmp(&b.info_hash));
    Scan { entries, unreadable }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The menu is built from `ALL`, so a client added to the enum and not to
    /// the array would simply never appear - with nothing to notice.
    #[test]
    fn every_source_is_in_all_exactly_once() {
        assert_eq!(Source::ALL.len(), 5);
        for s in Source::ALL {
            assert_eq!(Source::ALL.iter().filter(|o| **o == s).count(), 1, "{s:?}");
        }
    }

    /// Labels are used as identity - a duplicate would put two clients on one
    /// menu line.
    #[test]
    fn labels_are_distinct() {
        for (i, a) in Source::ALL.iter().enumerate() {
            for b in &Source::ALL[i + 1..] {
                assert_ne!(a.label(), b.label());
            }
        }
    }

    /// Only PicoTorrent shares this build's settings schema. If that ever
    /// changes, the tick box in the dialog has to change with it.
    #[test]
    fn only_picotorrent_claims_to_carry_settings() {
        for s in Source::ALL {
            assert_eq!(
                s.carries_settings(),
                s == Source::PicoTorrent,
                "{s:?}"
            );
        }
    }

    /// Detection must answer for every client without panicking, whatever this
    /// machine happens to have installed.
    #[test]
    fn detection_answers_for_all_of_them() {
        for s in Source::ALL {
            let _ = s.detect();
        }
    }

    /// A folder picked by hand can be laid out any way at all. These are the
    /// ways people actually point at a profile, and all of them must work -
    /// the alternative is telling someone their own backup is not there.
    #[test]
    fn a_hand_picked_folder_is_understood_however_it_was_chosen() {
        let base = std::env::temp_dir().join(format!("nt-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // A qBittorrent profile, somewhere that is not the usual place.
        let backup = base.join("MyBackup");
        let profile = backup.join("qBittorrent");
        std::fs::create_dir_all(profile.join("BT_backup")).unwrap();

        let q = Source::QBittorrent;
        // The profile itself.
        assert_eq!(q.resolve(&profile).as_deref(), Some(profile.as_path()));
        // The BT_backup folder inside it - the one people can see.
        assert_eq!(
            q.resolve(&profile.join("BT_backup")).as_deref(),
            Some(profile.as_path())
        );
        // The folder the profile sits in.
        assert_eq!(q.resolve(&backup).as_deref(), Some(profile.as_path()));
        // Somewhere with nothing in it at all.
        assert_eq!(q.resolve(&base.join("empty")), None);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// PicoTorrent is the odd one: its "profile" is a single file, so a
    /// folder picked by hand has to be looked into.
    #[test]
    fn picotorrent_resolves_a_folder_to_its_database() {
        let base = std::env::temp_dir().join(format!("nt-resolve-pico-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let profile = base.join("PicoTorrent");
        std::fs::create_dir_all(&profile).unwrap();
        let db = profile.join("PicoTorrent.sqlite");
        std::fs::write(&db, b"not really a database").unwrap();

        let p = Source::PicoTorrent;
        assert_eq!(p.resolve(&profile).as_deref(), Some(db.as_path()));
        assert_eq!(p.resolve(&base).as_deref(), Some(db.as_path()));
        assert_eq!(p.resolve(&db).as_deref(), Some(db.as_path()));
        assert_eq!(p.resolve(&base.join("elsewhere")), None);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `accepts` must not say yes to a folder that merely exists, or every
    /// wrong choice would be reported as an empty profile instead of a wrong
    /// folder.
    #[test]
    fn an_unrelated_folder_is_not_accepted() {
        let base = std::env::temp_dir().join(format!("nt-accepts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Pictures")).unwrap();
        for source in Source::ALL {
            assert!(!source.accepts(&base.join("Pictures")), "{source:?}");
            assert!(source.resolve(&base.join("Pictures")).is_none(), "{source:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A path that is not a profile is an error, never a panic and never a
    /// silent empty success that would look like "you had no torrents".
    #[test]
    fn scanning_somewhere_useless_fails_cleanly() {
        let nowhere = std::env::temp_dir().join(format!("nt-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&nowhere);
        for s in Source::ALL {
            assert!(s.scan(&nowhere, &AtomicBool::new(false)).is_err(), "{s:?} should have failed");
        }
    }
}

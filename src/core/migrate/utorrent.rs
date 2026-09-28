//! Reading an existing uTorrent / BitTorrent install.
//!
//! `resume.dat` is one bencoded dict whose KEYS are torrent filenames - there
//! is no list to look up, which is why `bencode::entries` exists. Two keys are
//! not torrents: `.fileguard` (an integrity marker) and `rec`. Following what
//! other readers do, an entry counts only if its key contains `.torrent`.
//!
//! # The trap: `path` is not the save path
//!
//! uTorrent's `path` is the full destination of the content - it INCLUDES the
//! torrent's own name, for a single file as well as a folder:
//!
//! ```text
//!   path = D:\Downloads\Some.Release.2026\      (multi-file)
//!   path = D:\Downloads\some.file.iso           (single file)
//! ```
//!
//! librqbit wants the containing directory, so the last component is dropped
//! in both cases - which is exactly what the established uTorrent-to-
//! qBittorrent converters do. Using `path` as-is would bury every migrated
//! torrent one folder deep, and the mistake only shows up as a full re-download.
//!
//! Split by hand rather than with `Path::parent`, because these are Windows
//! paths that may be read on a machine whose separator is `/`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::core::bencode;
use crate::core::pico_import::{ImportEntry, ImportSource, Scan};

/// uTorrent's profile directory, if it is there.
///
/// It also looks beside its own executable first (that is how a portable
/// install works), but nothing here knows where that executable is; a portable
/// install is reached by pointing the migration at the folder by hand.
pub fn detect() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
    // BitTorrent (the client) is the same program with a different name, and
    // writes the same resume.dat.
    [base.join("uTorrent"), base.join("BitTorrent")]
        .into_iter()
        .find(|p| p.join("resume.dat").is_file())
}

/// Every torrent uTorrent has in `profile`.
pub fn scan(profile: &Path, cancel: &AtomicBool) -> Result<Scan> {
    let resume_path = profile.join("resume.dat");
    let resume = std::fs::read(&resume_path)
        .with_context(|| format!("reading {}", resume_path.display()))?;

    let mut entries = Vec::new();
    let mut unreadable = 0usize;

    for (key, value) in bencode::entries(&resume) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let name = String::from_utf8_lossy(key);
        // `.fileguard` and `rec` are not torrents. Matching on `.torrent`
        // rather than excluding those two by name means a key nobody has
        // documented yet is ignored rather than parsed as a torrent.
        if !name.contains(".torrent") {
            continue;
        }
        match read_one(profile, &name, value) {
            Some(entry) => entries.push(entry),
            None => unreadable += 1,
        }
    }

    entries.sort_by(|a, b| a.info_hash.cmp(&b.info_hash));
    Ok(Scan { entries, unreadable })
}

fn read_one(profile: &Path, key: &str, value: &[u8]) -> Option<ImportEntry> {
    // The key is usually just a filename beside resume.dat, but uTorrent
    // stores an absolute path when the torrent was loaded from elsewhere.
    let candidate = Path::new(key);
    let torrent_path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        profile.join(key)
    };
    let bytes = std::fs::read(&torrent_path).ok()?;
    let info = bencode::dict_get(&bytes, b"info")?;
    let (v1, v2) = crate::bittorrent::metainfo::info_hashes(info);

    let save_path = bencode::dict_get(value, b"path")
        .and_then(bencode::text)
        .as_deref()
        .and_then(parent_of)
        .filter(|s| !s.is_empty());

    // `label` is the single label; `labels` is a newer list. Take the single
    // one, and the first of the list otherwise - this build has one label per
    // torrent and picking the first beats inventing a joined name.
    let label_name = bencode::dict_get(value, b"label")
        .and_then(bencode::text)
        .or_else(|| {
            let list = bencode::dict_get(value, b"labels")?;
            // A bencoded list: step over the leading 'l' and read the first
            // string in it.
            bencode::text(list.get(1..)?)
        })
        .filter(|s| !s.is_empty());

    Some(ImportEntry {
        info_hash: v1.or(v2)?,
        source: ImportSource::TorrentBytes(bytes),
        save_path,
        label_id: None,
        label_name,
    })
}

/// Everything before the last `/` or `\`, with the separator dropped.
///
/// `None` when there is nothing before it: a bare name has no parent to use,
/// and a root like `D:\` would leave an empty string that reads as "no path".
fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let cut = trimmed.rfind(['/', '\\'])?;
    let parent = &trimmed[..cut];
    // Keep the separator on a drive root: `D:` is not a directory, `D:\` is.
    if parent.ends_with(':') {
        return Some(format!("{parent}\\"));
    }
    (!parent.is_empty()).then(|| parent.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bencode::build;

    struct Profile(PathBuf);

    impl Profile {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nt-ut-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Profile(dir)
        }
        fn torrent(&self, file: &str, name: &str) -> String {
            let info = build::dict(&[
                ("length", build::int(1234)),
                ("name", build::str(name)),
                ("piece length", build::int(16384)),
                ("pieces", build::str("01234567890123456789")),
            ]);
            std::fs::write(self.0.join(file), build::dict(&[("info", info.clone())])).unwrap();
            crate::bittorrent::metainfo::info_hashes(&info).0.unwrap()
        }
        fn resume(&self, pairs: &[(&str, Vec<u8>)]) {
            std::fs::write(self.0.join("resume.dat"), build::dict(pairs)).unwrap();
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The whole point: `path` carries the torrent's own name and must not be
    /// used as the save path. Getting this wrong buries every torrent one
    /// folder deep and costs a full re-download.
    #[test]
    fn the_torrents_own_name_is_stripped_from_the_path() {
        let p = Profile::new("path");
        let hash = p.torrent("a.torrent", "Some.Release.2026");
        p.resume(&[
            (".fileguard", build::str("ABCDEF")),
            (
                "a.torrent",
                build::dict(&[("path", build::str("D:\\Downloads\\Some.Release.2026"))]),
            ),
        ]);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].info_hash, hash);
        assert_eq!(scan.entries[0].save_path.as_deref(), Some("D:\\Downloads"));
    }

    /// A single-file torrent points at the FILE, and the parent is still what
    /// is wanted - same rule, and the one people get wrong.
    #[test]
    fn a_single_file_path_is_treated_the_same_way() {
        assert_eq!(parent_of("D:\\Downloads\\some.iso").as_deref(), Some("D:\\Downloads"));
        assert_eq!(parent_of("/home/me/dl/some.iso").as_deref(), Some("/home/me/dl"));
        // A trailing separator must not produce an extra level.
        assert_eq!(parent_of("D:\\Downloads\\Name\\").as_deref(), Some("D:\\Downloads"));
        // Roots and bare names have no useful parent.
        assert_eq!(parent_of("D:\\Name").as_deref(), Some("D:\\"));
        assert_eq!(parent_of("bare-name"), None);
        assert_eq!(parent_of(""), None);
    }

    /// `.fileguard` and `rec` are not torrents, and neither is anything else
    /// without `.torrent` in its key.
    #[test]
    fn the_bookkeeping_keys_are_not_torrents() {
        let p = Profile::new("guard");
        p.torrent("real.torrent", "Real");
        p.resume(&[
            (".fileguard", build::str("ABCDEF")),
            ("rec", build::dict(&[("x", build::int(1))])),
            ("real.torrent", build::dict(&[("path", build::str("D:\\dl\\Real"))])),
        ]);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.unreadable, 0, ".fileguard and rec must not count as failures");
    }

    #[test]
    fn labels_come_across() {
        let p = Profile::new("label");
        p.torrent("a.torrent", "A");
        p.torrent("b.torrent", "B");
        p.resume(&[
            (
                "a.torrent",
                build::dict(&[
                    ("label", build::str("Films")),
                    ("path", build::str("D:\\dl\\A")),
                ]),
            ),
            (
                "b.torrent",
                build::dict(&[
                    ("labels", build::list(&[build::str("Music"), build::str("Ignored")])),
                    ("path", build::str("D:\\dl\\B")),
                ]),
            ),
        ]);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        let mut labels: Vec<_> = scan
            .entries
            .iter()
            .map(|e| e.label_name.clone().unwrap_or_default())
            .collect();
        labels.sort();
        assert_eq!(labels, ["Films", "Music"]);
    }

    /// A torrent listed in resume.dat whose file is gone: counted, not fatal.
    #[test]
    fn a_missing_torrent_file_is_counted() {
        let p = Profile::new("missing");
        p.torrent("here.torrent", "Here");
        p.resume(&[
            ("here.torrent", build::dict(&[("path", build::str("D:\\dl\\Here"))])),
            ("gone.torrent", build::dict(&[("path", build::str("D:\\dl\\Gone"))])),
        ]);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.unreadable, 1);
    }

    #[test]
    fn a_missing_resume_file_is_an_error_not_a_panic() {
        let p = Profile::new("none");
        assert!(scan(&p.0, &AtomicBool::new(false)).is_err());
    }
}

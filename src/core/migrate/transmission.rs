//! Reading an existing Transmission install.
//!
//! Two folders, paired by filename: `torrents/<name>.<hash16>.torrent` holds
//! the metadata, `resume/<name>.<hash16>.resume` holds the state. The stem is
//! the torrent's name plus the first 16 hex characters of its info hash - the
//! name is there to be human-readable and the hash to stop two torrents called
//! the same thing colliding.
//!
//! The `.torrent` leads and the `.resume` is optional: a torrent whose resume
//! file is missing still has its metadata, and arrives with the default save
//! path rather than being dropped. That is the way round that loses least -
//! the reverse (a `.resume` with no `.torrent`) has nothing to add from.
//!
//! Unlike uTorrent, Transmission's `destination` is already the containing
//! directory, so it is used as-is. Keys confirmed against `resume.cc`:
//! `destination`, `name`, `labels`, `added_date`, `paused`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::Result;

use crate::core::bencode;
use crate::core::pico_import::{ImportEntry, ImportSource, Scan};

/// Transmission's config directory, if it is there.
pub fn detect() -> Option<PathBuf> {
    let root = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|b| PathBuf::from(b).join("transmission"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/Transmission"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("transmission"))
    }?;
    // `transmission-daemon` keeps its own profile beside this one; whichever
    // has a torrents folder is the one worth offering.
    [root.clone(), root.with_file_name("transmission-daemon")]
        .into_iter()
        .find(|p| p.join("torrents").is_dir())
}

/// Every torrent Transmission has in `config`.
pub fn scan(config: &Path, cancel: &AtomicBool) -> Result<Scan> {
    super::scan_dir(&config.join("torrents"), "torrent", cancel, |p| read_one(config, p))
}

fn read_one(config: &Path, torrent: &Path) -> Option<ImportEntry> {
    let bytes = std::fs::read(torrent).ok()?;
    let info = bencode::dict_get(&bytes, b"info")?;
    let (v1, v2) = crate::bittorrent::metainfo::info_hashes(info);

    // Same stem, different folder and extension. Absent is fine.
    //
    // Built as a string, NOT with `with_extension`: the stem is
    // `<name>.<hash16>` and is full of dots, so `with_extension` would treat
    // `.a1b2c3d4e5f60718` as the extension and look for `Ubuntu.resume`.
    let stem = torrent.file_stem()?.to_string_lossy().into_owned();
    let resume_path = config.join("resume").join(format!("{stem}.resume"));
    let resume = std::fs::read(&resume_path).unwrap_or_default();

    let save_path = bencode::dict_get(&resume, b"destination")
        .and_then(bencode::text)
        .filter(|s| !s.is_empty());

    // `labels` is a bencoded list of strings; this build has one label per
    // torrent, so the first is taken rather than joining them into a name
    // nobody chose.
    let label_name = bencode::dict_get(&resume, b"labels")
        .and_then(|l| bencode::text(l.get(1..)?))
        .filter(|s| !s.is_empty());

    Some(ImportEntry {
        info_hash: v1.or(v2)?,
        source: ImportSource::TorrentBytes(bytes),
        save_path,
        label_id: None,
        label_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bencode::build;

    struct Config(PathBuf);

    impl Config {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nt-tr-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("torrents")).unwrap();
            std::fs::create_dir_all(dir.join("resume")).unwrap();
            Config(dir)
        }
        fn torrent(&self, stem: &str, name: &str) -> String {
            let info = build::dict(&[
                ("length", build::int(1234)),
                ("name", build::str(name)),
                ("piece length", build::int(16384)),
                ("pieces", build::str("01234567890123456789")),
            ]);
            std::fs::write(
                self.0.join("torrents").join(format!("{stem}.torrent")),
                build::dict(&[("info", info.clone())]),
            )
            .unwrap();
            crate::bittorrent::metainfo::info_hashes(&info).0.unwrap()
        }
        fn resume(&self, stem: &str, pairs: &[(&str, Vec<u8>)]) {
            std::fs::write(
                self.0.join("resume").join(format!("{stem}.resume")),
                build::dict(pairs),
            )
            .unwrap();
        }
    }

    impl Drop for Config {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pairs_a_torrent_with_its_resume_file() {
        let c = Config::new("pair");
        let hash = c.torrent("Ubuntu.a1b2c3d4e5f60718", "Ubuntu");
        c.resume(
            "Ubuntu.a1b2c3d4e5f60718",
            &[
                ("destination", build::str("/home/me/Downloads")),
                ("labels", build::list(&[build::str("iso"), build::str("later")])),
            ],
        );

        let scan = scan(&c.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].info_hash, hash);
        // Transmission's destination is ALREADY the parent - unlike uTorrent,
        // nothing is stripped.
        assert_eq!(scan.entries[0].save_path.as_deref(), Some("/home/me/Downloads"));
        assert_eq!(scan.entries[0].label_name.as_deref(), Some("iso"));
    }

    /// The metadata is what matters; a missing resume file costs the save path
    /// and the label, not the torrent.
    #[test]
    fn a_torrent_without_a_resume_file_still_comes_across() {
        let c = Config::new("noresume");
        let hash = c.torrent("Orphan.0011223344556677", "Orphan");

        let scan = scan(&c.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.unreadable, 0);
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].info_hash, hash);
        assert_eq!(scan.entries[0].save_path, None);
    }

    /// A stem with dots in it - Transmission's own naming - must still pair.
    #[test]
    fn a_dotted_stem_still_finds_its_resume() {
        let c = Config::new("dots");
        c.torrent("Some.Release.2026.deadbeefdeadbeef", "Some.Release.2026");
        c.resume(
            "Some.Release.2026.deadbeefdeadbeef",
            &[("destination", build::str("/data"))],
        );
        assert_eq!(scan(&c.0, &AtomicBool::new(false)).unwrap().entries[0].save_path.as_deref(), Some("/data"));
    }

    #[test]
    fn rubbish_is_counted_not_fatal() {
        let c = Config::new("rubbish");
        c.torrent("Good.1111111111111111", "Good");
        std::fs::write(c.0.join("torrents").join("bad.torrent"), b"not bencode").unwrap();
        std::fs::write(c.0.join("torrents").join("notes.txt"), b"ignored").unwrap();

        let scan = scan(&c.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.unreadable, 1);
    }

    #[test]
    fn a_missing_config_is_an_error_not_a_panic() {
        assert!(scan(Path::new("no-such-transmission-config"), &AtomicBool::new(false)).is_err());
    }
}

//! Reading an existing qBittorrent install.
//!
//! qBittorrent keeps one `<info hash>.fastresume` per torrent in `BT_backup`,
//! bencoded, holding libtorrent's own resume fields plus its own under a
//! `qBt-` prefix. The `.torrent` sits beside it under the same stem.
//!
//! What is taken and what is not:
//!
//! - The **`.torrent`**, whole. librqbit cannot read libtorrent resume data,
//!   so progress is not carried over - the engine rechecks the files on disk
//!   and recovers it, exactly as the PicoTorrent import does.
//! - The **save path**, from `qBt-savePath` if present and libtorrent's own
//!   `save_path` otherwise. qBittorrent writes both and they can disagree:
//!   `qBt-savePath` is the one its UI shows, so it wins.
//! - The **category**, as a label. Tags (`qBt-tags`) are a list and this
//!   build has one label per torrent, so they are left behind rather than
//!   silently flattening several into one.
//!
//! The newer experimental SQLite backend (`torrents.db`) is NOT read. It is
//! off by default; a profile using it has an empty or stale `BT_backup`, and
//! reporting "nothing found" is honest where guessing at a schema marked
//! experimental is not.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::Result;

use crate::core::bencode;
use crate::bittorrent::session::AddTorrentSource;
use crate::core::pico_import::{ImportEntry, Scan};

/// The profile directory qBittorrent uses by default, if it is there.
///
/// Only a directory that actually exists is returned, so the caller can grey
/// out a client that is not installed rather than offering a migration that
/// can only fail.
pub fn detect() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        // %APPDATA%\qBittorrent - the roaming half, which is where the
        // profile lives; %LOCALAPPDATA%\qBittorrent holds only the cache.
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }?;
    let root = base.join("qBittorrent");
    root.join("BT_backup").is_dir().then_some(root)
}

/// Every torrent qBittorrent has in `profile`.
///
/// `profile` is the directory holding `BT_backup`, not `BT_backup` itself, so
/// a caller can hand over what a folder picker returned.
pub fn scan(profile: &Path, cancel: &AtomicBool) -> Result<Scan> {
    super::scan_dir(&profile.join("BT_backup"), "fastresume", cancel, read_one)
}

/// One `.fastresume`, plus the `.torrent` beside it.
fn read_one(fastresume: &Path) -> Option<ImportEntry> {
    let resume = std::fs::read(fastresume).ok()?;

    // qBittorrent's own path wins over libtorrent's: they can disagree, and
    // this is the one its UI shows.
    let save_path = bencode::dict_get(&resume, b"qBt-savePath")
        .or_else(|| bencode::dict_get(&resume, b"save_path"))
        .and_then(bencode::text);

    // Category, not tags: a torrent has exactly one category, and this build
    // has one label per torrent, so the two line up. `qBt-tags` is a list and
    // flattening several into one would invent a grouping nobody chose.
    let label_name = bencode::dict_get(&resume, b"qBt-category")
        .and_then(bencode::text);

    let torrent = fastresume.with_extension("torrent");
    if let Ok(bytes) = std::fs::read(&torrent) {
        // The stem is the info hash, but it is not trusted - see
        // `from_torrent`.
        return ImportEntry::from_torrent(bytes, save_path, label_name);
    }

    // No `.torrent`: a magnet qBittorrent has not resolved yet. The link is
    // all there is, and it is enough to re-add.
    let magnet = bencode::dict_get(&resume, b"qBt-magnetUri")
        .or_else(|| bencode::dict_get(&resume, b"magnet-uri"))
        .and_then(bencode::text)?;
    let info_hash = magnet_info_hash(&magnet)?;
    Some(ImportEntry {
        info_hash,
        source: AddTorrentSource::MagnetUri(magnet),
        save_path,
        label_id: None,
        label_name,
    })
}

/// The v1 info hash out of a magnet's `xt=urn:btih:` parameter, lowercased.
///
/// Hex only. A base32 `btih` is legal and rare, and the engine handles it when
/// the link is added - but this value is only used to ask "do we already have
/// this?", and answering that wrongly is worse than not answering: an entry
/// with no usable hash is reported unreadable instead.
fn magnet_info_hash(magnet: &str) -> Option<String> {
    let xt = magnet
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("xt=urn:btih:"))?;
    let hex = xt.split('&').next()?;
    (hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bencode::build;

    /// A throwaway profile directory, like the database tests elsewhere:
    /// a crate for temporary directories would be a dependency for four lines.
    struct Profile(PathBuf);

    impl Profile {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("nt-qbt-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("BT_backup")).unwrap();
            Profile(dir)
        }
        fn backup(&self) -> PathBuf {
            self.0.join("BT_backup")
        }
        /// A minimal but REAL torrent: `info_hashes` needs `pieces` present to
        /// call it a v1 torrent at all.
        fn torrent(&self, stem: &str, name: &str) -> String {
            let info = build::dict(&[
                ("length", build::int(1234)),
                ("name", build::str(name)),
                ("piece length", build::int(16384)),
                ("pieces", build::str("01234567890123456789")),
            ]);
            let torrent = build::dict(&[("info", info.clone())]);
            std::fs::write(self.backup().join(format!("{stem}.torrent")), &torrent).unwrap();
            let (v1, _) = crate::bittorrent::metainfo::info_hashes(&info);
            v1.unwrap()
        }
        fn fastresume(&self, stem: &str, pairs: &[(&str, Vec<u8>)]) {
            std::fs::write(self.backup().join(format!("{stem}.fastresume")), build::dict(pairs))
                .unwrap();
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_a_torrent_with_its_save_path() {
        let p = Profile::new("basic");
        let hash = p.torrent("aaaa", "Example");
        p.fastresume("aaaa", &[("qBt-savePath", build::str("D:\\Downloads"))]);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.unreadable, 0);
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].info_hash, hash);
        assert_eq!(scan.entries[0].save_path.as_deref(), Some("D:\\Downloads"));
        assert!(matches!(scan.entries[0].source, AddTorrentSource::TorrentFileBytes(_)));
    }

    /// The hash comes from the info dict, never the filename - a `.fastresume`
    /// renamed by hand must not import under someone else's hash.
    #[test]
    fn the_filename_is_not_trusted_for_the_hash() {
        let p = Profile::new("stem");
        let real = p.torrent("not-a-hash-at-all", "Example");
        p.fastresume("not-a-hash-at-all", &[("save_path", build::str("/data"))]);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries[0].info_hash, real);
    }

    /// qBittorrent writes both paths and they can disagree; its own wins.
    #[test]
    fn qbt_save_path_beats_libtorrents() {
        let p = Profile::new("paths");
        p.torrent("bbbb", "Example");
        p.fastresume(
            "bbbb",
            &[
                ("qBt-savePath", build::str("D:\\What The UI Shows")),
                ("save_path", build::str("D:\\Stale")),
            ],
        );
        assert_eq!(
            scan(&p.0, &AtomicBool::new(false)).unwrap().entries[0].save_path.as_deref(),
            Some("D:\\What The UI Shows")
        );
    }

    /// A magnet qBittorrent has not resolved has no `.torrent` beside it.
    #[test]
    fn an_unresolved_magnet_comes_across_as_a_magnet() {
        let p = Profile::new("magnet");
        let hash = "0123456789abcdef0123456789abcdef01234567";
        p.fastresume(
            "cccc",
            &[("qBt-magnetUri", build::str(&format!("magnet:?xt=urn:btih:{hash}&dn=Thing")))],
        );

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.unreadable, 0);
        assert_eq!(scan.entries[0].info_hash, hash);
        assert!(matches!(scan.entries[0].source, AddTorrentSource::MagnetUri(_)));
    }

    /// Neither metadata nor a link: counted, not dropped silently. "42 of your
    /// 45 came across" is the one number a migration has to be honest about.
    #[test]
    fn a_torrent_with_nothing_to_add_from_is_counted() {
        let p = Profile::new("empty");
        p.torrent("good", "Example");
        p.fastresume("good", &[("save_path", build::str("/data"))]);
        p.fastresume("orphan", &[("save_path", build::str("/data"))]);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.unreadable, 1);
    }

    /// Nothing here may panic on a file another program wrote.
    #[test]
    fn rubbish_in_the_folder_is_survived() {
        let p = Profile::new("rubbish");
        std::fs::write(p.backup().join("truncated.fastresume"), b"d9:save_path").unwrap();
        std::fs::write(p.backup().join("notbencode.fastresume"), b"hello").unwrap();
        std::fs::write(p.backup().join("ignored.txt"), b"not ours").unwrap();
        std::fs::write(p.backup().join("half.torrent"), b"d4:infoe").unwrap();
        std::fs::write(p.backup().join("half.fastresume"), build::dict(&[])).unwrap();

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(scan.unreadable, 3, "the three .fastresume files, not the .txt");
    }

    /// A scan that has been cancelled stops where it is, rather than reading
    /// every remaining file first. This is the whole point of threading the
    /// flag down here: a profile with thousands of torrents took a minute to
    /// read, and the Cancel button did nothing at all until it finished.
    #[test]
    fn a_cancelled_scan_stops_instead_of_reading_everything() {
        let p = Profile::new("cancel");
        for i in 0..20 {
            let stem = format!("t{i:02}");
            p.torrent(&stem, &format!("Torrent {i}"));
            p.fastresume(&stem, &[("qBt-savePath", build::str("D:/dl"))]);
        }

        // Already cancelled: nothing should come back at all.
        let stopped = scan(&p.0, &AtomicBool::new(true)).unwrap();
        assert!(
            stopped.entries.is_empty(),
            "a cancelled scan read {} torrents",
            stopped.entries.len()
        );
        assert_eq!(stopped.unreadable, 0, "stopping early is not a failure");

        // And the same profile read normally finds all of them, so the test
        // above proves the flag and not an empty folder.
        let full = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(full.entries.len(), 20);
    }

    #[test]
    fn a_missing_profile_is_an_error_not_a_panic() {
        assert!(scan(Path::new("no-such-profile-anywhere"), &AtomicBool::new(false)).is_err());
    }
}

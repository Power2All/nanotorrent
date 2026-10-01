//! Reading an existing BitComet install.
//!
//! # Why this one is different
//!
//! The other three clients keep their state in bencode with published field
//! names. BitComet keeps a task list in `downloads.xml` and per-task detail in
//! `torrents/*.xml`, and **its schema is not documented anywhere public** -
//! not on the BitComet wiki, which describes only what each file is for, and
//! not by the forensic tools that read it, which publish the fields they
//! extract but not the element names they come from.
//!
//! So this does not parse a schema it cannot know. It leads with the
//! `torrents/*.torrent` files, which are an ordinary documented format and
//! give the info hash and metadata outright, and then looks in the sibling
//! `.xml` for the save path.
//!
//! ponytail: the save path is found by heuristic - the first value that looks
//! like an absolute path, preferring elements and attributes whose NAME hints
//! at one. The ceiling is that a profile whose schema disagrees loses its save
//! paths and falls back to the default download folder; the torrents
//! themselves still migrate, and the report says how many defaulted. Replace
//! with real element names once a genuine BitComet profile can be examined.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use quick_xml::events::Event;

use crate::core::bencode;
use crate::core::pico_import::{ImportEntry, ImportSource, Scan};

/// BitComet's profile directory, if it is there.
pub fn detect() -> Option<PathBuf> {
    // Windows-only in practice: the macOS and Linux builds are long dead.
    let root = std::env::var_os("APPDATA").map(|b| PathBuf::from(b).join("BitComet"))?;
    root.join("torrents").is_dir().then_some(root)
}

/// Every torrent BitComet has in `profile`.
pub fn scan(profile: &Path, cancel: &AtomicBool) -> Result<Scan> {
    super::scan_dir(&profile.join("torrents"), "torrent", cancel, read_one)
}

fn read_one(torrent: &Path) -> Option<ImportEntry> {
    let bytes = std::fs::read(torrent).ok()?;
    let info = bencode::dict_get(&bytes, b"info")?;
    let (v1, v2) = crate::bittorrent::metainfo::info_hashes(info);

    // Same stem, `.xml` instead. Built as a string rather than with
    // `with_extension`, which would eat everything after the first dot of a
    // torrent named `Some.Release.2026.torrent`.
    let stem = torrent.file_stem()?.to_string_lossy().into_owned();
    let detail = torrent.with_file_name(format!("{stem}.xml"));
    let save_path = std::fs::read_to_string(&detail)
        .ok()
        .and_then(|xml| save_path_in(&xml));

    Some(ImportEntry {
        info_hash: v1.or(v2)?,
        source: ImportSource::TorrentBytes(bytes),
        save_path,
        label_id: None,
        label_name: None,
    })
}

/// Hunt an absolute path out of XML whose element names are unknown.
///
/// Two passes' worth of preference in one walk: a value whose name hints at a
/// path wins over one that merely looks like a path, so a document that also
/// records, say, the torrent's origin URL does not win the race by being
/// first.
fn save_path_in(xml: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut best: Option<String> = None;
    let mut fallback: Option<String> = None;
    // The name of the element whose text is coming next.
    let mut current = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                current = e.local_name().as_ref().to_string();
                // Attributes carry it at least as often as text does.
                for attr in e.attributes().flatten() {
                    let key = attr.key.local_name().as_ref().to_string();
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map(|v| v.into_owned())
                        .unwrap_or_default();
                    consider(&key, &value, &mut best, &mut fallback);
                }
            }
            Ok(Event::Text(t)) => {
                consider(&current, t.as_ref(), &mut best, &mut fallback);
            }
            // A malformed document gives back whatever was found before the
            // damage: this is a best-effort read of another program's file.
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        if best.is_some() {
            break;
        }
    }
    best.or(fallback)
}

fn consider(name: &str, value: &str, best: &mut Option<String>, fallback: &mut Option<String>) {
    let value = value.trim();
    if !looks_absolute(value) {
        return;
    }
    let name = name.to_ascii_lowercase();
    let hinted = ["path", "dir", "folder", "save", "location"]
        .iter()
        .any(|h| name.contains(h));
    if hinted {
        best.get_or_insert_with(|| value.to_string());
    } else {
        fallback.get_or_insert_with(|| value.to_string());
    }
}

/// `C:\...`, `\\server\share` or `/...`, and not a URL.
fn looks_absolute(value: &str) -> bool {
    if value.len() < 2 || value.contains("://") {
        return false;
    }
    let b = value.as_bytes();
    let drive = b[0].is_ascii_alphabetic() && b[1] == b':';
    drive || value.starts_with("\\\\") || value.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bencode::build;

    struct Profile(PathBuf);

    impl Profile {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nt-bc-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("torrents")).unwrap();
            Profile(dir)
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
        fn detail(&self, stem: &str, xml: &str) {
            std::fs::write(self.0.join("torrents").join(format!("{stem}.xml")), xml).unwrap();
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The torrent is the source of truth; the XML only supplies the path.
    #[test]
    fn the_torrent_leads_and_the_xml_supplies_the_path() {
        let p = Profile::new("basic");
        let hash = p.torrent("task1", "Example");
        p.detail("task1", r#"<task><save_path>D:\Downloads</save_path></task>"#);

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].info_hash, hash);
        assert_eq!(scan.entries[0].save_path.as_deref(), Some("D:\\Downloads"));
    }

    /// Whatever the schema turns out to be, these shapes all have to work.
    #[test]
    fn the_path_is_found_however_it_is_spelled() {
        for xml in [
            r#"<task><save_path>D:\dl</save_path></task>"#,
            r#"<task><SavePath>D:\dl</SavePath></task>"#,
            r#"<task save_path="D:\dl"/>"#,
            r#"<task><Directory>D:\dl</Directory></task>"#,
            r#"<t><download_folder>D:\dl</download_folder></t>"#,
            r#"<t><file location="D:\dl"/></t>"#,
        ] {
            assert_eq!(save_path_in(xml).as_deref(), Some("D:\\dl"), "{xml}");
        }
    }

    /// A hinted name beats an unhinted one even when it comes later - the
    /// first absolute-looking string in the document is not necessarily the
    /// save path.
    #[test]
    fn a_named_path_beats_a_stray_one() {
        let xml = r#"<task><comment>/not/the/save/path</comment><save_dir>/the/real/one</save_dir></task>"#;
        assert_eq!(save_path_in(xml).as_deref(), Some("/the/real/one"));
    }

    /// A URL is not a path, however absolute it looks.
    #[test]
    fn urls_are_not_paths() {
        let xml = r#"<task><url>https://example.com/x</url></task>"#;
        assert_eq!(save_path_in(xml), None);
        assert!(!looks_absolute("https://example.com/x"));
        assert!(!looks_absolute(""));
        assert!(!looks_absolute("relative\\thing"));
        assert!(looks_absolute("D:\\dl"));
        assert!(looks_absolute("\\\\server\\share"));
        assert!(looks_absolute("/home/me"));
    }

    /// No XML, or XML with nothing path-shaped in it: the torrent still comes
    /// across, and the driver gives it the default download folder.
    #[test]
    fn a_torrent_with_no_usable_xml_still_migrates() {
        let p = Profile::new("noxml");
        p.torrent("task1", "NoXml");
        p.torrent("task2", "BadXml");
        p.detail("task2", "<task><name>nothing path shaped</name></task>");

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.unreadable, 0);
        assert!(scan.entries.iter().all(|e| e.save_path.is_none()));
    }

    /// Malformed XML must not lose the torrent, nor panic.
    #[test]
    fn broken_xml_is_survived() {
        let p = Profile::new("broken");
        p.torrent("task1", "Broken");
        p.detail("task1", "<task><save_path>D:\\dl</save_p");

        let scan = scan(&p.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.entries.len(), 1);
        // The text was read before the damage, so the path survives.
        assert_eq!(scan.entries[0].save_path.as_deref(), Some("D:\\dl"));
    }

    #[test]
    fn a_missing_profile_is_an_error_not_a_panic() {
        assert!(scan(Path::new("no-such-bitcomet-profile"), &AtomicBool::new(false)).is_err());
    }
}

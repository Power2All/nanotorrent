//! One-shot import of torrents from an existing PicoTorrent install.
//!
//! PicoTorrent (libtorrent) keeps each loaded torrent in its SQLite DB rather
//! than as files: the `torrent` table plus `torrent_resume_data` (a libtorrent
//! resume blob that embeds the torrent's `info` dict and `save_path`) and
//! `torrent_magnet_uri`. librqbit can't consume libtorrent resume data, so we
//! extract each torrent's `info` dict (or magnet) + save path and hand those to
//! librqbit, which rechecks the on-disk files to recover progress.

use std::path::Path;

use anyhow::{Context, Result};

use crate::bittorrent::session::AddTorrentSource;

pub struct ImportEntry {
    pub info_hash: String,
    /// For PicoTorrent, torrent bytes are a minimal `.torrent` rebuilt from
    /// the resume blob's `info` dict.
    pub source: AddTorrentSource,
    pub save_path: Option<String>,
    /// PicoTorrent's own numeric label, which shares this build's schema.
    pub label_id: Option<i32>,
    /// A label by NAME, which is all the other clients have. Resolved to an
    /// id by the driver, creating the label if it is new - the id in
    /// `label_id` means nothing in another program's database.
    pub label_name: Option<String>,
}

impl ImportEntry {
    /// An entry for a `.torrent` file's bytes, under the hash of its own info
    /// dict - v1, or v2 for a v2-only torrent. `None` when there is no info
    /// dict to hash.
    ///
    /// The hash is never taken from a file name or a resume record: a renamed
    /// file would then import under a hash that is not its own and collide
    /// with something else.
    pub fn from_torrent(
        bytes: Vec<u8>,
        save_path: Option<String>,
        label_name: Option<String>,
    ) -> Option<Self> {
        let info = crate::core::bencode::dict_get(&bytes, b"info")?;
        let (v1, v2) = crate::bittorrent::metainfo::info_hashes(info);
        Some(ImportEntry {
            info_hash: v1.or(v2)?,
            source: AddTorrentSource::TorrentFileBytes(bytes),
            save_path,
            label_id: None,
            label_name,
        })
    }
}

/// What one pass over a PicoTorrent database found.
pub struct Scan {
    pub entries: Vec<ImportEntry>,
    /// Rows with neither embedded metadata nor a magnet link - there is nothing
    /// to add them from. Counted rather than dropped silently, because "42 of
    /// your 45 torrents came across" is the one number an import has to be
    /// honest about.
    ///
    /// The caller adds the torrents the engine then refuses to this, and
    /// reports the total as what failed.
    pub unreadable: usize,
}

/// Settings from a PicoTorrent database, as `(key, JSON value)`.
///
/// The same table shape this build uses - PicoTorrent's own migration moved
/// every setting into a JSON `value` column, and NanoTorrent inherited that
/// schema verbatim. So a value read here needs no conversion; whether it means
/// anything is decided by whether this build has a setting of that name, which
/// `Configuration::write_value` answers by writing.
pub fn read_settings(db_path: &Path) -> Result<Vec<(String, String)>> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening {}", db_path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT key, value FROM setting WHERE value IS NOT NULL AND value <> ''",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Read every torrent PicoTorrent has stored in `db_path`.
pub fn read_torrents(db_path: &Path, cancel: &std::sync::atomic::AtomicBool) -> Result<Scan> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening {}", db_path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT t.info_hash, tmu.magnet_uri, trd.resume_data, tmu.save_path, t.label_id \
         FROM torrent t \
         LEFT JOIN torrent_magnet_uri  tmu ON t.info_hash = tmu.info_hash \
         LEFT JOIN torrent_resume_data trd ON t.info_hash = trd.info_hash \
         ORDER BY t.queue_position ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1).unwrap_or(None),
            row.get::<_, Option<Vec<u8>>>(2).unwrap_or(None),
            row.get::<_, Option<String>>(3).unwrap_or(None),
            row.get::<_, Option<i32>>(4).unwrap_or(None),
        ))
    })?;

    let mut out = Vec::new();
    let mut unreadable = 0usize;
    for row in rows {
        // Rows are cheap, but a database with thousands of them
        // still rebuilds a .torrent per row.
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let (info_hash, magnet, resume, mut save_path, label_id) = row?;
        let magnet = magnet.filter(|m| !m.is_empty());

        // Prefer reconstructing a .torrent from the resume blob's info dict;
        // fall back to the magnet link if there's no embedded metadata.
        let source = match resume.as_deref().filter(|b| !b.is_empty()) {
            Some(blob) => match crate::core::bencode::dict_get(blob, b"info") {
                Some(info) => {
                    if save_path.as_deref().unwrap_or("").is_empty() {
                        save_path = crate::core::bencode::dict_get(blob, b"save_path")
                            .and_then(crate::core::bencode::string)
                            .map(|b| String::from_utf8_lossy(b).into_owned());
                    }
                    // A minimal but valid metainfo: d 4:info <info> e.
                    let mut bytes = Vec::with_capacity(info.len() + 8);
                    bytes.extend_from_slice(b"d4:info");
                    bytes.extend_from_slice(info);
                    bytes.push(b'e');
                    Some(AddTorrentSource::TorrentFileBytes(bytes))
                }
                None => magnet.clone().map(AddTorrentSource::MagnetUri),
            },
            None => magnet.clone().map(AddTorrentSource::MagnetUri),
        };

        match source {
            Some(source) => out.push(ImportEntry {
                info_hash,
                source,
                save_path: save_path.filter(|s| !s.is_empty()),
                label_id: label_id.filter(|&id| id > 0),
                label_name: None,
            }),
            // No metadata and no magnet: the row names a torrent this cannot
            // reconstruct. Counted, so the user is told rather than left to
            // notice the gap themselves.
            None => unreadable += 1,
        }
    }

    Ok(Scan {
        entries: out,
        unreadable,
    })
}

// Minimal bencode scanning. We work on raw byte spans (rather than decoding and
// re-encoding) so the extracted `info` dict is byte-identical and keeps its
// original info-hash.

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal PicoTorrent database with the columns this reader
    /// actually touches.
    ///
    /// `std::env::temp_dir()` with the process id in the name, like the other
    /// database tests here - a crate to make a temporary file would be a
    /// dependency for four lines.
    struct Fixture(std::path::PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// One torrent row: info hash, its magnet link, its resume blob. Named,
    /// because the tuple is past what clippy will read without complaint - and
    /// past what a person will, come to that.
    type Row<'a> = (&'a str, Option<&'a str>, Option<&'a [u8]>);

    fn fixture(name: &str, rows: &[Row<'_>]) -> Fixture {
        let path = std::env::temp_dir()
            .join(format!("nt-pico-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).expect("open");
        conn.execute_batch(
            "CREATE TABLE torrent (info_hash TEXT PRIMARY KEY, queue_position INTEGER, label_id INTEGER);
             CREATE TABLE torrent_magnet_uri (info_hash TEXT, magnet_uri TEXT, save_path TEXT);
             CREATE TABLE torrent_resume_data (info_hash TEXT, resume_data BLOB);
             CREATE TABLE setting (id INTEGER PRIMARY KEY, key TEXT UNIQUE, value TEXT);",
        )
        .expect("schema");
        for (i, (hash, magnet, resume)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO torrent (info_hash, queue_position) VALUES (?1, ?2)",
                rusqlite::params![hash, i as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO torrent_magnet_uri (info_hash, magnet_uri, save_path) \
                 VALUES (?1, ?2, NULL)",
                rusqlite::params![hash, magnet],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO torrent_resume_data (info_hash, resume_data) VALUES (?1, ?2)",
                rusqlite::params![hash, resume],
            )
            .unwrap();
        }
        drop(conn);
        Fixture(path)
    }

    /// The number the import has to be honest about. A row with neither
    /// embedded metadata nor a magnet cannot be added from anything, and used
    /// to be dropped without a word - so "45 torrents" quietly became 42 and
    /// nothing said why.
    #[test]
    fn a_row_with_nothing_to_add_from_is_counted_not_dropped() {
        let db = fixture(
            "unreadable",
            &[
                ("aaa", Some("magnet:?xt=urn:btih:aaa"), None),
                ("bbb", None, Some(b"d4:infod6:lengthi5e4:name1:aee")),
                // Neither: nothing to reconstruct it from.
                ("ccc", None, None),
                ("ddd", Some(""), Some(b"")),
            ],
        );

        let scan = read_torrents(&db.0, &std::sync::atomic::AtomicBool::new(false)).expect("scan");
        assert_eq!(scan.entries.len(), 2, "aaa and bbb are addable");
        assert_eq!(scan.unreadable, 2, "ccc and ddd have nothing to add from");
    }

    /// Settings come across as the JSON they are stored as, which is the same
    /// shape this build stores its own in - so no conversion is needed and
    /// none is done.
    #[test]
    fn settings_are_read_as_stored() {
        let db = fixture("settings", &[]);
        let conn = rusqlite::Connection::open(&db.0).unwrap();
        conn.execute_batch(
            r#"INSERT INTO setting (key, value) VALUES ('default_save_path', '"/tmp/x"');
               INSERT INTO setting (key, value) VALUES ('move_completed', 'true');
               INSERT INTO setting (key, value) VALUES ('empty_one', '');
               INSERT INTO setting (key, value) VALUES ('null_one', NULL);"#,
        )
        .unwrap();
        drop(conn);

        let mut got = read_settings(&db.0).expect("settings");
        got.sort();
        assert_eq!(
            got,
            vec![
                (String::from("default_save_path"), String::from(r#""/tmp/x""#)),
                (String::from("move_completed"), String::from("true")),
            ],
            "blank and NULL values are not settings"
        );
    }

    #[test]
    fn skip_values() {
        assert_eq!(crate::core::bencode::skip(b"i42e", 0), Some(4));
        assert_eq!(crate::core::bencode::skip(b"3:abc", 0), Some(5));
        assert_eq!(crate::core::bencode::skip(b"l3:abci7ee", 0), Some(10));
        assert_eq!(crate::core::bencode::skip(b"d3:key5:valuee", 0), Some(14));
    }

    #[test]
    fn dict_get_raw_span() {
        // { "info": { "length": 100, "name": "abc" }, "save_path": "/tmp/x" }
        let blob = b"d4:infod6:lengthi100e4:name3:abce9:save_path6:/tmp/xe";
        let info = crate::core::bencode::dict_get(blob, b"info").unwrap();
        assert_eq!(info, b"d6:lengthi100e4:name3:abce");
        let sp = crate::core::bencode::dict_get(blob, b"save_path").unwrap();
        assert_eq!(crate::core::bencode::string(sp).unwrap(), b"/tmp/x");
        assert!(crate::core::bencode::dict_get(blob, b"missing").is_none());
    }

    #[test]
    fn reconstructed_torrent_wraps_info() {
        let blob = b"d4:infod6:lengthi5e4:name1:aee";
        let info = crate::core::bencode::dict_get(blob, b"info").unwrap();
        let mut torrent = Vec::new();
        torrent.extend_from_slice(b"d4:info");
        torrent.extend_from_slice(info);
        torrent.push(b'e');
        // A valid single-key metainfo dict.
        assert_eq!(torrent, b"d4:infod6:lengthi5e4:name1:aee".to_vec());
    }
}

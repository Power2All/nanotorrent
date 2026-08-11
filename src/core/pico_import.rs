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

pub enum ImportSource {
    /// A minimal `.torrent` reconstructed from the resume blob's `info` dict.
    TorrentBytes(Vec<u8>),
    Magnet(String),
}

pub struct ImportEntry {
    pub info_hash: String,
    pub source: ImportSource,
    pub save_path: Option<String>,
    pub label_id: Option<i32>,
}

/// Read every torrent PicoTorrent has stored in `db_path`.
pub fn read_torrents(db_path: &Path) -> Result<Vec<ImportEntry>> {
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
    for row in rows {
        let (info_hash, magnet, resume, mut save_path, label_id) = row?;
        let magnet = magnet.filter(|m| !m.is_empty());

        // Prefer reconstructing a .torrent from the resume blob's info dict;
        // fall back to the magnet link if there's no embedded metadata.
        let source = match resume.as_deref().filter(|b| !b.is_empty()) {
            Some(blob) => match bencode_dict_get(blob, b"info") {
                Some(info) => {
                    if save_path.as_deref().unwrap_or("").is_empty() {
                        save_path = bencode_dict_get(blob, b"save_path")
                            .and_then(bencode_string)
                            .map(|b| String::from_utf8_lossy(b).into_owned());
                    }
                    // A minimal but valid metainfo: d 4:info <info> e.
                    let mut bytes = Vec::with_capacity(info.len() + 8);
                    bytes.extend_from_slice(b"d4:info");
                    bytes.extend_from_slice(info);
                    bytes.push(b'e');
                    Some(ImportSource::TorrentBytes(bytes))
                }
                None => magnet.clone().map(ImportSource::Magnet),
            },
            None => magnet.clone().map(ImportSource::Magnet),
        };

        if let Some(source) = source {
            out.push(ImportEntry {
                info_hash,
                source,
                save_path: save_path.filter(|s| !s.is_empty()),
                label_id: label_id.filter(|&id| id > 0),
            });
        }
    }

    Ok(out)
}

// Minimal bencode scanning. We work on raw byte spans (rather than decoding and
// re-encoding) so the extracted `info` dict is byte-identical and keeps its
// original info-hash.

/// Index just past the bencoded value starting at `i`.
fn bencode_skip(data: &[u8], i: usize) -> Option<usize> {
    match data.get(i)? {
        b'i' => {
            let e = data[i + 1..].iter().position(|&b| b == b'e')?;
            Some(i + 1 + e + 1)
        }
        b'l' | b'd' => {
            let mut j = i + 1;
            while *data.get(j)? != b'e' {
                j = bencode_skip(data, j)?;
            }
            Some(j + 1)
        }
        b'0'..=b'9' => {
            let colon = data[i..].iter().position(|&b| b == b':')? + i;
            let len: usize = std::str::from_utf8(data.get(i..colon)?).ok()?.parse().ok()?;
            Some(colon + 1 + len)
        }
        _ => None,
    }
}

/// Raw bytes of `key`'s value in the top-level bencoded dict (`data` starts 'd').
fn bencode_dict_get<'a>(data: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    if *data.first()? != b'd' {
        return None;
    }
    let mut i = 1;
    while *data.get(i)? != b'e' {
        let kend = bencode_skip(data, i)?;
        let k = bencode_string(data.get(i..kend)?)?;
        let vend = bencode_skip(data, kend)?;
        if k == key {
            return data.get(kend..vend);
        }
        i = vend;
    }
    None
}

/// Decode a bencoded string token (`<len>:<bytes>`) into its raw bytes.
fn bencode_string(data: &[u8]) -> Option<&[u8]> {
    let colon = data.iter().position(|&b| b == b':')?;
    let len: usize = std::str::from_utf8(&data[..colon]).ok()?.parse().ok()?;
    data.get(colon + 1..colon + 1 + len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_values() {
        assert_eq!(bencode_skip(b"i42e", 0), Some(4));
        assert_eq!(bencode_skip(b"3:abc", 0), Some(5));
        assert_eq!(bencode_skip(b"l3:abci7ee", 0), Some(10));
        assert_eq!(bencode_skip(b"d3:key5:valuee", 0), Some(14));
    }

    #[test]
    fn dict_get_raw_span() {
        // { "info": { "length": 100, "name": "abc" }, "save_path": "/tmp/x" }
        let blob = b"d4:infod6:lengthi100e4:name3:abce9:save_path6:/tmp/xe";
        let info = bencode_dict_get(blob, b"info").unwrap();
        assert_eq!(info, b"d6:lengthi100e4:name3:abce");
        let sp = bencode_dict_get(blob, b"save_path").unwrap();
        assert_eq!(bencode_string(sp).unwrap(), b"/tmp/x");
        assert!(bencode_dict_get(blob, b"missing").is_none());
    }

    #[test]
    fn reconstructed_torrent_wraps_info() {
        let blob = b"d4:infod6:lengthi5e4:name1:aee";
        let info = bencode_dict_get(blob, b"info").unwrap();
        let mut torrent = Vec::new();
        torrent.extend_from_slice(b"d4:info");
        torrent.extend_from_slice(info);
        torrent.push(b'e');
        // A valid single-key metainfo dict.
        assert_eq!(torrent, b"d4:infod6:lengthi5e4:name1:aee".to_vec());
    }
}

//! Reading a `.torrent` far enough to show it in an "Add torrent" dialog.
//!
//! Shared rather than living in one UI: the desktop dialog and the web remote
//! (through `POST /api/torrents/inspect`) need the same name, size and file
//! list, and a second copy would be a second place for "(unnamed torrent)" to
//! drift - or worse, a second file ordering, since `only_files` indexes by it.

use librqbit::{ByteBufOwned, torrent_from_bytes};

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTorrent {
    pub name: String,
    pub total_size: i64,
    /// `(path, size)` in the order the metainfo lists them, which is also the
    /// order `Session::update_only_files` indexes by - so a dialog can hand
    /// back positions from this list directly.
    pub files: Vec<(String, u64)>,
}

/// Read a `.torrent` far enough to fill the Add dialog: name, total size and
/// the file list.
///
/// Deliberately not the engine's parser - this runs before anything is added,
/// so a malformed file has to come back as a message rather than as a failed
/// session operation.
pub fn parse(bytes: &[u8]) -> Result<ParsedTorrent, String> {
    let torrent = torrent_from_bytes::<ByteBufOwned>(bytes)
        .map_err(|err| format!("Failed to parse torrent file: {err:#}"))?;

    // A torrent with no name is malformed but not worth refusing over - the
    // info hash is what actually identifies it.
    let name = torrent
        .info
        .name
        .as_ref()
        .map(|b| String::from_utf8_lossy(b.as_ref()).into_owned())
        .unwrap_or_else(|| String::from("(unnamed torrent)"));

    let mut files = Vec::new();
    if let Ok(details) = torrent.info.iter_file_details() {
        for fd in details {
            files.push((
                fd.filename
                    .to_string()
                    .unwrap_or_else(|_| String::from("(invalid name)")),
                fd.len,
            ));
        }
    }

    let total_size = files.iter().map(|f| f.1 as i64).sum();

    Ok(ParsedTorrent {
        name,
        total_size,
        files,
    })
}

/// Pull magnet links out of pasted text, one per line.
///
/// A bare 40-character info hash counts too - people paste those from tracker
/// pages constantly, and refusing them helps nobody. Anything else on the line
/// is dropped rather than guessed at.
pub fn parse_magnet_links(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            line.starts_with("magnet:")
                || line.len() == 40 && line.chars().all(|c| c.is_ascii_hexdigit())
        })
        .map(|line| {
            if line.starts_with("magnet:") {
                line.to_string()
            } else {
                format!("magnet:?xt=urn:btih:{line}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal single-file v1 metainfo, built by hand so the test does not
    /// depend on a fixture file.
    fn single_file_torrent(name: &str, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"d4:infod6:lengthi");
        out.extend_from_slice(len.to_string().as_bytes());
        out.extend_from_slice(b"e4:name");
        out.extend_from_slice(name.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b"12:piece lengthi262144e6:pieces20:");
        out.extend_from_slice(&[0u8; 20]);
        out.extend_from_slice(b"ee");
        out
    }

    #[test]
    fn reads_name_size_and_files() {
        let parsed = parse(&single_file_torrent("Some.File.mkv", 4096)).unwrap();
        assert_eq!(parsed.name, "Some.File.mkv");
        assert_eq!(parsed.total_size, 4096);
        assert_eq!(parsed.files, vec![(String::from("Some.File.mkv"), 4096)]);
    }

    #[test]
    fn rejects_rubbish_rather_than_panicking() {
        // A dialog feeds this whatever the user picked, which may not be a
        // torrent at all.
        assert!(parse(b"not a torrent").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn magnet_links_and_bare_hashes_both_parse() {
        let text = "
            magnet:?xt=urn:btih:aaaabbbbccccddddeeeeffff00001111 22223333
            0123456789abcdef0123456789abcdef01234567
            not a link
            
            0123456789abcdef  
";
        let out = parse_magnet_links(text);
        assert_eq!(out.len(), 2, "got {out:?}");
        assert!(out[0].starts_with("magnet:?xt="));
        // A bare 40-char hash is wrapped rather than dropped.
        assert_eq!(out[1], "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567");
        // Too short to be an info hash, and not a magnet - dropped.
        assert!(!out.iter().any(|m| m.contains("0123456789abcdef\"")));
    }

    #[test]
    fn total_size_is_the_sum_of_the_files() {
        let parsed = parse(&single_file_torrent("a", 1)).unwrap();
        assert_eq!(
            parsed.total_size,
            parsed.files.iter().map(|f| f.1 as i64).sum::<i64>()
        );
    }
}

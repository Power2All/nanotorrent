//! Reading a `.torrent` far enough to show it in an "Add torrent" dialog.
//!
//! Shared rather than living in one UI: the desktop dialog and the web remote
//! (through `POST /api/torrents/inspect`) need the same name, size and file
//! list, and a second copy would be a second place for "(unnamed torrent)" to
//! drift - or worse, a second file ordering, since `only_files` indexes by it.

use librqbit::torrent_from_bytes;

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
    /// Position in the torrent's own file list. This is what `only_files`
    /// indexes by, and it is carried explicitly because padding files may be
    /// hidden - once anything is filtered out, a row's position in the list
    /// the user sees is no longer its index in the torrent.
    pub index: usize,
    pub path: String,
    pub size: u64,
    /// A BEP 47 padding file: a run of zeros inserted so the next real file
    /// starts on a piece boundary. Not content, and nothing is downloaded for
    /// it - the engine synthesises the zeros.
    pub padding: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTorrent {
    pub name: String,
    /// Real content only: padding is excluded, because it is not data anyone
    /// is choosing to download.
    pub total_size: i64,
    /// Every file the metainfo lists, padding included and flagged. Callers
    /// decide whether to show padding (`ui.show_padding_files`) but must use
    /// [`ParsedFile::index`] rather than position when handing back
    /// `only_files`.
    pub files: Vec<ParsedFile>,
}

/// Read a `.torrent` far enough to fill the Add dialog: name, total size and
/// the file list.
///
/// Deliberately not the engine's parser - this runs before anything is added,
/// so a malformed file has to come back as a message rather than as a failed
/// session operation.
pub fn parse(bytes: &[u8]) -> Result<ParsedTorrent, String> {
    // A v2-only torrent has no `pieces` key, so librqbit's parser fails on it
    // deep inside serde with "missing field `pieces`". We read those ourselves
    // (BEP 52 lists files in a nested `file tree` instead), and the engine can
    // download them - so this has to answer for them too, or the Add dialog
    // would refuse a torrent the session would happily take.
    //
    // Tried first, not as a fallback: it is a cheap key lookup, and doing it
    // this way keeps the error message for a genuinely broken file coming from
    // the v1 parser, which has more to say about one.
    match crate::bittorrent::v2::parse(bytes) {
        Ok(Some(meta)) if !meta.has_v1 => return Ok(parsed_from_v2(&meta)),
        // A hybrid is read through its v1 half below, the same way it is
        // downloaded. Err means the v2 half is malformed; fall through and let
        // the v1 parser have its say, since a hybrid may still be usable.
        Ok(_) | Err(_) => {}
    }

    let torrent = torrent_from_bytes(bytes)
        .map_err(|err| format!("Failed to parse torrent file: {err:#}"))?;

    // librqbit 9 validates the info dict (piece lengths, encoding, file list)
    // up front rather than on each accessor. Same failure mode as above: a
    // malformed file has to come back as a message, not a panic.
    let info = torrent
        .info
        .data
        .validate()
        .map_err(|err| format!("Failed to parse torrent file: {err:#}"))?;

    // A torrent with no name is malformed but not worth refusing over - the
    // info hash is what actually identifies it.
    let name = info
        .name()
        .map(|n| n.into_owned())
        .unwrap_or_else(|| String::from("(unnamed torrent)"));

    let mut files = Vec::new();
    for (index, fd) in info.iter_file_details().enumerate() {
        // librqbit 9 decodes the name with the torrent's own encoding, so
        // this is infallible where it used to return a Result.
        files.push(ParsedFile {
            index,
            path: fd.filename.to_string(),
            size: fd.len,
            padding: fd.attrs().padding,
        });
    }

    let total_size = files
        .iter()
        .filter(|f| !f.padding)
        .map(|f| f.size as i64)
        .sum();

    Ok(ParsedTorrent {
        name,
        total_size,
        files,
    })
}

/// Fill the dialog from a v2 `file tree`.
///
/// A v2 file tree has no padding files in it at all - alignment is implicit,
/// which is the whole point of hashing per file. The padding only appears in
/// the v1-shaped model the engine is given, so indices here are already the
/// engine's own.
fn parsed_from_v2(meta: &crate::bittorrent::v2::V2Meta) -> ParsedTorrent {
    let files: Vec<ParsedFile> = meta
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| ParsedFile {
            index,
            path: f.path(),
            size: f.length,
            padding: false,
        })
        .collect();
    ParsedTorrent {
        total_size: files.iter().map(|f| f.size as i64).sum(),
        name: meta.name.clone(),
        files,
    }
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
        assert_eq!(
            parsed.files,
            vec![ParsedFile {
                index: 0,
                path: String::from("Some.File.mkv"),
                size: 4096,
                padding: false,
            }]
        );
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

    /// Padding files must be flagged, excluded from the total, and must NOT
    /// shift anyone's index.
    ///
    /// This is the trap the whole `ParsedFile::index` field exists for: a
    /// hybrid torrent has padding files between its real ones, so once they
    /// are hidden a row's position in the list stops being its index in the
    /// torrent - and `only_files` indexes by the latter. Get it wrong and
    /// ticking two files downloads a different two.
    #[test]
    fn padding_is_flagged_and_does_not_shift_indices() {
        use crate::bittorrent::torrent_create::{CreateInput, TorrentVersion, build};

        let dir = std::env::temp_dir().join(format!("nt-pad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("d");
        std::fs::create_dir_all(&sub).unwrap();
        // The first file does not end on a piece boundary, so a hybrid has to
        // insert a padding file after it.
        std::fs::write(sub.join("a.bin"), vec![1u8; 20_000]).unwrap();
        std::fs::write(sub.join("b.bin"), vec![2u8; 8_000]).unwrap();

        let bytes = build(&CreateInput {
            source: &sub,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(16384),
            version: TorrentVersion::Hybrid,
        })
        .unwrap()
        .bytes;

        let parsed = parse(&bytes).unwrap();
        let pads: Vec<&ParsedFile> = parsed.files.iter().filter(|f| f.padding).collect();
        assert_eq!(pads.len(), 1, "expected exactly one padding file");
        assert!(pads[0].path.contains(".pad"), "{:?}", pads[0].path);

        // Indices are the torrent's own, padding included, so hiding padding
        // cannot make them wrong.
        for (position, f) in parsed.files.iter().enumerate() {
            assert_eq!(f.index, position, "indices must be the metainfo order");
        }
        // b.bin is index 2 even though it is the SECOND visible file.
        let visible: Vec<&ParsedFile> = parsed.files.iter().filter(|f| !f.padding).collect();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[1].index, 2, "hiding padding shifted an index");

        // And the total is content only.
        assert_eq!(
            parsed.total_size, 28_000,
            "padding was counted towards the size"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_size_is_the_sum_of_the_files() {
        let parsed = parse(&single_file_torrent("a", 1)).unwrap();
        assert_eq!(
            parsed.total_size,
            parsed.files.iter().map(|f| f.size as i64).sum::<i64>()
        );
    }
}

//! Torrent creation: v1, v2 (BEP 52) and hybrid.
//!
//! librqbit only creates v1 torrents, and not reliably enough to keep (see
//! `session::build_torrent`), so all three are built here from scratch:
//!
//! - **v1**: SHA-1 over `piece length` pieces of the files laid end to end,
//!   with `length` for a single file and a `files` list for a folder.
//! - **v2**: SHA-256 merkle trees over 16 KiB blocks (`pieces root` per file),
//!   a nested `file tree`, `piece layers`, and `meta version = 2`.
//! - **hybrid**: the same info dict *also* carries the v1 fields (`pieces`,
//!   `files`/`length`) describing identical data, with BEP 47 padding files so
//!   the v1 layout aligns each file to a piece boundary.
//!
//! Everything is deterministic and unit-tested (merkle vectors, bencode,
//! structure), but interop against a real v2 client (libtorrent 2.x) is the
//! ultimate check - see the tests at the bottom.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha1::Sha1;
use sha2::{Digest, Sha256};

pub(crate) const BLOCK: usize = 16 * 1024; // 16 KiB v2 block size

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TorrentVersion {
    V1,
    V2,
    Hybrid,
}

impl TorrentVersion {
    /// Map a combo-box index to a version, defaulting to v1 for anything out
    /// of range - v1 is the format every client can still read.
    pub fn from_index(i: usize) -> TorrentVersion {
        match i {
            1 => TorrentVersion::V2,
            2 => TorrentVersion::Hybrid,
            _ => TorrentVersion::V1,
        }
    }
}

// Minimal bencode encoder. A hand-rolled encoder is simpler than coercing
// serde to emit this bespoke, deeply-nested structure (a recursive file tree
// and a dict keyed by raw 32-byte hashes).

pub enum Ben {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Ben>),
    /// Keys are raw byte strings; sorted at encode time as bencode requires.
    Dict(Vec<(Vec<u8>, Ben)>),
}

impl Ben {
    /// A bencode byte string from UTF-8 text. Bencode has no string type of
    /// its own - everything is bytes - so this is just the common case.
    pub(crate) fn s(text: &str) -> Ben {
        Ben::Bytes(text.as_bytes().to_vec())
    }

    /// One entry of a v1 `files` list.
    pub(crate) fn file_entry(length: u64, components: &[String]) -> Ben {
        Ben::Dict(vec![
            (b"length".to_vec(), Ben::Int(length as i64)),
            (b"path".to_vec(), Ben::List(components.iter().map(|c| Ben::s(c)).collect())),
        ])
    }

    /// A BEP 47 padding file of `pad` bytes, for a v1 `files` list.
    pub(crate) fn pad_entry(pad: u64) -> Ben {
        Ben::Dict(vec![
            (b"attr".to_vec(), Ben::s("p")),
            (b"length".to_vec(), Ben::Int(pad as i64)),
            (b"path".to_vec(), Ben::List(vec![Ben::s(".pad"), Ben::s(&pad.to_string())])),
        ])
    }

    /// Append the bencoded form to `out`.
    ///
    /// Dictionary keys are sorted here rather than at construction. Bencode
    /// requires byte order, and the info dict's hash is the torrent's
    /// identity, so the wrong order produces a different info hash and a
    /// torrent nobody else can find.
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Ben::Int(n) => {
                out.push(b'i');
                out.extend_from_slice(n.to_string().as_bytes());
                out.push(b'e');
            }
            Ben::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            Ben::List(items) => {
                out.push(b'l');
                for it in items {
                    it.encode(out);
                }
                out.push(b'e');
            }
            Ben::Dict(entries) => {
                out.push(b'd');
                let mut sorted: Vec<&(Vec<u8>, Ben)> = entries.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                for (k, v) in sorted {
                    Ben::Bytes(k.clone()).encode(out);
                    v.encode(out);
                }
                out.push(b'e');
            }
        }
    }

    /// The complete bencoded form as a fresh buffer.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

// Hashing helpers

/// SHA-1 of one buffer - v1 piece hashes and the v1 info hash.
fn sha1(data: &[u8]) -> [u8; 20] {
    Sha1::digest(data).into()
}

/// SHA-256 of one buffer - v2 block hashes and the v2 info hash (BEP 52).
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// One interior node of a v2 merkle tree: SHA-256 over two child hashes.
pub(crate) fn hash_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    h.finalize().into()
}

/// Merkle root over `leaves` (already padded to a power-of-two count).
pub(crate) fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    let mut layer = leaves.to_vec();
    while layer.len() > 1 {
        layer = layer.chunks(2).map(|p| hash_pair(&p[0], &p[1])).collect();
    }
    layer[0]
}

// File walking

struct SrcFile {
    /// Path components relative to the torrent root (UTF-8).
    components: Vec<String>,
    abs: PathBuf,
    length: u64,
}

/// A file or folder name as the UTF-8 a torrent stores.
///
/// Refused rather than converted lossily: a name with a replacement character
/// in it no longer matches the file on disk, so the torrent would hash fine
/// and then fail to seed from the very folder it was made from.
fn utf8_name(name: &std::ffi::OsStr) -> Result<String> {
    name.to_str()
        .map(str::to_owned)
        .with_context(|| format!("file name is not valid Unicode: {}", name.to_string_lossy()))
}

/// Collect the files to include, sorted by path. For a single file the one
/// component is its name; for a directory, paths are relative to it.
///
/// The third element says whether the SOURCE was a single file. That is not
/// the same as "there is one file": a directory holding exactly one entry also
/// yields one file, but its torrent name is the directory, so it must still be
/// laid out as a multi-file torrent.
fn collect_files(source: &Path) -> Result<(String, Vec<SrcFile>, bool)> {
    let name = utf8_name(source.file_name().context("source has no file name")?)?;

    if source.is_file() {
        let length = source.metadata()?.len();
        return Ok((
            name.clone(),
            vec![SrcFile {
                components: vec![name],
                abs: source.to_path_buf(),
                length,
            }],
            true,
        ));
    }

    let mut files = Vec::new();
    walk(source, &mut Vec::new(), &mut Vec::new(), &mut files)?;
    // Deterministic order (bencode also requires sorted keys; this keeps the
    // v1 `files` list and v2 tree consistent).
    files.sort_by(|a, b| a.components.cmp(&b.components));
    Ok((name, files, false))
}

/// Collect every file under `dir`, depth-first, with path components relative
/// to the torrent root.
///
/// Entries are sorted by name at each level, so the same folder always
/// produces the same torrent - and therefore the same info hash.
///
/// `is_dir`/`is_file` follow symlinks, so a link to a file is included as that
/// file. Anything that is neither after following (a broken link, a socket, a
/// fifo) is skipped silently; a file that cannot be stat'ed fails the build,
/// because a torrent missing a file it was asked to include is worse than none.
///
/// A link back to a folder already being walked is skipped: following it would
/// list the same files again one level deeper, and again, until the path grew
/// too long for the OS - a torrent of thousands of copies, or no torrent at
/// all. `ancestors` holds the canonical path of each folder on the way down.
fn walk(
    dir: &Path,
    prefix: &mut Vec<String>,
    ancestors: &mut Vec<PathBuf>,
    out: &mut Vec<SrcFile>,
) -> Result<()> {
    let here = dir.canonicalize()?;
    if ancestors.contains(&here) {
        tracing::warn!("not following {}: it loops back to a parent folder", dir.display());
        return Ok(());
    }
    ancestors.push(here);
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = utf8_name(&entry.file_name())?;
        if path.is_dir() {
            prefix.push(name);
            walk(&path, prefix, ancestors, out)?;
            prefix.pop();
        } else if path.is_file() {
            let mut components = prefix.clone();
            components.push(name);
            let length = path.metadata()?.len();
            out.push(SrcFile {
                components,
                abs: path,
                length,
            });
        }
    }
    ancestors.pop();
    Ok(())
}

// v2 per-file merkle (pieces root + piece layer)

struct FileV2 {
    /// None for empty files (which have no `pieces root`).
    pieces_root: Option<[u8; 32]>,
    /// Concatenated piece-layer hashes; empty for single-piece (or empty) files.
    piece_layer: Vec<u8>,
}

/// Build one file's v2 merkle data: its pieces root and piece layer.
///
/// Per file, not per torrent - that is the v2 change. Each file is hashed
/// independently in 16 KiB blocks, which is what lets two torrents share a
/// file without sharing a piece alignment.
fn hash_file_v2(path: &Path, piece_length: u32) -> Result<FileV2> {
    let blocks_per_piece = piece_length as usize / BLOCK;
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;

    // SHA-256 each 16 KiB block; the final block is hashed as-is (not padded).
    let mut leaves: Vec<[u8; 32]> = Vec::new();
    // A short read only happens at EOF.
    let mut buf = Vec::with_capacity(BLOCK);
    loop {
        buf.clear();
        let n = (&mut f).take(BLOCK as u64).read_to_end(&mut buf)?;
        if n == 0 {
            break;
        }
        leaves.push(sha256(&buf));
        if n < BLOCK {
            break;
        }
    }

    let num_blocks = leaves.len();
    if num_blocks == 0 {
        return Ok(FileV2 {
            pieces_root: None,
            piece_layer: Vec::new(),
        });
    }

    // Pad the leaves to a power of two with zero-hashes, then take the root.
    let padded = num_blocks.next_power_of_two();
    leaves.resize(padded, [0u8; 32]);
    let pieces_root = merkle_root(&leaves);

    // Single-piece files (<= one piece of data) are omitted from piece layers.
    let piece_layer = if num_blocks <= blocks_per_piece {
        Vec::new()
    } else {
        // Reduce up to the layer where each node spans exactly one piece.
        let mut layer = leaves;
        let mut span = 1usize;
        while span < blocks_per_piece {
            layer = layer.chunks(2).map(|p| hash_pair(&p[0], &p[1])).collect();
            span *= 2;
        }
        // Keep only nodes that cover real data; trailing all-padding pieces are
        // "beyond the end of file" and omitted.
        let real_pieces = num_blocks.div_ceil(blocks_per_piece);
        let mut out = Vec::with_capacity(real_pieces * 32);
        for node in layer.iter().take(real_pieces) {
            out.extend_from_slice(node);
        }
        out
    };

    Ok(FileV2 {
        pieces_root: Some(pieces_root),
        piece_layer,
    })
}

// v1 piece hashing (for hybrid), with BEP 47 padding-file alignment

struct V1Hasher {
    piece_length: usize,
    cur: Vec<u8>,
    pieces: Vec<u8>,
}

impl V1Hasher {
    /// A hasher that emits one SHA-1 per `piece_length` bytes fed through it.
    fn new(piece_length: usize) -> Self {
        V1Hasher {
            piece_length,
            cur: Vec::with_capacity(piece_length),
            pieces: Vec::new(),
        }
    }

    /// Feed bytes in, hashing each complete piece as it fills.
    ///
    /// Takes arbitrary-sized chunks and buffers the remainder, so callers can
    /// stream a file in whatever read size they like without knowing where the
    /// piece boundaries fall.
    fn push(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = (self.piece_length - self.cur.len()).min(data.len());
            self.cur.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.cur.len() == self.piece_length {
                self.pieces.extend_from_slice(&sha1(&self.cur));
                self.cur.clear();
            }
        }
    }

    /// Zero-pad up to the next piece boundary; returns the padding length (0 if
    /// already aligned). The padding bytes are hashed into the current piece.
    fn pad_to_piece(&mut self) -> usize {
        let pad = (self.piece_length - self.cur.len()) % self.piece_length;
        if pad > 0 {
            let zeros = vec![0u8; pad];
            self.push(&zeros);
        }
        pad
    }

    /// The concatenated piece hashes, flushing a final partial piece.
    ///
    /// The last piece of a torrent is short unless the total happens to divide
    /// evenly, and it is hashed at its real length - not padded.
    fn finish(mut self) -> Vec<u8> {
        if !self.cur.is_empty() {
            self.pieces.extend_from_slice(&sha1(&self.cur));
        }
        self.pieces
    }
}

/// Read a file in fixed-size chunks, handing each to `sink`.
///
/// Streamed rather than read whole: torrents are made from files far larger
/// than memory.
fn stream_file(path: &Path, mut sink: impl FnMut(&[u8])) -> Result<()> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; BLOCK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sink(&buf[..n]);
    }
    Ok(())
}

// file tree (v2)

/// Insert a file into the nested `file tree` dict.
fn tree_insert(tree: &mut Vec<(Vec<u8>, Ben)>, comps: &[String], leaf: Ben) {
    let key = comps[0].as_bytes().to_vec();
    if comps.len() == 1 {
        tree.push((key, leaf));
        return;
    }
    // Find or create the sub-dict for comps[0].
    let entry = tree.iter_mut().find(|(k, _)| *k == key);
    let sub = match entry {
        Some((_, Ben::Dict(d))) => d,
        _ => {
            tree.push((key.clone(), Ben::Dict(Vec::new())));
            match &mut tree.last_mut().unwrap().1 {
                Ben::Dict(d) => d,
                _ => unreachable!(),
            }
        }
    };
    tree_insert(sub, &comps[1..], leaf);
}

// Public builder

pub struct CreateInput<'a> {
    pub source: &'a Path,
    pub version: TorrentVersion,
    /// Already validated: power of two, multiple of 16 KiB. `None` = auto.
    pub piece_length: Option<u32>,
    pub trackers: &'a [String],
    pub comment: &'a str,
    pub private: bool,
    pub created_by: String,
}

/// A power-of-two piece length aimed at a reasonable piece count.
pub fn auto_piece_length(total: u64) -> u32 {
    let mut pl: u64 = 256 * 1024;
    while total / pl > 2000 && pl < 16 * 1024 * 1024 {
        pl *= 2;
    }
    pl as u32
}

/// Validate a piece length: a power of two, at least 16 KiB.
///
/// v2 needs both - a piece is a subtree of 16 KiB merkle blocks. v1 alone
/// would take any size, but every client picks a power of two and the dialog
/// only offers those, so one rule serves all three versions.
pub fn validate_piece_length(pl: u32) -> Result<u32> {
    if pl < BLOCK as u32 {
        bail!("piece size must be at least 16 KiB");
    }
    if !pl.is_power_of_two() {
        bail!("piece size must be a power of two");
    }
    Ok(pl)
}

/// Build a finished `.torrent` of any version from a file or folder.
pub fn build(input: &CreateInput) -> Result<Vec<u8>> {
    let (name, files, source_is_file) = collect_files(input.source)?;
    if files.is_empty() {
        bail!("no files to add");
    }
    let total: u64 = files.iter().map(|f| f.length).sum();
    let piece_length =
        validate_piece_length(input.piece_length.unwrap_or_else(|| auto_piece_length(total)))?;
    // Whether the SOURCE was one file, not whether one file was found. A
    // directory containing a single entry used to satisfy the old
    // `files.len() == 1 && components.len() == 1` test and produced a hybrid
    // torrent that contradicted itself: v1 declared a single file named after
    // the DIRECTORY while the v2 file tree held the real filename inside it.
    // librqbit then tried to open the directory as a file and refused with
    // "Access is denied".
    let single = source_is_file;
    let hybrid = input.version == TorrentVersion::Hybrid;
    // Which halves this torrent carries. A hybrid carries both; the other two
    // carry exactly one. Getting this wrong is not a cosmetic problem: an info
    // dict with `meta version` but no `pieces` IS a v2-only torrent whatever
    // the dialog said, and no v1 client can read it.
    let want_v2 = matches!(input.version, TorrentVersion::V2 | TorrentVersion::Hybrid);
    let want_v1 = matches!(input.version, TorrentVersion::V1 | TorrentVersion::Hybrid);

    // --- v2: file tree + piece layers ------------------------------------
    let mut file_tree: Vec<(Vec<u8>, Ben)> = Vec::new();
    let mut piece_layers: Vec<(Vec<u8>, Ben)> = Vec::new();
    if want_v2 {
        for f in &files {
            let v2 = hash_file_v2(&f.abs, piece_length)?;
            let mut leaf_info: Vec<(Vec<u8>, Ben)> =
                vec![(b"length".to_vec(), Ben::Int(f.length as i64))];
            if let Some(root) = v2.pieces_root {
                leaf_info.push((b"pieces root".to_vec(), Ben::Bytes(root.to_vec())));
                if !v2.piece_layer.is_empty() {
                    piece_layers.push((root.to_vec(), Ben::Bytes(v2.piece_layer)));
                }
            }
            let leaf = Ben::Dict(vec![(b"".to_vec(), Ben::Dict(leaf_info))]);
            tree_insert(&mut file_tree, &f.components, leaf);
        }
    }

    // --- info dict -------------------------------------------------------
    let mut info: Vec<(Vec<u8>, Ben)> = vec![
        (b"name".to_vec(), Ben::s(&name)),
        (b"piece length".to_vec(), Ben::Int(piece_length as i64)),
    ];
    if want_v2 {
        info.push((b"meta version".to_vec(), Ben::Int(2)));
        info.push((b"file tree".to_vec(), Ben::Dict(file_tree)));
    }
    if input.private {
        info.push((b"private".to_vec(), Ben::Int(1)));
    }

    // --- v1 fields (v1-only and hybrid) -----------------------------------
    // Padding files are a HYBRID requirement, not a v1 one: they exist so the
    // v1 layout aligns each file to a piece boundary the way v2 already does.
    // A v1-only torrent has no second layout to agree with, and padding files
    // in one would just be dead weight older clients have to skip.
    if want_v1 {
        let mut hasher = V1Hasher::new(piece_length as usize);
        if single {
            stream_file(&files[0].abs, |chunk| hasher.push(chunk))?;
            info.push((b"length".to_vec(), Ben::Int(files[0].length as i64)));
        } else {
            let mut v1_files: Vec<Ben> = Vec::new();
            for (idx, f) in files.iter().enumerate() {
                stream_file(&f.abs, |chunk| hasher.push(chunk))?;
                v1_files.push(Ben::file_entry(f.length, &f.components));
                // Align every file but the last to a piece boundary with a
                // BEP 47 padding file. Hybrid only - see above.
                if hybrid && idx + 1 < files.len() {
                    let pad = hasher.pad_to_piece();
                    if pad > 0 {
                        v1_files.push(Ben::pad_entry(pad as u64));
                    }
                }
            }
            info.push((b"files".to_vec(), Ben::List(v1_files)));
        }
        info.push((b"pieces".to_vec(), Ben::Bytes(hasher.finish())));
    }

    let info = Ben::Dict(info);

    // --- outer dict ------------------------------------------------------
    let mut root: Vec<(Vec<u8>, Ben)> = Vec::new();
    if let Some(first) = input.trackers.first() {
        root.push((b"announce".to_vec(), Ben::s(first)));
        root.push((
            b"announce-list".to_vec(),
            Ben::List(
                input
                    .trackers
                    .iter()
                    .map(|t| Ben::List(vec![Ben::s(t)]))
                    .collect(),
            ),
        ));
    }
    if !input.comment.is_empty() {
        root.push((b"comment".to_vec(), Ben::s(input.comment)));
    }
    root.push((b"created by".to_vec(), Ben::s(&input.created_by)));
    root.push((
        b"creation date".to_vec(),
        Ben::Int(chrono::Utc::now().timestamp()),
    ));
    root.push((b"info".to_vec(), info));
    // Only ever present alongside a file tree; `want_v2` already gates that,
    // but the emptiness check also covers a torrent whose files all fit in a
    // single piece and so have no layer of their own.
    if !piece_layers.is_empty() {
        root.push((b"piece layers".to_vec(), Ben::Dict(piece_layers)));
    }

    Ok(Ben::Dict(root).to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Each version must emit ITS OWN keys and no others.
    ///
    /// This exists because "v1" once emitted `meta version` and a `file tree`
    /// and no `pieces` at all - byte-identical to a v2-only torrent. The
    /// dialog said v1, the file said v2, no v1 client could read it, and
    /// NanoTorrent itself then refused to add it. Nothing caught it because
    /// the only structural tests covered v2 and hybrid.
    #[test]
    fn each_version_emits_only_its_own_keys() {
        let dir = std::env::temp_dir().join(format!("nt-versions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = dir.join("payload.bin");
        std::fs::write(&payload, vec![7u8; 40_000]).unwrap();

        let make = |version| {
            build(&CreateInput {
                source: &payload,
                trackers: &[],
                comment: "",
                created_by: "test".into(),
                private: false,
                piece_length: Some(16384),
                version,
            })
            .unwrap()
        };
        // Look inside the info dict, not the whole file: `length` also appears
        // in every v2 file-tree leaf, so a whole-buffer search proves nothing.
        let info_has = |bytes: &[u8], key: &[u8]| {
            let info = crate::core::bencode::dict_get(bytes, b"info").unwrap();
            let mut probe = Vec::new();
            probe.extend_from_slice(key.len().to_string().as_bytes());
            probe.push(b':');
            probe.extend_from_slice(key);
            info.windows(probe.len()).any(|w| w == probe)
        };

        let v1 = make(TorrentVersion::V1);
        assert!(info_has(&v1, b"pieces"), "v1 has no pieces");
        assert!(info_has(&v1, b"length"), "v1 single-file has no length");
        assert!(
            !info_has(&v1, b"meta version"),
            "v1 declared meta version - that makes it a v2 torrent"
        );
        assert!(!info_has(&v1, b"file tree"), "v1 carries a v2 file tree");

        let v2 = make(TorrentVersion::V2);
        assert!(info_has(&v2, b"meta version"), "v2 has no meta version");
        assert!(info_has(&v2, b"file tree"), "v2 has no file tree");
        assert!(!info_has(&v2, b"pieces"), "v2-only carries v1 piece hashes");

        let hy = make(TorrentVersion::Hybrid);
        assert!(info_has(&hy, b"meta version"), "hybrid lost its v2 half");
        assert!(info_has(&hy, b"file tree"), "hybrid lost its file tree");
        assert!(info_has(&hy, b"pieces"), "hybrid lost its v1 half");

        // And the three must actually differ from one another.
        assert_ne!(v1, v2, "v1 and v2 produced identical bytes");
        assert_ne!(v2, hy, "v2 and hybrid produced identical bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Helper for manual testing: build a torrent of any shape from a folder.
    ///   NT_SRC=<dir> NT_OUT=<file.torrent> NT_VERSION=hybrid \
    ///     cargo test --bin nanotorrent-gui make_shaped_torrent -- --ignored
    #[test]
    #[ignore = "writes a torrent from NT_SRC to NT_OUT"]
    fn make_shaped_torrent() {
        let src = std::path::PathBuf::from(std::env::var("NT_SRC").unwrap());
        let out = std::path::PathBuf::from(std::env::var("NT_OUT").unwrap());
        let version = match std::env::var("NT_VERSION").unwrap_or_default().as_str() {
            "v2" => TorrentVersion::V2,
            "hybrid" => TorrentVersion::Hybrid,
            _ => TorrentVersion::V1,
        };
        let built = build(&CreateInput {
            source: &src,
            trackers: &[],
            comment: "",
            created_by: "nanotorrent gui test".into(),
            private: false,
            piece_length: Some(16384),
            version,
        })
        .unwrap();
        std::fs::write(&out, &built).unwrap();
        println!("wrote {} ({} bytes)", out.display(), built.len());
    }

    #[test]
    fn bencode_basic() {
        let d = Ben::Dict(vec![
            (b"b".to_vec(), Ben::s("x")),
            (b"a".to_vec(), Ben::Int(1)),
        ]);
        // Keys must come out sorted: a before b.
        assert_eq!(d.to_bytes(), b"d1:ai1e1:b1:xe");
        assert_eq!(Ben::List(vec![Ben::Int(0)]).to_bytes(), b"li0ee");
    }

    #[test]
    fn merkle_vectors() {
        let h = |b: u8| [b; 32];
        // One leaf: the root is the leaf.
        assert_eq!(merkle_root(&[h(1)]), h(1));
        // Two leaves: sha256(l0 || l1).
        assert_eq!(merkle_root(&[h(1), h(2)]), hash_pair(&h(1), &h(2)));
        // Four leaves: balanced tree.
        let expect = hash_pair(&hash_pair(&h(1), &h(2)), &hash_pair(&h(3), &h(4)));
        assert_eq!(merkle_root(&[h(1), h(2), h(3), h(4)]), expect);
    }

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nt-tc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }


    /// A directory holding exactly ONE file must still be a multi-file torrent.
    ///
    /// It used to come out as a single-file one named after the directory,
    /// while the v2 file tree carried the real filename - a hybrid torrent that
    /// contradicted itself. librqbit then tried to open the directory as a file
    /// and refused with "Access is denied", so the torrent could be created but
    /// never seeded.
    #[test]
    fn a_directory_with_one_file_is_still_multi_file() {
        let dir = std::env::temp_dir().join(format!("nt-tc-onefile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        File::create(dir.join("inner.bin"))
            .unwrap()
            .write_all(&vec![7u8; 40_000])
            .unwrap();

        let (name, files, source_is_file) = collect_files(&dir).unwrap();
        assert_eq!(name, dir.file_name().unwrap().to_string_lossy());
        assert_eq!(files.len(), 1);
        assert!(
            !source_is_file,
            "a directory source must never be treated as a single file"
        );
        // The one file keeps its own name, which is what the v2 tree uses -
        // so v1 must list it too rather than collapsing to `length`.
        assert_eq!(files[0].components, vec![String::from("inner.bin")]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the same rule: a real single-file source stays single.
    #[test]
    fn a_file_source_is_single() {
        let path = write_temp("solo.bin", &vec![3u8; 1000]);
        let (name, files, source_is_file) = collect_files(&path).unwrap();
        assert!(source_is_file);
        assert_eq!(name, "solo.bin");
        assert_eq!(files[0].components, vec![String::from("solo.bin")]);
    }

    #[test]
    fn v2_single_file_structure() {
        // A file spanning ~2.5 pieces at 16 KiB pieces (1 block/piece).
        let data = vec![0xABu8; BLOCK * 2 + 100];
        let path = write_temp("v2single.bin", &data);
        let built = build(&CreateInput {
            source: &path,
            version: TorrentVersion::V2,
            piece_length: Some(BLOCK as u32), // 1 block per piece
            trackers: &["http://tracker.example/announce".into()],
            comment: "hi",
            private: false,
            created_by: "test".into(),
        })
        .unwrap();
        let s = built;
        // Structural checks (bencode substrings).
        assert!(contains(&s, b"12:meta versioni2e"), "meta version");
        assert!(contains(&s, b"9:file tree"), "file tree");
        assert!(contains(&s, b"12:piece layers"), "piece layers");
        assert!(contains(&s, b"11:pieces root32:"), "pieces root 32 bytes");
        // Pure v2 must NOT carry a v1 pieces field.
        assert!(!contains(&s, b"6:pieces2"), "no v1 pieces");
    }

    #[test]
    fn hybrid_has_both_formats() {
        let a = write_temp("h_a.bin", &vec![1u8; BLOCK + 50]);
        let dir = a.parent().unwrap().to_path_buf();
        let b = dir.join("h_b.bin");
        File::create(&b).unwrap().write_all(&[2u8; 200]).unwrap();
        // Build from the directory (multi-file → padding files exercised).
        let built = build(&CreateInput {
            source: &dir,
            version: TorrentVersion::Hybrid,
            piece_length: Some(BLOCK as u32),
            trackers: &[],
            comment: "",
            private: true,
            created_by: "test".into(),
        })
        .unwrap();
        let s = built;
        assert!(contains(&s, b"12:meta versioni2e"), "v2 meta version");
        assert!(contains(&s, b"9:file tree"), "v2 file tree");
        assert!(contains(&s, b"6:pieces"), "v1 pieces");
        assert!(contains(&s, b"5:files"), "v1 files list");
        assert!(contains(&s, b"4:attr1:p"), "BEP47 padding file");
        assert!(contains(&s, b"7:privatei1e"), "private flag");
    }

    #[test]
    fn piece_length_validation() {
        assert!(validate_piece_length(BLOCK as u32).is_ok());
        assert!(validate_piece_length(256 * 1024).is_ok());
        assert!(validate_piece_length(1000).is_err()); // too small
        assert!(validate_piece_length(3 * BLOCK as u32).is_err()); // not power of two
    }

    /// A fresh, empty scratch folder for one test.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nt-tc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write `len` bytes of a pattern that differs per `seed`, creating parent
    /// folders. Patterned rather than constant so that two files, or a file
    /// and its own shifted copy, never hash alike by accident.
    fn put(root: &Path, rel: &str, len: usize, seed: u8) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data: Vec<u8> = (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect();
        std::fs::write(path, data).unwrap();
    }

    fn v1(source: &Path, piece_length: u32) -> Vec<u8> {
        build(&CreateInput {
            source,
            version: TorrentVersion::V1,
            piece_length: Some(piece_length),
            trackers: &[],
            comment: "",
            private: false,
            created_by: "test".into(),
        })
        .unwrap()
    }

    /// Check a v1 torrent against the data it was made from, the way a
    /// downloading peer would: parse it with the ENGINE's parser (not ours),
    /// lay the listed files end to end in the listed order, and SHA-1 every
    /// piece. Independent of how `build` hashes, so it catches a builder that
    /// is self-consistent but wrong. A hybrid's BEP 47 padding files are not on
    /// disk; they read as zeros, which is what they are defined to be.
    fn verify_v1(torrent: &[u8], source: &Path) {
        let meta = librqbit::torrent_from_bytes(torrent).expect("engine refused the torrent");
        let info = &meta.info.data;
        let piece_length = info.piece_length as usize;

        let mut data = Vec::new();
        match &info.files {
            None => data.extend(std::fs::read(source).unwrap()),
            Some(files) => {
                for f in files {
                    if f.attr.as_ref().is_some_and(|a| a.as_ref().contains(&b'p')) {
                        data.resize(data.len() + f.length as usize, 0);
                        continue;
                    }
                    let mut path = source.to_path_buf();
                    for c in &f.path {
                        path.push(std::str::from_utf8(c.as_ref()).unwrap());
                    }
                    let bytes = std::fs::read(&path).unwrap();
                    assert_eq!(bytes.len() as u64, f.length, "{} length", path.display());
                    data.extend(bytes);
                }
            }
        }

        let pieces = info.pieces.as_ref();
        assert_eq!(pieces.len() % 20, 0, "pieces is not a whole number of hashes");
        assert_eq!(
            pieces.len() / 20,
            data.len().div_ceil(piece_length),
            "wrong number of piece hashes for {} bytes at {piece_length}",
            data.len()
        );
        for (i, (chunk, want)) in data.chunks(piece_length).zip(pieces.chunks(20)).enumerate() {
            assert_eq!(&sha1(chunk)[..], want, "piece {i} does not match the data");
        }
    }

    /// Every v1 shape hashes correctly, including the two edges librqbit's own
    /// creator got wrong: a total that is an exact multiple of the piece size
    /// (it appended a hash of nothing, one more piece than there is data for),
    /// and empty files, which carry no data but must still be listed.
    #[test]
    fn v1_pieces_match_the_files_they_describe() {
        let root = scratch_dir("v1verify");
        put(&root, "unaligned.bin", 40_000, 1);
        put(&root, "aligned.bin", 32_768, 2);
        put(&root, "empty.bin", 0, 0);
        put(&root, "multi/b.bin", 20_000, 4);
        put(&root, "multi/a.bin", 50_000, 5);
        put(&root, "multi/sub/c.bin", 1, 6);
        put(&root, "multi/sub/a.bin", 16_384, 7);
        put(&root, "fit/a.bin", 16_384, 8);
        put(&root, "fit/b.bin", 16_384, 9);
        put(&root, "empties/a.bin", 0, 0);
        put(&root, "empties/m.bin", 20_000, 3);
        put(&root, "empties/n.bin", 0, 0);
        put(&root, "empties/z.bin", 0, 0);

        for case in ["unaligned.bin", "aligned.bin", "empty.bin", "multi", "fit", "empties"] {
            for piece_length in [16_384, 65_536] {
                let source = root.join(case);
                verify_v1(&v1(&source, piece_length), &source);
                // The v1 half of a hybrid has to verify the same way, padding
                // files and all, or v1-only peers cannot use it.
                let hybrid = build(&CreateInput {
                    source: &source,
                    version: TorrentVersion::Hybrid,
                    piece_length: Some(piece_length),
                    trackers: &[],
                    comment: "",
                    private: false,
                    created_by: "test".into(),
                })
                .unwrap();
                verify_v1(&hybrid, &source);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Where librqbit's creator was right, ours is byte-identical to it: same
    /// info dict, so the same info hash for the same data. This is what made it
    /// safe to stop using librqbit's creator for v1 - a torrent made by 0.4.3
    /// and one made now, from the same file, are the same torrent.
    ///
    /// Only shapes whose order librqbit fixes are compared: it lists a folder
    /// in the order the filesystem hands back, which is why it was replaced.
    #[test]
    fn v1_info_dict_matches_librqbit_where_it_was_right() {
        let root = scratch_dir("v1rqbit");
        put(&root, "single.bin", 40_000, 1);
        put(&root, "chain/x/y/z.bin", 70_000, 3);
        put(&root, "private.bin", 3_000, 4);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for case in ["single.bin", "chain", "private.bin"] {
            for piece_length in [16_384u32, 2 * 1024 * 1024] {
                let source = root.join(case);
                // Inside the async block: BlockingSpawner reads the runtime handle.
                let theirs = rt
                    .block_on(async {
                        librqbit::create_torrent(
                            &source,
                            librqbit::CreateTorrentOptions {
                                name: None,
                                trackers: Vec::new(),
                                piece_length: Some(piece_length),
                            },
                            &librqbit::spawn_utils::BlockingSpawner::new(1),
                        )
                        .await
                    })
                    .unwrap();
                let ours = v1(&source, piece_length);
                let ours_info = crate::core::bencode::dict_get(&ours, b"info").unwrap();
                assert_eq!(
                    ours_info,
                    theirs.as_info().info.raw_bytes.as_ref(),
                    "{case} at {piece_length}: info dict differs from librqbit's"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A folder is listed sorted by path, whatever order the files were
    /// written in - so the same folder makes the same torrent on every machine.
    #[test]
    fn v1_folder_order_is_sorted_and_repeatable() {
        let root = scratch_dir("v1order");
        // Created in reverse, which is the order tmpfs and some others list.
        for (i, name) in ["d/z.bin", "d/m/q.bin", "d/m/b.bin", "d/a.bin"].iter().enumerate() {
            put(&root, name, 1000 + i, i as u8);
        }
        let source = root.join("d");
        let first = v1(&source, 16_384);
        let meta = librqbit::torrent_from_bytes(&first).unwrap();
        let listed: Vec<String> = meta
            .info
            .data
            .files
            .as_ref()
            .unwrap()
            .iter()
            .map(|f| {
                f.path
                    .iter()
                    .map(|c| std::str::from_utf8(c.as_ref()).unwrap())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect();
        assert_eq!(listed, ["a.bin", "m/b.bin", "m/q.bin", "z.bin"]);

        let info = |t: &[u8]| crate::core::bencode::dict_get(t, b"info").unwrap().to_vec();
        assert_eq!(info(&first), info(&v1(&source, 16_384)), "same folder, different info dict");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The info hash of a fixed folder, per version, written down.
    ///
    /// A change to any of these is a change to which torrent a given folder
    /// becomes - it would no longer match one made by an earlier release, and
    /// seeding a re-made torrent would join a different swarm. That can be the
    /// right call, but it should be a decision, so it fails here first.
    const PINNED_V1: &str = "d7688e8e82e8b3291820c4f4fc3e55a279618e8c";
    const PINNED_V2: &str = "fc1629cf081391b1324d7d85be62df0d0e5d6066b8f5e02d74a04fc7e4c8e559";
    const PINNED_HYBRID_V1: &str = "e509f9286715b9239fdfaa165046403d8282d59f";
    const PINNED_HYBRID_V2: &str = "13557875cdcd5fa887466990b4dc11e7eb16c71d38d16f7777bd95c539dfd95b";

    #[test]
    fn info_hashes_are_pinned_per_version() {
        let root = scratch_dir("pinned");
        put(&root, "pinned/a.bin", 40_000, 1);
        put(&root, "pinned/sub/b.bin", 16_384, 2);
        put(&root, "pinned/sub/empty.bin", 0, 0);
        put(&root, "pinned/z.bin", 5, 3);
        let source = root.join("pinned");

        let hashes = |version| {
            let built = build(&CreateInput {
                source: &source,
                version,
                piece_length: Some(16_384),
                trackers: &["http://tracker.example/announce".into()],
                comment: "pinned",
                private: true,
                created_by: "test".into(),
            })
            .unwrap();
            let info = crate::core::bencode::dict_get(&built, b"info").unwrap();
            crate::bittorrent::metainfo::info_hashes(info)
        };

        let (v1_hash, v2_hash) = hashes(TorrentVersion::V1);
        assert_eq!(v1_hash.as_deref(), Some(PINNED_V1));
        assert_eq!(v2_hash, None, "a v1 torrent has no v2 hash");

        let (v1_hash, v2_hash) = hashes(TorrentVersion::V2);
        assert_eq!(v1_hash, None, "a v2 torrent has no v1 hash");
        assert_eq!(v2_hash.as_deref(), Some(PINNED_V2));

        let (v1_hash, v2_hash) = hashes(TorrentVersion::Hybrid);
        assert_eq!(v1_hash.as_deref(), Some(PINNED_HYBRID_V1));
        assert_eq!(v2_hash.as_deref(), Some(PINNED_HYBRID_V2));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A folder that links back to itself is walked once, not until the path
    /// is too long for the OS.
    #[cfg(unix)]
    #[test]
    fn a_symlink_back_to_a_parent_is_not_followed() {
        let root = scratch_dir("symloop");
        put(&root, "d/a.bin", 1000, 1);
        put(&root, "outside.bin", 500, 2);
        std::os::unix::fs::symlink(root.join("d"), root.join("d/loop")).unwrap();
        // A link to a FILE is still followed: it is included as that file.
        std::os::unix::fs::symlink(root.join("outside.bin"), root.join("d/linked.bin")).unwrap();

        let (_, files, _) = collect_files(&root.join("d")).unwrap();
        let names: Vec<_> = files.iter().map(|f| f.components.join("/")).collect();
        assert_eq!(names, ["a.bin", "linked.bin"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A name that is not UTF-8 is an error, not a torrent listing a file name
    /// that exists nowhere. librqbit's creator panicked here instead, which
    /// left the dialog spinning forever.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_name_that_is_not_unicode_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let root = scratch_dir("notutf8");
        std::fs::create_dir_all(root.join("d")).unwrap();
        let bad = root.join("d").join(std::ffi::OsStr::from_bytes(b"caf\xe9.bin"));
        std::fs::write(bad, b"x").unwrap();

        let err = build(&CreateInput {
            source: &root.join("d"),
            version: TorrentVersion::V1,
            piece_length: Some(16_384),
            trackers: &[],
            comment: "",
            private: false,
            created_by: "test".into(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("not valid Unicode"), "{err:#}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Auto piece size: 256 KiB until that would mean more than ~2000 pieces,
    /// then doubling, and never past 16 MiB.
    #[test]
    fn auto_piece_length_scales_with_size() {
        const K: u64 = 1024;
        const M: u64 = 1024 * K;
        assert_eq!(auto_piece_length(0), 256 * K as u32);
        assert_eq!(auto_piece_length(100 * M), 256 * K as u32);
        assert_eq!(auto_piece_length(2000 * 256 * K), 256 * K as u32);
        assert_eq!(auto_piece_length(2000 * 256 * K + 256 * K), 512 * K as u32);
        assert_eq!(auto_piece_length(4 * 1024 * M), 4 * M as u32);
        assert_eq!(auto_piece_length(u64::MAX), 16 * M as u32);
        for total in [0, 1, M, 10 * 1024 * M, u64::MAX] {
            assert!(validate_piece_length(auto_piece_length(total)).is_ok());
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}

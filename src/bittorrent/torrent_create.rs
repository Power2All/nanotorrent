//! BitTorrent v2 (BEP 52) and hybrid torrent creation.
//!
//! librqbit only creates v1 torrents (`TorrentMetaV1*`, SHA-1), so v2 and
//! hybrid are built here from scratch:
//!
//! - **v2**: SHA-256 merkle trees over 16 KiB blocks (`pieces root` per file),
//!   a nested `file tree`, `piece layers`, and `meta version = 2`.
//! - **hybrid**: the same info dict *also* carries the v1 fields (`pieces`,
//!   `files`/`length`) describing identical data, with BEP 47 padding files so
//!   the v1 layout aligns each file to a piece boundary.
//!
//! Everything is deterministic and unit-tested (merkle vectors, bencode,
//! structure), but interop against a real v2 client (qBittorrent / libtorrent
//! 2.x) is the ultimate check - see the tests at the bottom.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha1::Sha1;
use sha2::{Digest, Sha256};

const BLOCK: usize = 16 * 1024; // 16 KiB v2 block size

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
    fn s(text: &str) -> Ben {
        Ben::Bytes(text.as_bytes().to_vec())
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
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

// Hashing helpers

/// SHA-1 of one buffer - v1 piece hashes and the v1 info hash.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize().into()
}

/// SHA-256 of one buffer - v2 block hashes and the v2 info hash (BEP 52).
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// One interior node of a v2 merkle tree: SHA-256 over two child hashes.
fn hash_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    h.finalize().into()
}

/// Merkle root over `leaves` (already padded to a power-of-two count).
fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    let mut layer = leaves.to_vec();
    while layer.len() > 1 {
        layer = layer.chunks(2).map(|p| hash_pair(&p[0], &p[1])).collect();
    }
    layer[0]
}

/// Round up to a power of two.
///
/// A v2 merkle tree needs a full binary tree, so the leaf count is padded up
/// to the next power of two before the root is computed.
fn next_pow2(n: usize) -> usize {
    let mut p = 1;
    while p < n {
        p *= 2;
    }
    p
}

// File walking

struct SrcFile {
    /// Path components relative to the torrent root (UTF-8).
    components: Vec<String>,
    abs: PathBuf,
    length: u64,
}

/// Collect the files to include, sorted by path. For a single file the one
/// component is its name; for a directory, paths are relative to it.
///
/// The third element says whether the SOURCE was a single file. That is not
/// the same as "there is one file": a directory holding exactly one entry also
/// yields one file, but its torrent name is the directory, so it must still be
/// laid out as a multi-file torrent.
fn collect_files(source: &Path) -> Result<(String, Vec<SrcFile>, bool)> {
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("source has no file name")?;

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
    walk(source, &mut Vec::new(), &mut files)?;
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
fn walk(dir: &Path, prefix: &mut Vec<String>, out: &mut Vec<SrcFile>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            prefix.push(name);
            walk(&path, prefix, out)?;
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
    let mut buf = vec![0u8; BLOCK];
    loop {
        let n = read_up_to(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        leaves.push(sha256(&buf[..n]));
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
    let padded = next_pow2(num_blocks);
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

/// Read as much as possible into `buf` (a short read only happens at EOF).
fn read_up_to(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
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

pub struct Built {
    pub bytes: Vec<u8>,
}

/// A power-of-two piece length aimed at a reasonable piece count.
pub fn auto_piece_length(total: u64) -> u32 {
    let mut pl: u64 = 256 * 1024;
    while total / pl > 2000 && pl < 16 * 1024 * 1024 {
        pl *= 2;
    }
    pl as u32
}

/// Validate/normalize a piece length for v2/hybrid: power of two, >= 16 KiB.
pub fn validate_piece_length(pl: u32) -> Result<u32> {
    if pl < BLOCK as u32 {
        bail!("piece size must be at least 16 KiB for v2/hybrid torrents");
    }
    if !pl.is_power_of_two() {
        bail!("piece size must be a power of two for v2/hybrid torrents");
    }
    Ok(pl)
}

/// Build a v2 or hybrid torrent. (v1 stays on librqbit; call this only for
/// `V2`/`Hybrid`.)
pub fn build(input: &CreateInput) -> Result<Built> {
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

    // --- v2: file tree + piece layers ------------------------------------
    let mut file_tree: Vec<(Vec<u8>, Ben)> = Vec::new();
    let mut piece_layers: Vec<(Vec<u8>, Ben)> = Vec::new();
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

    // --- info dict -------------------------------------------------------
    let mut info: Vec<(Vec<u8>, Ben)> = vec![
        (b"name".to_vec(), Ben::s(&name)),
        (b"piece length".to_vec(), Ben::Int(piece_length as i64)),
        (b"meta version".to_vec(), Ben::Int(2)),
        (b"file tree".to_vec(), Ben::Dict(file_tree)),
    ];
    if input.private {
        info.push((b"private".to_vec(), Ben::Int(1)));
    }

    // --- hybrid: add the v1 fields describing identical data --------------
    if hybrid {
        let mut hasher = V1Hasher::new(piece_length as usize);
        if single {
            stream_file(&files[0].abs, |chunk| hasher.push(chunk))?;
            info.push((b"length".to_vec(), Ben::Int(files[0].length as i64)));
        } else {
            let mut v1_files: Vec<Ben> = Vec::new();
            for (idx, f) in files.iter().enumerate() {
                stream_file(&f.abs, |chunk| hasher.push(chunk))?;
                let path = Ben::List(f.components.iter().map(|c| Ben::s(c)).collect());
                v1_files.push(Ben::Dict(vec![
                    (b"length".to_vec(), Ben::Int(f.length as i64)),
                    (b"path".to_vec(), path),
                ]));
                // Align every file but the last to a piece boundary with a
                // BEP 47 padding file.
                if idx + 1 < files.len() {
                    let pad = hasher.pad_to_piece();
                    if pad > 0 {
                        v1_files.push(Ben::Dict(vec![
                            (b"attr".to_vec(), Ben::s("p")),
                            (b"length".to_vec(), Ben::Int(pad as i64)),
                            (
                                b"path".to_vec(),
                                Ben::List(vec![Ben::s(".pad"), Ben::s(&pad.to_string())]),
                            ),
                        ]));
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
    if !piece_layers.is_empty() {
        root.push((b"piece layers".to_vec(), Ben::Dict(piece_layers)));
    }

    Ok(Built {
        bytes: Ben::Dict(root).to_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    #[test]
    fn next_pow2_works() {
        assert_eq!(next_pow2(1), 1);
        assert_eq!(next_pow2(3), 4);
        assert_eq!(next_pow2(4), 4);
        assert_eq!(next_pow2(5), 8);
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
        let s = built.bytes;
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
        let s = built.bytes;
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

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}

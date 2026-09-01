//! Reading BitTorrent v2 (BEP 52) metainfo, and the merkle arithmetic that
//! verifies a v2 piece.
//!
//! `torrent_create.rs` already *writes* v2 and hybrid torrents. This is the
//! other direction, and the two are deliberately independent implementations
//! of the same spec: the tests at the bottom feed this reader torrents built
//! by that writer, so a mistake in either one shows up as a mismatch rather
//! than cancelling out.
//!
//! # What a v2 piece hash actually is
//!
//! Not a hash of the piece. Each file is split into 16 KiB blocks, each block
//! is SHA-256'd, and those hashes are the leaves of a per-file binary merkle
//! tree padded out to a power of two with all-zero hashes. A *piece* hash is
//! the interior node covering that piece's blocks; the file's `pieces root` is
//! the root of the whole tree. So verifying a piece means re-deriving one
//! subtree root, not hashing bytes - which is why this cannot reuse the v1
//! path at all.
//!
//! Two consequences worth knowing before touching anything here:
//!
//! - The final block of a file is hashed over its **real bytes**, not padded
//!   to 16 KiB. The zero padding happens at the hash level (whole `[0u8; 32]`
//!   leaves), never at the data level.
//! - A file whose data fits in one piece has no entry in `piece layers` at
//!   all; its single piece hash *is* its `pieces root`. Special-cased below,
//!   and easy to get wrong because the padding target changes with it.
//!
//! # Layout
//!
//! v2 aligns every file to a piece boundary - there are no pieces spanning two
//! files, which is the whole point of hashing per file. [`V2Layout`] flattens
//! that into the piece list the engine wants, carrying for each piece the
//! number of REAL bytes in it (a file's last piece is short) and how far to
//! pad when re-deriving its root.

use std::collections::HashMap;

use super::torrent_create::{BLOCK, hash_pair, merkle_root, next_pow2, sha256};

/// One file from a v2 `file tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2File {
    /// Path components relative to the torrent root, already in tree order.
    pub components: Vec<String>,
    pub length: u64,
    /// `None` for empty files, which carry no `pieces root` per BEP 52.
    pub pieces_root: Option<[u8; 32]>,
}

impl V2File {
    /// The path as the rest of the app spells it: components joined by `/`.
    pub fn path(&self) -> String {
        self.components.join("/")
    }
}

/// One piece of the flattened torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Piece {
    /// Index into [`V2Meta::files`].
    pub file_index: usize,
    /// Byte offset of this piece within its file.
    pub file_offset: u64,
    /// Real bytes in this piece. Less than `piece_length` for a file's last
    /// piece; the remainder of the piece is alignment padding that is neither
    /// stored nor hashed.
    pub real_len: u32,
    /// The expected merkle root for this piece.
    pub hash: [u8; 32],
    /// How many leaves this piece's subtree has once padded. Normally
    /// `piece_length / BLOCK`, but a file small enough to fit in one piece
    /// pads only to `next_pow2(its block count)` - its piece hash IS the
    /// file's `pieces root`, and that root was computed over a smaller tree.
    pub pad_blocks: usize,
}

/// The flattened piece list for a whole v2 torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Layout {
    pub piece_length: u32,
    pub pieces: Vec<V2Piece>,
}

/// A parsed v2 (or hybrid) torrent.
#[derive(Debug, Clone)]
pub struct V2Meta {
    pub name: String,
    pub piece_length: u32,
    pub files: Vec<V2File>,
    pub layout: V2Layout,
    /// True when the same info dict also carries the v1 keys, i.e. a hybrid.
    /// Hybrids can be downloaded either way; v2-only cannot.
    pub has_v1: bool,
    pub private: bool,
    /// SHA-256 of the info dict - the v2 info hash.
    pub info_hash_v2: [u8; 32],
    /// The raw info dict, for BEP 9 metadata exchange.
    pub info_bytes: Vec<u8>,
}

impl V2Meta {
    /// The 20-byte info hash a v2-only torrent uses on the wire.
    ///
    /// BEP 52 truncates the SHA-256 info hash to 20 bytes for the BEP 3
    /// handshake, the DHT and tracker announces, precisely so that the v1
    /// transport keeps working unchanged. This is what makes a v2 swarm
    /// reachable without any new peer messages.
    pub fn truncated_info_hash(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out.copy_from_slice(&self.info_hash_v2[..20]);
        out
    }
}

// ---------------------------------------------------------------- merkle ---

/// The padding hash for a subtree `levels` deep, i.e. the value an all-padding
/// node takes. Leaves pad with `[0u8; 32]`; every level above is the pair hash
/// of two of the level below.
fn zero_hash(levels: u32) -> [u8; 32] {
    let mut h = [0u8; 32];
    for _ in 0..levels {
        h = hash_pair(&h, &h);
    }
    h
}

/// `log2(n)` for a power of two.
fn log2_exact(n: usize) -> u32 {
    n.trailing_zeros()
}

/// The merkle root of one piece's worth of data, computed in one go.
///
/// `data` is the piece's REAL bytes (a file's last piece is short). `pad_blocks`
/// is the leaf count of the subtree, from [`V2Piece::pad_blocks`].
///
/// Test-only on purpose. The engine verifies through [`V2PieceHasher`], which
/// streams and so never holds a whole piece; this is the obvious-by-inspection
/// version the tests check that one against. Two implementations of the same
/// rule is the point - a mistake in the streaming one shows up as a mismatch
/// instead of agreeing with itself.
#[cfg(test)]
fn piece_merkle_root(data: &[u8], pad_blocks: usize) -> [u8; 32] {
    let mut leaves: Vec<[u8; 32]> = data.chunks(BLOCK).map(sha256).collect();
    // A wholly-padding piece cannot occur (pieces are only created for real
    // data), but an empty slice would otherwise index out of bounds below.
    if leaves.is_empty() {
        leaves.push(sha256(&[]));
    }
    leaves.resize(pad_blocks.max(leaves.len()).next_power_of_two(), [0u8; 32]);
    merkle_root(&leaves)
}

/// Re-derive a file's `pieces root` from its piece hashes and check it.
///
/// This is the integrity check that makes `piece layers` trustworthy: the
/// layer arrives outside the info dict, so it is NOT covered by the info hash
/// and a hostile peer or tracker could otherwise hand over piece hashes for
/// data of its choosing. Checking it against `pieces root` - which IS inside
/// the info dict - is what closes that.
fn piece_layer_matches_root(
    piece_hashes: &[[u8; 32]],
    num_blocks: usize,
    blocks_per_piece: usize,
    pieces_root: &[u8; 32],
) -> bool {
    let total_leaves = next_pow2(num_blocks);
    let nodes = (total_leaves / blocks_per_piece).max(1);
    if piece_hashes.len() > nodes {
        return false;
    }
    let mut layer = piece_hashes.to_vec();
    layer.resize(nodes, zero_hash(log2_exact(blocks_per_piece)));
    merkle_root(&layer) == *pieces_root
}

// ---------------------------------------------------- BEP 52 hash exchange ---

/// What to ask a peer for, to obtain one run of a file's piece layer.
///
/// Mirrors the `hash request` message (engine patch 0008). Only needed for a
/// **magnet**: a `.torrent` already carries `piece layers`, and a file that
/// fits in a single piece never needs one because its `pieces root` IS its
/// piece hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashRequestPlan {
    /// Layers above the leaves at which the requested hashes sit. For the
    /// piece layer this is `log2(piece_length / 16 KiB)`.
    pub base_layer: u32,
    /// Offset of the first requested hash within that layer.
    pub index: u32,
    /// How many hashes to ask for. A power of two, at least 2, at most 512.
    pub length: u32,
    /// Ancestor hashes needed to walk the answer back up to `pieces root`.
    pub proof_layers: u32,
}

/// The BEP's own ceiling on one request: "Length SHOULD NOT be greater than
/// 512." A file with more piece-layer nodes than this needs several requests.
const MAX_HASHES_PER_REQUEST: usize = 512;

/// Plan the requests that fetch a file's whole piece layer.
///
/// Empty when the file needs no layer at all - either it has no data, or it
/// fits in one piece and its `pieces root` is already the piece hash.
pub fn plan_piece_layer_requests(file_length: u64, piece_length: u32) -> Vec<HashRequestPlan> {
    let blocks_per_piece = piece_length as usize / BLOCK;
    let num_blocks = (file_length as usize).div_ceil(BLOCK);
    if num_blocks <= blocks_per_piece {
        return Vec::new();
    }

    let total_leaves = next_pow2(num_blocks);
    let base_layer = log2_exact(blocks_per_piece);
    let nodes = total_leaves / blocks_per_piece;
    let root_level = log2_exact(total_leaves);

    let per_request = nodes.min(MAX_HASHES_PER_REQUEST);
    let mut plans = Vec::new();
    let mut index = 0usize;
    while index < nodes {
        // Runs are a power of two and index must be a multiple of the run
        // length, which `per_request` being a power-of-two divisor of `nodes`
        // guarantees.
        let length = per_request;
        plans.push(HashRequestPlan {
            base_layer,
            index: index as u32,
            length: length as u32,
            proof_layers: root_level - base_layer - log2_exact(length),
        });
        index += length;
    }
    plans
}

/// Check a peer's `hashes` answer against the file's `pieces root`.
///
/// `base` is the run of hashes from the requested layer; `proof` is the uncle
/// hashes that follow, ordered from just above the base upwards - "ends with
/// the uncle hash closest to the root", as BEP 52 puts it.
///
/// This is the only thing standing between a peer and piece hashes of its
/// choosing: `pieces root` is inside the info dict and therefore covered by
/// the info hash, while everything the peer just sent is not.
pub fn verify_hashes(
    base: &[[u8; 32]],
    proof: &[[u8; 32]],
    index: u32,
    pieces_root: &[u8; 32],
) -> bool {
    if base.is_empty() || !base.len().is_power_of_two() {
        return false;
    }
    if !(index as usize).is_multiple_of(base.len()) {
        return false;
    }
    let mut node = merkle_root(base);
    let mut pos = index as usize / base.len();
    for sibling in proof {
        node = if pos.is_multiple_of(2) {
            hash_pair(&node, sibling)
        } else {
            hash_pair(sibling, &node)
        };
        pos /= 2;
    }
    node == *pieces_root
}

// --------------------------------------------------------------- bencode ---

/// A borrowed bencode value. Only what BEP 52 needs to be read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Val<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<Val<'a>>),
    /// Kept as a list, not a map: bencode dict keys are ordered, and the v2
    /// file tree's order IS the torrent's file order.
    Dict(Vec<(&'a [u8], Val<'a>)>),
}

impl<'a> Val<'a> {
    fn get(&self, key: &[u8]) -> Option<&Val<'a>> {
        match self {
            Val::Dict(entries) => entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_int(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            _ => None,
        }
    }
    fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            Val::Bytes(b) => Some(b),
            _ => None,
        }
    }
    fn as_dict(&self) -> Option<&[(&'a [u8], Val<'a>)]> {
        match self {
            Val::Dict(d) => Some(d),
            _ => None,
        }
    }
}

/// Decode one bencode value at `i`, returning it and the index just past it.
fn decode(b: &[u8], i: usize) -> Option<(Val<'_>, usize)> {
    match *b.get(i)? {
        b'i' => {
            let end = b[i..].iter().position(|c| *c == b'e')? + i;
            let n: i64 = std::str::from_utf8(&b[i + 1..end]).ok()?.parse().ok()?;
            Some((Val::Int(n), end + 1))
        }
        b'l' => {
            let mut items = Vec::new();
            let mut j = i + 1;
            while *b.get(j)? != b'e' {
                let (v, next) = decode(b, j)?;
                items.push(v);
                j = next;
            }
            Some((Val::List(items), j + 1))
        }
        b'd' => {
            let mut items = Vec::new();
            let mut j = i + 1;
            while *b.get(j)? != b'e' {
                let (k, after) = decode_str(b, j)?;
                let (v, next) = decode(b, after)?;
                items.push((k, v));
                j = next;
            }
            Some((Val::Dict(items), j + 1))
        }
        b'0'..=b'9' => decode_str(b, i).map(|(s, end)| (Val::Bytes(s), end)),
        _ => None,
    }
}

/// Decode a bencode byte string at `i`.
fn decode_str(b: &[u8], i: usize) -> Option<(&[u8], usize)> {
    let colon = b[i..].iter().position(|c| *c == b':')? + i;
    let len: usize = std::str::from_utf8(&b[i..colon]).ok()?.parse().ok()?;
    let start = colon + 1;
    let end = start.checked_add(len)?;
    b.get(start..end).map(|s| (s, end))
}

// ----------------------------------------------------------------- parse ---

/// Parse a v2 or hybrid `.torrent`.
///
/// Returns `Ok(None)` for a plain v1 torrent - not an error, just nothing for
/// this module to do. Errors are strings because every caller is showing them
/// to a person who just picked a file.
pub fn parse(torrent_bytes: &[u8]) -> Result<Option<V2Meta>, String> {
    let info_bytes = match super::metainfo::bencode_lookup(torrent_bytes, b"info") {
        Some(b) => b,
        None => return Err("torrent has no info dictionary".into()),
    };

    // `piece layers` sits at the TOP level, outside the info dict, keyed by
    // each file's `pieces root`. A magnet has none of it, which is why the
    // info dict is parsed separately below.
    let mut layers: HashMap<[u8; 32], Vec<[u8; 32]>> = HashMap::new();
    if let Some(raw) = super::metainfo::bencode_lookup(torrent_bytes, b"piece layers")
        && let Some((v, _)) = decode(raw, 0)
        && let Some(entries) = v.as_dict()
    {
        for (k, v) in entries {
            if let (Ok(root), Some(bytes)) = (<[u8; 32]>::try_from(*k), v.as_bytes()) {
                if !bytes.len().is_multiple_of(32) {
                    return Err("a piece layers entry is not a whole number of hashes".into());
                }
                layers.insert(
                    root,
                    bytes
                        .chunks_exact(32)
                        .map(|c| <[u8; 32]>::try_from(c).unwrap())
                        .collect(),
                );
            }
        }
    }

    let Some(info) = parse_info_dict(info_bytes)? else {
        return Ok(None);
    };
    Ok(Some(info.into_meta(&layers)?))
}

/// The part of a v2 torrent that lives inside the info dict.
///
/// Split out from [`parse`] because a **magnet** only ever has this much: the
/// piece layers arrive separately, over the BEP 52 hash exchange, and the
/// layout cannot be built until they do.
#[derive(Debug, Clone)]
pub struct V2Info {
    pub name: String,
    pub piece_length: u32,
    pub files: Vec<V2File>,
    pub has_v1: bool,
    pub private: bool,
    pub info_hash_v2: [u8; 32],
    pub info_bytes: Vec<u8>,
}

impl V2Info {
    /// Combine with the piece layers, from wherever they came, into the full
    /// picture. Every layer is checked against its file's `pieces root` here.
    pub fn into_meta(self, layers: &HashMap<[u8; 32], Vec<[u8; 32]>>) -> Result<V2Meta, String> {
        let layout = build_layout(&self.files, self.piece_length, layers)?;
        Ok(V2Meta {
            name: self.name,
            piece_length: self.piece_length,
            files: self.files,
            layout,
            has_v1: self.has_v1,
            private: self.private,
            info_hash_v2: self.info_hash_v2,
            info_bytes: self.info_bytes,
        })
    }
}

/// Parse a v2 info dictionary. `Ok(None)` for a v1-only dict.
pub fn parse_info_dict(info_bytes: &[u8]) -> Result<Option<V2Info>, String> {
    let (info, _) = decode(info_bytes, 0).ok_or("info dictionary is not valid bencode")?;

    match info.get(b"meta version").and_then(Val::as_int) {
        None => return Ok(None), // v1 only: not ours
        Some(2) => {}
        Some(v) => return Err(format!("unsupported meta version {v}; only v2 is defined")),
    }

    let piece_length: u32 = info
        .get(b"piece length")
        .and_then(Val::as_int)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or("v2 torrent has no usable piece length")?;
    if piece_length < BLOCK as u32 || !piece_length.is_power_of_two() {
        return Err(format!(
            "invalid v2 piece length {piece_length}: must be a power of two and at least {BLOCK}"
        ));
    }

    let name = info
        .get(b"name")
        .and_then(Val::as_bytes)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_else(|| String::from("(unnamed torrent)"));

    let tree = info
        .get(b"file tree")
        .and_then(Val::as_dict)
        .ok_or("v2 torrent has no file tree")?;

    let mut files = Vec::new();
    let mut path = Vec::new();
    walk_file_tree(tree, &mut path, &mut files)?;
    if files.is_empty() {
        return Err("v2 file tree contains no files".into());
    }

    Ok(Some(V2Info {
        name,
        piece_length,
        files,
        has_v1: info.get(b"pieces").is_some(),
        private: info.get(b"private").and_then(Val::as_int) == Some(1),
        info_hash_v2: sha256(info_bytes),
        info_bytes: info_bytes.to_vec(),
    }))
}

/// Depth-first walk of the nested `file tree`, in bencode key order.
///
/// A leaf is a dict holding the empty-string key; everything else is a
/// directory. That empty key is what makes the format unambiguous, since a
/// directory and a file can otherwise look alike.
fn walk_file_tree(
    dict: &[(&[u8], Val<'_>)],
    path: &mut Vec<String>,
    out: &mut Vec<V2File>,
) -> Result<(), String> {
    for (key, value) in dict {
        if key.is_empty() {
            // Leaf: this node's own path is the file.
            let length = value
                .get(b"length")
                .and_then(Val::as_int)
                .and_then(|v| u64::try_from(v).ok())
                .ok_or_else(|| format!("v2 file {} has no length", path.join("/")))?;
            let pieces_root = match value.get(b"pieces root").and_then(Val::as_bytes) {
                Some(b) => Some(
                    <[u8; 32]>::try_from(b)
                        .map_err(|_| format!("v2 file {} has a malformed pieces root", path.join("/")))?,
                ),
                None if length == 0 => None,
                None => {
                    return Err(format!("v2 file {} has no pieces root", path.join("/")));
                }
            };
            if length > 0 && pieces_root.is_none() {
                return Err(format!("v2 file {} has no pieces root", path.join("/")));
            }
            out.push(V2File {
                components: path.clone(),
                length,
                pieces_root,
            });
            continue;
        }
        let component = String::from_utf8_lossy(key).into_owned();
        // A path component that escapes the torrent root would let a torrent
        // write anywhere on the disk. Refuse rather than sanitise: a torrent
        // containing one is not a torrent anyone should be downloading.
        if component == ".." || component == "." || component.contains('/') || component.contains('\\') {
            return Err(format!("v2 file tree has an illegal path component {component:?}"));
        }
        let entries = value
            .as_dict()
            .ok_or_else(|| format!("v2 file tree entry {component:?} is not a dictionary"))?;
        path.push(component);
        walk_file_tree(entries, path, out)?;
        path.pop();
    }
    Ok(())
}

/// Flatten the files into the piece list, validating each `piece layers` entry
/// against the file's `pieces root` as it goes.
fn build_layout(
    files: &[V2File],
    piece_length: u32,
    layers: &HashMap<[u8; 32], Vec<[u8; 32]>>,
) -> Result<V2Layout, String> {
    let blocks_per_piece = piece_length as usize / BLOCK;
    let mut pieces = Vec::new();

    for (file_index, f) in files.iter().enumerate() {
        let Some(root) = f.pieces_root else {
            continue; // empty file: no data, no pieces
        };
        let num_blocks = (f.length as usize).div_ceil(BLOCK);
        let num_pieces = (f.length).div_ceil(piece_length as u64) as usize;

        let hashes: Vec<[u8; 32]> = if num_blocks <= blocks_per_piece {
            // Single-piece file: its one piece hash IS the pieces root, and
            // there is deliberately no piece layers entry.
            if layers.contains_key(&root) {
                return Err(format!(
                    "v2 file {} fits in one piece but still has a piece layers entry",
                    f.path()
                ));
            }
            vec![root]
        } else {
            let hashes = layers.get(&root).cloned().ok_or_else(|| {
                format!("v2 torrent has no piece layers entry for {}", f.path())
            })?;
            if hashes.len() != num_pieces {
                return Err(format!(
                    "v2 piece layers entry for {} has {} hashes, expected {}",
                    f.path(),
                    hashes.len(),
                    num_pieces
                ));
            }
            if !piece_layer_matches_root(&hashes, num_blocks, blocks_per_piece, &root) {
                return Err(format!(
                    "v2 piece layers for {} do not hash to its pieces root",
                    f.path()
                ));
            }
            hashes
        };

        let pad_blocks = if num_blocks <= blocks_per_piece {
            next_pow2(num_blocks)
        } else {
            blocks_per_piece
        };

        for (i, hash) in hashes.into_iter().enumerate() {
            let file_offset = i as u64 * piece_length as u64;
            let real_len = (f.length - file_offset).min(piece_length as u64) as u32;
            pieces.push(V2Piece {
                file_index,
                file_offset,
                real_len,
                hash,
                pad_blocks,
            });
        }
    }

    Ok(V2Layout {
        piece_length,
        pieces,
    })
}

// -------------------------------------------------------------- verifier ---

/// Verifies v2 pieces for one torrent, through librqbit's piece-verification
/// seam (engine patch 0008).
///
/// Indexed by the engine's global piece index, which lines up with
/// [`V2Layout::pieces`] because [`synthetic_v1`] lays the files out in the same
/// order and pads each to a piece boundary.
#[derive(Debug)]
pub struct V2Verifier {
    pieces: Vec<V2Piece>,
}

impl V2Verifier {
    pub fn new(layout: &V2Layout) -> Self {
        Self {
            pieces: layout.pieces.clone(),
        }
    }
}

impl librqbit::PieceVerifier for V2Verifier {
    fn hasher(&self, piece_index: u32) -> Option<Box<dyn librqbit::PieceHasher>> {
        let piece = self.pieces.get(piece_index as usize)?;
        Some(Box::new(V2PieceHasher {
            remaining: piece.real_len as usize,
            block: Vec::with_capacity(BLOCK),
            leaves: Vec::new(),
            expected: piece.hash,
            pad_blocks: piece.pad_blocks,
        }))
    }
}

/// Hashes one piece into its merkle root as the bytes arrive.
///
/// Streaming rather than buffering the piece: pieces can be 16 MB and the
/// initial check walks every one of them, so this keeps a single 16 KiB block
/// in hand and folds each finished block into a leaf.
struct V2PieceHasher {
    /// Real bytes still wanted. Anything past this is the alignment padding
    /// that follows a file's last piece, which is neither stored nor hashed -
    /// the engine feeds it to us because its flat layout has a padding file
    /// there, and dropping it here is what makes the two views agree.
    remaining: usize,
    block: Vec<u8>,
    leaves: Vec<[u8; 32]>,
    expected: [u8; 32],
    pad_blocks: usize,
}

impl librqbit::PieceHasher for V2PieceHasher {
    fn update(&mut self, buf: &[u8]) {
        let take = buf.len().min(self.remaining);
        let mut buf = &buf[..take];
        self.remaining -= take;
        while !buf.is_empty() {
            let want = BLOCK - self.block.len();
            let n = want.min(buf.len());
            self.block.extend_from_slice(&buf[..n]);
            buf = &buf[n..];
            if self.block.len() == BLOCK {
                self.leaves.push(sha256(&self.block));
                self.block.clear();
            }
        }
    }

    fn verify(mut self: Box<Self>) -> bool {
        // A trailing partial block is hashed over its real bytes, never padded
        // out to 16 KiB - see the module docs.
        if !self.block.is_empty() {
            self.leaves.push(sha256(&self.block));
        }
        if self.leaves.is_empty() {
            return false;
        }
        let target = self.pad_blocks.max(self.leaves.len()).next_power_of_two();
        self.leaves.resize(target, [0u8; 32]);
        merkle_root(&self.leaves) == self.expected
    }
}

// ----------------------------------------------------------- v2 magnets ---

/// Drives a v2-only magnet from a bare info hash to a downloadable torrent.
///
/// A magnet gives us nothing but the truncated SHA-256 hash. The info dict
/// arrives over BEP 9, but `piece layers` is **not** part of it, so the piece
/// hashes have to be fetched from the peer with the BEP 52 hash messages
/// before anything can be verified. This object is the whole conversation:
///
/// 1. [`MetadataInterceptor::verify_info`] - the engine's own check is SHA-1,
///    which is the wrong function for a v2 torrent.
/// 2. [`MetadataInterceptor::hash_requests`] - what to ask the peer for.
/// 3. [`MetadataInterceptor::on_hashes`] - each answer, verified against the
///    file's `pieces root` before it is believed.
/// 4. [`MetadataInterceptor::substitute_info`] - the v1-shaped model the
///    engine drives, once everything has arrived.
///
/// It is then installed as the torrent's [`librqbit::PieceVerifier`] too, so
/// the layers it collected are what pieces are checked against. The same
/// object plays both parts precisely so they cannot disagree.
#[derive(Debug, Default)]
pub struct V2Magnet {
    inner: std::sync::Mutex<MagnetInner>,
}

#[derive(Debug, Default)]
struct MagnetInner {
    info: Option<V2Info>,
    /// `pieces root` -> the runs received so far, keyed by base-layer index.
    /// A BTreeMap so they reassemble in order however they arrive.
    received: HashMap<[u8; 32], std::collections::BTreeMap<u32, Vec<[u8; 32]>>>,
    /// `pieces root` -> how many runs were asked for.
    expected: HashMap<[u8; 32], usize>,
    /// `pieces root` -> how many piece hashes the file actually has. The last
    /// run is padded up to a power of two, and the surplus covers nothing.
    wanted: HashMap<[u8; 32], usize>,
    /// Complete, once every layer has arrived and verified.
    meta: Option<V2Meta>,
}

impl MagnetInner {
    fn complete(&self) -> bool {
        self.expected
            .iter()
            .all(|(root, n)| self.received.get(root).map(|m| m.len()) == Some(*n))
    }

    /// Assemble what arrived into the finished picture.
    fn finalise(&mut self) -> Result<(), String> {
        let info = self.info.clone().ok_or("bug: no info dict")?;
        let mut layers: HashMap<[u8; 32], Vec<[u8; 32]>> = HashMap::new();
        for (root, runs) in &self.received {
            let mut all: Vec<[u8; 32]> = Vec::new();
            for run in runs.values() {
                all.extend_from_slice(run);
            }
            // Trim the tree padding: nodes past the file's real piece count
            // cover nothing but zeros.
            if let Some(want) = self.wanted.get(root) {
                all.truncate(*want);
            }
            layers.insert(*root, all);
        }
        self.meta = Some(info.into_meta(&layers)?);
        Ok(())
    }
}

impl V2Magnet {
    pub fn new() -> Self {
        Self::default()
    }
}

impl librqbit::MetadataInterceptor for V2Magnet {
    fn verify_info(&self, info_bytes: &[u8], info_hash: librqbit::Id20) -> bool {
        // BEP 52: a v2-only torrent is known by its SHA-256 info hash
        // truncated to 20 bytes.
        let ok = sha256(info_bytes)[..20] == info_hash.0[..];
        tracing::debug!(len = info_bytes.len(), ok, "v2 magnet: verify_info");
        ok
    }

    fn hash_requests(
        &self,
        info_bytes: &[u8],
    ) -> anyhow::Result<Vec<librqbit_peer_protocol::HashRequest>> {
        tracing::debug!(len = info_bytes.len(), "v2 magnet: hash_requests");
        let info = crate::bittorrent::v2::parse_info_dict(info_bytes)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .ok_or_else(|| anyhow::anyhow!("a v2 magnet resolved to a v1 info dictionary"))?;

        let mut out = Vec::new();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("v2 magnet state poisoned"))?;

        for f in &info.files {
            let Some(root) = f.pieces_root else { continue };
            let plans = plan_piece_layer_requests(f.length, info.piece_length);
            if plans.is_empty() {
                continue; // fits in one piece: its pieces root IS the hash
            }
            inner.expected.insert(root, plans.len());
            inner.wanted.insert(
                root,
                f.length.div_ceil(info.piece_length as u64) as usize,
            );
            for p in plans {
                out.push(librqbit_peer_protocol::HashRequest {
                    pieces_root: root,
                    base_layer: p.base_layer,
                    index: p.index,
                    length: p.length,
                    proof_layers: p.proof_layers,
                });
            }
        }
        tracing::debug!(files = info.files.len(), requests = out.len(), "v2 magnet: planned");
        inner.info = Some(info);
        Ok(out)
    }

    fn on_hashes(
        &self,
        request: &librqbit_peer_protocol::HashRequest,
        hashes: &[[u8; 32]],
    ) -> anyhow::Result<bool> {
        let want = request.length as usize;
        if hashes.len() < want {
            anyhow::bail!(
                "peer sent {} hashes, fewer than the {want} requested",
                hashes.len()
            );
        }
        let (base, proof) = hashes.split_at(want);

        // Verified BEFORE it is stored. `pieces root` is inside the info dict
        // and covered by the info hash; nothing the peer just sent is.
        if !verify_hashes(base, proof, request.index, &request.pieces_root) {
            anyhow::bail!("peer sent hashes that do not match the file's pieces root");
        }

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("v2 magnet state poisoned"))?;
        inner
            .received
            .entry(request.pieces_root)
            .or_default()
            .insert(request.index, base.to_vec());

        if !inner.complete() {
            return Ok(false);
        }
        inner.finalise().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    }

    fn substitute_info(&self, info_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        tracing::debug!("v2 magnet: substitute_info");
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("v2 magnet state poisoned"))?;
        if inner.meta.is_none() {
            // No file needed a piece layer - every one of them fits in a
            // single piece, so its pieces root is already its piece hash.
            if inner.info.is_none() {
                inner.info = crate::bittorrent::v2::parse_info_dict(info_bytes)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            inner.finalise().map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let meta = inner.meta.as_ref().expect("finalise sets it or errors");
        // The info dict, NOT a whole torrent - see synthetic_v1.
        Ok(synthetic_v1_info(meta))
    }
}

impl librqbit::PieceVerifier for V2Magnet {
    fn hasher(&self, piece_index: u32) -> Option<Box<dyn librqbit::PieceHasher>> {
        let inner = self.inner.lock().ok()?;
        let piece = inner
            .meta
            .as_ref()?
            .layout
            .pieces
            .get(piece_index as usize)?;
        Some(Box::new(V2PieceHasher {
            remaining: piece.real_len as usize,
            block: Vec::with_capacity(BLOCK),
            leaves: Vec::new(),
            expected: piece.hash,
            pad_blocks: piece.pad_blocks,
        }))
    }
}

// ------------------------------------------------------- v1-shaped view ---

/// Rewrite a v2 torrent into the v1 shape the engine knows how to drive.
///
/// librqbit models a torrent as a flat byte stream cut into pieces. v2 does
/// not: it hashes each file separately and starts every file on a piece
/// boundary. The two views are reconciled the same way a hybrid torrent does
/// it - with BEP 47 padding files, which the engine already understands and
/// hides from the file list.
///
/// The `pieces` blob is **filler**. It exists so the engine can compute a
/// piece count and index into something; every actual comparison goes through
/// [`V2Verifier`], which checks the real merkle roots. Nothing here is ever
/// sent to a peer: it is a local driving model, and the genuine info dict is
/// kept in [`V2Meta::info_bytes`] for metadata exchange.
pub fn synthetic_v1(meta: &V2Meta) -> Vec<u8> {
    use super::torrent_create::Ben;
    // A whole .torrent. `substitute_info` needs the INFO DICT ALONE - the two
    // are one nesting level apart, and mixing them up parses as a torrent with
    // no `pieces`, which is exactly how a real v2 magnet failed.
    Ben::Dict(vec![(b"info".to_vec(), synthetic_v1_info_ben(meta))]).to_bytes()
}

/// Just the info dictionary, which is what BEP 9 delivers and what the engine
/// parses as `TorrentMetaV1Info`.
pub fn synthetic_v1_info(meta: &V2Meta) -> Vec<u8> {
    synthetic_v1_info_ben(meta).to_bytes()
}

fn synthetic_v1_info_ben(meta: &V2Meta) -> super::torrent_create::Ben {
    use super::torrent_create::Ben;

    let piece_length = meta.piece_length as u64;
    let mut files: Vec<Ben> = Vec::new();
    let last_real = meta
        .files
        .iter()
        .rposition(|f| f.length > 0)
        .unwrap_or(usize::MAX);

    for (i, f) in meta.files.iter().enumerate() {
        files.push(Ben::Dict(vec![
            (b"length".to_vec(), Ben::Int(f.length as i64)),
            (
                b"path".to_vec(),
                Ben::List(f.components.iter().map(|c| Ben::s(c)).collect()),
            ),
        ]));
        // Pad up to the next piece boundary, except after the final file with
        // data - the torrent legitimately ends mid-piece there, and padding it
        // would invent bytes that no peer has.
        if i == last_real || f.length == 0 {
            continue;
        }
        let pad = (piece_length - f.length % piece_length) % piece_length;
        if pad > 0 {
            files.push(Ben::Dict(vec![
                (b"attr".to_vec(), Ben::s("p")),
                (b"length".to_vec(), Ben::Int(pad as i64)),
                (
                    b"path".to_vec(),
                    Ben::List(vec![Ben::s(".pad"), Ben::s(&pad.to_string())]),
                ),
            ]));
        }
    }

    let info = Ben::Dict(vec![
        (b"files".to_vec(), Ben::List(files)),
        (b"name".to_vec(), Ben::s(&meta.name)),
        (
            b"piece length".to_vec(),
            Ben::Int(meta.piece_length as i64),
        ),
        (
            b"pieces".to_vec(),
            Ben::Bytes(vec![0u8; meta.layout.pieces.len() * 20]),
        ),
        (
            b"private".to_vec(),
            Ben::Int(if meta.private { 1 } else { 0 }),
        ),
    ]);

    info
}

/// Everything the engine needs to take on a v2-only torrent.
pub struct PreparedV2 {
    /// The v1-shaped bytes the engine is actually given.
    pub synthetic: Vec<u8>,
    /// The real wire identity: SHA-256 of the info dict, truncated per BEP 52.
    pub wire_hash: librqbit::Id20,
    /// The real info dict, for BEP 9 metadata exchange.
    pub info_bytes: Vec<u8>,
    pub verifier: std::sync::Arc<dyn librqbit::PieceVerifier>,
    pub files: usize,
    pub pieces: usize,
}

/// What adding a `.torrent` requires, by the shape of the torrent.
pub enum V2Prep {
    /// A plain v1 torrent. Nothing to do.
    V1Only,
    /// A hybrid. The engine drives it through its v1 half, which is the
    /// interoperable one - but it also has a v2 identity, and BEP 52 expects a
    /// hybrid client to be present in BOTH swarms. Carries the truncated v2
    /// hash to announce alongside the v1 one.
    Hybrid { secondary: librqbit::Id20 },
    /// A v2-only torrent, which needs the whole synthetic model.
    V2Only(Box<PreparedV2>),
}

/// Work out how a `.torrent` has to be added.
pub fn prepare(torrent_bytes: &[u8]) -> Result<V2Prep, String> {
    let Some(meta) = parse(torrent_bytes)? else {
        return Ok(V2Prep::V1Only);
    };
    let truncated = librqbit::Id20::new(meta.truncated_info_hash());
    if meta.has_v1 {
        return Ok(V2Prep::Hybrid {
            secondary: truncated,
        });
    }
    let synthetic = synthetic_v1(&meta);
    Ok(V2Prep::V2Only(Box::new(PreparedV2 {
        synthetic,
        wire_hash: truncated,
        info_bytes: meta.info_bytes.clone(),
        verifier: std::sync::Arc::new(V2Verifier::new(&meta.layout)),
        files: meta.files.len(),
        pieces: meta.layout.pieces.len(),
    })))
}

// ---------------------------------------------------------------- magnet ---

/// The info hashes a magnet link carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MagnetHashes {
    /// `urn:btih:` - 40 hex chars or 32 base32 chars.
    pub v1: Option<[u8; 20]>,
    /// `urn:btmh:` with the multihash prefix `1220` (sha2-256, 32 bytes).
    pub v2: Option<[u8; 32]>,
}

impl MagnetHashes {
    /// A link that names only a v2 torrent. It still resolves - see
    /// [`normalise_magnet`] and [`V2Magnet`] - but it needs the hash exchange
    /// on top of ordinary metadata download.
    pub fn is_v2_only(&self) -> bool {
        self.v1.is_none() && self.v2.is_some()
    }

}

/// Pull the info hashes out of a magnet URI.
///
/// Written here rather than taken from librqbit because its `Magnet` only
/// surfaces one of the two at a time and refuses a v2-only link outright, and
/// the Add flow needs to tell "no hash at all" apart from "a v2 hash we can
/// truncate".
pub fn magnet_hashes(uri: &str) -> MagnetHashes {
    let mut out = MagnetHashes::default();
    let Some(query) = uri.strip_prefix("magnet:?") else {
        return out;
    };
    for param in query.split('&') {
        let Some(value) = param.strip_prefix("xt=").or_else(|| param.strip_prefix("xt.1=")) else {
            continue;
        };
        if let Some(hex) = value.strip_prefix("urn:btih:") {
            if let Some(b) = decode_btih(hex) {
                out.v1 = Some(b);
            }
        } else if let Some(hex) = value.strip_prefix("urn:btmh:") {
            // Multihash: 0x12 = sha2-256, 0x20 = 32-byte digest. Anything else
            // is a hash function we cannot verify with, so it is not usable.
            let bytes = decode_hex(hex);
            if let Some(b) = bytes
                .as_deref()
                .and_then(|b| b.strip_prefix(&[0x12, 0x20][..]))
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
            {
                out.v2 = Some(b);
            }
        }
    }
    out
}

/// Rewrite a magnet so the engine can read the info hash out of it.
///
/// Two things go wrong otherwise. librqbit only inspects the `xt` key, so a
/// hybrid magnet that puts its v1 hash in `xt.1` - which BEP 52 permits, and
/// which real clients emit - looks like a v2-only link and is refused. And a
/// v2-only link has no `btih` at all, though BEP 52 says exactly what its
/// 20-byte identity is: the SHA-256 info hash truncated. Promote that into
/// `xt` and the engine can join the swarm; [`V2Magnet`] handles the rest.
///
/// Returns the URI to hand over, unchanged when there is nothing to fix.
pub fn normalise_magnet(uri: &str) -> Result<String, String> {
    let hashes = magnet_hashes(uri);
    let v1 = match (hashes.v1, hashes.v2) {
        (Some(v1), _) => v1,
        (None, Some(v2)) => {
            // v2-only: its wire identity IS the truncated hash.
            let mut t = [0u8; 20];
            t.copy_from_slice(&v2[..20]);
            t
        }
        // No hash we recognise. Hand it over unchanged and let the engine say
        // so - it parses more spellings of a magnet than this does.
        (None, None) => return Ok(uri.to_string()),
    };

    // Already in the form the engine reads: leave the string alone, so nothing
    // downstream sees a URI that differs from the one the user pasted.
    let hex: String = v1.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    if uri.contains(&format!("xt=urn:btih:{hex}")) {
        return Ok(uri.to_string());
    }

    // Rebuild around the v1 hash, carrying over everything the engine uses.
    let mut out = format!("magnet:?xt=urn:btih:{hex}");
    if let Some(query) = uri.strip_prefix("magnet:?") {
        for param in query.split('&') {
            if param.starts_with("tr=") || param.starts_with("dn=") || param.starts_with("so=") {
                out.push('&');
                out.push_str(param);
            }
        }
    }
    Ok(out)
}

fn decode_btih(s: &str) -> Option<[u8; 20]> {
    if s.len() == 40 {
        return decode_hex(s).and_then(|b| <[u8; 20]>::try_from(b.as_slice()).ok());
    }
    if s.len() == 32 {
        return decode_base32(s);
    }
    None
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
        .collect()
}

/// RFC 4648 base32, the other spelling of a v1 info hash in magnet links.
fn decode_base32(s: &str) -> Option<[u8; 20]> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut acc: u64 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(20);
    for c in s.bytes() {
        let v = A.iter().position(|a| *a == c.to_ascii_uppercase())? as u64;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    <[u8; 20]>::try_from(out.as_slice()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bittorrent::torrent_create::{CreateInput, TorrentVersion, build};

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nt-v2-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a torrent for `payload` with our own writer, then read it back.
    fn round_trip(name: &str, payload: &[u8], piece_length: u32, version: TorrentVersion) -> V2Meta {
        let dir = tmpdir(name);
        let file = dir.join("payload.bin");
        std::fs::write(&file, payload).unwrap();
        let built = build(&CreateInput {
            source: &file,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(piece_length),
            version,
        })
        .unwrap();
        let meta = parse(&built.bytes)
            .expect("v2 torrent failed to parse")
            .expect("writer produced no meta version");
        let _ = std::fs::remove_dir_all(&dir);
        meta
    }

    /// The core claim of the module: every piece hash the writer recorded is
    /// reproduced by the verifier from the piece's real bytes.
    ///
    /// Sizes are chosen around the awkward boundaries - exactly one block, a
    /// partial trailing block, exactly one piece, a non-power-of-two block
    /// count - because those are where the padding rule changes.
    #[test]
    fn every_piece_verifies_against_its_own_bytes() {
        let pl = 4 * BLOCK as u32; // 64 KiB: 4 blocks per piece
        for &len in &[
            1usize,
            BLOCK - 1,
            BLOCK,
            BLOCK + 1,
            3 * BLOCK,
            4 * BLOCK,          // exactly one piece
            4 * BLOCK + 1,      // spills into a second piece
            6 * BLOCK,          // non-power-of-two block count
            8 * BLOCK,
            9 * BLOCK + 123,
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let meta = round_trip(&format!("len{len}"), &payload, pl, TorrentVersion::V2);

            assert_eq!(meta.files.len(), 1, "len={len}");
            assert_eq!(meta.files[0].length, len as u64, "len={len}");
            assert_eq!(
                meta.layout.pieces.len(),
                len.div_ceil(pl as usize),
                "wrong piece count for len={len}"
            );

            for (i, piece) in meta.layout.pieces.iter().enumerate() {
                let start = piece.file_offset as usize;
                let end = start + piece.real_len as usize;
                let got = piece_merkle_root(&payload[start..end], piece.pad_blocks);
                assert_eq!(
                    got, piece.hash,
                    "piece {i} of a {len}-byte file did not verify"
                );
            }
        }
    }

    /// A wrong byte must fail, or the check above proves nothing.
    #[test]
    fn a_corrupt_piece_does_not_verify() {
        let pl = 4 * BLOCK as u32;
        let mut payload: Vec<u8> = (0..(9 * BLOCK + 5)).map(|i| (i * 13) as u8).collect();
        let meta = round_trip("corrupt", &payload, pl, TorrentVersion::V2);

        payload[BLOCK * 5 + 3] ^= 0xff; // inside piece 1
        let p = &meta.layout.pieces[1];
        let start = p.file_offset as usize;
        let got = piece_merkle_root(&payload[start..start + p.real_len as usize], p.pad_blocks);
        assert_ne!(got, p.hash, "a flipped bit still verified");

        // ...and only that piece is affected.
        let p0 = &meta.layout.pieces[0];
        assert_eq!(
            piece_merkle_root(&payload[..p0.real_len as usize], p0.pad_blocks),
            p0.hash,
            "an untouched piece stopped verifying"
        );
    }

    /// Multi-file torrents are where the per-file alignment matters: piece
    /// indices must restart at each file rather than running across the join.
    #[test]
    fn files_are_piece_aligned() {
        let pl = 4 * BLOCK as u32;
        let dir = tmpdir("multi");
        let sub = dir.join("data");
        std::fs::create_dir_all(&sub).unwrap();
        // Deliberately not a multiple of the piece length: file "a" ends mid
        // piece, and "b" must still start on a fresh one.
        let a: Vec<u8> = (0..(5 * BLOCK)).map(|i| (i * 3) as u8).collect();
        let b: Vec<u8> = (0..(2 * BLOCK + 9)).map(|i| (i * 7) as u8).collect();
        std::fs::write(sub.join("a.bin"), &a).unwrap();
        std::fs::write(sub.join("b.bin"), &b).unwrap();

        let built = build(&CreateInput {
            source: &sub,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(pl),
            version: TorrentVersion::V2,
        })
        .unwrap();
        let meta = parse(&built.bytes).unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(meta.files.len(), 2);
        let payloads = [&a, &b];
        // a: 5 blocks over a 4-block piece = 2 pieces. b: 3 blocks = 1 piece.
        assert_eq!(meta.layout.pieces.len(), 3);
        assert_eq!(meta.layout.pieces[0].file_index, 0);
        assert_eq!(meta.layout.pieces[1].file_index, 0);
        assert_eq!(meta.layout.pieces[2].file_index, 1);
        assert_eq!(
            meta.layout.pieces[2].file_offset, 0,
            "the second file's first piece must start at its own offset 0"
        );

        for p in &meta.layout.pieces {
            let data = payloads[p.file_index];
            let start = p.file_offset as usize;
            assert_eq!(
                piece_merkle_root(&data[start..start + p.real_len as usize], p.pad_blocks),
                p.hash,
                "file {} piece at {} did not verify",
                p.file_index,
                p.file_offset
            );
        }
    }

    /// A hybrid must be recognised as carrying both, so the Add flow can keep
    /// preferring the v1 half.
    #[test]
    fn hybrid_is_flagged_and_v1_is_not_parsed() {
        let payload: Vec<u8> = (0..(5 * BLOCK)).map(|i| i as u8).collect();
        let hybrid = round_trip("hybrid", &payload, 4 * BLOCK as u32, TorrentVersion::Hybrid);
        assert!(hybrid.has_v1, "hybrid did not report a v1 half");

        let v2 = round_trip("v2only", &payload, 4 * BLOCK as u32, TorrentVersion::V2);
        assert!(!v2.has_v1, "v2-only reported a v1 half");

        // A plain v1 torrent is not this module's business.
        let dir = tmpdir("v1");
        let f = dir.join("payload.bin");
        std::fs::write(&f, &payload).unwrap();
        let built = build(&CreateInput {
            source: &f,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(4 * BLOCK as u32),
            version: TorrentVersion::V1,
        })
        .unwrap();
        assert!(
            parse(&built.bytes).unwrap().is_none(),
            "a v1 torrent was parsed as v2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The truncated hash is what reaches trackers and the DHT, so it must be
    /// the first 20 bytes of the SHA-256 and nothing cleverer.
    #[test]
    fn truncated_info_hash_is_the_first_20_bytes() {
        let payload: Vec<u8> = (0..(2 * BLOCK)).map(|i| i as u8).collect();
        let meta = round_trip("trunc", &payload, 4 * BLOCK as u32, TorrentVersion::V2);
        assert_eq!(meta.truncated_info_hash()[..], meta.info_hash_v2[..20]);
        assert_eq!(meta.info_hash_v2, sha256(&meta.info_bytes));
    }

    /// Tampering with `piece layers` must be caught. It travels outside the
    /// info dict, so the info hash does not cover it - this check is the only
    /// thing standing between a peer and hashes of its own choosing.
    #[test]
    fn a_forged_piece_layer_is_rejected() {
        let pl = 4 * BLOCK as u32;
        let payload: Vec<u8> = (0..(9 * BLOCK)).map(|i| (i * 5) as u8).collect();
        let dir = tmpdir("forge");
        let file = dir.join("payload.bin");
        std::fs::write(&file, &payload).unwrap();
        let built = build(&CreateInput {
            source: &file,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(pl),
            version: TorrentVersion::V2,
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        // Flip a byte inside the piece layers blob. Its position is found by
        // searching for the real layer, so this stays correct if the writer's
        // key order changes.
        let good = parse(&built.bytes).unwrap().unwrap();
        let layer_hash = good.layout.pieces[0].hash;
        let pos = built
            .bytes
            .windows(32)
            .position(|w| w == layer_hash)
            .expect("piece layer hash not found in the torrent");
        let mut tampered = built.bytes.clone();
        tampered[pos + 5] ^= 0x01;

        let err = parse(&tampered).expect_err("a forged piece layer was accepted");
        assert!(
            err.contains("pieces root"),
            "unexpected rejection reason: {err}"
        );
    }

    /// The whole v1-shaped-view design rests on this: the engine's global
    /// piece index must be the index into `layout.pieces`. If the padding
    /// arithmetic is off by one piece anywhere, every piece after it verifies
    /// against the wrong hash - so this checks the total, not just the shape.
    #[test]
    fn the_synthetic_layout_has_exactly_the_v2_pieces() {
        let pl = 4 * BLOCK as u32;
        let dir = tmpdir("synth");
        let sub = dir.join("data");
        std::fs::create_dir_all(&sub).unwrap();
        // Sizes chosen so every padding case appears: a file ending mid-piece,
        // one ending exactly on a boundary, an empty file, and a final file
        // that must NOT be padded.
        let files: [(&str, usize); 4] = [
            ("a.bin", 5 * BLOCK),     // 2 pieces, ends mid-piece -> pad
            ("b.bin", 8 * BLOCK),     // 2 pieces, exact boundary -> no pad
            ("c.bin", 0),             // empty -> no pieces at all
            ("d.bin", 2 * BLOCK + 7), // 1 piece, last file -> no pad
        ];
        for (n, len) in files {
            let data: Vec<u8> = (0..len).map(|i| (i * 11 + n.len()) as u8).collect();
            std::fs::write(sub.join(n), &data).unwrap();
        }

        let built = build(&CreateInput {
            source: &sub,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(pl),
            version: TorrentVersion::V2,
        })
        .unwrap();
        let meta = parse(&built.bytes).unwrap().unwrap();

        // 2 + 2 + 0 + 1
        assert_eq!(meta.layout.pieces.len(), 5, "unexpected v2 piece count");

        // Now the synthetic view: its total length, cut into pieces, must give
        // the same count.
        let synth = synthetic_v1(&meta);
        let info = crate::bittorrent::metainfo::bencode_lookup(&synth, b"info").unwrap();
        let (v, _) = decode(info, 0).unwrap();
        let entries = match v.get(b"files").unwrap() {
            Val::List(l) => l.clone(),
            _ => panic!("synthetic info has no file list"),
        };
        let total: u64 = entries
            .iter()
            .map(|e| e.get(b"length").unwrap().as_int().unwrap() as u64)
            .sum();
        assert_eq!(
            total.div_ceil(pl as u64) as usize,
            meta.layout.pieces.len(),
            "the synthetic layout has a different number of pieces than v2 does"
        );

        // And each real file must begin exactly on a piece boundary.
        let mut offset = 0u64;
        for e in &entries {
            let len = e.get(b"length").unwrap().as_int().unwrap() as u64;
            let is_pad = e.get(b"attr").and_then(Val::as_bytes) == Some(b"p");
            if !is_pad && len > 0 {
                assert_eq!(
                    offset % pl as u64,
                    0,
                    "a real file started {} bytes into a piece",
                    offset % pl as u64
                );
            }
            offset += len;
        }

        // The filler `pieces` blob has to agree on the count too, or the
        // engine's own bounds checks disagree with ours.
        let pieces = v.get(b"pieces").unwrap().as_bytes().unwrap();
        assert_eq!(pieces.len(), meta.layout.pieces.len() * 20);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The seam itself: feed the engine's view of a piece (real bytes followed
    /// by the padding it invents) and check the verifier still says yes - and
    /// says no when a byte is wrong.
    #[test]
    fn the_verifier_accepts_padded_pieces_and_rejects_corruption() {
        use librqbit::PieceVerifier;

        let pl = 4 * BLOCK as u32;
        let payload: Vec<u8> = (0..(5 * BLOCK)).map(|i| (i * 17) as u8).collect();
        let meta = round_trip("verifier", &payload, pl, TorrentVersion::V2);
        let verifier = V2Verifier::new(&meta.layout);

        assert_eq!(meta.layout.pieces.len(), 2);

        for (idx, piece) in meta.layout.pieces.iter().enumerate() {
            let start = piece.file_offset as usize;
            let real = &payload[start..start + piece.real_len as usize];

            // The engine hands over a full piece: real bytes then padding.
            let mut h = verifier.hasher(idx as u32).unwrap();
            h.update(real);
            let padding = vec![0u8; pl as usize - real.len()];
            h.update(&padding);
            assert!(h.verify(), "piece {idx} failed with its padding attached");

            // Fed in small arbitrary slices, the answer must not change.
            let mut h = verifier.hasher(idx as u32).unwrap();
            for chunk in real.chunks(1000) {
                h.update(chunk);
            }
            h.update(&padding);
            assert!(h.verify(), "piece {idx} failed when fed in small chunks");
        }

        // One wrong byte in the first piece.
        let mut bad = payload.clone();
        bad[3] ^= 0xff;
        let mut h = verifier.hasher(0).unwrap();
        h.update(&bad[..meta.layout.pieces[0].real_len as usize]);
        assert!(!h.verify(), "a corrupt piece verified");

        // An index past the end is not ours to answer for.
        assert!(verifier.hasher(99).is_none());
    }

    #[test]
    fn magnet_hashes_are_read_from_both_urns() {
        let v1 = "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862&dn=x";
        let h = magnet_hashes(v1);
        assert!(h.v1.is_some() && h.v2.is_none());

        let v2 = "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e";
        let h = magnet_hashes(v2);
        assert!(h.v1.is_none(), "a v2 magnet reported a v1 hash");
        assert_eq!(h.v2.map(|b| b[0]), Some(0xca));

        // Hybrid magnet: both urns present, v1 wins on the wire.
        let both = "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862\
                    &xt.1=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e";
        let h = magnet_hashes(both);
        assert!(h.v1.is_some() && h.v2.is_some());

        // A multihash naming some other function is not something we can
        // verify with, so it must not be mistaken for a usable v2 hash.
        let sha3 = "magnet:?xt=urn:btmh:1620caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e";
        assert!(magnet_hashes(sha3).v2.is_none());

        assert_eq!(magnet_hashes("not a magnet"), MagnetHashes::default());
    }

    /// Hybrid magnets must work whichever way round the two urns are written -
    /// BEP 52 allows both, and the engine only ever looks at `xt`.
    /// Build the full merkle tree for a payload, layer by layer, exactly as a
    /// seeder holds it. Returns `tree[0] = leaves`, `tree[n] = layer n`.
    fn full_tree(payload: &[u8]) -> Vec<Vec<[u8; 32]>> {
        let mut layer: Vec<[u8; 32]> = payload.chunks(BLOCK).map(sha256).collect();
        layer.resize(next_pow2(layer.len()), [0u8; 32]);
        let mut tree = vec![layer.clone()];
        while layer.len() > 1 {
            layer = layer.chunks(2).map(|p| hash_pair(&p[0], &p[1])).collect();
            tree.push(layer.clone());
        }
        tree
    }

    /// Answer a hash request out of the full tree, the way a seeding peer
    /// would: the requested run, then the uncle hashes upward.
    fn answer(tree: &[Vec<[u8; 32]>], plan: &HashRequestPlan) -> (Vec<[u8; 32]>, Vec<[u8; 32]>) {
        let base_layer = &tree[plan.base_layer as usize];
        let start = plan.index as usize;
        let base = base_layer[start..start + plan.length as usize].to_vec();

        let mut proof = Vec::new();
        // The run's own root sits this far up; its siblings are the uncles.
        let mut level = plan.base_layer as usize + log2_exact(plan.length as usize) as usize;
        let mut pos = start / plan.length as usize;
        for _ in 0..plan.proof_layers {
            let sibling = if pos.is_multiple_of(2) { pos + 1 } else { pos - 1 };
            proof.push(tree[level][sibling]);
            level += 1;
            pos /= 2;
        }
        (base, proof)
    }

    /// The hash exchange end to end, without a network: plan the requests a
    /// magnet would need, answer them from the tree a seeder holds, and check
    /// the answers fold back to the `pieces root` in the info dict.
    #[test]
    fn a_peers_hash_answer_folds_back_to_the_pieces_root() {
        let pl = 4 * BLOCK as u32; // 4 blocks per piece
        // 33 blocks: not a power of two, so the tree is padded and the last
        // piece is short - both of the cases that break naive folding.
        let payload: Vec<u8> = (0..(33 * BLOCK)).map(|i| (i * 19) as u8).collect();
        let meta = round_trip("hashreq", &payload, pl, TorrentVersion::V2);
        let root = meta.files[0].pieces_root.unwrap();
        let tree = full_tree(&payload);
        assert_eq!(*tree.last().unwrap(), vec![root], "test tree disagrees");

        let plans = plan_piece_layer_requests(payload.len() as u64, pl);
        assert_eq!(plans.len(), 1, "one request should cover 64 nodes");
        let plan = plans[0];
        assert_eq!(plan.base_layer, 2, "piece layer is 2 above the leaves");
        assert_eq!(plan.length, 16, "64 padded leaves / 4 per piece");
        assert_eq!(plan.proof_layers, 0, "the run already spans the whole tree");

        let (base, proof) = answer(&tree, &plan);
        assert!(
            verify_hashes(&base, &proof, plan.index, &root),
            "an honest answer was rejected"
        );

        // The real piece hashes are the first ceil(len/piece) of the run.
        let want = payload.len().div_ceil(pl as usize);
        for (i, piece) in meta.layout.pieces.iter().enumerate().take(want) {
            assert_eq!(base[i], piece.hash, "piece {i} hash disagrees with the layer");
        }

        // One flipped hash must fail.
        let mut bad = base.clone();
        bad[3][0] ^= 0xff;
        assert!(
            !verify_hashes(&bad, &proof, plan.index, &root),
            "a forged hash was accepted"
        );
    }

    /// A file big enough to need several requests exercises the proof layers,
    /// which the single-request case above skips entirely.
    #[test]
    fn a_multi_request_layer_verifies_with_its_proofs() {
        let pl = BLOCK as u32; // 1 block per piece -> lots of piece-layer nodes
        let payload: Vec<u8> = (0..(600 * BLOCK)).map(|i| (i * 7) as u8).collect();
        let tree = full_tree(&payload);
        let root = *tree.last().unwrap().first().unwrap();

        let plans = plan_piece_layer_requests(payload.len() as u64, pl);
        assert!(plans.len() > 1, "600 blocks should need several requests");
        assert!(
            plans.iter().all(|p| p.proof_layers > 0),
            "a partial run must carry proofs"
        );

        let mut layer = Vec::new();
        for plan in &plans {
            let (base, proof) = answer(&tree, plan);
            assert!(
                verify_hashes(&base, &proof, plan.index, &root),
                "run at index {} was rejected",
                plan.index
            );
            layer.extend_from_slice(&base);
        }
        assert_eq!(layer.len(), 1024, "1024 padded leaves at one block a piece");
        assert_eq!(layer[..600], tree[0][..600], "recovered layer is wrong");

        // A proof from the wrong position must not verify.
        let (base, _) = answer(&tree, &plans[0]);
        let (_, wrong_proof) = answer(&tree, &plans[1]);
        assert!(
            !verify_hashes(&base, &wrong_proof, plans[0].index, &root),
            "a mismatched proof was accepted"
        );
    }

    /// The entire v2 magnet path, with the network replaced by a tree.
    ///
    /// This is the flow the engine drives in `peer_info_reader`: verify the
    /// info dict, ask for the piece layers, feed the answers back, then parse
    /// the substituted model and verify pieces against it. A magnet has none
    /// of the `piece layers` a `.torrent` carries, so every hash here came
    /// over the wire and had to be proved against `pieces root`.
    #[test]
    fn a_v2_magnet_resolves_from_nothing_but_its_info_hash() {
        use librqbit::{MetadataInterceptor, PieceVerifier};

        let pl = 4 * BLOCK as u32;
        let dir = tmpdir("magnetflow");
        let sub = dir.join("d");
        std::fs::create_dir_all(&sub).unwrap();
        // One file needing several piece-layer nodes, and one small enough to
        // need none at all - the two paths through hash_requests.
        let big: Vec<u8> = (0..(33 * BLOCK)).map(|i| (i * 19) as u8).collect();
        let small: Vec<u8> = (0..(2 * BLOCK)).map(|i| (i * 3) as u8).collect();
        std::fs::write(sub.join("big.bin"), &big).unwrap();
        std::fs::write(sub.join("small.bin"), &small).unwrap();

        let built = build(&CreateInput {
            source: &sub,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(pl),
            version: TorrentVersion::V2,
        })
        .unwrap();

        // All a magnet gives us: the info dict (over BEP 9) and the hash.
        let full = parse(&built.bytes).unwrap().unwrap();
        let info_bytes = full.info_bytes.clone();
        let info_hash = librqbit::Id20::new(full.truncated_info_hash());

        let magnet = V2Magnet::new();

        assert!(
            magnet.verify_info(&info_bytes, info_hash),
            "the real info dict failed its own hash check"
        );
        assert!(
            !magnet.verify_info(&info_bytes, librqbit::Id20::new([0u8; 20])),
            "a wrong info hash was accepted"
        );

        let requests = magnet.hash_requests(&info_bytes).unwrap();
        assert!(!requests.is_empty(), "no piece layers were requested");
        // Only the big file needs a layer; the small one fits in a piece.
        let big_root = full.files.iter().find(|f| f.path() == "big.bin").unwrap();
        assert!(
            requests.iter().all(|r| r.pieces_root == big_root.pieces_root.unwrap()),
            "a layer was requested for a file that does not need one"
        );

        // Answer them the way a seeding peer would.
        let tree = full_tree(&big);
        let mut done = false;
        for (i, r) in requests.iter().enumerate() {
            let plan = HashRequestPlan {
                base_layer: r.base_layer,
                index: r.index,
                length: r.length,
                proof_layers: r.proof_layers,
            };
            let (base, proof) = answer(&tree, &plan);
            let mut all = base.clone();
            all.extend_from_slice(&proof);
            done = magnet.on_hashes(r, &all).unwrap();
            assert_eq!(
                done,
                i == requests.len() - 1,
                "completion was reported at the wrong request"
            );
        }
        assert!(done, "the exchange never completed");

        // The engine parses this as an INFO DICT, not as a whole torrent -
        // asserting the wrong one here is what let a real v2 magnet fail with
        // "missing field `pieces`" while this test passed.
        let substituted = magnet.substitute_info(&info_bytes).unwrap();
        let raw: librqbit::TorrentMetaV1Info<librqbit::ByteBuf> =
            bencode::from_bytes(&substituted)
                .expect("substitute_info must return the info dict alone");
        let info = raw.validate().unwrap();
        assert_eq!(
            info.lengths().total_pieces() as usize,
            full.layout.pieces.len(),
            "the substituted model has a different piece count"
        );

        // And the collected layers verify real data.
        let payloads = [&big, &small];
        for (idx, piece) in full.layout.pieces.iter().enumerate() {
            let data = payloads[piece.file_index];
            let start = piece.file_offset as usize;
            let real = &data[start..start + piece.real_len as usize];
            let mut h = magnet.hasher(idx as u32).expect("no hasher for a real piece");
            h.update(real);
            h.update(&vec![0u8; pl as usize - real.len()]);
            assert!(h.verify(), "piece {idx} failed against the fetched layers");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A peer that answers with hashes for data of its own choosing must be
    /// caught - `piece layers` is not covered by the info hash.
    #[test]
    fn a_magnet_rejects_forged_hashes() {
        use librqbit::MetadataInterceptor;

        let pl = 4 * BLOCK as u32;
        let payload: Vec<u8> = (0..(33 * BLOCK)).map(|i| (i * 11) as u8).collect();
        let meta = round_trip("magnetforge", &payload, pl, TorrentVersion::V2);

        let magnet = V2Magnet::new();
        let requests = magnet.hash_requests(&meta.info_bytes).unwrap();
        assert!(!requests.is_empty());

        let tree = full_tree(&payload);
        let r = &requests[0];
        let plan = HashRequestPlan {
            base_layer: r.base_layer,
            index: r.index,
            length: r.length,
            proof_layers: r.proof_layers,
        };
        let (mut base, proof) = answer(&tree, &plan);
        base[2][0] ^= 0xff; // one hash swapped for something else
        let mut all = base;
        all.extend_from_slice(&proof);

        let err = magnet
            .on_hashes(r, &all)
            .expect_err("forged hashes were accepted");
        assert!(
            err.to_string().contains("pieces root"),
            "unexpected reason: {err}"
        );

        // Too few hashes must also be refused rather than read past the end.
        assert!(magnet.on_hashes(r, &[]).is_err());
    }

    /// A v2-only torrent, through the REAL engine, with the data already on
    /// disk: the initial check must verify every piece and report complete.
    ///
    /// The unit tests above prove the merkle arithmetic. This proves the
    /// arithmetic is actually reached - that the verifier is installed, that
    /// the synthetic layout lines up with what the engine reads off disk, and
    /// that the file lands where the torrent says it should.
    #[tokio::test]
    async fn a_v2_only_torrent_verifies_through_the_engine() {
        let dir = tmpdir("engine-v2");
        let payload: Vec<u8> = (0..(5 * BLOCK)).map(|i| (i * 29) as u8).collect();
        let file = dir.join("payload.bin");
        std::fs::write(&file, &payload).unwrap();

        let built = build(&CreateInput {
            source: &file,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(4 * BLOCK as u32),
            version: TorrentVersion::V2,
        })
        .unwrap();

        let prepared = match prepare(&built.bytes).unwrap() {
            V2Prep::V2Only(p) => p,
            _ => panic!("not recognised as v2-only"),
        };

        let session = librqbit::Session::new_with_opts(
            dir.clone(),
            librqbit::SessionOptions {
                dht: None,
                listen: None,
                disable_trackers: true,
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let handle = session
            .add_torrent(
                librqbit::AddTorrent::from_bytes(prepared.synthetic.clone()),
                Some(librqbit::AddTorrentOptions {
                    overwrite: true,
                    override_info_hash: Some(prepared.wire_hash),
                    override_info_bytes: Some(prepared.info_bytes.clone().into()),
                    piece_verifier: Some(prepared.verifier.clone()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        // The identity on the wire is the truncated SHA-256, not the hash of
        // the synthetic bytes the engine was handed.
        assert_eq!(handle.info_hash(), prepared.wire_hash);

        let done = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            handle.wait_until_completed(),
        )
        .await;

        let stats = handle.stats();
        assert!(
            done.is_ok(),
            "initial check did not complete: {}/{} bytes, {:?}",
            stats.progress_bytes,
            stats.total_bytes,
            stats.state
        );
        done.unwrap().unwrap();

        // Where the data has to be: the torrent names one file, so it is that
        // file - NOT a directory of the same name containing it.
        assert!(
            file.exists(),
            "the engine did not use the file already on disk"
        );
        assert_eq!(std::fs::read(&file).unwrap(), payload, "data was rewritten");

        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same, multi-file - which is the case where the layout could go
    /// wrong.
    ///
    /// librqbit only inserts the torrent name as a containing directory when
    /// the info dict lists two or more files (`get_default_subfolder_for_torrent`
    /// returns None below that). Our synthetic dict always uses the multi-file
    /// *form*, so a single-file v2 torrent gets no subfolder - correct - and a
    /// genuinely multi-file one does. This pins the second half of that.
    #[tokio::test]
    async fn a_multi_file_v2_torrent_lands_in_the_right_places() {
        let dir = tmpdir("engine-v2-multi");
        let src = dir.join("payload");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let a: Vec<u8> = (0..(5 * BLOCK)).map(|i| (i * 13) as u8).collect();
        let b: Vec<u8> = (0..(2 * BLOCK + 77)).map(|i| (i * 7) as u8).collect();
        std::fs::write(src.join("a.bin"), &a).unwrap();
        std::fs::write(src.join("sub").join("b.bin"), &b).unwrap();

        let built = build(&CreateInput {
            source: &src,
            trackers: &[],
            comment: "",
            created_by: "test".into(),
            private: false,
            piece_length: Some(4 * BLOCK as u32),
            version: TorrentVersion::V2,
        })
        .unwrap();
        let prepared = match prepare(&built.bytes).unwrap() {
            V2Prep::V2Only(p) => p,
            _ => panic!("not recognised as v2-only"),
        };
        assert_eq!(prepared.files, 2);

        // Output folder is the PARENT: the engine adds "payload" itself.
        let session = librqbit::Session::new_with_opts(
            dir.clone(),
            librqbit::SessionOptions {
                dht: None,
                listen: None,
                disable_trackers: true,
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let handle = session
            .add_torrent(
                librqbit::AddTorrent::from_bytes(prepared.synthetic.clone()),
                Some(librqbit::AddTorrentOptions {
                    overwrite: true,
                    override_info_hash: Some(prepared.wire_hash),
                    override_info_bytes: Some(prepared.info_bytes.clone().into()),
                    piece_verifier: Some(prepared.verifier.clone()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        let done = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            handle.wait_until_completed(),
        )
        .await;
        let stats = handle.stats();
        assert!(
            done.is_ok(),
            "did not verify: {}/{} bytes, {:?}",
            stats.progress_bytes,
            stats.total_bytes,
            stats.state
        );
        done.unwrap().unwrap();

        // The existing files were recognised in place - nothing was written to
        // a doubled-up path like payload/payload/a.bin.
        assert_eq!(std::fs::read(src.join("a.bin")).unwrap(), a);
        assert_eq!(std::fs::read(src.join("sub").join("b.bin")).unwrap(), b);
        assert!(
            !src.join("payload").exists(),
            "the torrent name was applied twice"
        );

        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A REAL v2-only magnet, against the live swarm.
    ///
    /// This is the one thing no amount of local testing can stand in for. The
    /// whole v2 magnet path has to work against peers we did not write:
    ///
    /// 1. join the swarm under the SHA-256 hash truncated to 20 bytes,
    /// 2. fetch the info dict over BEP 9 and accept it - the engine's own
    ///    check is SHA-1 and would reject it,
    /// 3. fetch `piece layers`, which are NOT in the info dict, using the
    ///    BEP 52 hash messages,
    /// 4. verify every answer against the file's `pieces root`,
    /// 5. download and hash-check pieces against the layers we collected.
    ///
    /// Run with:
    ///   cargo test --bin nanotorrent-gui a_real_v2_magnet -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "joins a real BitTorrent v2 swarm over the network"]
    async fn a_real_v2_magnet_resolves_against_the_live_swarm() {
        const URI: &str = "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e&dn=bittorrent-v2-test";

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "nanotorrent=debug,librqbit=info".into()),
            )
            .with_writer(std::io::stderr)
            .try_init();

        let hashes = magnet_hashes(URI);
        assert!(hashes.is_v2_only(), "the fixture is not a v2-only magnet");

        let fixed = normalise_magnet(URI).unwrap();
        let want: String = hashes.v2.unwrap()[..20]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            fixed.contains(&format!("xt=urn:btih:{want}")),
            "not promoted to the truncated hash: {fixed}"
        );
        eprintln!("joining swarm as {want}");

        let dir = tmpdir("live-v2-magnet");
        let magnet = std::sync::Arc::new(V2Magnet::new());

        // DHT on: the magnet carries no trackers, so it is the only way to
        // find anyone. A listener too, so peers can reach back.
        let session = librqbit::Session::new_with_opts(
            dir.clone(),
            librqbit::SessionOptions {
                dht: Some(Default::default()),
                listen: Some(Default::default()),
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let added = tokio::time::timeout(
            std::time::Duration::from_secs(240),
            session.add_torrent(
                librqbit::AddTorrent::from_url(fixed),
                Some(librqbit::AddTorrentOptions {
                    overwrite: true,
                    metadata_interceptor: Some(magnet.clone()),
                    piece_verifier: Some(magnet.clone()),
                    ..Default::default()
                }),
            ),
        )
        .await;

        let handle = match added {
            Err(_) => panic!("timed out before the metadata resolved - no peers, or the hash exchange never completed"),
            Ok(Err(e)) => panic!("add failed: {e:#}"),
            Ok(Ok(r)) => r.into_handle().unwrap(),
        };

        // Step 1-4 are done by here: the info dict was accepted, the piece
        // layers were fetched and each verified against its pieces root.
        let meta = handle.metadata.load_full().expect("no metadata after add");
        eprintln!(
            "resolved: name={:?} files={} total={} pieces={}",
            handle.name(),
            meta.file_infos.len(),
            meta.file_infos.iter().map(|f| f.len).sum::<u64>(),
            meta.lengths().total_pieces()
        );
        for f in meta.file_infos.iter().take(10) {
            eprintln!("  {} ({} bytes)", f.relative_filename.display(), f.len);
        }

        // Step 5: pieces must actually verify against the layers we fetched.
        //
        // Deliberately NOT "download the whole thing": the torrent is 1.45 GiB
        // and pulling it on every run would be a poor test. What has to be
        // true is that real pieces from real peers pass the merkle check - a
        // single verified piece proves the layers are right, because a wrong
        // layer fails every piece rather than some of them.
        //
        // (Run once to completion by hand: it reached 1.43 GiB of 1.45 GiB in
        // 300s before this assertion was relaxed, all of it verified.)
        let piece_len = meta.lengths().default_piece_length() as u64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
        let mut verified = 0u64;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            verified = handle.stats().progress_bytes;
            if verified >= piece_len * 4 {
                break;
            }
        }
        let stats = handle.stats();
        eprintln!(
            "verified {} bytes of {} ({} pieces), state {:?}",
            verified,
            stats.total_bytes,
            verified / piece_len.max(1),
            stats.state
        );
        assert!(
            verified >= piece_len,
            "not one piece verified against the fetched layers ({verified} bytes)"
        );

        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Is the DHT actually healthy on this machine?
    ///
    /// Separates "our lookup is broken" from "that swarm has nobody in it",
    /// which are indistinguishable from a magnet that resolves nothing. On
    /// Windows this used to be the former: the DHT socket died on the first
    /// ICMP unreachable (see the sockets patch), so the routing table never
    /// grew past whatever was loaded from cache.
    #[tokio::test]
    #[ignore = "talks to the real DHT"]
    async fn the_dht_is_alive() {
        let dir = tmpdir("dht-health");
        let session = librqbit::Session::new_with_opts(
            dir.clone(),
            librqbit::SessionOptions {
                dht: Some(Default::default()),
                listen: Some(Default::default()),
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let dht = session.get_dht().expect("no DHT").clone();
        let mut last = 0usize;
        for i in 0..12 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let s = dht.stats();
            eprintln!(
                "t+{:>3}s  routing table {}  outstanding {}",
                (i + 1) * 5,
                s.routing_table_size,
                s.outstanding_requests
            );
            last = s.routing_table_size;
        }
        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            last > 50,
            "the routing table only reached {last} nodes - the DHT is not working"
        );
    }

    /// Ask the DHT directly whether anyone is holding a given info hash.
    ///
    /// The last step in telling a broken client from an empty swarm: this
    /// bypasses everything of ours except the hash itself.
    #[tokio::test]
    #[ignore = "queries the real DHT for peers"]
    async fn who_has_this_infohash() {
        use futures::StreamExt;

        // Defaults to the libtorrent v2 test torrent's truncated hash.
        let hex = std::env::var("NT_INFOHASH")
            .unwrap_or_else(|_| "caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa".into());
        let bytes: Vec<u8> = (0..20)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let id = librqbit::Id20::new(bytes.try_into().unwrap());
        eprintln!("asking the DHT who has {hex}");

        let dir = tmpdir("dht-peers");
        let session = librqbit::Session::new_with_opts(
            dir.clone(),
            librqbit::SessionOptions {
                dht: Some(Default::default()),
                listen: Some(Default::default()),
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let dht = session.get_dht().expect("no DHT").clone();

        let mut stream = dht.get_peers(id, None);
        let mut found = 0usize;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => break,
                Ok(None) => break,
                Ok(Some(addr)) => {
                    found += 1;
                    if found <= 10 {
                        eprintln!("  peer: {addr}");
                    }
                }
            }
        }
        eprintln!("DHT returned {found} peers for {hex}");
        session.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hybrid_magnets_are_normalised_whichever_way_round() {
        let v1hex = "cab507494d02ebb1178b38f2e9d7be299c86b862";
        let v2hex = "1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e";

        // v1 first: nothing to do, and the string must come back untouched.
        let a = format!("magnet:?xt=urn:btih:{v1hex}&xt.1=urn:btmh:{v2hex}&dn=x&tr=udp%3A%2F%2Ft");
        assert_eq!(normalise_magnet(&a).unwrap(), a);

        // v2 first: this is the one the engine refuses today.
        let b = format!("magnet:?xt=urn:btmh:{v2hex}&xt.1=urn:btih:{v1hex}&dn=x&tr=udp%3A%2F%2Ft");
        let fixed = normalise_magnet(&b).unwrap();
        assert!(
            fixed.starts_with(&format!("magnet:?xt=urn:btih:{v1hex}")),
            "v1 hash was not promoted to xt: {fixed}"
        );
        assert!(fixed.contains("dn=x"), "display name was dropped: {fixed}");
        assert!(
            fixed.contains("tr=udp%3A%2F%2Ft"),
            "tracker was dropped: {fixed}"
        );
        assert!(!fixed.contains("btmh"), "the v2 urn should not survive");

        // A base32 v1 hash is promoted to hex, which is what the engine reads.
        let b32 = "magnet:?xt=urn:btih:ZK2QOSKNALV3CF4LHDZOTV56FGOINODC";
        assert_eq!(
            normalise_magnet(b32).unwrap(),
            format!("magnet:?xt=urn:btih:{v1hex}")
        );

        // v2-only: promoted to its truncated identity, which is what BEP 52
        // says the swarm is keyed by.
        let v2only = format!("magnet:?xt=urn:btmh:{v2hex}&dn=y");
        let fixed = normalise_magnet(&v2only).unwrap();
        let want: String = magnet_hashes(&v2only).v2.unwrap()[..20]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            fixed.starts_with(&format!("magnet:?xt=urn:btih:{want}")),
            "v2-only was not promoted to its truncated hash: {fixed}"
        );
        assert!(fixed.contains("dn=y"), "display name was dropped: {fixed}");

        // Unrecognised: passed through, so the engine gets to report it.
        assert_eq!(normalise_magnet("magnet:?dn=x").unwrap(), "magnet:?dn=x");
    }

    #[test]
    fn base32_info_hashes_decode() {
        // Same hash, both spellings.
        let hex = "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862";
        let b32 = "magnet:?xt=urn:btih:ZK2QOSKNALV3CF4LHDZOTV56FGOINODC";
        assert_eq!(magnet_hashes(hex).v1, magnet_hashes(b32).v1);
    }

    /// Path components that would escape the download folder are refused.
    #[test]
    fn traversal_in_the_file_tree_is_refused() {
        // Assembled here rather than written out: the writer cannot produce
        // one of these, and hand-written bencode with a raw 32-byte hash in it
        // is too easy to get subtly wrong.
        let mut leaf = Vec::new();
        leaf.extend_from_slice(b"d6:lengthi1e11:pieces root32:");
        leaf.extend_from_slice(&[0u8; 32]);
        leaf.push(b'e');

        let mut info = Vec::new();
        info.extend_from_slice(b"d9:file treed2:..d7:payloadd0:");
        info.extend_from_slice(&leaf);
        info.extend_from_slice(b"eee"); // closes "payload", "..", the file tree
        info.extend_from_slice(b"12:meta versioni2e4:name1:x12:piece lengthi16384e");
        info.push(b'e'); //                closes the info dict

        let mut evil = Vec::new();
        evil.extend_from_slice(b"d4:info");
        evil.extend_from_slice(&info);
        evil.push(b'e');

        let err = parse(&evil).expect_err("a traversal path was accepted");
        assert!(err.contains("illegal path component"), "got: {err}");
    }
}

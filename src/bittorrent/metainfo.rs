//! Reading a torrent's metainfo without parsing it into a struct.
//!
//! Everything here walks the raw bencode, because the questions it answers are
//! about keys the engine's parser does not model: `librqbit`'s metainfo struct
//! is `TorrentMetaV1Info`, so it has no notion of `meta version` or a v2
//! `file tree` and cannot be asked whether a torrent has them.
//!
//! Lives here rather than in `session.rs` because two unrelated features need
//! it: the details panel labels its info hashes by version, and the Add flow
//! turns a v2-only torrent into a message a person can act on.

use crate::core::bencode::dict_get;

/// The v1 and v2 info hashes of a torrent, from its raw bencoded info dict.
///
/// librqbit only ever reports the v1 `Id20`, so nothing downstream could tell
/// a v1 torrent from a hybrid one. Both hashes are taken over the SAME bytes -
/// that is precisely what a hybrid torrent is - so this hashes one buffer
/// twice rather than parsing anything twice.
///
/// Which ones EXIST is decided by the dictionary's own keys, per BEP 52:
/// `pieces` means there is a v1 hash, `meta version` 2 means there is a v2
/// one, and a hybrid carries both. Hashing unconditionally and labelling by
/// key is the only way to get this right - SHA-256 of a v1 dict is a number,
/// but it is not an info hash anyone can use.
pub fn info_hashes(info_bytes: &[u8]) -> (Option<String>, Option<String>) {
    use sha1::{Digest, Sha1};
    use sha2::Sha256;

    let has_v1 = dict_get(info_bytes, b"pieces").is_some();
    // The value, not just the key: BEP 52 defines exactly version 2, and a
    // future version would need its own hashing rule rather than this one.
    let has_v2 = dict_get(info_bytes, b"meta version") == Some(b"i2e");

    let hex = |bytes: &[u8]| bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });

    (
        has_v1.then(|| hex(&Sha1::digest(info_bytes))),
        has_v2.then(|| hex(&Sha256::digest(info_bytes))),
    )
}


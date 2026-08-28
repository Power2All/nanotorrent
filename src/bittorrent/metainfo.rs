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

    let has_v1 = bencode_lookup(info_bytes, b"pieces").is_some();
    // The value, not just the key: BEP 52 defines exactly version 2, and a
    // future version would need its own hashing rule rather than this one.
    let has_v2 = bencode_lookup(info_bytes, b"meta version") == Some(b"i2e");

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

/// The raw bencode value stored under a TOP-LEVEL key of `dict`.
///
/// A substring search would be wrong: `pieces` and `meta version` are both
/// perfectly legal file names inside a v2 `file tree`, so finding those bytes
/// somewhere in the buffer proves nothing. This walks values properly and only
/// looks at the outermost dictionary, which is the scope BEP 52 defines the
/// keys in.
fn bencode_lookup<'a>(dict: &'a [u8], want: &[u8]) -> Option<&'a [u8]> {
    if dict.first() != Some(&b'd') {
        return None;
    }
    let mut i = 1;
    while *dict.get(i)? != b'e' {
        let (key, after_key) = bencode_str(dict, i)?;
        let end = bencode_skip(dict, after_key, 0)?;
        if key == want {
            return Some(&dict[after_key..end]);
        }
        i = end;
    }
    None
}

/// A bencode byte string at `i`: its contents, and the index just past it.
fn bencode_str(b: &[u8], i: usize) -> Option<(&[u8], usize)> {
    let colon = i + b.get(i..)?.iter().position(|c| *c == b':')?;
    let len: usize = std::str::from_utf8(b.get(i..colon)?).ok()?.parse().ok()?;
    let start = colon + 1;
    let end = start.checked_add(len)?;
    (end <= b.len()).then(|| (&b[start..end], end))
}

/// The index just past the bencode value starting at `i`.
///
/// `depth` is a guard, not bookkeeping: this recurses through nested lists and
/// dicts, and a v2 `file tree` is attacker-supplied. librqbit has already
/// parsed the buffer by the time we see it, so a bomb should be impossible -
/// which is exactly the assumption worth not betting the stack on.
fn bencode_skip(b: &[u8], i: usize, depth: u32) -> Option<usize> {
    const MAX_DEPTH: u32 = 32;
    if depth > MAX_DEPTH {
        return None;
    }
    match *b.get(i)? {
        b'i' => Some(i + b.get(i..)?.iter().position(|c| *c == b'e')? + 1),
        b'l' | b'd' => {
            let mut j = i + 1;
            while *b.get(j)? != b'e' {
                j = bencode_skip(b, j, depth + 1)?;
            }
            Some(j + 1)
        }
        b'0'..=b'9' => bencode_str(b, i).map(|(_, end)| end),
        _ => None,
    }
}

/// Whether these `.torrent` bytes are a **v2-only** torrent.
///
/// The distinction that matters to a user: a hybrid carries the v1 keys as
/// well and downloads fine, while a v2-only torrent has nothing a v1 engine
/// can use. `librqbit` 8.1.1 fails it deep in serde with "missing field
/// `pieces`", which is true but tells nobody anything.
///
/// Takes the whole file, not the info dict - this runs on bytes that failed to
/// parse, so there is no parsed torrent to take an info dict from.
pub fn is_v2_only(torrent_bytes: &[u8]) -> bool {
    let Some(info) = bencode_lookup(torrent_bytes, b"info") else {
        return false;
    };
    bencode_lookup(info, b"meta version") == Some(b"i2e")
        && bencode_lookup(info, b"pieces").is_none()
}

#[cfg(test)]
mod tests {
    /// The three torrent shapes must be told apart from the info dict alone,
    /// because librqbit reports only the v1 id for all of them.
    ///
    /// The nested-key case is the one that matters: a v2 `file tree` can hold
    /// a file literally called "pieces", and a substring search would then
    /// report a v1 hash that does not exist.
    #[test]
    fn info_hashes_follow_the_dict_keys() {
        // v1: pieces, no meta version.
        let v1 = b"d6:lengthi4e4:name1:x12:piece lengthi16384e6:pieces0:e";
        let (a, b) = super::info_hashes(v1);
        assert!(a.is_some(), "v1 torrent has no v1 hash");
        assert!(b.is_none(), "v1 torrent reported a v2 hash");
        assert_eq!(a.as_deref().map(str::len), Some(40), "v1 hash is not SHA-1");

        // v2: meta version 2 and a file tree, no pieces at the top level -
        // but a file INSIDE the tree is called "pieces".
        let v2 = b"d9:file treed6:piecesd0:d6:lengthi4e11:pieces rooti0eeee12:meta versioni2e4:name1:x12:piece lengthi16384ee";
        let (a, b) = super::info_hashes(v2);
        assert!(a.is_none(), "the nested \"pieces\" was mistaken for a v1 dict");
        assert!(b.is_some(), "v2 torrent has no v2 hash");
        assert_eq!(b.as_deref().map(str::len), Some(64), "v2 hash is not SHA-256");

        // Hybrid: both, hashed over the same bytes.
        let hy = b"d9:file treede12:meta versioni2e4:name1:x12:piece lengthi16384e6:pieces0:e";
        let (a, b) = super::info_hashes(hy);
        assert!(a.is_some() && b.is_some(), "hybrid is missing a hash");
        assert_ne!(a, b);

        // Not a dict, and a truncated one: no panic, no hashes.
        assert_eq!(super::info_hashes(b"junk"), (None, None));
        assert_eq!(super::info_hashes(b"d6:pieces"), (None, None));

        // A version this code does not know how to hash is not called v2.
        assert_eq!(super::info_hashes(b"d12:meta versioni3ee"), (None, None));
    }

    /// The three shapes have to be told apart from the file alone, because
    /// this runs on bytes the engine has already refused to parse.
    ///
    /// The v2 fixture holds a file literally named "pieces" inside its
    /// `file tree`: a substring search would see that, conclude there is a v1
    /// half, and send the user back the unreadable serde error.
    #[test]
    fn only_a_v2_only_torrent_is_flagged() {
        let wrap = |info: &[u8]| {
            let mut v = b"d8:announce3:foo4:info".to_vec();
            v.extend_from_slice(info);
            v.push(b'e');
            v
        };

        let v1 = b"d6:lengthi4e4:name1:x12:piece lengthi16384e6:pieces0:e";
        let v2 = b"d9:file treed6:piecesd0:d6:lengthi4e11:pieces rooti0eeee12:meta versioni2e4:name1:x12:piece lengthi16384ee";
        let hybrid = b"d9:file treede12:meta versioni2e4:name1:x12:piece lengthi16384e6:pieces0:e";

        assert!(super::is_v2_only(&wrap(v2)), "v2-only was not detected");
        assert!(!super::is_v2_only(&wrap(v1)), "a v1 torrent was called v2-only");
        assert!(
            !super::is_v2_only(&wrap(hybrid)),
            "a hybrid was called v2-only - it downloads fine as v1"
        );

        // Nothing here may panic: it runs on input that already failed to
        // parse, so malformed is the expected case rather than the odd one.
        assert!(!super::is_v2_only(b"junk"));
        assert!(!super::is_v2_only(b""));
        assert!(!super::is_v2_only(b"d4:info"));
        // Truncated mid-key: the walk must give up, not read past the end.
        assert!(!super::is_v2_only(&wrap(b"d12:meta versi")));
        // A bare `meta version` 2 with no `pieces` IS the v2-only signature,
        // even without the rest of the dict - that is the whole test.
        assert!(super::is_v2_only(&wrap(b"d12:meta versioni2e")));
    }
}


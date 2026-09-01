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
pub(crate) fn bencode_lookup<'a>(dict: &'a [u8], want: &[u8]) -> Option<&'a [u8]> {
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

#[cfg(test)]
mod tests {
}


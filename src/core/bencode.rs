//! Just enough bencode to read another client's state files.
//!
//! Three of the four clients NanoTorrent migrates from keep their torrent list
//! in bencode - qBittorrent's `.fastresume`, uTorrent's `resume.dat`,
//! Transmission's `.resume` - as does every `.torrent` file. These started as
//! private helpers in `pico_import`, which needed only `dict_get`; the
//! migrations need to walk a dict whose keys are not known in advance, so they
//! moved here rather than being copied per client.
//!
//! Deliberately a reader, not a parser: nothing here builds a tree. Every
//! function returns a borrowed slice of the input, so reading one key out of a
//! 40 MB resume file copies nothing. A malformed file yields `None` rather
//! than an error - these files come from other programs and a torrent that
//! cannot be read is reported as unreadable, never as a failure of the whole
//! migration.

/// Index just past the bencoded value starting at `i`.
pub fn skip(data: &[u8], i: usize) -> Option<usize> {
    skip_at(data, i, 0)
}

/// `depth` is a guard, not bookkeeping: this recurses through nested lists and
/// dicts, and a `.torrent` - a v2 `file tree` especially - is attacker-supplied.
/// 32 is far deeper than any real file nests, and far shallower than the stack.
fn skip_at(data: &[u8], i: usize, depth: u32) -> Option<usize> {
    const MAX_DEPTH: u32 = 32;
    if depth > MAX_DEPTH {
        return None;
    }
    match *data.get(i)? {
        b'i' => Some(i + data.get(i..)?.iter().position(|&b| b == b'e')? + 1),
        b'l' | b'd' => {
            let mut j = i + 1;
            while *data.get(j)? != b'e' {
                j = skip_at(data, j, depth + 1)?;
            }
            Some(j + 1)
        }
        b'0'..=b'9' => string_at(data, i).map(|(_, end)| end),
        _ => None,
    }
}

/// Raw bytes of `key`'s value in the top-level bencoded dict (`data` starts 'd').
///
/// Only the outermost dict, walked value by value. A substring search would be
/// wrong: `pieces` and `meta version` are both legal file names inside a v2
/// `file tree`, so finding those bytes somewhere in a torrent proves nothing.
pub fn dict_get<'a>(data: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    entries(data).find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// Every `(key, value)` of a bencoded dict, as borrowed slices.
///
/// What `dict_get` cannot do: uTorrent's `resume.dat` is one dict whose keys
/// are the `.torrent` filenames, so the keys ARE the data and there is nothing
/// to look up by name.
///
/// Stops at the first malformed entry rather than reporting it. A truncated
/// resume file then yields the torrents before the damage, which is the useful
/// answer - the caller counts what it got against what it expected.
pub fn entries(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut i = 1;
    let valid = data.first() == Some(&b'd');
    std::iter::from_fn(move || {
        if !valid || data.get(i)? == &b'e' {
            return None;
        }
        let kend = skip(data, i)?;
        let k = string(data.get(i..kend)?)?;
        let vend = skip(data, kend)?;
        let v = data.get(kend..vend)?;
        i = vend;
        Some((k, v))
    })
}

/// Decode a bencoded string token (`<len>:<bytes>`) into its raw bytes.
pub fn string(data: &[u8]) -> Option<&[u8]> {
    string_at(data, 0).map(|(s, _)| s)
}

/// The bencoded string at `i`: its contents, and the index just past it.
///
/// Checked both ways, because the length prefix is whatever the file says: an
/// end past the buffer is `None`, and so is one that overflows on the way.
pub fn string_at(data: &[u8], i: usize) -> Option<(&[u8], usize)> {
    let colon = i + data.get(i..)?.iter().position(|&b| b == b':')?;
    let len: usize = std::str::from_utf8(data.get(i..colon)?).ok()?.parse().ok()?;
    let start = colon + 1;
    let end = start.checked_add(len)?;
    data.get(start..end).map(|s| (s, end))
}

/// A bencoded string as UTF-8, lossily. `None` for an empty one as well as a
/// missing one: to the importers an empty path or label is no path or label.
///
/// Lossy on purpose: these are paths written by another program on another
/// machine, and a save path with one undecodable byte is still worth having -
/// the alternative is dropping the torrent entirely over a mojibake filename.
pub fn text(data: &[u8]) -> Option<String> {
    string(data)
        .filter(|b| !b.is_empty())
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

#[cfg(test)]
pub mod build {
    pub fn str(s: &str) -> Vec<u8> {
        format!("{}:{}", s.len(), s).into_bytes()
    }
    pub fn int(v: i64) -> Vec<u8> {
        format!("i{v}e").into_bytes()
    }
    pub fn list(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"l".to_vec();
        out.extend(items.iter().flatten());
        out.push(b'e');
        out
    }
    /// Entries in the order given - bencode wants them sorted, and a reader
    /// that depends on the order is a reader worth catching.
    pub fn dict(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"d".to_vec();
        for (k, v) in pairs {
            out.extend(str(k));
            out.extend(v);
        }
        out.push(b'e');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::build;
    use super::*;

    #[test]
    fn reads_the_three_token_kinds() {
        let d = b"d3:str5:hello4:listli1ei2eee";
        assert_eq!(text(dict_get(d, b"str").unwrap()).as_deref(), Some("hello"));
        // A list comes back whole, for a caller that wants to walk it itself.
        assert_eq!(dict_get(d, b"list"), Some(&b"li1ei2ee"[..]));
        assert_eq!(dict_get(d, b"absent"), None);
    }

    /// The case `dict_get` cannot serve: keys that are themselves the data.
    #[test]
    fn walks_a_dict_whose_keys_are_unknown() {
        let d = build::dict(&[
            ("a.torrent", build::dict(&[("path", build::str("foo"))])),
            ("b.torrent", build::dict(&[("path", build::str("bar"))])),
        ]);
        let d = &d[..];
        let got: Vec<_> = entries(d)
            .map(|(k, v)| {
                (
                    String::from_utf8_lossy(k).into_owned(),
                    text(dict_get(v, b"path").unwrap()).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a.torrent".to_string(), "foo".to_string()),
                ("b.torrent".to_string(), "bar".to_string())
            ]
        );
    }

    /// A dict that simply stops is read as far as it goes, deliberately: a
    /// resume file truncated by a power cut still lists the torrents written
    /// before it, and those are worth migrating.
    #[test]
    fn an_unterminated_dict_yields_what_it_has() {
        assert_eq!(text(dict_get(b"d3:key5:value", b"key").unwrap()).as_deref(), Some("value"));
    }

    /// These files come from other programs. Every one of these used to be a
    /// panic in some reader or other; none may be one here.
    #[test]
    fn rubbish_yields_none_not_a_panic() {
        for bad in [
            &b""[..],
            b"d",
            b"e",
            b"not bencode at all",
            b"d3:key",             // key with no value
            b"d3:key9:short",      // string shorter than its length claims
            b"i",
            b"ie",
            b"d999999999999:xe",   // length that would overflow a slice
            b"llllllll",           // deeply unterminated
        ] {
            assert_eq!(dict_get(bad, b"key"), None, "dict_get({bad:?})");
            let _ = entries(bad).count();
            let _ = skip(bad, 0);
        }
    }

    /// The two attacks a `.torrent` can make on a walker: nesting deep enough
    /// to exhaust the stack, and a length prefix that overflows on its way to
    /// an index. Both are refused, not followed.
    #[test]
    fn hostile_depth_and_lengths_are_refused() {
        let nested = |depth: usize| [b"l".repeat(depth), b"e".repeat(depth)].concat();
        assert!(skip(&nested(30), 0).is_some(), "real files nest this deep");
        assert_eq!(skip(&nested(10_000), 0), None, "a nesting bomb");

        let overflow = format!("{}:x", usize::MAX);
        assert_eq!(string(overflow.as_bytes()), None);
        assert_eq!(skip(overflow.as_bytes(), 0), None);
        assert_eq!(string_at(b"3:abc", 0), Some((&b"abc"[..], 5)));
    }
}

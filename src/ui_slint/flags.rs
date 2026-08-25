//! Country flags for the peers list.
//!
//! The same `res/flags/*.png` the Win32 list uses (32x24, public domain - see
//! res/flags/SOURCE.md), compiled in by build.rs. Decoded on first use of a
//! given country rather than all at once: a swarm touches a handful of the 252
//! flags, so decoding the rest would be work thrown away.

use std::cell::RefCell;
use std::collections::HashMap;

use slint::{Image, SharedPixelBuffer};

// pub static FLAG_PNGS: &[(&str, &[u8])] - ISO 3166-1 alpha-2 -> PNG bytes.
include!(concat!(env!("OUT_DIR"), "/flag_table.rs"));

/// Decoded flags, keyed by lowercase ISO code.
///
/// `None` is cached too: a country with no artwork must not be re-decoded on
/// every refresh tick just to fail again.
#[derive(Default)]
pub struct Flags {
    cache: RefCell<HashMap<String, Option<Image>>>,
}

impl Flags {
    /// The flag for an ISO 3166-1 alpha-2 code, or `None` when there is no
    /// artwork for it - which is also what an empty code gives.
    pub fn get(&self, iso: &str) -> Option<Image> {
        if iso.is_empty() {
            return None;
        }
        let key = iso.to_ascii_lowercase();
        if let Some(cached) = self.cache.borrow().get(&key) {
            return cached.clone();
        }
        let image = decode(&key);
        self.cache.borrow_mut().insert(key, image.clone());
        image
    }
}

/// Decode one flag PNG into a Slint image, or `None` when no artwork ships
/// for that country code.
fn decode(iso: &str) -> Option<Image> {
    let (_, bytes) = FLAG_PNGS.iter().find(|(code, _)| *code == iso)?;

    let mut decoder = png::Decoder::new(std::io::Cursor::new(*bytes));
    // The source art is an indexed palette with a tRNS chunk. EXPAND turns
    // that into plain RGB/RGBA - without it every flag comes back as palette
    // indices and none of them decode.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;

    // The source art is 8-bit RGB or RGBA; anything else would need a
    // conversion this does not have, so it is skipped rather than drawn wrong.
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };

    Some(Image::from_rgba8(SharedPixelBuffer::clone_from_slice(
        &rgba,
        info.width,
        info.height,
    )))
}

#[cfg(test)]
mod tests {
    use super::{FLAG_PNGS, Flags};

    #[test]
    fn a_known_flag_decodes_and_an_unknown_one_does_not() {
        let flags = Flags::default();
        assert!(flags.get("nl").is_some(), "nl.png ships in res/flags");
        // Case-insensitive: GeoIP hands back upper case.
        assert!(flags.get("NL").is_some());
        assert!(flags.get("zz").is_none());
        assert!(flags.get("").is_none());
    }

    /// Every flag must decode, or a peer from that country silently shows
    /// nothing while its neighbours show a picture.
    #[test]
    fn every_shipped_flag_decodes() {
        let flags = Flags::default();
        let bad: Vec<&str> = FLAG_PNGS
            .iter()
            .map(|(code, _)| *code)
            .filter(|code| flags.get(code).is_none())
            .collect();
        assert!(bad.is_empty(), "these flags did not decode: {bad:?}");
    }
}

// Country flag icons for the peers list and the language picker.
//
// The PNGs (32x24, public domain - see res/flags/SOURCE.md) are compiled in by
// build.rs. They are decoded into a single Win32 image list, but only on first
// use of a given country: a peer swarm touches a handful of the 252 flags, so
// decoding them all up front would be work thrown away.

use std::cell::RefCell;
use std::collections::HashMap;

use native_windows_gui as nwg;

// pub static FLAG_PNGS: &[(&str, &[u8])] - ISO 3166-1 alpha-2 -> PNG bytes.
include!(concat!(env!("OUT_DIR"), "/flag_table.rs"));

/// Logical size of a flag in a list view, in 96-dpi units. 4:3 like the source
/// art, and one pixel of headroom under the default 16px row icon.
const FLAG_W: u32 = 16;
const FLAG_H: u32 = 12;

pub struct Flags {
    list: nwg::ImageList,
    decoder: nwg::ImageDecoder,
    /// ISO code -> image list index. Absent = not decoded yet; `None` = no such
    /// flag, so we do not retry the lookup on every list refresh.
    indices: RefCell<HashMap<String, Option<i32>>>,
    size: (i32, i32),
}

impl Flags {
    /// `dpi` is the real window DPI (96 = 100%), so the icons stay sharp at
    /// 150%/200% instead of being stretched from 16px.
    pub fn new(dpi: u32) -> Result<Flags, nwg::NwgError> {
        let w = (FLAG_W * dpi / 96).max(1) as i32;
        let h = (FLAG_H * dpi / 96).max(1) as i32;

        let mut list = nwg::ImageList::default();
        nwg::ImageList::builder()
            .size((w, h))
            .initial(8)
            .grow(16)
            .build(&mut list)?;

        let mut decoder = nwg::ImageDecoder::default();
        nwg::ImageDecoder::builder().build(&mut decoder)?;

        Ok(Flags {
            list,
            decoder,
            indices: RefCell::new(HashMap::new()),
            size: (w, h),
        })
    }

    pub fn image_list(&self) -> &nwg::ImageList {
        &self.list
    }

    /// Image-list index for an ISO 3166-1 alpha-2 code, decoding on first use.
    /// `None` for anything we have no flag for (GeoIP can return e.g. "EU").
    pub fn index_of(&self, iso_code: &str) -> Option<i32> {
        let key = iso_code.to_ascii_lowercase();

        if let Some(cached) = self.indices.borrow().get(&key) {
            return *cached;
        }

        let index = self.decode(&key);
        self.indices.borrow_mut().insert(key, index);
        index
    }

    fn decode(&self, key: &str) -> Option<i32> {
        let (_, png) = FLAG_PNGS.iter().find(|(c, _)| *c == key)?;

        let source = self.decoder.from_stream(png).ok()?;
        let frame = source.frame(0).ok()?;
        // Downsample 32x24 -> the DPI-correct size; WIC does this far better
        // than letting the image list stretch it.
        let scaled = self
            .decoder
            .resize_image(&frame, [self.size.0 as u32, self.size.1 as u32])
            .ok()?;
        let bitmap = scaled.as_bitmap().ok()?;

        match self.list.add_bitmap(&bitmap) {
            i if i >= 0 => Some(i),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_table_is_populated_and_keyed_by_iso_code() {
        assert!(FLAG_PNGS.len() >= 240, "only {} flags", FLAG_PNGS.len());
        for (code, png) in FLAG_PNGS {
            assert_eq!(code.len(), 2, "{code} is not an alpha-2 code");
            assert!(code.bytes().all(|b| b.is_ascii_lowercase()), "{code} not lowercase");
            assert!(png.starts_with(b"\x89PNG"), "{code} is not a PNG");
        }
        // Every locale we ship must resolve to a flag for the dropdown.
        for (locale, _) in crate::ui::translator::EMBEDDED_LANGS {
            let key = crate::ui_native::locale_country(locale)
                .unwrap_or_else(|| panic!("{locale} has no country subtag"));
            assert!(
                FLAG_PNGS.iter().any(|(c, _)| *c == key),
                "no flag for {locale} (looked for {key}.png)"
            );
        }
    }

    #[test]
    fn legacy_locale_tags_map_to_real_countries() {
        assert_eq!(crate::ui_native::locale_country("nl-NL").as_deref(), Some("nl"));
        // sr-SP: "SP" is not an ISO country code; Serbia is RS.
        assert_eq!(crate::ui_native::locale_country("sr-SP").as_deref(), Some("rs"));
        assert_eq!(crate::ui_native::locale_country("en").as_deref(), None);
    }
}

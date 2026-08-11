// Native Win32 UI built with native-windows-gui - the same framework family
// (Win32 common controls) the original wxWidgets client rendered through.

mod darkmode;
mod dialogs;
pub mod flags;
mod mainwindow;

use native_windows_gui as nwg;

use crate::AppContext;

/// The original project shipped a few locale tags that are no longer valid:
/// "SP" was Serbia and Montenegro, retired in 2006 and never an ISO 3166-1
/// country code. Mapped once here because BOTH the display name (Windows
/// answers "Unknown Locale (sr-SP)") and the flag lookup (there is no sp.png)
/// need the modern tag.
const LEGACY_LOCALE_ALIASES: &[(&str, &str)] = &[("sr-SP", "sr-Latn-RS")];

fn canonical_locale(locale: &str) -> &str {
    LEGACY_LOCALE_ALIASES
        .iter()
        .find(|(old, _)| *old == locale)
        .map_or(locale, |(_, new)| *new)
}

/// ISO 3166-1 alpha-2 country code for a locale, lowercased for the flag table
/// ("nl-NL" -> "nl", "sr-SP" -> "rs"). `None` for language-only tags.
// Currently exercised only by the flag tests: the language picker itself cannot
// show icons until the combo box is owner-drawn (or swapped for a ComboBoxEx).
#[allow(dead_code)]
pub fn locale_country(locale: &str) -> Option<String> {
    let canonical = canonical_locale(locale);
    let region = canonical.rsplit('-').next()?;
    // Skip script subtags ("Latn") and language-only tags ("en").
    (region.len() == 2 && region != canonical).then(|| region.to_ascii_lowercase())
}

/// Makes a button wrap its label instead of clipping it at the right edge.
///
/// Verified in examples/dialog_preview.rs: at the original size the Dutch
/// "Minimaliseren naar systeemvak" renders as "inimaliseren naar systeemva",
/// clipped at both ends; with BS_MULTILINE and a taller button it wraps onto
/// two lines and fits. Growing height rather than width is what keeps the
/// horizontal button rows intact.
///
/// The style is applied after creation, which works for buttons (unlike combo
/// boxes, whose list is built from the style at creation time).
pub fn set_button_multiline(button: &nwg::Button) {
    use winapi::um::winuser::{GWL_STYLE, GetWindowLongW, SetWindowLongW};
    const BS_MULTILINE: i32 = 0x0000_2000;

    if let Some(hwnd) = button.handle.hwnd() {
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            SetWindowLongW(hwnd, GWL_STYLE, style | BS_MULTILINE);
        }
    }
}

/// Caps how many items a combo box's drop-down shows before it scrolls.
///
/// Since Vista a combo box shows a MINIMUM of 30 items, which is why resizing
/// the control had no effect: with 41 languages the list sized itself to 30,
/// had no scroll range, and everything above the fold was unreachable. This is
/// the message that actually governs it.
pub fn set_dropdown_visible_items(combo_hwnd: winapi::shared::windef::HWND, items: usize) {
    // winapi 0.3 does not export CB_SETMINVISIBLE (CBM_FIRST + 1).
    const CB_SETMINVISIBLE: u32 = 0x1701;

    if combo_hwnd.is_null() {
        return;
    }
    unsafe {
        winapi::um::winuser::SendMessageW(combo_hwnd, CB_SETMINVISIBLE, items, 0);
    }
}

/// Full, human-readable name for a locale ("nl-NL" -> "Nederlands (Nederland)").
///
/// Windows already carries every locale's display name, so there is no table to
/// ship. The NATIVE name (the language written in that language) is deliberate:
/// someone who cannot read the current UI language still has to find their own
/// entry in the list. Windows synthesizes "Unknown Locale (xx-XX)" for a
/// well-formed tag it does not know, so the bare-code fallback below only fires
/// when the call itself fails.
pub fn locale_display_name(locale: &str) -> String {
    // winapi 0.3 does not export these LCTYPEs.
    const LOCALE_SNATIVEDISPLAYNAME: u32 = 0x0000_0073;

    let wide: Vec<u16> = canonical_locale(locale)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = [0u16; 128];
    let len = unsafe {
        winapi::um::winnls::GetLocaleInfoEx(
            wide.as_ptr(),
            LOCALE_SNATIVEDISPLAYNAME,
            buf.as_mut_ptr(),
            buf.len() as i32,
        )
    };

    if len <= 1 {
        return locale.to_string();
    }
    // The count includes the terminating NUL.
    String::from_utf16_lossy(&buf[..(len - 1) as usize])
}

#[cfg(test)]
mod tests {
    use super::locale_display_name;

    #[test]
    fn locale_names_resolve_and_are_native() {
        assert!(locale_display_name("en-US").contains("English"));
        assert_eq!(locale_display_name("nl-NL"), "Nederlands (Nederland)");
        // Windows synthesizes a label for unknown-but-well-formed tags; the
        // point is that it is never empty and always carries the code.
        assert!(locale_display_name("zz-ZZ").contains("zz-ZZ"));
        // No stray NUL from the Win32 buffer.
        assert!(!locale_display_name("de-DE").contains('\0'));
    }

    #[test]
    fn every_shipped_locale_has_a_real_name() {
        // A genuine native name never embeds the tag ("Nederlands
        // (Nederland)"), whereas Windows' synthesized placeholder always does
        // ("Unknown Locale (sr-SP)") - and that holds whatever language
        // Windows itself is running in, unlike matching the English text.
        let unresolved: Vec<&str> = crate::ui::translator::EMBEDDED_LANGS
            .iter()
            .map(|(l, _)| *l)
            .filter(|l| locale_display_name(l).contains(*l))
            .collect();
        assert!(unresolved.is_empty(), "no real display name for: {unresolved:?}");
    }
}

pub fn run(ctx: AppContext) -> anyhow::Result<()> {
    // Make the process DPI aware, like the original (which declared it in the
    // application manifest). NWG's high-dpi feature scales all logical
    // coordinates by the real DPI, but only does anything useful when the
    // process is actually DPI aware - nwg::init() itself never sets it.
    unsafe {
        winapi::um::winuser::SetProcessDPIAware();
    }

    nwg::init().map_err(|err| anyhow::anyhow!("failed to init NWG: {err}"))?;

    // Global default font. set_global_family alone creates a font with the
    // default (small, non-DPI-scaled) size, which made controls built with
    // the default font visibly smaller than the explicit caption fonts -
    // give it the same logical size 16 (scaled by the real DPI) everywhere.
    let mut default_font = nwg::Font::default();
    nwg::Font::builder()
        .family("Segoe UI")
        .size(16)
        .build(&mut default_font)
        .map_err(|err| anyhow::anyhow!("failed to create default font: {err}"))?;
    nwg::Font::set_global_default(Some(default_font));

    let start_position = ctx.cfg.get_int("start_position").unwrap_or(0);
    let has_tray = ctx.cfg.get_bool("show_in_notification_area");

    let window = mainwindow::MainWindow::build(ctx)?;

    // Port of the start_position setting (Normal / Minimized / Hidden /
    // Maximized).
    match start_position {
        1 => window.window.minimize(),
        // Hidden is only safe when a tray icon exists to bring the window
        // back; otherwise fall through to a normal window (matches the
        // original, which showed the frame when there was no notify icon).
        2 if has_tray => window.window.set_visible(false),
        3 => window.window.maximize(),
        _ => {}
    }

    nwg::dispatch_thread_events();

    Ok(())
}

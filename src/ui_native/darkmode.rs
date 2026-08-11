// Win32 dark mode support - the Rust equivalent of what
// wxApp::MSWEnableDarkMode() did for the original client:
//
//  - dark DWM title bar
//  - dark popup menus (undocumented uxtheme SetPreferredAppMode)
//  - owner-drawn dark menu BAR (the WM_UAHDRAWMENU technique used by
//    Notepad++, wxWidgets and others - classic menu bars ignore themes)
//  - DarkMode_Explorer/DarkMode_ItemsView themes + explicit colors for
//    list views
//  - subclassed tab pages (background + label colors)
//  - owner-drawn status bar parts
//
// All subclass procedures branch on a global flag, so switching the theme
// at runtime only requires flipping the flag and repainting.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
use winapi::shared::windef::{HBRUSH, HDC, HMENU, HWND, RECT};

use crate::core::configuration::Configuration;

// Palette
pub const BG: [u8; 3] = [32, 32, 32];
pub const BG_HOT: [u8; 3] = [62, 62, 62];
pub const FG: [u8; 3] = [240, 240, 240];
pub const FG_DIM: [u8; 3] = [200, 200, 200];
pub const FG_DISABLED: [u8; 3] = [128, 128, 128];
pub const EDIT_BG: [u8; 3] = [56, 56, 56];

// Progress-bar cell colors (rendered in the list's progress column).
const PROGRESS_TRACK: [u8; 3] = [56, 56, 56];
const PROGRESS_FILL: [u8; 3] = [40, 170, 90];
const PROGRESS_BORDER: [u8; 3] = [90, 90, 90];

/// (list HWND as isize, progress column index) for lists whose progress column
/// is drawn as a bar. Registered by the main window after the lists are built.
static PROGRESS_COLS: Mutex<Vec<(isize, i32)>> = Mutex::new(Vec::new());

pub fn register_progress_column(hwnd: HWND, col: i32) {
    if let Ok(mut v) = PROGRESS_COLS.lock() {
        let key = hwnd as isize;
        if !v.iter().any(|&(h, _)| h == key) {
            v.push((key, col));
        }
    }
}

fn progress_column_for(hwnd: HWND) -> Option<i32> {
    let v = PROGRESS_COLS.lock().ok()?;
    v.iter().find(|&&(h, _)| h == hwnd as isize).map(|&(_, c)| c)
}

/// Parse a "12.3 %" progress cell back into a 0.0..=1.0 fraction.
fn parse_percent(buf: &[u16]) -> Option<f32> {
    let s = String::from_utf16_lossy(buf);
    let s = s.trim().trim_end_matches('%').trim();
    s.parse::<f32>().ok().map(|v| (v / 100.0).clamp(0.0, 1.0))
}

static DARK: AtomicBool = AtomicBool::new(false);
static STATUS_TEXTS: Mutex<[String; 4]> = Mutex::new([String::new(), String::new(), String::new(), String::new()]);

pub fn set_enabled(dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
}

pub fn is_enabled() -> bool {
    DARK.load(Ordering::Relaxed)
}

fn rgb(c: [u8; 3]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16)
}

fn brush(cell: &AtomicUsize, color: [u8; 3]) -> HBRUSH {
    let existing = cell.load(Ordering::Relaxed);
    if existing != 0 {
        return existing as HBRUSH;
    }
    let created = unsafe { winapi::um::wingdi::CreateSolidBrush(rgb(color)) };
    cell.store(created as usize, Ordering::Relaxed);
    created
}

static BG_BRUSH: AtomicUsize = AtomicUsize::new(0);
static BG_HOT_BRUSH: AtomicUsize = AtomicUsize::new(0);
static WHITE_BRUSH: AtomicUsize = AtomicUsize::new(0);
static EDIT_BG_BRUSH: AtomicUsize = AtomicUsize::new(0);
static FG_BRUSH: AtomicUsize = AtomicUsize::new(0);
static ACCENT_BRUSH: AtomicUsize = AtomicUsize::new(0);
static LIGHT_TRACK_BRUSH: AtomicUsize = AtomicUsize::new(0);

// Control handle the WM_DRAWITEM handler needs to recognize.
static STATUS_HWND: AtomicUsize = AtomicUsize::new(0);

fn bg_brush() -> HBRUSH {
    brush(&BG_BRUSH, BG)
}
fn bg_hot_brush() -> HBRUSH {
    brush(&BG_HOT_BRUSH, BG_HOT)
}
fn white_brush() -> HBRUSH {
    brush(&WHITE_BRUSH, [255, 255, 255])
}
fn edit_bg_brush() -> HBRUSH {
    brush(&EDIT_BG_BRUSH, EDIT_BG)
}
fn fg_brush() -> HBRUSH {
    brush(&FG_BRUSH, FG)
}
fn accent_brush() -> HBRUSH {
    // Windows accent blue.
    brush(&ACCENT_BRUSH, [0, 120, 215])
}
fn light_track_brush() -> HBRUSH {
    brush(&LIGHT_TRACK_BRUSH, [229, 229, 229])
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// Theme detection - port of Configuration::IsDarkMode / IsSystemDarkMode

pub fn is_system_dark_mode() -> bool {
    use winapi::um::winnt::KEY_QUERY_VALUE;
    use winapi::um::winreg::{HKEY_CURRENT_USER, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};

    let subkey = to_wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value_name = to_wide("AppsUseLightTheme");

    let mut value: u32 = 1;
    unsafe {
        let mut key = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_QUERY_VALUE, &mut key) == 0 {
            let mut size = std::mem::size_of::<u32>() as u32;
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut value as *mut u32 as *mut u8,
                &mut size,
            );
            RegCloseKey(key);
        }
    }

    value == 0
}

pub fn is_dark_mode(cfg: &Configuration) -> bool {
    match cfg
        .get_string("theme_id")
        .unwrap_or_else(|| String::from("system"))
        .as_str()
    {
        "light" => false,
        "dark" => true,
        _ => is_system_dark_mode(),
    }
}

// Application-wide pieces

/// Dark (or light) popup menus, process wide. Undocumented uxtheme export
/// ordinal 135: SetPreferredAppMode (ForceDark = 2, ForceLight = 3).
pub fn apply_app_mode(dark: bool) {
    use winapi::um::libloaderapi::{GetModuleHandleA, GetProcAddress};

    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();

    unsafe {
        let uxtheme = GetModuleHandleA(c"uxtheme.dll".as_ptr());
        if uxtheme.is_null() {
            return;
        }

        let set_mode = GetProcAddress(uxtheme, 135 as *const i8);
        if !set_mode.is_null() {
            let set_mode: SetPreferredAppMode = std::mem::transmute(set_mode);
            set_mode(if dark { 2 } else { 3 });
        }

        let flush = GetProcAddress(uxtheme, 136 as *const i8);
        if !flush.is_null() {
            let flush: FlushMenuThemes = std::mem::transmute(flush);
            flush();
        }
    }
}

/// Dark title bar via DWMWA_USE_IMMERSIVE_DARK_MODE.
pub fn apply_to_window(hwnd: HWND, dark: bool) {
    use winapi::um::dwmapi::DwmSetWindowAttribute;

    let enabled: i32 = if dark { 1 } else { 0 };
    unsafe {
        // 20 on 20H1+, 19 on earlier Windows 10 builds.
        for attr in [20u32, 19u32] {
            if DwmSetWindowAttribute(
                hwnd,
                attr,
                &enabled as *const i32 as *const _,
                std::mem::size_of::<i32>() as u32,
            ) == 0
            {
                break;
            }
        }
    }
}

/// Item/text/background colors for a list view (NWG's set_background_color
/// misses LVM_SETTEXTBKCOLOR which produces white-on-white rows). Split out
/// from the theme application because the stored color state gets reset by
/// system broadcasts (WM_THEMECHANGED / WM_SYSCOLORCHANGE / WM_SETTINGCHANGE)
/// and must be re-applied.
pub fn apply_listview_colors(hwnd: HWND, dark: bool) {
    use winapi::um::winuser::SendMessageW;

    const LVM_FIRST: u32 = 0x1000;
    const LVM_GETBKCOLOR: u32 = LVM_FIRST + 0;
    const LVM_SETBKCOLOR: u32 = LVM_FIRST + 1;
    const LVM_GETTEXTCOLOR: u32 = LVM_FIRST + 35;
    const LVM_SETTEXTCOLOR: u32 = LVM_FIRST + 36;
    const LVM_GETTEXTBKCOLOR: u32 = LVM_FIRST + 37;
    const LVM_SETTEXTBKCOLOR: u32 = LVM_FIRST + 38;

    let (bg, fg) = if dark {
        (rgb(BG) as isize, rgb(FG) as isize)
    } else {
        (rgb([255, 255, 255]) as isize, rgb([0, 0, 0]) as isize)
    };

    // Only write when the stored state differs - this runs every refresh
    // tick and unconditional sets would repaint (flicker) the whole list.
    unsafe {
        if SendMessageW(hwnd, LVM_GETBKCOLOR, 0, 0) != bg {
            SendMessageW(hwnd, LVM_SETBKCOLOR, 0, bg);
        }
        if SendMessageW(hwnd, LVM_GETTEXTBKCOLOR, 0, 0) != bg {
            SendMessageW(hwnd, LVM_SETTEXTBKCOLOR, 0, bg);
        }
        if SendMessageW(hwnd, LVM_GETTEXTCOLOR, 0, 0) != fg {
            SendMessageW(hwnd, LVM_SETTEXTCOLOR, 0, fg);
        }
    }
}

/// Full treatment for a list view.
///
/// In dark mode the control keeps the DarkMode_Explorer theme - purely for
/// its modern dark scrollbars. Every other themed part is unreliable across
/// Windows 11 builds (item/header/background can render white), so those
/// are all painted by us: items via NMLVCUSTOMDRAW colors, the header fully
/// owner-drawn (and explicitly UNthemed - themed headers ignore the custom
/// draw on some builds), and the empty area overdrawn in WM_PAINT.
pub fn apply_to_listview(hwnd: HWND, dark: bool) {
    use winapi::um::uxtheme::SetWindowTheme;
    use winapi::um::winuser::{InvalidateRect, SendMessageW};

    const LVM_FIRST: u32 = 0x1000;
    const LVM_GETHEADER: u32 = LVM_FIRST + 31;

    unsafe {
        let header = SendMessageW(hwnd, LVM_GETHEADER, 0, 0) as HWND;

        if dark {
            // Modern dark scrollbars.
            let explorer = to_wide("DarkMode_Explorer");
            SetWindowTheme(hwnd, explorer.as_ptr(), std::ptr::null());
            // The header stays unthemed so our owner drawing wins.
            let empty = to_wide("");
            if !header.is_null() {
                SetWindowTheme(header, empty.as_ptr(), empty.as_ptr());
            }
        } else {
            // Restore default theming.
            SetWindowTheme(hwnd, std::ptr::null(), std::ptr::null());
            if !header.is_null() {
                SetWindowTheme(header, std::ptr::null(), std::ptr::null());
            }
        }

        apply_listview_colors(hwnd, dark);

        InvalidateRect(hwnd, std::ptr::null(), 1);
    }
}

/// Dark scrollbars/border for an edit control; interior colors come from
/// WM_CTLCOLOREDIT at its parent. Also subclasses the edit to overpaint the
/// slack around its text formatting rectangle, which the control leaves
/// light in dark mode.
pub fn apply_to_edit(hwnd: HWND, dark: bool) {
    use winapi::um::uxtheme::SetWindowTheme;
    use winapi::um::winuser::InvalidateRect;

    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(edit_fill_subclass), 0x505A, 0);

        if dark {
            let theme = to_wide("DarkMode_Explorer");
            SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null());
        } else {
            SetWindowTheme(hwnd, std::ptr::null(), std::ptr::null());
        }
        center_single_line_edit(hwnd);
        InvalidateRect(hwnd, std::ptr::null(), 1);
    }
}

/// Vertically center the text in a single-line edit by moving its formatting
/// rectangle. A single-line edit otherwise draws its text at the top of the
/// client area, leaving the slack below - so in a control taller than the font
/// line the text looks stuck to the top. Multiline boxes are left untouched.
pub fn center_single_line_edit(hwnd: HWND) {
    use winapi::shared::windef::HGDIOBJ;
    use winapi::um::wingdi::{GetTextMetricsW, SelectObject, TEXTMETRICW};
    use winapi::um::winuser::{
        EM_GETRECT, EM_SETRECTNP, GWL_STYLE, GetClientRect, GetDC, GetWindowLongW, ReleaseDC,
        SendMessageW, WM_GETFONT,
    };
    const ES_MULTILINE: u32 = 0x0004;

    unsafe {
        if GetWindowLongW(hwnd, GWL_STYLE) as u32 & ES_MULTILINE != 0 {
            return;
        }

        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let ch = client.bottom - client.top;

        // Font line height via the control's current font.
        let font = SendMessageW(hwnd, WM_GETFONT as UINT, 0, 0);
        let dc = GetDC(hwnd);
        if dc.is_null() {
            return;
        }
        let old = SelectObject(dc, font as HGDIOBJ);
        let mut tm: TEXTMETRICW = std::mem::zeroed();
        GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        ReleaseDC(hwnd, dc);

        let line = tm.tmHeight;
        if line <= 0 || ch <= line {
            return; // nothing to center (control not taller than the text)
        }

        // Preserve the current left/right margins from the format rect.
        let mut fmt: RECT = std::mem::zeroed();
        SendMessageW(hwnd, EM_GETRECT as UINT, 0, &mut fmt as *mut RECT as LPARAM);
        let top = (ch - line) / 2;
        let centered = RECT {
            left: fmt.left,
            top,
            right: fmt.right,
            bottom: top + line,
        };
        SendMessageW(
            hwnd,
            EM_SETRECTNP as UINT,
            0,
            &centered as *const RECT as LPARAM,
        );
    }
}

/// Single-line edits only paint their text FORMATTING rect with the
/// WM_CTLCOLOREDIT brush; when the control is taller than a text line, the
/// slack above/below stays light. Overpaint it after the default paint.
unsafe extern "system" fn edit_fill_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::winuser::{
        EM_GETRECT, FillRect, GetClientRect, GetDC, ReleaseDC, SendMessageW, WM_PAINT,
    };

    unsafe {
        if msg == WM_PAINT && is_enabled() {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);

            let hdc = GetDC(hwnd);
            if !hdc.is_null() {
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let mut fmt: RECT = std::mem::zeroed();
                SendMessageW(hwnd, EM_GETRECT as UINT, 0, &mut fmt as *mut RECT as LPARAM);

                let brush = edit_bg_brush();
                if fmt.top > rc.top {
                    FillRect(hdc, &RECT { left: rc.left, top: rc.top, right: rc.right, bottom: fmt.top }, brush);
                }
                if fmt.bottom < rc.bottom {
                    FillRect(hdc, &RECT { left: rc.left, top: fmt.bottom, right: rc.right, bottom: rc.bottom }, brush);
                }
                if fmt.left > rc.left {
                    FillRect(hdc, &RECT { left: rc.left, top: fmt.top, right: fmt.left, bottom: fmt.bottom }, brush);
                }
                if fmt.right < rc.right {
                    FillRect(hdc, &RECT { left: fmt.right, top: fmt.top, right: rc.right, bottom: fmt.bottom }, brush);
                }
                ReleaseDC(hwnd, hdc);
            }

            return result;
        }

        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

/// Dark treatment for a tab control: strip the themed (always light) drawing
/// and let the subclass paint it entirely (flat, modern tabs) - restored to
/// standard themed drawing in light mode.
pub fn apply_to_tab_control(hwnd: HWND, dark: bool) {
    use winapi::um::uxtheme::SetWindowTheme;
    use winapi::um::winuser::InvalidateRect;

    unsafe {
        if dark {
            let empty = to_wide("");
            SetWindowTheme(hwnd, empty.as_ptr(), empty.as_ptr());
        } else {
            SetWindowTheme(hwnd, std::ptr::null(), std::ptr::null());
        }
        InvalidateRect(hwnd, std::ptr::null(), 1);
    }
}

// Status bar - dark background + owner drawn parts

const SB_SETTEXTW: u32 = 0x400 + 11;
const SB_SETBKCOLOR: u32 = 0x2001; // CCM_SETBKCOLOR
const SBT_OWNERDRAW: usize = 0x1000;
const CLR_DEFAULT: isize = 0xFF000000u32 as i32 as isize;

/// Set a status bar part's text, owner-drawn in dark mode so the text can
/// be painted light-on-dark (status bars have no text color API).
pub fn set_status_text(hwnd: HWND, index: usize, text: &str) {
    use winapi::um::winuser::SendMessageW;

    if index >= 4 {
        return;
    }

    {
        let mut texts = STATUS_TEXTS.lock().unwrap();
        if texts[index] == text {
            return;
        }
        texts[index] = text.to_string();
    }

    unsafe {
        if is_enabled() {
            SendMessageW(hwnd, SB_SETTEXTW, index | SBT_OWNERDRAW, index as LPARAM);
        } else {
            let wide = to_wide(text);
            SendMessageW(hwnd, SB_SETTEXTW, index, wide.as_ptr() as LPARAM);
        }
    }
}

/// Re-send all stored status texts (used when the theme changes to flip
/// between plain and owner-drawn parts).
pub fn refresh_status(hwnd: HWND) {
    use winapi::um::winuser::{InvalidateRect, SendMessageW};

    let texts = STATUS_TEXTS.lock().unwrap().clone();

    unsafe {
        SendMessageW(
            hwnd,
            SB_SETBKCOLOR,
            0,
            if is_enabled() { rgb(BG) as isize } else { CLR_DEFAULT },
        );

        for (index, text) in texts.iter().enumerate() {
            if is_enabled() {
                SendMessageW(hwnd, SB_SETTEXTW, index | SBT_OWNERDRAW, index as LPARAM);
            } else {
                let wide = to_wide(text);
                SendMessageW(hwnd, SB_SETTEXTW, index, wide.as_ptr() as LPARAM);
            }
        }

        InvalidateRect(hwnd, std::ptr::null(), 1);
    }
}

// Main window subclass: dark menu bar (WM_UAHDRAWMENU) + status bar
// owner draw (WM_DRAWITEM)

const WM_UAHDRAWMENU: UINT = 0x0091;
const WM_UAHDRAWMENUITEM: UINT = 0x0092;

#[repr(C)]
struct UahMenu {
    hmenu: HMENU,
    hdc: HDC,
    dw_flags: u32,
}

#[repr(C)]
struct UahMenuItemMetrics {
    // Union of 4 {cx, cy} pairs in the real struct.
    data: [u32; 8],
}

#[repr(C)]
struct UahMenuPopupMetrics {
    rgcx: [u32; 4],
    f_update_max_widths: u32,
}

#[repr(C)]
struct UahMenuItem {
    position: i32,
    umim: UahMenuItemMetrics,
    umpm: UahMenuPopupMetrics,
}

#[repr(C)]
struct UahDrawMenuItem {
    dis: winapi::um::winuser::DRAWITEMSTRUCT,
    um: UahMenu,
    umi: UahMenuItem,
}

fn menubar_rect(hwnd: HWND) -> Option<RECT> {
    use winapi::um::winuser::{GetMenuBarInfo, GetWindowRect, MENUBARINFO};

    const OBJID_MENU: i32 = -3;

    unsafe {
        let mut mbi: MENUBARINFO = std::mem::zeroed();
        mbi.cbSize = std::mem::size_of::<MENUBARINFO>() as u32;
        if GetMenuBarInfo(hwnd, OBJID_MENU, 0, &mut mbi) == 0 {
            return None;
        }

        let mut rc_window: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut rc_window);

        let mut rc = mbi.rcBar;
        rc.left -= rc_window.left;
        rc.right -= rc_window.left;
        rc.top -= rc_window.top;
        rc.bottom -= rc_window.top;

        Some(rc)
    }
}

unsafe extern "system" fn main_window_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::wingdi::{SetBkMode, SetTextColor, TRANSPARENT};
    use winapi::um::winuser::{
        DRAWITEMSTRUCT, DT_CENTER, DT_HIDEPREFIX, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DrawTextW,
        FillRect, GetMenuItemInfoW, GetWindowDC, MENUITEMINFOW, MIIM_STRING, ODS_DISABLED,
        ODS_GRAYED, ODS_HOTLIGHT, ODS_NOACCEL, ODS_SELECTED, ReleaseDC, WM_DRAWITEM,
        WM_NCACTIVATE, WM_NCPAINT,
    };

    unsafe {
        // Dark item colors for the torrent list (its custom-draw
        // notifications arrive here, at its parent).
        if let Some(result) = handle_listview_item_customdraw(msg, lparam) {
            return result;
        }

        // Dark colors for the console input (WM_CTLCOLOREDIT arrives here).
        if let Some(result) = handle_dark_ctl_color(msg, wparam) {
            return result;
        }

        match msg {
            WM_UAHDRAWMENU if is_enabled() => {
                // Paint the menu bar background dark.
                let pudm = lparam as *const UahMenu;
                if let Some(rc) = menubar_rect(hwnd) {
                    FillRect((*pudm).hdc, &rc, bg_brush());
                }
                0
            }
            WM_UAHDRAWMENUITEM if is_enabled() => {
                let pudmi = lparam as *const UahDrawMenuItem;
                let dis = &(*pudmi).dis;

                // Item caption
                let mut buf = [0u16; 256];
                let mut mii: MENUITEMINFOW = std::mem::zeroed();
                mii.cbSize = std::mem::size_of::<MENUITEMINFOW>() as u32;
                mii.fMask = MIIM_STRING;
                mii.dwTypeData = buf.as_mut_ptr();
                mii.cch = (buf.len() - 1) as u32;
                GetMenuItemInfoW(
                    (*pudmi).um.hmenu,
                    (*pudmi).umi.position as u32,
                    1,
                    &mut mii,
                );

                let state = dis.itemState;
                let hot = state & (ODS_HOTLIGHT | ODS_SELECTED) != 0;
                let disabled = state & (ODS_GRAYED | ODS_DISABLED) != 0;

                FillRect(
                    dis.hDC,
                    &dis.rcItem,
                    if hot { bg_hot_brush() } else { bg_brush() },
                );

                SetBkMode(dis.hDC, TRANSPARENT as i32);
                SetTextColor(dis.hDC, rgb(if disabled { FG_DISABLED } else { FG }));

                let mut flags = DT_CENTER | DT_SINGLELINE | DT_VCENTER;
                if state & ODS_NOACCEL != 0 {
                    flags |= DT_HIDEPREFIX;
                }

                let mut rc = dis.rcItem;
                DrawTextW(dis.hDC, buf.as_ptr(), mii.cch as i32, &mut rc, flags);
                0
            }
            WM_NCPAINT | WM_NCACTIVATE if is_enabled() => {
                // Let the default paint run, then repaint the 1px line the
                // theme draws under the menu bar.
                let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                if let Some(rc) = menubar_rect(hwnd) {
                    let line = RECT {
                        left: rc.left,
                        top: rc.bottom,
                        right: rc.right,
                        bottom: rc.bottom + 1,
                    };
                    let hdc = GetWindowDC(hwnd);
                    if !hdc.is_null() {
                        FillRect(hdc, &line, bg_brush());
                        ReleaseDC(hwnd, hdc);
                    }
                }
                result
            }
            WM_DRAWITEM if is_enabled() => {
                let dis = lparam as *const DRAWITEMSTRUCT;

                if (*dis).hwndItem as usize == STATUS_HWND.load(Ordering::Relaxed) {
                    // Owner-drawn status bar parts.
                    let index = (*dis).itemID as usize;

                    let text = STATUS_TEXTS
                        .lock()
                        .ok()
                        .and_then(|texts| texts.get(index).cloned())
                        .unwrap_or_default();

                    FillRect((*dis).hDC, &(*dis).rcItem, bg_brush());
                    SetBkMode((*dis).hDC, TRANSPARENT as i32);
                    SetTextColor((*dis).hDC, rgb(FG));

                    // Use the status bar's own (DPI scaled) font - the DC
                    // comes with the small default font selected.
                    let font = winapi::um::winuser::SendMessageW(
                        (*dis).hwndItem,
                        winapi::um::winuser::WM_GETFONT,
                        0,
                        0,
                    );
                    let old_font = if font != 0 {
                        winapi::um::wingdi::SelectObject((*dis).hDC, font as _)
                    } else {
                        std::ptr::null_mut()
                    };

                    let wide = to_wide(&text);
                    let mut rc = (*dis).rcItem;
                    rc.left += 4;
                    DrawTextW(
                        (*dis).hDC,
                        wide.as_ptr(),
                        -1,
                        &mut rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                    );

                    if !old_font.is_null() {
                        winapi::um::wingdi::SelectObject((*dis).hDC, old_font);
                    }
                    return 1;
                }

                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            winapi::um::winuser::WM_ERASEBKGND if is_enabled() => {
                // The window background peeks through between the panels.
                let mut rc: RECT = std::mem::zeroed();
                winapi::um::winuser::GetClientRect(hwnd, &mut rc);
                FillRect(wparam as HDC, &rc, bg_brush());
                1
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

// Tab control subclass: flat modern dark tabs (full custom paint)

unsafe extern "system" fn tab_control_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::winuser::{FillRect, GetClientRect, WM_ERASEBKGND, WM_PAINT};

    unsafe {
        if msg == WM_ERASEBKGND && is_enabled() {
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            FillRect(wparam as HDC, &rc, bg_brush());
            return 1;
        }

        if msg == WM_PAINT && is_enabled() {
            // Full custom paint: flat modern tabs (the classic control draws
            // 3D borders around the buttons and the display area).
            paint_tab_control(hwnd);
            return 0;
        }

        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

/// Paint a whole tab control flat and dark: background, one shaded pill per
/// tab button plus an accent underline for the selected one.
unsafe fn paint_tab_control(hwnd: HWND) {
    use winapi::um::commctrl::{TCIF_TEXT, TCITEMW};
    use winapi::um::wingdi::{SelectObject, SetBkMode, SetTextColor, TRANSPARENT};
    use winapi::um::winuser::{
        BeginPaint, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DrawTextW, EndPaint, FillRect,
        GetClientRect, PAINTSTRUCT, SendMessageW, WM_GETFONT,
    };

    const TCM_FIRST: u32 = 0x1300;
    const TCM_GETITEMCOUNT: u32 = TCM_FIRST + 4;
    const TCM_GETITEMRECT: u32 = TCM_FIRST + 10;
    const TCM_GETCURSEL: u32 = TCM_FIRST + 11;
    const TCM_GETITEMW: u32 = TCM_FIRST + 60;

    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_null() {
            return;
        }

        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        FillRect(hdc, &rc, bg_brush());

        let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
        let old_font = if font != 0 {
            SelectObject(hdc, font as _)
        } else {
            std::ptr::null_mut()
        };
        SetBkMode(hdc, TRANSPARENT as i32);

        let count = SendMessageW(hwnd, TCM_GETITEMCOUNT, 0, 0);
        let selected = SendMessageW(hwnd, TCM_GETCURSEL, 0, 0);

        for i in 0..count {
            let mut item_rc: RECT = std::mem::zeroed();
            if SendMessageW(
                hwnd,
                TCM_GETITEMRECT,
                i as usize,
                &mut item_rc as *mut RECT as LPARAM,
            ) == 0
            {
                continue;
            }

            let is_selected = i == selected;
            if is_selected {
                FillRect(hdc, &item_rc, bg_hot_brush());
                // Accent underline, scaled with the tab height.
                let underline = RECT {
                    left: item_rc.left,
                    top: item_rc.bottom - ((item_rc.bottom - item_rc.top) / 10).max(2),
                    right: item_rc.right,
                    bottom: item_rc.bottom,
                };
                FillRect(hdc, &underline, fg_brush());
            }

            let mut buf = [0u16; 128];
            let mut item: TCITEMW = std::mem::zeroed();
            item.mask = TCIF_TEXT;
            item.pszText = buf.as_mut_ptr();
            item.cchTextMax = (buf.len() - 1) as i32;
            SendMessageW(
                hwnd,
                TCM_GETITEMW,
                i as usize,
                &mut item as *mut TCITEMW as LPARAM,
            );

            SetTextColor(hdc, rgb(if is_selected { FG } else { FG_DIM }));
            let mut text_rc = item_rc;
            DrawTextW(
                hdc,
                buf.as_ptr(),
                -1,
                &mut text_rc,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
        }

        if !old_font.is_null() {
            SelectObject(hdc, old_font);
        }
        EndPaint(hwnd, &ps);
    }
}

// Tab page subclass: page background + label colors, both themes

unsafe extern "system" fn tab_page_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::wingdi::{SetBkColor, SetTextColor};
    use winapi::um::winuser::{FillRect, GetClientRect, WM_CTLCOLORSTATIC, WM_ERASEBKGND};

    unsafe {
        // Dark item colors for the detail lists (files/peers/trackers) whose
        // parent is the tab page.
        if let Some(result) = handle_listview_item_customdraw(msg, lparam) {
            return result;
        }

        // Dark colors for labels, checkboxes, edits and combo dropdowns.
        if let Some(result) = handle_dark_ctl_color(msg, wparam) {
            return result;
        }

        match msg {
            WM_ERASEBKGND => {
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let brush = if is_enabled() { bg_brush() } else { white_brush() };
                FillRect(wparam as HDC, &rc, brush);
                1
            }
            WM_CTLCOLORSTATIC => {
                // Light mode (dark is handled by handle_dark_ctl_color).
                let hdc = wparam as HDC;
                SetTextColor(hdc, rgb([0, 0, 0]));
                SetBkColor(hdc, rgb([255, 255, 255]));
                white_brush() as LRESULT
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

/// Dark-mode handling for the standard control-color messages, shared by the
/// tab-page and dialog subclasses: labels and checkbox text (light on dark),
/// edit fields and combo dropdown lists (slightly lighter interior). Returns
/// None in light mode or for other messages.
unsafe fn handle_dark_ctl_color(msg: UINT, wparam: WPARAM) -> Option<LRESULT> {
    use winapi::um::wingdi::{SetBkColor, SetTextColor};
    use winapi::um::winuser::{
        WM_CTLCOLORBTN, WM_CTLCOLOREDIT, WM_CTLCOLORLISTBOX, WM_CTLCOLORSTATIC,
    };

    if !is_enabled() {
        return None;
    }

    unsafe {
        let hdc = wparam as HDC;
        match msg {
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                SetTextColor(hdc, rgb(FG));
                SetBkColor(hdc, rgb(BG));
                Some(bg_brush() as LRESULT)
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                SetTextColor(hdc, rgb(FG));
                SetBkColor(hdc, rgb(EDIT_BG));
                Some(edit_bg_brush() as LRESULT)
            }
            _ => None,
        }
    }
}

// Dialog window subclass + setup - the add-magnet / add-torrent / preferences
// dialogs run on their own threads; this gives their content the same
// treatment as the main window (dark background, control colors, owner-drawn
// tab buttons, dark list rows).

unsafe extern "system" fn dialog_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::winuser::{FillRect, GetClientRect, WM_ERASEBKGND};

    unsafe {
        // Dark rows for list views living directly on the dialog (the
        // add-torrent file list).
        if let Some(result) = handle_listview_item_customdraw(msg, lparam) {
            return result;
        }

        if let Some(result) = handle_dark_ctl_color(msg, wparam) {
            return result;
        }

        match msg {
            WM_ERASEBKGND => {
                // Dark fill in dark mode; white in light mode so the dialog
                // matches its (white) labels and tab pages - the window
                // class brush is the grey COLOR_BTNFACE.
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let brush = if is_enabled() { bg_brush() } else { white_brush() };
                FillRect(wparam as HDC, &rc, brush);
                1
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

// Checkbox subclass: modern themed dark checkboxes. The themed painter
// ignores WM_CTLCOLORSTATIC text colors, and stripping the theme falls back
// to legacy-looking classic glyphs - so in dark mode the whole control is
// painted here: the glyph via the DarkMode_Explorer Button theme, the text
// with the palette colors.

unsafe extern "system" fn checkbox_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::winuser::{WM_ERASEBKGND, WM_PAINT};

    unsafe {
        match msg {
            WM_PAINT if is_enabled() => {
                paint_checkbox(hwnd);
                0
            }
            WM_ERASEBKGND if is_enabled() => 1,
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint_checkbox(hwnd: HWND) {
    use winapi::shared::windef::SIZE;
    use winapi::um::uxtheme::{
        CloseThemeData, DrawThemeBackground, GetThemePartSize, OpenThemeData, TS_TRUE,
    };
    use winapi::um::wingdi::{SelectObject, SetBkMode, SetTextColor, TRANSPARENT};
    use winapi::um::winuser::{
        BeginPaint, DFC_BUTTON, DFCS_BUTTONCHECK, DFCS_CHECKED, DT_LEFT, DT_SINGLELINE,
        DT_VCENTER, DrawFrameControl, DrawTextW, EndPaint, FillRect, GetClientRect,
        GetWindowTextLengthW, GetWindowTextW, PAINTSTRUCT, SendMessageW, WM_GETFONT,
    };

    const BM_GETCHECK: u32 = 0x00F0;
    const BST_CHECKED: isize = 1;
    const BP_CHECKBOX: i32 = 3;
    const CBS_UNCHECKEDNORMAL: i32 = 1;
    const CBS_CHECKEDNORMAL: i32 = 5;

    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_null() {
            return;
        }

        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        FillRect(hdc, &rc, bg_brush());

        let checked = SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED;
        let state = if checked {
            CBS_CHECKEDNORMAL
        } else {
            CBS_UNCHECKEDNORMAL
        };

        // Glyph: DarkMode_Explorer is set on the control, so opening the
        // Button theme through its handle yields the dark checkbox parts.
        let class = to_wide("Button");
        let theme = OpenThemeData(hwnd, class.as_ptr());

        let mut glyph = SIZE { cx: 16, cy: 16 };
        if !theme.is_null() {
            GetThemePartSize(
                theme,
                hdc,
                BP_CHECKBOX,
                state,
                std::ptr::null(),
                TS_TRUE,
                &mut glyph,
            );
        }

        let glyph_top = rc.top + ((rc.bottom - rc.top) - glyph.cy) / 2;
        let mut glyph_rc = RECT {
            left: rc.left,
            top: glyph_top,
            right: rc.left + glyph.cx,
            bottom: glyph_top + glyph.cy,
        };

        if !theme.is_null() {
            DrawThemeBackground(theme, hdc, BP_CHECKBOX, state, &glyph_rc, std::ptr::null());
            CloseThemeData(theme);
        } else {
            // No theme available - classic glyph as a fallback.
            let flags = DFCS_BUTTONCHECK | if checked { DFCS_CHECKED } else { 0 };
            DrawFrameControl(hdc, &mut glyph_rc, DFC_BUTTON, flags);
        }

        // Caption text, in the control's own (DPI scaled) font.
        let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
        let old_font = if font != 0 {
            SelectObject(hdc, font as _)
        } else {
            std::ptr::null_mut()
        };

        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, rgb(FG));

        let len = GetWindowTextLengthW(hwnd);
        if len > 0 {
            let mut buf = vec![0u16; len as usize + 1];
            let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let mut text_rc = RECT {
                left: glyph_rc.right + glyph.cx / 3,
                top: rc.top,
                right: rc.right,
                bottom: rc.bottom,
            };
            DrawTextW(
                hdc,
                buf.as_ptr(),
                len,
                &mut text_rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }

        if !old_font.is_null() {
            SelectObject(hdc, old_font);
        }
        EndPaint(hwnd, &ps);
    }
}

/// Prepare a dialog window for the current theme: subclass it, dark title
/// bar, and per-class treatment for every child control. Called once per
/// dialog right after its controls are built (dialogs are recreated on every
/// open, and the theme cannot change while one is up - the main window is
/// disabled).
pub fn prepare_dialog(hwnd: HWND) {
    use winapi::um::winuser::EnumChildWindows;

    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(dialog_subclass), 0x5058, 0);
        if is_enabled() {
            apply_to_window(hwnd, true);
        }
        EnumChildWindows(hwnd, Some(prepare_dialog_child), is_enabled() as LPARAM);
    }
}

unsafe extern "system" fn prepare_dialog_child(hwnd: HWND, lparam: LPARAM) -> i32 {
    use winapi::um::uxtheme::SetWindowTheme;
    use winapi::um::winuser::{GWL_STYLE, GetClassNameW, GetWindowLongW};

    let dark = lparam != 0;

    unsafe {
        let mut buf = [0u16; 64];
        let len = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;

        match String::from_utf16_lossy(&buf[..len]).as_str() {
            "Button" if dark => {
                // Everything gets the dark theme; checkboxes are additionally
                // painted by their own subclass (dark glyph + light text -
                // the themed painter alone draws unreadable dark text).
                let theme = to_wide("DarkMode_Explorer");
                SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null());

                let checkable =
                    matches!(GetWindowLongW(hwnd, GWL_STYLE) as u32 & 0xF, 2..=7 | 9);
                if checkable {
                    winapi::um::commctrl::SetWindowSubclass(
                        hwnd,
                        Some(checkbox_subclass),
                        0x5059,
                        0,
                    );
                }
            }
            "Edit" => {
                // Dark scrollbars for multiline boxes; interior colors come
                // from WM_CTLCOLOREDIT at the parent.
                if dark {
                    let theme = to_wide("DarkMode_Explorer");
                    SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null());
                }
                // Vertically center the text (single-line edits top-align it).
                center_single_line_edit(hwnd);
            }
            // The interior colors come from WM_CTLCOLORLISTBOX at the parent;
            // this is only for the scrollbar, which would otherwise render
            // light against a dark list (the language picker always scrolls).
            "ListBox" if dark => {
                let theme = to_wide("DarkMode_Explorer");
                SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null());
            }
            "ComboBox" if dark => {
                let theme = to_wide("DarkMode_CFD");
                SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null());
            }
            "SysListView32" => {
                install_listview_subclass(hwnd);
                apply_to_listview(hwnd, dark);
            }
            "SysTabControl32" => {
                install_tab_control_subclass(hwnd);
                apply_to_tab_control(hwnd, dark);
            }
            "NWG_TAB" => {
                install_tab_page_subclass(hwnd);
            }
            _ => {}
        }
    }

    1
}

// List view subclass: custom-draw the header text light in dark mode (the
// DarkMode_ItemsView theme darkens the header background but leaves the
// text color to the app - same fix Notepad++ applies)

/// Handles notifications the LIST VIEW receives from its HEADER child.
/// In dark mode the header is fully custom drawn: the DarkMode_ItemsView
/// theme is not reliable across Windows builds (on some it leaves the
/// header white), so painting it ourselves is the only deterministic way.
unsafe extern "system" fn listview_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::{
        CDDS_ITEMPREPAINT, CDDS_POSTPAINT, CDDS_PREPAINT, CDRF_DODEFAULT, CDRF_NOTIFYITEMDRAW,
        CDRF_NOTIFYPOSTPAINT, CDRF_SKIPDEFAULT, DefSubclassProc, HDF_SORTDOWN, HDF_SORTUP,
        HDI_FORMAT, HDITEMW, NMCUSTOMDRAW,
    };
    use winapi::um::wingdi::{SetBkMode, SetTextColor, TRANSPARENT};
    use winapi::um::winuser::{
        DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DrawTextW, FillRect, NMHDR,
        SendMessageW, WM_NOTIFY,
    };

    const NM_CUSTOMDRAW: u32 = (-12i32) as u32;
    const HDM_FIRST: u32 = 0x1200;
    const HDM_GETITEMW: u32 = HDM_FIRST + 11;
    const HDI_TEXT: u32 = 0x0002;

    unsafe {
        if msg == WM_NOTIFY && is_enabled() {
            let nmhdr = lparam as *const NMHDR;
            if (*nmhdr).code == NM_CUSTOMDRAW {
                let cd = lparam as *mut NMCUSTOMDRAW;
                match (*cd).dwDrawStage {
                    CDDS_PREPAINT => {
                        // Fill the whole header strip (covers the filler
                        // area beyond the last column too - the customdraw
                        // rc alone does not always span the full width).
                        let mut rc: RECT = std::mem::zeroed();
                        winapi::um::winuser::GetClientRect((*nmhdr).hwndFrom, &mut rc);
                        FillRect((*cd).hdc, &rc, bg_brush());
                        FillRect((*cd).hdc, &(*cd).rc, bg_brush());
                        return (CDRF_NOTIFYITEMDRAW | CDRF_NOTIFYPOSTPAINT) as LRESULT;
                    }
                    CDDS_POSTPAINT => {
                        // The classic header paints its filler area (right of
                        // the last column) AFTER the prepaint fill - overdraw
                        // it dark again.
                        let header = (*nmhdr).hwndFrom;
                        const HDM_GETITEMCOUNT: u32 = 0x1200;
                        const HDM_GETITEMRECT: u32 = 0x1200 + 7;

                        let mut rc: RECT = std::mem::zeroed();
                        winapi::um::winuser::GetClientRect(header, &mut rc);

                        let count = SendMessageW(header, HDM_GETITEMCOUNT, 0, 0);
                        let mut left = rc.left;
                        if count > 0 {
                            let mut irc: RECT = std::mem::zeroed();
                            if SendMessageW(
                                header,
                                HDM_GETITEMRECT,
                                (count - 1) as usize,
                                &mut irc as *mut RECT as LPARAM,
                            ) != 0
                            {
                                left = irc.right;
                            }
                        }
                        if left < rc.right {
                            let fill = RECT {
                                left,
                                top: rc.top,
                                right: rc.right,
                                bottom: rc.bottom,
                            };
                            FillRect((*cd).hdc, &fill, bg_brush());
                        }
                        return CDRF_DODEFAULT as LRESULT;
                    }
                    CDDS_ITEMPREPAINT => {
                        let header = (*nmhdr).hwndFrom;
                        let column = (*cd).dwItemSpec as usize;

                        let mut buf = [0u16; 128];
                        let mut item: HDITEMW = std::mem::zeroed();
                        item.mask = HDI_TEXT | HDI_FORMAT;
                        item.pszText = buf.as_mut_ptr();
                        item.cchTextMax = (buf.len() - 1) as i32;
                        SendMessageW(
                            header,
                            HDM_GETITEMW,
                            column,
                            &mut item as *mut HDITEMW as LPARAM,
                        );

                        let mut text = String::from_utf16_lossy(
                            &buf[..buf.iter().position(|c| *c == 0).unwrap_or(0)],
                        );
                        if item.fmt & HDF_SORTUP != 0 {
                            text.push_str(" \u{25B4}");
                        } else if item.fmt & HDF_SORTDOWN != 0 {
                            text.push_str(" \u{25BE}");
                        }

                        FillRect((*cd).hdc, &(*cd).rc, bg_brush());
                        SetBkMode((*cd).hdc, TRANSPARENT as i32);
                        SetTextColor((*cd).hdc, rgb(FG));

                        let wide = to_wide(&text);
                        let mut rc = (*cd).rc;
                        rc.left += 6;
                        rc.right -= 2;
                        DrawTextW(
                            (*cd).hdc,
                            wide.as_ptr(),
                            -1,
                            &mut rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                        );

                        return CDRF_SKIPDEFAULT as LRESULT;
                    }
                    _ => return CDRF_DODEFAULT as LRESULT,
                }
            }
        }

        if msg == winapi::um::winuser::WM_THEMECHANGED
            || msg == winapi::um::winuser::WM_SYSCOLORCHANGE
            || msg == winapi::um::winuser::WM_SETTINGCHANGE
            || msg == winapi::um::winuser::WM_DWMCOLORIZATIONCOLORCHANGED
        {
            // System broadcasts reset the list view's stored colors (classic
            // controls re-read the system palette) - re-apply after the
            // default processing.
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            apply_listview_colors(hwnd, is_enabled());
            winapi::um::winuser::InvalidateRect(hwnd, std::ptr::null(), 1);
            return result;
        }

        if msg == winapi::um::winuser::WM_ERASEBKGND && is_enabled() {
            // Paint the list view background (the empty area below the
            // items) ourselves - the control's own erase paths don't honor
            // LVM_SETBKCOLOR consistently on all Windows 11 builds.
            let mut rc: RECT = std::mem::zeroed();
            winapi::um::winuser::GetClientRect(hwnd, &mut rc);
            winapi::um::winuser::FillRect(wparam as HDC, &rc, bg_brush());
            return 1;
        }

        if msg == winapi::um::winuser::WM_PAINT && is_enabled() {
            // Belt and braces: after the control painted, overdraw the empty
            // region below the last item directly on the visible surface.
            // Some paint pipelines skip/ignore the erase step entirely, which
            // leaves that region white no matter what the LVM colors say.
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            paint_listview_empty_area(hwnd);
            return result;
        }

        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

unsafe fn paint_listview_empty_area(hwnd: HWND) {
    use winapi::um::winuser::{FillRect, GetClientRect, GetDC, ReleaseDC, SendMessageW};

    const LVM_FIRST: u32 = 0x1000;
    const LVM_GETITEMCOUNT: u32 = LVM_FIRST + 4;
    const LVM_GETITEMRECT: u32 = LVM_FIRST + 14;
    const LVM_GETHEADER: u32 = LVM_FIRST + 31;

    unsafe {
        let hdc = GetDC(hwnd);
        if hdc.is_null() {
            return;
        }

        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);

        let header = SendMessageW(hwnd, LVM_GETHEADER, 0, 0) as HWND;
        let mut header_h = 0i32;
        if !header.is_null() {
            let mut hrc: RECT = std::mem::zeroed();
            GetClientRect(header, &mut hrc);
            header_h = hrc.bottom;
        }

        // Top of the empty region: bottom of the last item, or the bottom
        // of the header when the list is empty.
        let count = SendMessageW(hwnd, LVM_GETITEMCOUNT, 0, 0);
        let mut top = header_h;
        let mut items_right = rc.right;

        if count > 0 {
            let mut item_rc: RECT = std::mem::zeroed();
            item_rc.left = 0; // LVIR_BOUNDS
            if SendMessageW(
                hwnd,
                LVM_GETITEMRECT,
                (count - 1) as usize,
                &mut item_rc as *mut RECT as LPARAM,
            ) != 0
            {
                top = item_rc.bottom;
                items_right = item_rc.right;
            }
        }

        if top < rc.bottom {
            let fill = RECT {
                left: rc.left,
                top,
                right: rc.right,
                bottom: rc.bottom,
            };
            FillRect(hdc, &fill, bg_brush());
        }

        // Also the strip to the right of the columns, next to the items.
        if items_right < rc.right && top > header_h {
            let fill = RECT {
                left: items_right,
                top: header_h,
                right: rc.right,
                bottom: top,
            };
            FillRect(hdc, &fill, bg_brush());
        }

        ReleaseDC(hwnd, hdc);
    }
}

/// NM_CUSTOMDRAW handling for the list view ITEMS - arrives at the list
/// view's PARENT. Forces per-item colors so the rows stay dark even on
/// Windows builds where the LVM color state gets reset by the theme engine.
/// Returns Some(lresult) when the message was consumed.
unsafe fn handle_listview_item_customdraw(msg: UINT, lparam: LPARAM) -> Option<LRESULT> {
    use winapi::um::commctrl::{
        CDDS_ITEMPREPAINT, CDDS_PREPAINT, CDRF_DODEFAULT, CDRF_NOTIFYITEMDRAW,
        CDRF_SKIPDEFAULT, NMLVCUSTOMDRAW,
    };
    use winapi::um::winuser::{NMHDR, WM_NOTIFY};

    const NM_CUSTOMDRAW: u32 = (-12i32) as u32;

    unsafe {
        if msg != WM_NOTIFY || !is_enabled() {
            return None;
        }

        let nmhdr = lparam as *const NMHDR;
        if (*nmhdr).code != NM_CUSTOMDRAW {
            return None;
        }

        // The only custom-draw sources on these parents are our list views.
        let cd = lparam as *mut NMLVCUSTOMDRAW;
        match (*cd).nmcd.dwDrawStage {
            CDDS_PREPAINT => Some(CDRF_NOTIFYITEMDRAW as LRESULT),
            CDDS_ITEMPREPAINT => {
                // The themed item painter draws its own (light) selection
                // highlight regardless of the custom-draw colors and state
                // flags on some Windows 11 builds - so it never runs: the
                // whole row is painted here instead.
                draw_listview_row((*nmhdr).hwndFrom, &*cd);
                Some(CDRF_SKIPDEFAULT as LRESULT)
            }
            _ => Some(CDRF_DODEFAULT as LRESULT),
        }
    }
}

/// Fully paint one list view row in dark mode: background, selection shade
/// and every cell's text.
unsafe fn draw_listview_row(lv: HWND, cd: &winapi::um::commctrl::NMLVCUSTOMDRAW) {
    use winapi::um::commctrl::LVITEMW;
    use winapi::um::wingdi::{SetBkMode, SetTextColor, TRANSPARENT};
    use winapi::um::winuser::{
        DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DrawTextW, FillRect,
        SendMessageW,
    };

    const LVM_FIRST: u32 = 0x1000;
    const LVM_GETITEMRECT: u32 = LVM_FIRST + 14;
    const LVM_GETHEADER: u32 = LVM_FIRST + 31;
    const LVM_GETITEMSTATE: u32 = LVM_FIRST + 44;
    const LVM_GETSUBITEMRECT: u32 = LVM_FIRST + 56;
    const LVM_GETITEMTEXTW: u32 = LVM_FIRST + 115;
    const HDM_GETITEMCOUNT: u32 = 0x1200;
    const LVIS_SELECTED: isize = 0x0002;
    const LVIR_BOUNDS: i32 = 0;
    const LVIR_LABEL: i32 = 2;

    unsafe {
        let item = cd.nmcd.dwItemSpec;
        let hdc = cd.nmcd.hdc;

        let selected =
            SendMessageW(lv, LVM_GETITEMSTATE, item, LVIS_SELECTED) & LVIS_SELECTED != 0;

        // Row background (spans the full row width, all columns).
        let mut row_rc: RECT = std::mem::zeroed();
        row_rc.left = LVIR_BOUNDS;
        if SendMessageW(lv, LVM_GETITEMRECT, item, &mut row_rc as *mut RECT as LPARAM) == 0 {
            return;
        }
        FillRect(
            hdc,
            &row_rc,
            if selected { bg_hot_brush() } else { bg_brush() },
        );

        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, rgb(FG));

        let header = SendMessageW(lv, LVM_GETHEADER, 0, 0) as HWND;
        let columns = if header.is_null() {
            1
        } else {
            SendMessageW(header, HDM_GETITEMCOUNT, 0, 0).max(1) as usize
        };

        let progress_col = progress_column_for(lv);

        for col in 0..columns {
            // Cell rectangle. For subitem 0, LVIR_BOUNDS returns the whole
            // row - use the label rect instead.
            let mut rc: RECT = std::mem::zeroed();
            rc.top = col as i32;
            rc.left = if col == 0 { LVIR_LABEL } else { LVIR_BOUNDS };
            if SendMessageW(lv, LVM_GETSUBITEMRECT, item, &mut rc as *mut RECT as LPARAM) == 0
            {
                continue;
            }

            let mut buf = [0u16; 512];
            let mut lvi: LVITEMW = std::mem::zeroed();
            lvi.iSubItem = col as i32;
            lvi.pszText = buf.as_mut_ptr();
            lvi.cchTextMax = (buf.len() - 1) as i32;
            let len = SendMessageW(
                lv,
                LVM_GETITEMTEXTW,
                item,
                &mut lvi as *mut LVITEMW as LPARAM,
            );
            if len <= 0 {
                continue;
            }

            // Progress column: draw a bar filling the cell (auto-resizes with
            // the column width) instead of the "%" text.
            if progress_col == Some(col as i32) {
                if let Some(frac) = parse_percent(&buf[..len as usize]) {
                    draw_progress_cell(hdc, &rc, frac);
                    continue;
                }
            }

            rc.left += 6;
            rc.right -= 4;
            DrawTextW(
                hdc,
                buf.as_ptr(),
                len as i32,
                &mut rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
    }
}

/// Draw a progress bar (track + green fill + border + centered %) inside a
/// list cell rect. Matches PicoTorrent's Progress column; resizes with the
/// column because it uses the live subitem rect.
unsafe fn draw_progress_cell(hdc: winapi::shared::windef::HDC, cell: &RECT, frac: f32) {
    use winapi::um::wingdi::{
        CreateSolidBrush, DeleteObject, SetBkMode, SetTextColor, TRANSPARENT,
    };
    use winapi::um::winuser::{
        DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DrawTextW, FillRect, FrameRect,
    };

    unsafe {
        let mut bar = *cell;
        bar.left += 4;
        bar.right -= 4;
        bar.top += 3;
        bar.bottom -= 3;
        if bar.right <= bar.left || bar.bottom <= bar.top {
            return;
        }

        // Track background.
        let track = CreateSolidBrush(rgb(PROGRESS_TRACK));
        FillRect(hdc, &bar, track);
        DeleteObject(track as _);

        // Filled portion.
        let width = ((bar.right - bar.left) as f32 * frac.clamp(0.0, 1.0)).round() as i32;
        if width > 0 {
            let mut fill = bar;
            fill.right = bar.left + width;
            let green = CreateSolidBrush(rgb(PROGRESS_FILL));
            FillRect(hdc, &fill, green);
            DeleteObject(green as _);
        }

        // Border.
        let border = CreateSolidBrush(rgb(PROGRESS_BORDER));
        FrameRect(hdc, &bar, border);
        DeleteObject(border as _);

        // Percentage text centered over the bar.
        let mut txt: Vec<u16> = format!("{:.1} %", frac * 100.0).encode_utf16().collect();
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, rgb(FG));
        let mut tr = bar;
        DrawTextW(
            hdc,
            txt.as_mut_ptr(),
            txt.len() as i32,
            &mut tr,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }
}

pub fn install_listview_subclass(hwnd: HWND) {
    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(listview_subclass), 0x5056, 0);
    }
}

pub fn register_status_bar(hwnd: HWND) {
    STATUS_HWND.store(hwnd as usize, Ordering::Relaxed);
}

pub fn register_tab_control(hwnd: HWND) {
    install_tab_control_subclass(hwnd);
}

pub fn install_tab_control_subclass(hwnd: HWND) {
    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(tab_control_subclass), 0x5057, 0);
    }
}

pub fn install_main_subclass(hwnd: HWND) {
    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(main_window_subclass), 0x5054, 0);
    }
}

pub fn install_tab_page_subclass(hwnd: HWND) {
    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(tab_page_subclass), 0x5055, 0);
    }
}

// Piece progress bar - an owner-drawn strip showing which pieces of the
// selected torrent are downloaded (accent) and which are missing (track).

static PIECE_BAR: Mutex<(Vec<u8>, usize)> = Mutex::new((Vec::new(), 0));

pub fn install_piece_bar(hwnd: HWND) {
    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(piece_bar_subclass), 0x5060, 0);
    }
}

/// Overpaint an NWG Label's non-client strips with the theme background.
///
/// NWG's Label vertically centers its text by carving a smaller client area and
/// painting the leftover top/bottom strips with COLOR_WINDOW (white) - visible
/// as an "up/bottom border" box around the control. This runs after the default
/// NC paint and fills those strips with the theme brush so they blend in, at any
/// DPI and in either theme.
pub fn install_field_bg(hwnd: HWND) {
    unsafe {
        winapi::um::commctrl::SetWindowSubclass(hwnd, Some(field_bg_subclass), 0x5061, 0);
    }
}

unsafe extern "system" fn field_bg_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::shared::windef::POINT;
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::winuser::{
        ClientToScreen, FillRect, GetClientRect, GetWindowDC, GetWindowRect, ReleaseDC, WM_NCPAINT,
    };

    unsafe {
        // Let the default (and NWG's hook) paint first, then overpaint the NC
        // strips - this is order-independent, so it always wins.
        let result = DefSubclassProc(hwnd, msg, wparam, lparam);

        if msg == WM_NCPAINT {
            let hdc = GetWindowDC(hwnd);
            if !hdc.is_null() {
                let mut wr: RECT = std::mem::zeroed();
                let mut cr: RECT = std::mem::zeroed();
                GetWindowRect(hwnd, &mut wr);
                GetClientRect(hwnd, &mut cr);
                // Client's top-left in window-DC coordinates.
                let mut p = POINT { x: 0, y: 0 };
                ClientToScreen(hwnd, &mut p);
                let ox = p.x - wr.left;
                let oy = p.y - wr.top;
                let ww = wr.right - wr.left;
                let wh = wr.bottom - wr.top;
                let cw = cr.right - cr.left;
                let ch = cr.bottom - cr.top;

                let brush = if is_enabled() { bg_brush() } else { white_brush() };
                let fill = |t: i32, b: i32| {
                    if b > t {
                        let r = RECT { left: 0, top: t, right: ww, bottom: b };
                        FillRect(hdc, &r, brush);
                    }
                };
                fill(0, oy); // top strip
                fill(oy + ch, wh); // bottom strip
                if ox > 0 {
                    let r = RECT { left: 0, top: 0, right: ox, bottom: wh };
                    FillRect(hdc, &r, brush);
                }
                if ox + cw < ww {
                    let r = RECT { left: ox + cw, top: 0, right: ww, bottom: wh };
                    FillRect(hdc, &r, brush);
                }
                ReleaseDC(hwnd, hdc);
            }
        }

        result
    }
}

/// Update the bar's bitfield (one bit per piece, MSB first) and repaint if
/// it changed.
pub fn set_piece_bar(hwnd: HWND, bytes: Vec<u8>, total: usize) {
    let changed = {
        let mut bar = PIECE_BAR.lock().unwrap();
        if bar.0 != bytes || bar.1 != total {
            *bar = (bytes, total);
            true
        } else {
            false
        }
    };
    if changed {
        unsafe {
            winapi::um::winuser::InvalidateRect(hwnd, std::ptr::null(), 0);
        }
    }
}

unsafe extern "system" fn piece_bar_subclass(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use winapi::um::commctrl::DefSubclassProc;
    use winapi::um::winuser::{WM_ERASEBKGND, WM_PAINT};

    unsafe {
        match msg {
            WM_PAINT => {
                paint_piece_bar(hwnd);
                0
            }
            WM_ERASEBKGND => 1,
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint_piece_bar(hwnd: HWND) {
    use winapi::um::winuser::{BeginPaint, EndPaint, FillRect, GetClientRect, PAINTSTRUCT};

    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_null() {
            return;
        }

        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);

        let track = if is_enabled() {
            edit_bg_brush()
        } else {
            light_track_brush()
        };
        FillRect(hdc, &rc, track);

        let (bytes, total) = PIECE_BAR.lock().unwrap().clone();
        let width = rc.right - rc.left;
        if total > 0 && width > 0 {
            let have_at = |x: i32| -> bool {
                let piece = (x as usize * total) / width as usize;
                bytes
                    .get(piece / 8)
                    .is_some_and(|b| (b >> (7 - piece % 8)) & 1 == 1)
            };

            // Paint contiguous runs of downloaded pieces.
            let mut x = 0;
            while x < width {
                let have = have_at(x);
                let mut end = x + 1;
                while end < width && have_at(end) == have {
                    end += 1;
                }
                if have {
                    let seg = RECT {
                        left: x,
                        top: rc.top,
                        right: end,
                        bottom: rc.bottom,
                    };
                    FillRect(hdc, &seg, accent_brush());
                }
                x = end;
            }
        }

        EndPaint(hwnd, &ps);
    }
}

/// Force a full repaint of a window tree (used after theme switches).
pub fn redraw_all(hwnd: HWND) {
    use winapi::um::winuser::{
        DrawMenuBar, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RedrawWindow,
    };

    unsafe {
        DrawMenuBar(hwnd);
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_ERASE | RDW_FRAME | RDW_INVALIDATE | RDW_ALLCHILDREN,
        );
    }
}

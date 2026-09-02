//! Real desktop notifications: the Win10/11 bottom-right popup and Action
//! Center entry on Windows, the D-Bus notification daemon on Linux, and
//! Notification Center on macOS.
//!
//! The three platforms share nothing but the entry points below. Windows is
//! hand-rolled because of the AUMID dance described next; Linux and macOS both
//! go through `notify-rust`, which asks for nothing at startup beyond telling
//! macOS whose notification this is.
//!
//! An *unpackaged* desktop app only gets toasts if Windows can find its
//! AppUserModelID on a Start Menu shortcut - the registry key alone never
//! registers it as a notification source, which is why it never appeared under
//! Settings > Notifications. So at startup we drop a shortcut carrying the
//! AUMID (idempotent) and tag the process with the same AUMID, then fire
//! toasts against it.
//!
//! Everything here uses `winapi`, deliberately NOT the `windows` crate: the
//! latter's Win32_UI_Shell module pulls a raw-dylib `comctl32` import
//! (GetWindowSubclass) that fails to load against the classic comctl32 this
//! app runs on for dark-mode owner-drawing.

#[cfg(windows)]
const AUMID: &str = "Power2All.NanoTorrent";

// Toast images must be PNG/JPEG (an .ico won't render), so ship a PNG copy of
// the app icon and drop it on disk for the toast to reference by file path.
#[cfg(windows)]
const TOAST_ICON_PNG: &[u8] = include_bytes!("../../res/app.png");

/// Stable on-disk path for the toast icon (written once from TOAST_ICON_PNG).
#[cfg(windows)]
fn icon_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::Path::new(&base)
            .join("NanoTorrent")
            .join("toast_icon.png"),
    )
}

/// Ensure the toast icon PNG exists on disk; returns its path if available.
#[cfg(windows)]
fn ensure_icon_file() -> Option<std::path::PathBuf> {
    let path = icon_path()?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, TOAST_ICON_PNG);
    }
    path.exists().then_some(path)
}

#[cfg(windows)]
#[link(name = "shell32")]
unsafe extern "system" {
    fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
}

#[cfg(windows)]
fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// One-time setup so Windows recognises and displays our toasts. Best-effort:
/// any failure is logged, never fatal.
#[cfg(windows)]
pub fn register() {
    // Legacy/registry hint (taskbar grouping display name + notification icon).
    let key = format!("Software\\Classes\\AppUserModelId\\{AUMID}");
    let _ = crate::core::file_assoc::set_string(&key, Some("DisplayName"), "NanoTorrent");
    if let Some(icon) = ensure_icon_file() {
        let _ =
            crate::core::file_assoc::set_string(&key, Some("IconUri"), &icon.display().to_string());
    }

    if let Err(e) = ensure_shortcut() {
        tracing::warn!("toast: could not create Start Menu shortcut: {e:#}");
    }

    // Tag this process so toasts and taskbar map to the same identity.
    let id = wide(std::ffi::OsStr::new(AUMID));
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
    }
}

/// macOS attributes a notification to a bundle identifier and falls back to
/// Terminal's when it is never told one, which would put "Terminal" above every
/// finished download. Claim our own, once.
///
/// Failure is expected and harmless for a binary run outside the .app bundle
/// (`cargo run`), so it is logged rather than treated as a problem.
#[cfg(target_os = "macos")]
pub fn register() {
    if let Err(e) = notify_rust::set_application(IDENTITY) {
        tracing::warn!("toast: could not claim the notification identity: {e}");
    }
}

/// Nothing to register: the D-Bus daemon takes the application name and icon
/// with each notification rather than up front.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn register() {}

/// Create `%APPDATA%\..\Start Menu\Programs\NanoTorrent.lnk` with the AUMID
/// property, unless it already exists. This is what lets Windows deliver
/// toasts to an unpackaged app and list it in notification settings.
#[cfg(windows)]
fn ensure_shortcut() -> anyhow::Result<()> {
    use winapi::shared::winerror::SUCCEEDED;
    use winapi::shared::wtypes::VT_LPWSTR;
    use winapi::um::combaseapi::{CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree};
    use winapi::um::objbase::COINIT_APARTMENTTHREADED;
    use winapi::um::objidl::IPersistFile;
    use winapi::um::propidl::PROPVARIANT;
    use winapi::um::propkey::PKEY_AppUserModel_ID;
    use winapi::um::propsys::IPropertyStore;
    use winapi::um::shobjidl_core::{IShellLinkW, ShellLink};
    use winapi::{Class, Interface};

    // CLSCTX_INPROC_SERVER; keep as a literal to avoid winapi module churn.
    const CLSCTX_INPROC_SERVER: u32 = 0x1;

    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Ok(());
    };
    let lnk = std::path::Path::new(&appdata)
        .join(r"Microsoft\Windows\Start Menu\Programs\NanoTorrent.lnk");
    let exe = std::env::current_exe()?;
    // Rewrite the shortcut whenever it does not point at the running exe.
    //
    // This used to early-out on the mere existence of the .lnk plus a marker
    // file, which meant a shortcut written by an older install pointed at that
    // install forever. That is not cosmetic: `register` below tags this process
    // with the AUMID, and Windows resolves the TASKBAR icon through the
    // shortcut carrying it - so a stale target left the taskbar on the generic
    // app glyph. Renaming the executable (0.2.5 split the GUI out as
    // nanotorrent-gui.exe) is exactly the case that broke it.
    //
    // The marker holds the path it was last written for, so the common case is
    // still one small file read and no COM at all.
    let marker = icon_path().map(|p| p.with_file_name(".shortcut_target"));
    let want = exe.display().to_string();
    let current = marker
        .as_ref()
        .and_then(|m| std::fs::read_to_string(m).ok());
    if lnk.exists() && current.as_deref() == Some(want.as_str()) {
        return Ok(());
    }

    unsafe {
        CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);

        let mut link: *mut IShellLinkW = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &ShellLink::uuidof(),
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IShellLinkW::uuidof(),
            &mut link as *mut _ as *mut _,
        );
        if !SUCCEEDED(hr) || link.is_null() {
            anyhow::bail!("CoCreateInstance(ShellLink) failed: {hr:#010x}");
        }

        (*link).SetPath(wide(exe.as_os_str()).as_ptr());
        // The shortcut's icon is what Windows shows as the toast/Action-Center
        // attribution icon; use the exe's embedded icon (index 0).
        (*link).SetIconLocation(wide(exe.as_os_str()).as_ptr(), 0);

        // Stamp the AUMID onto the shortcut via IPropertyStore.
        let mut store: *mut IPropertyStore = std::ptr::null_mut();
        if SUCCEEDED(
            (*link).QueryInterface(&IPropertyStore::uuidof(), &mut store as *mut _ as *mut _),
        ) && !store.is_null()
        {
            // Build a VT_LPWSTR PROPVARIANT holding the AUMID. SetValue copies
            // it, so the CoTaskMem buffer is freed again right after.
            let src = wide(std::ffi::OsStr::new(AUMID));
            let buf = CoTaskMemAlloc(src.len() * 2) as *mut u16;
            if !buf.is_null() {
                std::ptr::copy_nonoverlapping(src.as_ptr(), buf, src.len());
                let mut pv: PROPVARIANT = std::mem::zeroed();
                pv.vt = VT_LPWSTR as u16;
                *pv.data.pwszVal_mut() = buf;
                (*store).SetValue(&PKEY_AppUserModel_ID, &pv);
                (*store).Commit();
                CoTaskMemFree(buf as *mut _);
            }
            (*store).Release();
        }

        // Persist the .lnk.
        let mut pf: *mut IPersistFile = std::ptr::null_mut();
        let hr = (*link).QueryInterface(&IPersistFile::uuidof(), &mut pf as *mut _ as *mut _);
        if SUCCEEDED(hr) && !pf.is_null() {
            if SUCCEEDED((*pf).Save(wide(lnk.as_os_str()).as_ptr(), 1))
                && let Some(m) = &marker
            {
                let _ = std::fs::write(m, want.as_bytes());
            }
            (*pf).Release();
        }
        (*link).Release();
    }
    Ok(())
}

/// Configuration key gating the notification below.
pub const ENABLED_KEY: &str = "notifications.download_complete";

/// One string doing two jobs on the two platforms that need it: the icon name
/// Linux looks up in the icon theme (matching `Icon=` in the `.desktop` entry
/// installed by `packaging/linux/install-desktop-entry.sh`) and the bundle
/// identifier macOS attributes notifications to (matching `CFBundleIdentifier`
/// in the .app built by the release workflow). Windows uses `AUMID` above
/// instead. Changing this means changing all three.
#[cfg(unix)]
const IDENTITY: &str = "org.nanotorrent.NanoTorrent";

/// Show a "download complete" toast for the given torrent.
///
/// The caller checks [`ENABLED_KEY`] first; this stays unconditional so the
/// setting is read in one place rather than plumbing a Configuration in here.
#[cfg(windows)]
pub fn download_complete(title: &str, name: &str) {
    use tauri_winrt_notification::{IconCrop, Toast};
    let mut toast = Toast::new(AUMID).title(title).text1(name);
    if let Some(icon) = icon_path().filter(|p| p.exists()) {
        toast = toast.icon(&icon, IconCrop::Square, "NanoTorrent");
    }
    if let Err(e) = toast.show() {
        tracing::warn!("failed to show toast notification: {e:#}");
    }
}

/// Linux and macOS, both through `notify-rust`.
///
/// `icon` and `appname` are no-ops on macOS (it uses the bundle claimed in
/// [`register`]); on Linux the icon is a name the daemon looks up in the icon
/// theme, and it is deliberately the same string the `.desktop` entry's `Icon=`
/// key carries, so an install that has one shows the real icon and one that
/// does not falls back to the generic glyph rather than showing something wrong.
///
/// The returned handle is dropped straight away. That is not neglect: on Linux
/// it holds nothing, and on macOS dropping an unused handle is what actually
/// posts the notification.
#[cfg(unix)]
pub fn download_complete(title: &str, name: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .appname("NanoTorrent")
        .icon(IDENTITY)
        .summary(title)
        .body(name)
        .show()
    {
        // A headless box with no session bus lands here for every completed
        // torrent. That is worth saying once per download and no louder: the
        // download itself succeeded.
        tracing::warn!("failed to show desktop notification: {e}");
    }
}

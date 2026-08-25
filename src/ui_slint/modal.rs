//! Making the main window modal while a dialog is open.
//!
//! Three parts, and all three are needed:
//!
//! * **Ownership.** Each dialog is made an owned window of the main one. An
//!   owned window stays above its owner and, when it is destroyed, Windows
//!   hands activation back to that owner. Without this the dialog is an
//!   unrelated top-level, and closing it dropped the main window behind
//!   whatever happened to be next in the Z-order - usually another
//!   application, which looked like the main window had minimised itself.
//! * **`EnableWindow(owner, false)`.** What makes a real modal: the title bar
//!   stops responding, and clicking the blocked window flashes the dialog
//!   rather than raising itself over it. Nothing in Slint can do that part.
//! * **A `blocked` flag** the window reads to lay a transparent TouchArea over
//!   its content, so the behaviour degrades to "inert" rather than to nothing
//!   on platforms without the two calls above.

use slint::ComponentHandle;

use super::MainWindow;

/// Bring a window to the front and give it focus.
///
/// `slint::Window` has no raise or focus call, so restoring from the tray left
/// the window behind whatever was in front of it.
#[cfg_attr(not(windows), allow(unused_variables))]
pub fn raise(window: &MainWindow) {
    #[cfg(windows)]
    {
        let Some(hwnd) = hwnd(window) else { return };
        unsafe {
            // SW_RESTORE first: a window that was minimised comes back
            // minimised otherwise, and SetForegroundWindow will not un-do that.
            winapi::um::winuser::ShowWindow(hwnd, winapi::um::winuser::SW_RESTORE);
            winapi::um::winuser::SetForegroundWindow(hwnd);
        }
    }
}

/// Block or unblock the main window.
pub fn set_blocked(window: &MainWindow, blocked: bool) {
    window.set_blocked(blocked);
    #[cfg(windows)]
    enable_native(window, !blocked);
}

/// Make `dialog` an owned window of `owner`, if it is not already.
///
/// Called from the modal poll rather than at each `show()`, so a dialog opened
/// by any path is covered and re-showing an existing one costs one comparison.
#[cfg_attr(not(windows), allow(unused_variables))]
pub fn own<T: ComponentHandle>(dialog: &T, owner: &MainWindow) {
    #[cfg(windows)]
    {
        use winapi::um::winuser::{GWLP_HWNDPARENT, GetWindowLongPtrW, SetWindowLongPtrW};

        let (Some(dialog), Some(owner)) = (hwnd(dialog), hwnd(owner)) else {
            return;
        };
        unsafe {
            if GetWindowLongPtrW(dialog, GWLP_HWNDPARENT) != owner as isize {
                SetWindowLongPtrW(dialog, GWLP_HWNDPARENT, owner as isize);
            }
        }
    }
}

/// Enable or disable the main window at the Win32 level, which is what makes
/// it refuse clicks while a dialog is up.
#[cfg(windows)]
fn enable_native(window: &MainWindow, enabled: bool) {
    let Some(hwnd) = hwnd(window) else { return };
    unsafe {
        // No SetForegroundWindow to follow: callers re-enable BEFORE the
        // dialog hides, so Windows picks this window - the owner, and now
        // enabled - on its own. Grabbing the foreground afterwards worked but
        // showed another application for a frame first.
        winapi::um::winuser::EnableWindow(hwnd, i32::from(enabled));
    }
}

/// The Win32 handle behind a Slint window, or `None` before it is shown.
#[cfg(windows)]
fn hwnd<T: ComponentHandle>(component: &T) -> Option<winapi::shared::windef::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = component.window().window_handle();
    let handle = handle.window_handle().ok()?;
    match handle.as_raw() {
        // isize::from is how the 0.6 NonZeroIsize reaches the winapi HWND.
        RawWindowHandle::Win32(win32) => Some(isize::from(win32.hwnd) as _),
        _ => None,
    }
}

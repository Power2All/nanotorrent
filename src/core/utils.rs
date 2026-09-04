// Port of src/picotorrent/core/utils.{hpp,cpp}

use std::path::Path;

/// Mimics Win32 StrFormatByteSize64 which PicoTorrent used via
/// Utils::toHumanFileSize.
pub fn to_human_file_size(bytes: i64) -> String {
    const UNITS: [&str; 7] = ["bytes", "KB", "MB", "GB", "TB", "PB", "EB"];

    if bytes < 0 {
        return String::from("-");
    }

    if bytes < 1024 {
        return format!("{} {}", bytes, UNITS[0]);
    }

    let mut value = bytes as f64;
    let mut unit = 0usize;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    // StrFormatByteSize shows up to two decimals
    format!("{:.2} {}", value, UNITS[unit])
}

/// A transfer rate as a human-readable string, e.g. `1.4 MB/s`.
///
/// Formats whatever it is given; deciding that a rate is too small to be worth
/// showing belongs to the caller (see `ui::format::speed_text`).
pub fn to_human_speed(bytes_per_second: i64) -> String {
    format!("{}/s", to_human_file_size(bytes_per_second))
}

/// Show a downloaded torrent's folder in the desktop's file manager.
///
/// Port of Utils::openAndSelect. Windows selects the folder inside its parent,
/// which is what explorer's /select does; everywhere else the folder is simply
/// opened, there being no portable equivalent of "reveal this item".
pub fn open_and_select(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // A '"' in the path would close the quoting below and inject extra
        // explorer arguments (explorer can launch programs). A real Windows
        // path can't contain one, so treat its presence as hostile and just
        // open the parent folder instead of selecting.
        if path.to_string_lossy().contains('"') {
            if let Some(parent) = path.parent() {
                let _ = open::that(parent);
            }
            return;
        }
        // explorer parses "/select,<path>" itself and is picky about it. The
        // path MUST be quoted, or spaces / parentheses in it break the parse
        // and explorer silently opens Documents instead. Rust's normal arg
        // quoting would wrap the whole "/select,<path>" token (which explorer
        // misreads), so pass it verbatim with raw_arg and quote the path only.
        if let Err(err) = std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
        {
            tracing::error!("cannot open explorer for {}: {err}", path.display());
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // The path IS the folder holding the download, so open it - not its
        // parent. Mirroring the Windows call was wrong: explorer's /select
        // highlights the folder inside its parent, whereas open::that simply
        // opens whatever it is given, so this used to land a level too high
        // (the home directory instead of the download folder).
        //
        // There is no portable "reveal this item" equivalent; opening the
        // containing folder is what the menu entry promises anyway.
        //
        // Under a sandbox this goes through the desktop's OpenURI portal, and
        // a save path outside what the sandbox can reach may be refused - which
        // is why the error is logged rather than dropped.
        if let Err(err) = open::that(path) {
            tracing::error!("cannot open {}: {err}", path.display());
        }
    }
}

/// Free and total bytes on the filesystem holding `path`, or `None` if it
/// cannot be determined (a path that does not exist, a permission error, an
/// unsupported platform).
///
/// There is no std API for this, so it is one small platform call each. The
/// caller must treat `None` as "do not know" and take no action on it -
/// pausing every torrent because a stat failed would be worse than the disk
/// filling.
#[cfg(windows)]
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;

    // GetDiskFreeSpaceExW takes a directory; walk up until one exists, so a
    // save path that has not been created yet still reports its drive.
    let mut dir = path;
    while !dir.is_dir() {
        dir = dir.parent()?;
    }

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: a zeroed ULARGE_INTEGER is a valid value of the union, `wide` is
    // NUL-terminated and outlives the call, and both outputs are ours.
    unsafe {
        let mut avail: winapi::um::winnt::ULARGE_INTEGER = std::mem::zeroed();
        let mut total: winapi::um::winnt::ULARGE_INTEGER = std::mem::zeroed();
        // Available-to-caller rather than total free, so a disk quota is
        // respected - the same reason the unix arm reads f_bavail.
        let ok = winapi::um::fileapi::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut avail,
            &mut total,
            std::ptr::null_mut(),
        );
        (ok != 0).then(|| (*avail.QuadPart(), *total.QuadPart()))
    }
}

#[cfg(unix)]
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let mut dir = path;
    while !dir.is_dir() {
        dir = dir.parent()?;
    }

    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed statvfs is a valid starting state and c_path is a valid
    // NUL-terminated string that outlives the call.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }

    // f_bavail, not f_bfree: the reserved blocks root can still use are not
    // available to this process and counting them would report space that
    // cannot actually be written.
    let unit = st.f_frsize as u64;
    Some((st.f_bavail as u64 * unit, st.f_blocks as u64 * unit))
}

/// Free space as a percentage of the volume, for the low-disk guard.
pub fn free_space_percent(path: &Path) -> Option<f64> {
    let (free, total) = disk_space(path)?;
    (total > 0).then(|| free as f64 * 100.0 / total as f64)
}

/// Height of the primary monitor's work area in PHYSICAL pixels, or `None`
/// when it cannot be determined.
///
/// Work area, not screen: it excludes the taskbar, which is exactly the space
/// a window may actually occupy. The caller must divide by the window's scale
/// factor to get the logical pixels a layout speaks in - on a 200% display the
/// two differ by a factor of two, and using the raw number would let a dialog
/// grow to twice the screen.
#[cfg(windows)]
pub fn work_area_height_at(x: i32, y: i32) -> Option<f32> {
    use winapi::um::winuser::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };

    // Was SPI_GETWORKAREA, which answers for the PRIMARY monitor and nothing
    // else. On a second screen - shorter, or scaled differently - the caller
    // divided the primary's height by this window's scale factor and got a
    // limit taller than the monitor the window was actually on, so the clamp
    // never engaged and tall dialogs ran off the bottom of the screen.
    let point = winapi::shared::windef::POINT { x, y };
    // SAFETY: a by-value POINT and a documented flag; the returned handle is
    // not owned and is only passed straight back to GetMonitorInfoW.
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }

    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY: cbSize is set, which is the whole contract of this call.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info) };
    (ok != 0).then(|| (info.rcWork.bottom - info.rcWork.top) as f32)
}

/// No equivalent wired up off Windows yet: the dialogs simply size to their
/// content there, which is what they did before the clamp existed.
#[cfg(not(windows))]
pub fn work_area_height_at(_x: i32, _y: i32) -> Option<f32> {
    None
}

#[cfg(test)]
mod tests {
    /// The probe must agree with itself and stay in range on a path that
    /// certainly exists. Exact figures are the filesystem's business, but a
    /// percentage outside 0-100, or free exceeding total, means the union
    /// fields or the block-size multiply are wrong - and this silently decides
    /// whether every torrent gets paused.
    #[test]
    fn disk_space_is_self_consistent() {
        let here = std::env::temp_dir();
        let (free, total) = super::disk_space(&here).expect("temp dir is on a real volume");
        assert!(total > 0, "volume reports zero size");
        assert!(free <= total, "free {free} exceeds total {total}");

        let pct = super::free_space_percent(&here).unwrap();
        assert!((0.0..=100.0).contains(&pct), "percentage out of range: {pct}");

        // A path that cannot exist reports "do not know" rather than zero -
        // the guard must never read a failure as a full disk.
        let missing = std::path::Path::new("nanotorrent-no-such-dir-9f3a2b");
        assert!(super::disk_space(missing).is_none());
    }

    use super::*;

    #[test]
    fn human_file_size_boundaries() {
        assert_eq!(to_human_file_size(-1), "-");
        assert_eq!(to_human_file_size(0), "0 bytes");
        assert_eq!(to_human_file_size(1023), "1023 bytes");
        assert_eq!(to_human_file_size(1024), "1.00 KB");
        assert_eq!(to_human_file_size(1536), "1.50 KB");
        assert_eq!(to_human_file_size(1024 * 1024), "1.00 MB");
        assert_eq!(to_human_file_size(1024i64.pow(3)), "1.00 GB");
        assert_eq!(to_human_file_size(1024i64.pow(4)), "1.00 TB");
    }

    #[test]
    fn human_speed_suffixes_per_second() {
        assert_eq!(to_human_speed(1024), "1.00 KB/s");
        assert_eq!(to_human_speed(0), "0 bytes/s");
    }
}


#[cfg(test)]
mod work_area {
    /// The probe must answer for the monitor containing the point, not for the
    /// primary one.
    ///
    /// It used to call SPI_GETWORKAREA, which only ever answers for the
    /// primary monitor. On a second screen of a different height or scale the
    /// caller divided the primary's height by this window's scale factor, got
    /// a limit taller than the screen the window was on, and never clamped -
    /// which is how a tall dialog ran off the bottom.
    #[test]
    #[cfg(windows)]
    fn every_monitor_answers_for_itself() {
        use winapi::um::winuser::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };

        // Probe a wide spread of the virtual desktop. Whatever monitor each
        // point lands on, the answer must match that monitor's own work area.
        for x in [-4000, 0, 1000, 3000, 5200, 9000] {
            for y in [-2000, 0, 500, 1500, 3000] {
                let point = winapi::shared::windef::POINT { x, y };
                let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
                assert!(!monitor.is_null(), "NEAREST returned no monitor");

                let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
                info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                assert!(unsafe { GetMonitorInfoW(monitor, &mut info) } != 0);
                let expected = (info.rcWork.bottom - info.rcWork.top) as f32;

                assert_eq!(
                    super::work_area_height_at(x, y),
                    Some(expected),
                    "({x},{y}) did not report its own monitor"
                );
            }
        }
    }

    /// A point nowhere near a monitor still gets an answer rather than None -
    /// None means "unclamped", and silently unclamping is the bug.
    #[test]
    #[cfg(windows)]
    fn a_far_off_point_still_clamps() {
        let h = super::work_area_height_at(-100_000, -100_000);
        assert!(h.is_some_and(|h| h > 200.0), "got {h:?}");
    }
}


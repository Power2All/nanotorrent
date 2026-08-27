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
        // Inside a Flatpak this goes through the OpenURI portal. A save path
        // outside the sandbox's reach can still be refused, which is why the
        // error is logged rather than dropped.
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
pub fn work_area_height() -> Option<f32> {
    let mut rect: winapi::shared::windef::RECT = unsafe { std::mem::zeroed() };
    // SAFETY: SPI_GETWORKAREA writes a RECT, which is what we pass.
    let ok = unsafe {
        winapi::um::winuser::SystemParametersInfoW(
            winapi::um::winuser::SPI_GETWORKAREA,
            0,
            &mut rect as *mut _ as *mut winapi::ctypes::c_void,
            0,
        )
    };
    (ok != 0).then(|| (rect.bottom - rect.top) as f32)
}

/// No equivalent wired up off Windows yet: the dialogs simply size to their
/// content there, which is what they did before the clamp existed.
#[cfg(not(windows))]
pub fn work_area_height() -> Option<f32> {
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

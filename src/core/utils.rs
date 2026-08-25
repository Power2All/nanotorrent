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

/// Port of Utils::openAndSelect - opens Windows Explorer with the
/// given file selected.
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
        let _ = std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn();
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(parent) = path.parent() {
            let _ = open::that(parent);
        }
    }
}

#[cfg(test)]
mod tests {
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

// Port of src/picotorrent/core/environment.{hpp,cpp}

use std::path::PathBuf;
use std::time::SystemTime;

pub struct Environment {
    startup_time: SystemTime,
}

impl Environment {
    /// Work out where this installation keeps its data, logs and translations.
    ///
    /// Resolved once at startup and passed around: every path in the app
    /// derives from here, so there is one answer rather than one per caller.
    pub fn create() -> Environment {
        Environment {
            startup_time: SystemTime::now(),
        }
    }

    /// The directory where the executable lives.
    pub fn get_application_path(&self) -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Port of Environment::GetApplicationDataPath.
    ///
    /// The C++ version checks the Windows registry to see if the app is
    /// installed and uses %LOCALAPPDATA%\<app> in that case, falling
    /// back to the application directory for portable installs. Here a
    /// `portable.txt` marker file (or the NANOTORRENT_PORTABLE env var) next
    /// to the executable selects portable mode instead.
    pub fn get_application_data_path(&self) -> PathBuf {
        let app_path = self.get_application_path();

        let portable = std::env::var_os("NANOTORRENT_PORTABLE").is_some()
            || app_path.join("portable.txt").exists()
            || app_path.join("portable").exists();

        if portable {
            return app_path;
        }

        Self::user_data_dir().unwrap_or(app_path)
    }

    /// Per-user data directory, following each platform's own convention.
    ///
    /// Hand-rolled rather than pulling in `directories`: it is three rules, and
    /// the crate would be a dependency carried on every platform to answer a
    /// question each one answers differently anyway.
    fn user_data_dir() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("NanoTorrent"))
        }

        #[cfg(target_os = "macos")]
        {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Library/Application Support/NanoTorrent"))
        }

        #[cfg(not(any(windows, target_os = "macos")))]
        {
            // Lowercased on Linux, where a capitalised directory in ~/.local/share
            // would look out of place next to every other application's.
            //
            // The XDG spec says a relative XDG_DATA_HOME must be IGNORED rather
            // than resolved against the cwd, hence the is_absolute filter - a
            // torrent client that put its database somewhere relative would move
            // it every time it was launched from a different directory.
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .map(|d| d.join("nanotorrent"))
        }
    }

    /// The settings database, `NanoTorrent.sqlite` in the profile folder.
    pub fn get_database_file_path(&self) -> PathBuf {
        self.get_application_data_path().join("NanoTorrent.sqlite")
    }

    /// Path to an existing PicoTorrent settings database, if one is present
    /// (`%LOCALAPPDATA%\PicoTorrent\PicoTorrent.sqlite`). Used by the one-shot
    /// torrent importer.
    pub fn get_picotorrent_db_path(&self) -> Option<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA")?;
        let path = PathBuf::from(local)
            .join("PicoTorrent")
            .join("PicoTorrent.sqlite");
        path.exists().then_some(path)
    }

    /// One-time migration after the PicoTorrent -> NanoTorrent rename: if
    /// the NanoTorrent data folder does not exist yet but a PicoTorrent one
    /// does, copy its settings database and session state over (the old
    /// folder is left untouched).
    pub fn migrate_legacy_data(&self) {
        let Some(local) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };

        let new_dir = self.get_application_data_path();
        let old_dir = PathBuf::from(local).join("PicoTorrent");

        if new_dir.exists() || !old_dir.exists() || new_dir == old_dir {
            return;
        }

        let _ = std::fs::create_dir_all(&new_dir);
        let _ = std::fs::copy(
            old_dir.join("PicoTorrent.sqlite"),
            new_dir.join("NanoTorrent.sqlite"),
        );
        let _ = std::fs::copy(old_dir.join("dht.json"), new_dir.join("dht.json"));
        copy_dir(&old_dir.join("session"), &new_dir.join("session"));
    }

    /// Folder where the librqbit session persists fastresume state. This
    /// replaces the torrent_resume_data table of the original.
    pub fn get_session_state_path(&self) -> PathBuf {
        self.get_application_data_path().join("session")
    }

    /// The log file. Its parent directory is also where a panic backtrace is
    /// written, so a crash leaves something to read.
    pub fn get_log_file_path(&self) -> PathBuf {
        let ts: chrono::DateTime<chrono::Local> = self.startup_time.into();

        self.get_application_data_path()
            .join("logs")
            .join(format!("NanoTorrent.{}.log", ts.format("%Y%m%d%H%M%S")))
    }

    /// The optional `lang/` folder beside the executable.
    ///
    /// Translations are compiled in, so this need not exist - a file dropped
    /// here overrides the built-in copy for that locale, which is the quickest
    /// way to edit one without rebuilding.
    pub fn get_lang_path(&self) -> PathBuf {
        // Look next to the executable first, then in the dev tree.
        let next_to_exe = self.get_application_path().join("lang");
        if next_to_exe.exists() {
            return next_to_exe;
        }

        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lang")
    }

    /// Port of Environment::GetKnownFolderPath(KnownFolder::UserDownloads).
    pub fn get_downloads_path() -> PathBuf {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(profile).join("Downloads");
        }

        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Downloads");
        }

        PathBuf::from(".")
    }
}

/// The MSIX package family name this process is running under, or `None` for
/// an ordinary installer or portable build.
///
/// This is how a Microsoft Store copy tells itself apart from an NSIS one, and
/// it matters because the two are separate installs that can sit on the same
/// machine at once - see `updatechecker::download_url`, which is the only
/// caller that has needed it so far.
///
/// `GetCurrentPackageFamilyName` answers `APPMODEL_ERROR_NO_PACKAGE` when
/// there is no package identity, which is the documented test. Declared by
/// hand rather than by turning on another winapi feature, matching how
/// `core::toast` reaches into shell32.
#[cfg(windows)]
pub fn package_family_name() -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentPackageFamilyName(length: *mut u32, name: *mut u16) -> i32;
    }

    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

    // Two calls, the usual Win32 shape: the first asks how long the name is.
    // A packaged process answers "buffer too small"; anything else (in
    // practice APPMODEL_ERROR_NO_PACKAGE) means there is nothing to ask for.
    let mut len: u32 = 0;
    if unsafe { GetCurrentPackageFamilyName(&mut len, std::ptr::null_mut()) }
        != ERROR_INSUFFICIENT_BUFFER
    {
        return None;
    }

    let mut buf = vec![0u16; len as usize];
    if unsafe { GetCurrentPackageFamilyName(&mut len, buf.as_mut_ptr()) } != 0 {
        return None;
    }

    // `len` comes back as a character count that includes the trailing NUL.
    let chars = (len as usize).saturating_sub(1).min(buf.len());
    (chars > 0).then(|| String::from_utf16_lossy(&buf[..chars]))
}

/// No MSIX anywhere but Windows, so nothing to ask.
#[cfg(not(windows))]
pub fn package_family_name() -> Option<String> {
    None
}

/// Recursively copy a directory, skipping anything that cannot be read.
///
/// Used only by the one-time PicoTorrent data takeover. Best-effort by design:
/// a locked file in someone else's profile must not abort the migration and
/// leave it half-done.
fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    let _ = std::fs::create_dir_all(to);
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        match entry.file_type() {
            Ok(t) if t.is_dir() => copy_dir(&entry.path(), &target),
            Ok(t) if t.is_file() => {
                let _ = std::fs::copy(entry.path(), target);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cargo test` never runs inside an MSIX container, so the answer here is
    /// known. It is worth pinning because the failure mode of the two-call
    /// buffer dance is not a crash: it is `Some(garbage)`, which would send
    /// every installer user to the Microsoft Store instead of the release
    /// they actually came from.
    #[test]
    fn a_plain_build_reports_no_package() {
        assert_eq!(package_family_name(), None);
    }

    /// Guards the per-platform branching in `user_data_dir`. Only the host's
    /// arm is compiled, so CI on each OS is what covers the other two - the
    /// point here is that whichever arm ships is absolute (a relative data
    /// path would relocate the database per working directory) and actually
    /// lands in an app-specific folder rather than the bare data root.
    #[test]
    fn user_data_dir_is_absolute_and_app_specific() {
        let dir = Environment::user_data_dir().expect("no per-user data dir on this platform");
        assert!(dir.is_absolute(), "{dir:?} is not absolute");

        let leaf = dir
            .file_name()
            .expect("no final component")
            .to_string_lossy()
            .into_owned();
        assert!(
            leaf.eq_ignore_ascii_case("nanotorrent"),
            "{dir:?} does not end in an app-specific folder"
        );
    }

    /// Portable mode must win over the per-user directory, otherwise a
    /// portable install silently writes to the profile it was meant to avoid.
    /// Worth pinning because `user_data_dir` is now consulted right below that
    /// branch - reorder the two and portable mode fails silently.
    #[test]
    fn portable_marker_overrides_user_data_dir() {
        // SAFETY: cargo runs tests on multiple threads, so this is only sound
        // because NANOTORRENT_PORTABLE is read nowhere else in the suite - the
        // rest of the code reaches Environment via get_downloads_path, which
        // does not consult it. Anything else that reads it needs a lock here.
        unsafe { std::env::set_var("NANOTORRENT_PORTABLE", "1") };
        let env = Environment::create();
        let data = env.get_application_data_path();
        unsafe { std::env::remove_var("NANOTORRENT_PORTABLE") };

        assert_eq!(data, env.get_application_path());
    }
}

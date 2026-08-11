// Port of src/picotorrent/core/environment.{hpp,cpp}

use std::path::PathBuf;
use std::time::SystemTime;

pub struct Environment {
    startup_time: SystemTime,
}

impl Environment {
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

        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("NanoTorrent");
        }

        app_path
    }

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

    pub fn get_log_file_path(&self) -> PathBuf {
        let ts: chrono::DateTime<chrono::Local> = self.startup_time.into();

        self.get_application_data_path()
            .join("logs")
            .join(format!("NanoTorrent.{}.log", ts.format("%Y%m%d%H%M%S")))
    }

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

    /// Port of Environment::GetCurrentLocale.
    ///
    /// No longer used to pick the startup locale: NanoTorrent always starts in
    /// English (`DEFAULT_LOCALE`) and only follows `locale_name` once the user
    /// has chosen one. Kept because it is a direct port of the original.
    #[allow(dead_code)]
    pub fn get_current_locale() -> String {
        // GetUserDefaultLocaleName equivalent; fall back to en-US.
        std::env::var("LANG")
            .ok()
            .and_then(|l| l.split('.').next().map(|s| s.replace('_', "-")))
            .unwrap_or_else(|| String::from("en-US"))
    }
}

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

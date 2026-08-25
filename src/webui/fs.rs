//! Server-side filesystem browsing for the web interface.
//!
//! A browser cannot show a picker for paths on the machine NanoTorrent runs on,
//! so choosing a download folder from a remote session needs the server to
//! enumerate its own filesystem.
//!
//! # Threat model
//!
//! This is a remote filesystem browser, and `mkdir` below is a remote write.
//! That is the requested behaviour - picking any download location means
//! reaching any directory - so there is no confinement root to escape and
//! `..` is not an attack, just navigation.
//!
//! Authentication is therefore the entire boundary, which is why the server
//! refuses to start without a password and refuses plaintext off loopback.
//! The secondary boundary is the OS: the process runs as the user, so it can
//! only reach what that user could reach anyway.
//!
//! What is still enforced here:
//!
//! - paths must be absolute, so nothing depends on the process working
//!   directory (which differs between a shell launch and a service);
//! - every path is canonicalised before use, so what a client is told it
//!   browsed is what was actually read;
//! - `mkdir` creates exactly one level and refuses to overwrite, so a typo
//!   cannot recursively materialise a tree.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// `None` for directories and for anything whose metadata cannot be read -
    /// a listing must not fail because one entry is unreadable.
    pub size: Option<u64>,
}

#[derive(Serialize)]
pub struct Listing {
    pub path: String,
    /// `None` at a root, so a client knows not to offer "up".
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
}

#[derive(Deserialize)]
pub struct PathRequest {
    pub path: String,
}

/// Top-level starting points: drive letters on Windows, `/` and `$HOME`
/// elsewhere. Also carries the default download directory, since that is the
/// entry a folder picker should open on.
#[derive(Serialize)]
pub struct Roots {
    pub roots: Vec<Entry>,
    pub downloads: String,
    pub separator: char,
}

/// The starting points for the save-path browser: drive letters on Windows,
/// `/` plus the user's home elsewhere.
pub fn roots() -> Roots {
    let mut roots = Vec::new();

    #[cfg(windows)]
    {
        // GetLogicalDrives returns a bitmask, bit 0 = A:. Probing with
        // is_dir() instead would spin up empty removable drives.
        let mask = unsafe { winapi::um::fileapi::GetLogicalDrives() };
        for bit in 0..26u32 {
            if mask & (1 << bit) != 0 {
                let letter = (b'A' + bit as u8) as char;
                roots.push(Entry {
                    name: format!("{letter}:"),
                    path: format!("{letter}:\\"),
                    is_dir: true,
                    size: None,
                });
            }
        }
    }

    #[cfg(not(windows))]
    {
        roots.push(Entry {
            name: String::from("/"),
            path: String::from("/"),
            is_dir: true,
            size: None,
        });
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            if home.is_dir() {
                roots.push(Entry {
                    name: String::from("Home"),
                    path: home.to_string_lossy().into_owned(),
                    is_dir: true,
                    size: None,
                });
            }
        }
    }

    Roots {
        roots,
        downloads: crate::core::environment::Environment::get_downloads_path()
            .to_string_lossy()
            .into_owned(),
        separator: std::path::MAIN_SEPARATOR,
    }
}

/// Resolve a client-supplied path to a real, absolute one.
///
/// Rejects relative paths outright rather than resolving them against the
/// working directory: the same request would then mean different things
/// depending on how the process was launched.
fn resolve(path: &str) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    if !raw.is_absolute() {
        return Err(String::from("path must be absolute"));
    }
    // Canonicalise so `..`, symlinks and mixed separators collapse to the one
    // real location, and the response reports where it actually looked.
    std::fs::canonicalize(raw).map_err(|e| format!("{path}: {e}"))
}

/// On Windows, canonicalize returns a `\\?\C:\...` extended-length path. It is
/// valid, but it is not what anyone expects to see in a text box, and pasting
/// it back into other tools often fails.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        // Only strip the plain disk-designator form; \\?\UNC\... must keep it.
        if let Some(stripped) = text.strip_prefix(r"\\?\")
            && stripped.len() >= 2
            && stripped.as_bytes()[1] == b':'
        {
            return stripped.to_string();
        }
    }
    text.into_owned()
}

/// The entries directly under `path`, plus its parent so a caller can walk
/// back up.
///
/// Directories first, then files case-insensitively by name - the order every
/// file manager uses, so a picker does not have to re-sort. Files carry their
/// size; directories do not. An entry whose metadata cannot be read is still
/// listed rather than failing the whole listing.
pub fn list(path: &str) -> Result<Listing, String> {
    let dir = resolve(path)?;
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", display_path(&dir)));
    }

    let read = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", display_path(&dir)))?;

    let mut entries: Vec<Entry> = read
        .flatten()
        .map(|e| {
            // Two fallible reads per entry, and neither is worth failing the
            // whole listing over: a locked file or a broken symlink is normal.
            let meta = e.metadata().ok();
            let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
            Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path: display_path(&e.path()),
                is_dir,
                size: meta.filter(|m| m.is_file()).map(|m| m.len()),
            }
        })
        .collect();

    // Directories first, then case-insensitively by name - the order every
    // file manager uses, so a picker does not have to re-sort.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(Listing {
        path: display_path(&dir),
        parent: dir.parent().map(display_path),
        entries,
    })
}

/// Create one directory. The parent must already exist.
///
/// Deliberately not `create_dir_all`: a client sending a mistyped or
/// half-substituted path would otherwise silently bring a whole tree of
/// directories into being, and there would be no way to tell that from success.
pub fn mkdir(path: &str) -> Result<Listing, String> {
    let raw = Path::new(path);
    if !raw.is_absolute() {
        return Err(String::from("path must be absolute"));
    }
    // The new directory does not exist yet, so canonicalise the PARENT and
    // rebuild - canonicalize on the full path would just fail.
    let parent = raw
        .parent()
        .ok_or_else(|| String::from("path has no parent directory"))?;
    let name = raw
        .file_name()
        .ok_or_else(|| String::from("path has no final component"))?;

    let parent = resolve(&parent.to_string_lossy())?;
    let target = parent.join(name);

    if target.exists() {
        return Err(format!("{} already exists", display_path(&target)));
    }

    std::fs::create_dir(&target).map_err(|e| format!("{}: {e}", display_path(&target)))?;
    list(&display_path(&target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nt-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn relative_paths_are_rejected() {
        // Otherwise the same request means different directories depending on
        // how the process was launched.
        assert!(list("some/relative").is_err());
        assert!(list("").is_err());
        assert!(mkdir("relative/new").is_err());
    }

    #[test]
    fn lists_directories_first_then_by_name() {
        let dir = temp("sort");
        std::fs::create_dir_all(dir.join("zebra")).unwrap();
        std::fs::create_dir_all(dir.join("Apple")).unwrap();
        std::fs::write(dir.join("banana.txt"), b"x").unwrap();
        std::fs::write(dir.join("Cherry.txt"), b"xx").unwrap();

        let listing = list(&dir.to_string_lossy()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "zebra", "banana.txt", "Cherry.txt"]);

        let cherry = listing.entries.iter().find(|e| e.name == "Cherry.txt").unwrap();
        assert_eq!(cherry.size, Some(2));
        assert!(listing.entries.iter().find(|e| e.name == "Apple").unwrap().size.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traversal_resolves_rather_than_escaping() {
        // `..` is navigation here, not an attack - but the reported path must
        // be the resolved one, never the literal string the client sent.
        let dir = temp("traverse");
        std::fs::create_dir_all(dir.join("child")).unwrap();

        let sneaky = dir.join("child").join("..").to_string_lossy().into_owned();
        let listing = list(&sneaky).unwrap();
        assert!(!listing.path.contains(".."), "unresolved path leaked: {}", listing.path);
        assert_eq!(listing.path, display_path(&std::fs::canonicalize(&dir).unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mkdir_creates_one_level_and_refuses_to_clobber() {
        let dir = temp("mkdir");
        let target = dir.join("new-folder");

        assert!(mkdir(&target.to_string_lossy()).is_ok());
        assert!(target.is_dir());

        // Second attempt must fail rather than silently succeed.
        assert!(mkdir(&target.to_string_lossy()).is_err());

        // A missing intermediate must NOT be conjured up.
        let deep = dir.join("missing").join("deeper");
        assert!(mkdir(&deep.to_string_lossy()).is_err());
        assert!(!dir.join("missing").exists(), "created an intermediate directory");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let dir = temp("notdir");
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(list(&file.to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roots_are_absolute_and_listable() {
        let r = roots();
        assert!(!r.roots.is_empty(), "no filesystem roots reported");
        for root in &r.roots {
            assert!(
                Path::new(&root.path).is_absolute(),
                "root {} is not absolute",
                root.path
            );
        }
        assert!(Path::new(&r.downloads).is_absolute());
    }

    #[cfg(windows)]
    #[test]
    fn extended_length_prefix_is_stripped() {
        // canonicalize hands back \\?\C:\... which is valid but unpasteable.
        let dir = temp("prefix");
        let listing = list(&dir.to_string_lossy()).unwrap();
        assert!(
            !listing.path.starts_with(r"\\?\"),
            "extended-length prefix leaked to the client: {}",
            listing.path
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

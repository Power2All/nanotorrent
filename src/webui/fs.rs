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

/// A directory under the one listed.
#[derive(Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
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

/// Where a folder picker should open: the default download directory.
#[derive(Serialize)]
pub struct Roots {
    pub downloads: String,
}

pub fn roots() -> Roots {
    Roots {
        downloads: crate::core::environment::Environment::get_downloads_path()
            .to_string_lossy()
            .into_owned(),
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

/// The directories directly under `path`, plus its parent so a caller can
/// walk back up.
///
/// Sorted case-insensitively by name, so a picker does not have to re-sort.
/// Files are left out: this picks somewhere to put them. An entry whose
/// metadata cannot be read is skipped rather than failing the whole listing.
pub fn list(path: &str) -> Result<Listing, String> {
    let dir = resolve(path)?;
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", display_path(&dir)));
    }

    let read = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", display_path(&dir)))?;

    // Neither fallible read is worth failing the whole listing over: a
    // locked file or a broken symlink is normal.
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter(|e| e.metadata().is_ok_and(|m| m.is_dir()))
        .map(|e| Entry {
            name: e.file_name().to_string_lossy().into_owned(),
            path: display_path(&e.path()),
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());

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

    std::fs::create_dir(&target).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => format!("{} already exists", display_path(&target)),
        _ => format!("{}: {e}", display_path(&target)),
    })?;
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
    fn lists_only_directories_by_name() {
        let dir = temp("sort");
        std::fs::create_dir_all(dir.join("zebra")).unwrap();
        std::fs::create_dir_all(dir.join("Apple")).unwrap();
        std::fs::write(dir.join("banana.txt"), b"x").unwrap();
        std::fs::write(dir.join("Cherry.txt"), b"xx").unwrap();

        let listing = list(&dir.to_string_lossy()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "zebra"]);

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
    fn the_downloads_root_is_absolute() {
        assert!(Path::new(&roots().downloads).is_absolute());
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

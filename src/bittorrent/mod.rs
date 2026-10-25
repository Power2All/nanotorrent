pub mod limits;
pub mod manager;
pub mod metainfo;
pub mod mse;
pub mod session;
pub mod torrent_create;
pub mod torrentstatus;
pub mod v2;
pub mod webseed;

/// A fresh, empty folder for one test, unique to this process and `name`.
///
/// Every test in `bittorrent` shares the namespace and they run in parallel,
/// so each caller prefixes its module (`v2-`, `tc-`, ...).
#[cfg(test)]
pub fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nt-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

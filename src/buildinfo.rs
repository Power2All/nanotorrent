// Port of src/picotorrent/buildinfo.{hpp,cpp.in}

/// The crate version, which is the single source of truth for the version
/// everywhere - the About box, the update check and the installer names.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The User-Agent sent to trackers and to GitHub for the update check.
pub fn user_agent() -> String {
    format!("NanoTorrent/{}", version())
}

/// Compile timestamp, stamped by build.rs.
pub fn build_stamp() -> &'static str {
    env!("PT_BUILD_STAMP")
}

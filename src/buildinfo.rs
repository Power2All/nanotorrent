// Port of src/picotorrent/buildinfo.{hpp,cpp.in}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn user_agent() -> String {
    format!("NanoTorrent/{}", version())
}

/// Compile timestamp, stamped by build.rs.
pub fn build_stamp() -> &'static str {
    env!("PT_BUILD_STAMP")
}

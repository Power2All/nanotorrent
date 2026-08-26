// Port of src/picotorrent/buildinfo.{hpp,cpp.in}

/// The crate version, which is the single source of truth for the version
/// everywhere - the About box, the update check and the installer names.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `NanoTorrent/x.y.z` - the User-Agent for the update check, and the
/// `created by` field stamped into torrents this build creates.
///
/// NOT what trackers see: peer and tracker traffic goes through librqbit,
/// which sets no User-Agent of its own. The peer id is what identifies this
/// client on the wire - see `build_session_options`.
pub fn user_agent() -> String {
    format!("NanoTorrent/{}", version())
}

/// How this client names itself to peers: the BEP 10 extended-handshake `v`
/// string, e.g. `NanoTorrent 0.2.0`.
///
/// Space-separated, not the `Name/version` of a User-Agent - that is the
/// convention peers display ("qBittorrent v4.6.0", "rqbit 8.1.1"), and this
/// value goes straight into other clients' Client column.
///
/// The peer id carries the same identity in its own encoding (`-NT0200-`, see
/// `build_session_options`); the two must agree on the version, which they do
/// because both derive from `CARGO_PKG_VERSION`.
pub fn client_id() -> String {
    format!("NanoTorrent {}", version())
}

/// Compile timestamp, stamped by build.rs.
pub fn build_stamp() -> &'static str {
    env!("PT_BUILD_STAMP")
}

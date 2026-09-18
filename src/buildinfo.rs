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
/// convention peers display ("rqbit 8.1.1"), and this
/// value goes straight into other clients' Client column.
///
/// The peer id carries the same identity in its own encoding (`-NT0200-`, see
/// `build_session_options`); the two must agree on the version, which they do
/// because both derive from `CARGO_PKG_VERSION`.
pub fn client_id() -> String {
    format!("NanoTorrent {}", version())
}

/// When the running executable was last written, as `YYYY-MM-DD HH:MM UTC`.
///
/// Read from the file at run time rather than compiled in. A timestamp baked
/// into the binary makes every build of the same source a different file, so
/// nobody - not even whoever built it - can check a download against a hash
/// they worked out for themselves.
///
/// The trade is worth naming: this is a modification time, so it says when the
/// binary arrived rather than when it was compiled. A copy that preserves
/// timestamps keeps them identical; one that does not moves the date to the
/// copy. It still answers what the title bar is actually asking - "is this the
/// build I just made?" - which is why the stamp is shown at all, the app being
/// single-instance and therefore able to show an OLD instance's window when a
/// new exe is launched.
///
/// Cached: the title bar, the log line and the About box each ask for it, and
/// none of them should cost a stat.
pub fn build_stamp() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| {
        std::env::current_exe()
            .and_then(|exe| exe.metadata())
            .and_then(|meta| meta.modified())
            .map(|when| {
                chrono::DateTime::<chrono::Utc>::from(when)
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            })
            // Every one of these can fail on a locked-down filesystem, and none
            // of them is worth refusing to start over.
            .unwrap_or_else(|_| String::from("unknown"))
    })
}

#[cfg(test)]
mod tests {
    /// The stamp has to be a date, not the "unknown" fallback.
    ///
    /// Every step of reading it can fail, and each failure is swallowed so the
    /// app still starts - which is right, but it means a stamp that silently
    /// stopped working would look exactly like one that never worked. The test
    /// binary is an executable with a modification time like any other, so the
    /// same path that serves the title bar is the one exercised here.
    #[test]
    fn the_build_stamp_is_a_real_date() {
        let stamp = super::build_stamp();
        assert_ne!(stamp, "unknown", "could not read the executable's own date");

        // YYYY-MM-DD HH:MM UTC
        let (date, rest) = stamp.split_once(' ').expect("a space after the date");
        let (time, zone) = rest.split_once(' ').expect("a space before the zone");
        assert_eq!(zone, "UTC", "{stamp:?}");
        assert_eq!(date.len(), 10, "{stamp:?}");
        assert_eq!(time.len(), 5, "{stamp:?}");
        assert!(
            date.split('-').all(|part| part.chars().all(|c| c.is_ascii_digit())),
            "{stamp:?}"
        );
        assert!(
            time.split(':').all(|part| part.chars().all(|c| c.is_ascii_digit())),
            "{stamp:?}"
        );
    }
}

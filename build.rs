/// Application manifest. The Common-Controls v6 dependency is REQUIRED (see the
/// note in main() - NWG imports comctl32 v6-only `GetWindowSubclass`).
#[cfg(windows)]
const APP_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*" />
    </dependentAssembly>
  </dependency>
</assembly>
"#;

fn main() {
    verify_librqbit_patches();

    // Embed the app icon into the .exe as a Win32 resource so Explorer and the
    // taskbar show it for the executable file itself.
    //
    // We must ALSO embed the application manifest here: winresource replaces the
    // exe's resource section, which would otherwise drop native-windows-gui's
    // own manifest. Without the Common-Controls v6 dependency the loader binds
    // to comctl32 v5.82 (system32), which lacks `GetWindowSubclass` that NWG
    // imports -> "entry point GetWindowSubclass could not be located" at launch.
    // So the icon and manifest travel together in one resource.
    println!("cargo:rerun-if-changed=res/app.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("res/app.ico");
        res.set_manifest(APP_MANIFEST);
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed exe icon/manifest: {e}");
        }
    }

    // Build timestamp for the window title, so it's always obvious which
    // build is actually running (the app is single-instance: launching a
    // new exe while an old instance runs shows the OLD instance's window).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Simple civil-date conversion (UTC) without pulling build dependencies.
    let days = now / 86400;
    let secs = now % 86400;
    let (hh, mm) = (secs / 3600, (secs % 3600) / 60);

    // Days-to-date (proleptic Gregorian), based on Howard Hinnant's algorithm.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    println!(
        "cargo:rustc-env=PT_BUILD_STAMP={:04}-{:02}-{:02} {:02}:{:02} UTC",
        y, m, d, hh, mm
    );
}

/// Guard: the vendored librqbit must carry the NanoTorrent visibility
/// patches (piece bar + seed counts depend on them). Verifying here turns
/// the otherwise cryptic missing-method compile errors into a clear message.
/// Deliberately a check, not a build-time rewrite: mutating dependency
/// sources during compilation breaks cargo's caching and reproducibility -
/// re-vendoring is an explicit step (tools/update-librqbit.ps1).
fn verify_librqbit_patches() {
    let checks = [
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "pub fn with_chunk_tracker",
            "patches/0001-expose-chunk-tracker.patch",
        ),
        (
            "vendor/librqbit/src/torrent_state/live/mod.rs",
            "pub fn per_peer_have_pieces",
            "patches/0002-per-peer-have-pieces.patch",
        ),
        (
            "vendor/librqbit/src/stream_connect.rs",
            "pub trait StreamTransform",
            "patches/0003-stream-transform-seam.patch",
        ),
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "pub disable_pex",
            "patches/0004-pex-toggle.patch",
        ),
        (
            "vendor/librqbit/src/stream_connect.rs",
            "pub trait IncomingStreamTransform",
            "patches/0005-incoming-stream-transform-seam.patch",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "pub proxy_trackers",
            "patches/0006-proxy-scope.patch",
        ),
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "pub anonymize",
            "patches/0007-anonymous-mode.patch",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "tracker_stats_snapshot",
            "patches/0008-tracker-stats.patch (librqbit side)",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "tracker_tiers_snapshot",
            "patches/0008-tracker-stats.patch (tracker tiers, librqbit side)",
        ),
        (
            "vendor/librqbit-tracker-comms/src/tracker_comms.rs",
            "pub struct TrackerStat",
            "patches/0008-tracker-stats.patch (tracker-comms side; this crate is \
             separately vendored - see PATCHES.md)",
        ),
    ];

    for (file, marker, patch) in checks {
        println!("cargo:rerun-if-changed={file}");
        let content = std::fs::read_to_string(file).unwrap_or_default();
        if !content.contains(marker) {
            panic!(
                "\n\nvendored librqbit is missing the visibility patch `{patch}`\n\
                 ({file} does not contain `{marker}`).\n\n\
                 Run: powershell -File tools/update-librqbit.ps1 -Version <current>\n\
                 or apply the patches manually - see vendor/librqbit/PATCHES.md.\n"
            );
        }
    }
}

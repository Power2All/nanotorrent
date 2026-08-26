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
    embed_language_files();
    embed_flag_images();
    compile_slint_ui();

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
        // Without these the exe's properties sheet shows the lowercase crate
        // name for both the product and the description, and no copyright.
        // FileVersion / ProductVersion come from CARGO_PKG_VERSION already.
        res.set("ProductName", "NanoTorrent");
        res.set("FileDescription", "NanoTorrent");
        res.set("LegalCopyright", "Power2All");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed exe icon/manifest: {e}");
        }
    }

    // Build timestamp for the window title, so it's always obvious which
    // build is actually running (the app is single-instance: launching a
    // new exe while an old instance runs shows the OLD instance's window).
    //
    // This MUST re-run when any source file changes. Declaring the asset paths
    // above narrows cargo's rerun set to exactly those, which froze the stamp
    // across every code-only rebuild - the title then reports an older build
    // than the one actually running, defeating the whole point of having it.
    println!("cargo:rerun-if-changed=src");
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

/// Compiles the Slint UI into OUT_DIR when the `ui-slint` feature is on.
///
/// Gated on the feature so a headless build neither pulls in slint-build nor
/// needs the .slint sources to parse.
fn compile_slint_ui() {
    #[cfg(feature = "ui-slint")]
    {
        println!("cargo:rerun-if-changed=src/ui_slint");
        slint_build::compile("src/ui_slint/app.slint").expect("failed to compile the Slint UI");
    }
}

/// Generates the `include_str!` table for `lang/*.json` so every translation
/// ships inside the .exe. Generated rather than hand-written so adding a
/// language file is all it takes - no list to forget to update.
fn embed_language_files() {
    println!("cargo:rerun-if-changed=lang");

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lang");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    let mut out = String::from("pub static EMBEDDED_LANGS: &[(&str, &str)] = &[\n");
    for path in &files {
        let locale = path.file_stem().unwrap().to_str().unwrap();
        // Forward slashes: include_str! takes them on Windows and it keeps the
        // generated file free of backslash-escaping.
        let full = path.to_str().unwrap().replace('\\', "/");
        out.push_str(&format!("    (\"{locale}\", include_str!(\"{full}\")),\n"));
    }
    out.push_str("];\n");

    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("lang_table.rs");
    std::fs::write(&dest, out).expect("failed to write lang table");
}

/// Generates the `include_bytes!` table for `res/flags/*.png` (32x24 country
/// flags, keyed by ISO 3166-1 alpha-2). Same generated-table approach as the
/// language files, for the same reason: nothing to hand-maintain.
fn embed_flag_images() {
    println!("cargo:rerun-if-changed=res/flags");

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("res/flags");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .collect();
    files.sort();

    let mut out = String::from("pub static FLAG_PNGS: &[(&str, &[u8])] = &[\n");
    for path in &files {
        let code = path.file_stem().unwrap().to_str().unwrap();
        let full = path.to_str().unwrap().replace('\\', "/");
        out.push_str(&format!("    (\"{code}\", include_bytes!(\"{full}\")),\n"));
    }
    out.push_str("];\n");

    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("flag_table.rs");
    std::fs::write(&dest, out).expect("failed to write flag table");
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
            "vendor/librqbit/src/storage/filesystem/fs.rs",
            "while !buf.is_empty()",
            "patches/0010-windows-pread-pwrite-exact.patch",
        ),
        (
            "vendor/librqbit/src/lib.rs",
            "pub static CLIENT_NAME",
            "patches/0011-client-name.patch",
        ),
        (
            "vendor/librqbit/src/session_persistence/json.rs",
            "tmp.sync_all()",
            "patches/0012-fsync-session-index.patch",
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

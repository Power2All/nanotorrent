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
    // Every translation ships inside the .exe, keyed by locale.
    embed_table(
        "lang",
        "json",
        "pub static EMBEDDED_LANGS: &[(&str, &str)]",
        "include_str",
        "lang_table.rs",
    );
    // 32x24 country flags, keyed by ISO 3166-1 alpha-2.
    embed_table(
        "res/flags",
        "png",
        "pub static FLAG_PNGS: &[(&str, &[u8])]",
        "include_bytes",
        "flag_table.rs",
    );
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

    // No build timestamp is stamped in here any more, and `rerun-if-changed=src`
    // went with it - that directive existed only to keep the stamp fresh, and
    // nothing else in this file reads `src/` (the Slint compile declares
    // `src/ui_slint` for itself).
    //
    // A timestamp compiled into the binary makes two builds of identical source
    // two different files, which costs anyone the ability to check a download
    // against a hash they computed themselves. `buildinfo::build_stamp` reads
    // the executable's own modification time at run time instead.
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

/// Generates a table of every `*.EXT` file in `dir`, keyed by file stem, as
/// `DECL = &[("stem", INCLUDE!("path")), ...];` in `OUT_DIR/out_file`.
/// Generated rather than hand-written so adding a file is all it takes - no
/// list to forget to update.
fn embed_table(dir: &str, ext: &str, decl: &str, include: &str, out_file: &str) {
    println!("cargo:rerun-if-changed={dir}");

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut files: Vec<_> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect();
    files.sort();

    let mut out = format!("{decl} = &[\n");
    for path in &files {
        let key = path.file_stem().unwrap().to_str().unwrap();
        // Forward slashes: include_str! takes them on Windows and it keeps the
        // generated file free of backslash-escaping.
        let full = path.to_str().unwrap().replace('\\', "/");
        out.push_str(&format!("    (\"{key}\", {include}!(\"{full}\")),\n"));
    }
    out.push_str("];\n");

    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join(out_file);
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("failed to write {out_file}: {e}"));
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
            "patches/0001-engine-visibility.patch (chunk tracker)",
        ),
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "pub fn set_file_priorities",
            "patches/0017-file-priorities.patch",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "pub replace_trackers: bool",
            "patches/0018-custom-tracker-tier.patch",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "pub output_folder_subfolder: bool",
            "patches/0020-output-folder-subfolder.patch",
        ),
        (
            "vendor/librqbit/src/storage/mod.rs",
            "fn release_write_access",
            "patches/0021-release-write-handles.patch (the trait method)",
        ),
        (
            "vendor/librqbit/Cargo.toml",
            "librqbit-dualstack-sockets/axum",
            "patches/0023-axum-only-under-http-api.patch (without it axum, 
             axum-extra and four more crates are compiled into a build 
             that never serves an axum route)",
        ),
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "fn safe_relative_path",
            "patches/0022-refuse-prefixed-path-components.patch (SECURITY, \n             with 0015 - without it `relative_filename` may carry a Windows \n             drive prefix, and every reader outside the storage layer joins \n             it unguarded)",
        ),
        (
            "vendor/librqbit/src/storage/filesystem/opened_file.rs",
            "pub fn lock_for_write",
            "patches/0021-release-write-handles.patch (without it a finished              download stays unopenable by other programs until NanoTorrent exits)",
        ),
        (
            "vendor/librqbit/src/session_persistence/mod.rs",
            "trackers: Some(restored)",
            "patches/0018-custom-tracker-tier.patch (restore)",
        ),
        (
            "vendor/librqbit/src/torrent_state/live/mod.rs",
            "pub fn per_peer_have_pieces",
            "patches/0001-engine-visibility.patch (per-peer have-pieces)",
        ),
        (
            "vendor/librqbit/src/torrent_state/live/mod.rs",
            "pub fn inflight_piece_indices",
            "patches/0025-inflight-piece-indices.patch (the public accessor)",
        ),
        (
            "vendor/librqbit/src/piece_tracker.rs",
            "pub fn inflight_pieces",
            "patches/0025-inflight-piece-indices.patch (the iterator it reads)",
        ),
        (
            "vendor/librqbit/src/stream_connect.rs",
            "pub trait StreamTransform",
            "patches/0002-stream-transform-seams.patch (outgoing half)",
        ),
        (
            "vendor/librqbit/src/stream_connect.rs",
            "pub trait IncomingStreamTransform",
            "patches/0002-stream-transform-seams.patch (incoming half)",
        ),
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "pub disable_pex",
            "patches/0003-per-torrent-toggles.patch (PeX)",
        ),
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "pub anonymize",
            "patches/0003-per-torrent-toggles.patch (anonymous mode)",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "pub proxy_trackers",
            "patches/0004-proxy-scope.patch",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "tracker_stats_snapshot",
            "patches/0005-tracker-stats.patch (announce stats)",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "tracker_tiers_snapshot",
            "patches/0005-tracker-stats.patch (announce tiers)",
        ),
        (
            "vendor/librqbit-tracker-comms/src/tracker_comms.rs",
            "pub fn reconcile_tiers",
            "patches/0024-tracker-tier-failover-comms.patch (tier reconciliation)",
        ),
        (
            "vendor/librqbit-tracker-comms/src/tracker_comms.rs",
            "async fn run_tier",
            "patches/0024-tracker-tier-failover-comms.patch (the tier runner)",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "tracker_comms::reconcile_tiers",
            "patches/0024-tracker-tier-failover.patch",
        ),
        (
            "vendor/librqbit-tracker-comms/src/tracker_comms.rs",
            "pub struct TrackerStat",
            "patches/0005-tracker-stats-comms.patch (the OTHER crate - see              PATCHES.md)",
        ),
        (
            "vendor/librqbit/src/session_persistence/json.rs",
            "tmp.sync_all()",
            "patches/0006-session-persistence.patch (fsync before rename)",
        ),
        (
            "vendor/librqbit/src/session_persistence/json.rs",
            "next_id: std::sync::atomic::AtomicUsize",
            "patches/0006-session-persistence.patch (next_id must reserve, not              report - without it a batch add collapses into one torrent)",
        ),
        (
            "vendor/librqbit/src/torrent_state/live/mod.rs",
            "(**g).try_flush_bitv",
            "patches/0007-flush-bitfield-on-pause.patch (without it an unclean              exit loses up to 16 MB of verified pieces)",
        ),
        (
            "vendor/librqbit/src/piece_verify.rs",
            "pub trait PieceVerifier",
            "patches/0008-bittorrent-v2.patch (the trait; this patch              ADDS a file, so a dropped patch leaves it missing entirely)",
        ),
        (
            "vendor/librqbit/src/file_ops.rs",
            "fn piece_hasher",
            "patches/0008-bittorrent-v2.patch (the call sites -              without them BitTorrent v2 pieces are checked with SHA-1 and              every one of them fails)",
        ),
        (
            "vendor/librqbit/src/torrent_state/mod.rs",
            "pub secondary_info_hash",
            "patches/0009-hybrid-dual-swarm.patch (without it a hybrid \n             only ever joins its v1 swarm)",
        ),
        (
            "vendor/librqbit/src/piece_verify.rs",
            "pub trait MetadataInterceptor",
            "patches/0008-bittorrent-v2.patch (without it a v2 magnet \n             has its info dict rejected as a bad SHA-1 and never even \n             reaches the piece layers)",
        ),
        (
            "vendor/librqbit-peer-protocol/src/lib.rs",
            "pub struct HashRequest",
            "patches/0008-bittorrent-v2-peerproto.patch (a THIRD vendored \n             crate - see PATCHES.md)",
        ),
        (
            "vendor/librqbit-peer-protocol/src/lib.rs",
            "pub fn supports_v2",
            "patches/0008-bittorrent-v2-peerproto.patch (the v2 \
             handshake bit)",
        ),
        (
            "vendor/librqbit-peer-protocol/src/lib.rs",
            "pub fn supports_fast",
            "patches/0010-fast-extension-peerproto.patch (BEP 6 messages)",
        ),
        (
            "vendor/librqbit/src/torrent_state/live/mod.rs",
            "fn on_have_all_or_none",
            "patches/0010-fast-extension.patch (without it we \
             advertise the fast extension and then ignore its messages)",
        ),
        (
            "vendor/librqbit/src/torrent_state/live/mod.rs",
            "handshake.upload_only = Some(1)",
            "patches/0012-upload-only.patch (BEP 21)",
        ),
        (
            "vendor/librqbit-dualstack-sockets/src/socket.rs",
            "SIO_UDP_CONNRESET",
            "patches/0013-windows-udp-connreset-sockets.patch (a FIFTH vendored \n             crate; without it the DHT dies on Windows within a second of \n             starting and magnets never resolve)",
        ),
        (
            "vendor/librqbit/src/storage/filesystem/fs.rs",
            "fn safe_join",
            "patches/0015-torrent-path-escape.patch (SECURITY - without it a \n             torrent whose filename carries a drive prefix, e.g. \n             \"C:evil.txt\", escapes the save folder entirely on Windows)",
        ),
        (
            "vendor/librqbit-dualstack-sockets/src/bind_device.rs",
            "pub fn bind_ip",
            "patches/0014-windows-bind-interface-sockets.patch (without it 
             bind-to-interface is a Linux/macOS-only feature and any 
             Windows user who sets one cannot start the app at all)",
        ),
        (
            "vendor/librqbit/src/torrent_state/paused.rs",
            "storage_deferred",
            "patches/0016-defer-paused-storage.patch (without it every restored \n             torrent re-creates its files on startup, and one whose data has \n             been deleted or moved fails its check against them and loses \n             its progress)",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "pub async fn add_synthetic_peer",
            "patches/0011-synthetic-peer.patch (WebSeed hangs off this)",
        ),
        (
            "vendor/librqbit/src/torrent_state/live/mod.rs",
            "pub fn peer_counts_by_source",
            "patches/0019-swarm-and-seeding.patch (peer source attribution - 
             without it the Trackers tab cannot say which source found 
             which peers)",
        ),
        (
            "vendor/librqbit/src/piece_verify.rs",
            "pub trait HashProvider",
            "patches/0019-swarm-and-seeding.patch (BEP 52 seeding - without it 
             an incoming `hash request` is ignored rather than answered, 
             and nobody can bootstrap a v2 magnet from us)",
        ),
        (
            "vendor/librqbit/src/piece_tracker.rs",
            "pub fn pick_rarest",
            "patches/0019-swarm-and-seeding.patch (rarest-first - without it 
             piece selection silently reverts to first-come order, which 
             no test would catch because downloads still work)",
        ),
        (
            "vendor/librqbit/src/session.rs",
            "pub override_info_hash",
            "patches/0008-bittorrent-v2.patch (without it a v2-only              torrent announces the hash of its synthetic model, which no peer              or tracker has ever heard of)",
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

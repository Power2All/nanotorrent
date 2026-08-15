# NanoTorrent

NanoTorrent is a tiny, hackable BitTorrent client for Windows — a Rust 2024
port of [PicoTorrent](https://github.com/picotorrent/picotorrent). It keeps the
original's application structure, settings database and behaviour while
replacing the C++ / Rasterbar-libtorrent / wxWidgets stack with pure-Rust
building blocks.

Site: <https://www.nanotorrent.org>

> This port is built predominantly with AI assistance under human review — see
> [AI-DECLARATION.md](AI-DECLARATION.md).

| Aspect                | PicoTorrent (C++)               | NanoTorrent (Rust)                                   |
| --------------------- | ------------------------------- | ---------------------------------------------------- |
| BitTorrent engine     | Rasterbar-libtorrent            | [librqbit](https://crates.io/crates/librqbit) (vendored + patched) |
| GUI                   | wxWidgets (native Win32)        | native-windows-gui (native Win32 common controls), dark mode |
| Settings storage      | SQLite (`PicoTorrent.sqlite`)   | Same schema (`NanoTorrent.sqlite`)                   |
| Resume data           | libtorrent resume blobs in DB   | librqbit session persistence (JSON + `.bitv` mmap)   |
| Translations          | `lang/*.json` embedded in a DB  | Same `lang/*.json` compiled into the exe             |
| Single instance / IPC | Win32 mutex + `WM_COPYDATA`     | Loopback TCP on port 37549                           |
| Notifications         | tray balloons                   | Windows 11 WinRT toasts                              |
| Logging               | boost::log to file              | `tracing` to file                                    |

On first run NanoTorrent does a one-time copy of an existing
`%LOCALAPPDATA%\PicoTorrent` data folder (settings + session state) into
`%LOCALAPPDATA%\NanoTorrent`, leaving the original untouched.

## Building

Requires Rust 1.85+ (edition 2024) and the MSVC toolchain Rust already uses on
Windows. No C++ dependencies.

```
cargo build --release
```

The binary is `target/release/nanotorrent.exe` and ships standalone: every
`lang/*.json` file is compiled in by `build.rs`, so no folder needs to travel
with it. A `lang/` folder next to the executable still takes precedence per
locale, which is the quickest way to edit a translation without rebuilding.

Country flags for the peers list live in `res/flags` (252 public-domain 32x24
PNGs from flagpedia.net — see `res/flags/SOURCE.md`) and are embedded the same
way; refresh them with `tools/update-flags.ps1`.

The engine is vendored: `vendor/librqbit` (and `vendor/librqbit-tracker-comms`)
are the published crates.io sources plus a small stack of mostly
visibility-only patches, wired in via `[patch.crates-io]`. See
`vendor/librqbit/PATCHES.md`; `build.rs` verifies the patches are present and
fails with instructions if a re-vendor dropped one. Re-vendor with
`tools/update-librqbit.ps1`.

## What's included

- **Session** — DHT (persisted routing table), UDP trackers, listen port,
  `-NT-` Azureus-style (or random, in anonymous mode) peer id, up/down rate
  limits, SOCKS proxy with per-scope toggles (peers / trackers / hostnames),
  fast-resume across restarts. Settings are applied **live** — Preferences ▸ OK
  rebuilds the session, no restart needed.
- **MSE / PE encryption** — both outgoing and incoming, RC4, require-encryption
  toggles (`src/bittorrent/mse.rs`, injected through a vendored transform seam).
- **libtorrent tuning knobs** — every setting PicoTorrent exposed that can
  function against librqbit: active-download/seed/overall limits, pause on low
  disk space, PeX toggle, anonymous mode, proxy scoping. Ones with no librqbit
  mechanism are documented as no-ops.
- **Settings database** — the exact original migration SQL (incl. the custom
  `get_known_folder_path()` / `get_user_default_ui_language()` SQLite
  functions); a pre-existing PicoTorrent DB migrates cleanly.
- **Main window** — torrent list with all 16 columns, a **rendered progress
  bar** in the Progress column, click-to-sort, multi-select, context menu
  (pause/resume, remove with/without files, force recheck, move storage,
  labels, copy info hash / magnet, open in Explorer). Paused torrents blank
  their live columns.
- **Details tabs** — Overview (with a piece-availability bar), Files (per-file
  include toggles), Peers (with GeoIP country **and its flag**), and
  **Trackers** grouped into
  announce tiers with per-tracker seeds/leeches/fails/next-announce plus
  DHT/LSD/PeX source rows.
- **Add flows** — Add torrent (parsed file list, save path, label, start
  toggle); Add magnet, which **fetches the metadata first** and then shows the
  same dialog with the real file list.
- **Torrent creation** — BitTorrent v1, v2 and hybrid (BEP 52), with tracker /
  comment / private options.
- **Notifications** — real Windows 11 toasts on download-complete, under a
  registered AppUserModelID; a notification-area (tray) icon with close-to-tray
  prompt, shown for both the window's close button and File ▸ Exit.
- **File associations** — register NanoTorrent for `.torrent` files and
  `magnet:` links from Preferences.
- **GeoIP & IP filter** — DB-IP country lookup for peers; eMule/PeerGuardian
  blocklists.
- **Import** — one-shot import of every torrent from an existing PicoTorrent
  database (File ▸ Import from PicoTorrent).
- **Filters** — the PQL query subset (`status = "downloading" and dl > 1kbps`),
  plus an optional console input.
- **Labels** — colors, save paths, per-torrent assignment, filtering,
  auto-apply.
- **Single-instance** — a second launch forwards its command line (torrent
  files / magnet links) to the running instance and exits.
- **Translations** — all original language files, compiled into the exe and
  picked from a scrollable list in Preferences, each shown by its native name
  ("Nederlands (Nederland)", from Windows' own locale data). A fresh install
  always starts in English (the OS locale is deliberately not consulted) and
  English is the first entry; changing the language offers to restart, since
  the UI is built from the translator at startup. The UI text is localized
  with an embedded en-US fallback.
- **Update check** — asks GitHub for this repo's latest release on startup
  (`/releases/latest`, so never a draft or prerelease) and shows a tray balloon
  when its tag beats the running version; clicking the balloon opens the
  release page. Endpoint and the enabled/ignored-version toggles live in
  `update_checks.*`, so it can be pointed at a fork.
- **Crash log** — a panic hook writes a backtrace to the logs folder (the
  original used Crashpad; there is no upload). A failure during startup, before
  the window exists, is shown in a message box rather than lost to the GUI
  subsystem's absent stderr.

## Known differences / not yet ported

- **uTP peer transport** is deferred until librqbit 9 ships a stable release,
  at which point it will be implemented. librqbit 9 integrates
  [librqbit-utp](https://crates.io/crates/librqbit-utp) (BEP 29) and configures
  it through `SessionOptions.listen`, so this becomes an engine upgrade rather
  than a transport to write — but 9.x is currently a prerelease (9.0.0-rc.0),
  its restructured transport layer is exactly where the MSE stream-transform
  patches (0003 / 0005) sit, and upstream has not yet enabled uTP by default.
  The vendored 8.1.1 is stable, so we wait. UDP trackers and DHT work today;
  only µTP peer connections are missing.
- **LSD** is deferred to the same librqbit 9 upgrade as uTP above — 9.x
  configures it through `SessionOptions.disable_local_service_discovery`; 8.1.1
  has no equivalent. Its Preferences checkbox is disabled until then, and
  `libtorrent.enable_lsd` is the one and only setting the dialog stores without
  applying. Separately, librqbit does not attribute peer counts to a discovery
  source, so the DHT/LSD/PeX tracker rows are status-only (no seeds/leeches
  numbers there) — that part is unchanged by the upgrade.
- Windows will not let an app force-set the **magnet** protocol default when
  another client registered it system-wide (anti-hijacking); the associations
  button registers NanoTorrent and opens Settings ▸ Default apps so you can
  confirm it.
- Language files other than `en-US.json` are PicoTorrent's original
  translations and still name that product in a few strings; `en-US` is the
  fallback and has been corrected.

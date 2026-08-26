# NanoTorrent

NanoTorrent is a tiny, hackable BitTorrent client for **Windows, Linux and
macOS** — a Rust 2024 port of
[PicoTorrent](https://github.com/picotorrent/picotorrent) by Viktor Elofsson.
It keeps the original's application structure, settings database and behaviour
while replacing the C++ / Rasterbar-libtorrent / wxWidgets stack with pure-Rust
building blocks.

Site: <https://www.nanotorrent.org>

> This port is built predominantly with AI assistance under human review — see
> [AI-DECLARATION.md](AI-DECLARATION.md).

| Aspect                | PicoTorrent (C++)               | NanoTorrent (Rust)                                   |
| --------------------- | ------------------------------- | ---------------------------------------------------- |
| BitTorrent engine     | Rasterbar-libtorrent            | [librqbit](https://crates.io/crates/librqbit) (vendored + patched) |
| GUI                   | wxWidgets (native Win32)        | [Slint](https://slint.dev) — one window on all three platforms |
| Platforms             | Windows                         | Windows, Linux, macOS (plus a headless build)        |
| Settings storage      | SQLite (`PicoTorrent.sqlite`)   | Same schema (`NanoTorrent.sqlite`)                   |
| Resume data           | libtorrent resume blobs in DB   | librqbit session persistence (JSON + `.bitv` mmap)   |
| Translations          | `lang/*.json` embedded in a DB  | Same `lang/*.json` compiled into the executable      |
| Single instance / IPC | Win32 mutex + `WM_COPYDATA`     | Loopback TCP on port 37549                           |
| Notifications         | tray balloons                   | Windows 11 WinRT toasts, plus in-app toasts everywhere |
| Remote access         | —                               | Optional authenticated HTTPS web interface           |
| Logging               | boost::log to file              | `tracing` to file                                    |

On first run NanoTorrent does a one-time copy of an existing
`%LOCALAPPDATA%\PicoTorrent` data folder (settings + session state) into
`%LOCALAPPDATA%\NanoTorrent`, leaving the original untouched.

## Installing

Download an installer from [Releases](https://github.com/Power2All/nanotorrent/releases),
or build one yourself:

| Platform | File | Built by |
| --- | --- | --- |
| Windows | `NanoTorrent-<ver>-Setup.exe` | NSIS, UPX-compressed payload — `installer/build-installer.bat` |
| Debian / Ubuntu | `nanotorrent-<ver>.deb` | `cargo deb` |
| Fedora / RHEL / openSUSE | `nanotorrent-<ver>.rpm` | `cargo generate-rpm` |
| macOS | `nanotorrent-<ver>.dmg` | `hdiutil`, from a hand-assembled `.app` |

The macOS bundle is unsigned and unnotarised, so Gatekeeper asks on first
launch (right-click ▸ Open). The Linux packages install the binary, a desktop
entry and an icon; the `.deb` targets glibc 2.35 or newer (Ubuntu 22.04+).

## Building

Requires Rust 1.85+ (edition 2024). No C++ dependencies, and no OpenSSL — the
librqbit `rust-tls` feature keeps libssl out of the tree entirely.

```
cargo build --release
```

There are two configurations:

| Command | What you get |
| --- | --- |
| `cargo build --release` | The GUI — the same window on all three platforms. |
| `cargo build --release --no-default-features` | Headless — no window, driven through the web interface. |

Linux additionally needs `libfontconfig1-dev`; see
[docs/BUILDING.md](docs/BUILDING.md) for the full Linux/macOS notes.

The binary ships standalone: every `lang/*.json` file is compiled in by
`build.rs`, so no folder needs to travel with it. A `lang/` folder next to the
executable still takes precedence per locale, which is the quickest way to edit
a translation without rebuilding.

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
  `-NT-` Azureus-style (or random, in anonymous mode) peer id and a matching
  `NanoTorrent <version>` in the BEP 10 handshake — the string other clients
  show in their Client column — up/down rate
  limits, SOCKS proxy with per-scope toggles (peers / trackers / hostnames),
  fast-resume across restarts. Settings are applied **live** — Preferences ▸ OK
  rebuilds the session, no restart needed.
- **MSE / PE encryption** — both outgoing and incoming, RC4, require-encryption
  toggles (`src/bittorrent/mse.rs`, injected through a vendored transform seam).
- **libtorrent tuning knobs** — every setting PicoTorrent exposed that can
  function against librqbit: active-download/seed/overall limits, PeX toggle,
  anonymous mode, proxy scoping. Ones with no librqbit mechanism are documented
  as no-ops.
- **Low-disk guard** - pause everything when free space on the default save
  path drops below a percentage of the volume. librqbit has no such mechanism,
  so this is checked here every 30s. Off by default; 5% when enabled.
- **Settings database** — the exact original migration SQL (incl. the custom
  `get_known_folder_path()` / `get_user_default_ui_language()` SQLite
  functions); a pre-existing PicoTorrent DB migrates cleanly.
- **Main window** — torrent list with all 16 columns, a **rendered progress
  bar** in the Progress column, click-to-sort, multi-select, context menu
  (pause/resume, remove with/without files, force recheck, move storage,
  labels, copy info hash / magnet, open in file manager). Paused torrents blank
  their live columns. Column widths follow the language, so a longer
  translation still fits.
- **Details tabs** — Overview (with a piece-availability bar), Files (per-file
  include toggles), Peers (with GeoIP country **and its flag**), and
  **Trackers** grouped into announce tiers with per-tracker
  seeds/leeches/fails/next-announce plus DHT/LSD/PeX source rows. Any value
  that is too long to fit shows in full on hover, and a click copies it.
- **Add flows** — Add torrent(s): pick any number of `.torrent` files and they
  arrive in **one dialog**, listed down the side, with each one's file tree
  shown as you select it. File selection is per torrent; save path and start
  apply to the batch. Add magnet, which **fetches the metadata first** and then
  shows the same dialog with the real file list.
- **Torrent creation** — BitTorrent v1, v2 and hybrid (BEP 52), with tracker /
  comment / private options.
- **Web interface** — an optional authenticated HTTPS remote: session and
  torrent listings, add / pause / resume / recheck / remove / move / label, and
  a save-path browser. Adding takes several at once — magnet links one per
  line, or a multi-file `.torrent` picker — and `POST /api/torrents/inspect`
  reads them server-side so the remote shows the same name, size and file tree
  the desktop dialog does, with the same per-file checkboxes. It reuses the
  desktop's own parser, so the two cannot disagree about a torrent's file
  order (which `only_files` indexes by). Argon2id password hashing, HTTP Basic
  over TLS
  (self-signed by default, or bring your own certificate), and it refuses to
  listen off-loopback in plaintext. Configurable from Preferences ▸ Web
  interface — pressing OK restarts it in place — or from the command line with
  `--webui`, `--set-web-password`, `--webui-status` and `--webui-set`.
  `--version` and `--help` work too, though on Windows the GUI build has no
  console to print to.
- **Notifications** — real Windows 11 toasts on download-complete under a
  registered AppUserModelID, switchable off in Preferences; a notification-area
  (tray) icon with close-to-tray prompt, shown for both the window's close
  button and File ▸ Exit.
- **File associations** — register NanoTorrent for `.torrent` files and
  `magnet:` links from Preferences.
- **GeoIP & IP filter** — DB-IP country lookup for peers; eMule/PeerGuardian
  blocklists.
- **Import** — one-shot import of every torrent from an existing PicoTorrent
  database (File ▸ Import from PicoTorrent).
- **Filters** — the PQL query subset (`status = "downloading" and dl > 1kbps`),
  plus an optional console input.
- **Labels** — colors, save paths, per-torrent assignment, filtering,
  auto-apply. Both labels and filters are managed from Preferences ▸ Labels and
  filters, with the filter expression validated as you type.
- **Single-instance** — a second launch forwards its command line (torrent
  files / magnet links) to the running instance and exits.
- **Translations** — all original language files, compiled into the executable
  and picked from a scrollable list in Preferences, each shown by its native
  name ("Nederlands (Nederland)"). A fresh install always starts in English
  (the OS locale is deliberately not consulted) and English is the first entry.
  **Changing the language applies immediately** — no restart.
- **Update check** — asks GitHub for this repo's latest release on startup
  (`/releases/latest`, so never a draft or prerelease) and reports when its tag
  beats the running version. Endpoint and the enabled/ignored-version toggles
  live in `update_checks.*`, so it can be pointed at a fork.
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
- **Desktop toasts are Windows-only.** Linux and macOS get the in-app toast;
  the OS-level notification is not wired up on those platforms yet.
- Windows will not let an app force-set the **magnet** protocol default when
  another client registered it system-wide (anti-hijacking); the associations
  button registers NanoTorrent and opens Settings ▸ Default apps so you can
  confirm it.
- On **Wayland** the window icon comes from the installed `.desktop` entry
  (there is no protocol for a client to set one), so a binary run straight from
  `target/release` shows a generic icon until
  `packaging/linux/install-desktop-entry.sh` has been run. The packages do this
  for you.
- Language files other than `en-US.json` are PicoTorrent's original
  translations and still name that product in a few strings; `en-US` is the
  fallback and has been corrected.

## History

**v0.2.1** added batch torrent adding to both UIs, the low-disk guard, and
gave the client its own name in the BEP 10 handshake — until then peers saw
`rqbit` rather than NanoTorrent. It also fixed a bug that could leave the app
unable to start: the session index was renamed into place without being
flushed first, so a full disk or a crash could replace it with an empty file.
It is now fsynced before the rename, and an unreadable index is moved aside
rather than being fatal.

**v0.2.0** replaced the Win32 front end (native-windows-gui) with Slint,
bringing Linux and macOS support, and added the web interface, live language
switching and the labels/filters management UI. The original UI is in the
history up to that tag.

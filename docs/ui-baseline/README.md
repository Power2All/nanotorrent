# Win32 UI baseline

Reference screenshots of the native Win32 UI (`src/ui_native`), captured before
the cross-platform port begins. They define what "renders correctly" means for
the replacement UI: every screen here has a counterpart to match.

Without these, "does the new UI look right?" can only be answered from memory.

## How they were captured

- `tools/screenshot.ps1 -Title <window> -Out <png>` against an **isolated
  portable instance** - a copy of the exe in a temp folder with a `portable.txt`
  marker beside it, so it used its own database and never touched
  `%LOCALAPPDATA%\NanoTorrent`.
- The three torrents are **synthetic**: locally generated payload plus a
  hand-built v1 metainfo pointing at `tracker.invalid`. Nothing real is named,
  and nothing was fetched from the network. That is why the tracker rows read
  "No such host is known" - it is the fixture working as intended, not a fault.
- Build stamp on every shot: `0.1.2 (build 2026-08-21 11:50 UTC)`.

## Known cosmetic issues visible here

Recorded so the port does not faithfully reproduce them:

- **Availability reads `-0.00`.** `sync_list` dashes the column when
  `availability < 0.0`, which is false for negative zero, so `-0.0` formats with
  its sign intact. Should be `0.00` (or `-`).
- **Progress column differs by theme.** Dark mode owner-draws a progress bar
  (`12-main-light.png` vs `03-main-rows.png`); light mode renders plain text.
  The port should pick one and use it in both.
- **Duplicate `&A` accelerator** in the File menu: "&Add torrent" and "&Add
  magnet link(s)" both claim A, so Alt+F,A cycles instead of activating.

## Files

| File | Screen |
|---|---|
| `01-main-empty.png` | Main window, no torrents |
| `02-add-torrent.png` | Add torrent(s) dialog |
| `03-main-rows.png` | Main window, 3 torrents, all 16 columns |
| `04-tab-overview.png` | Details: Overview (piece bar + fields) |
| `05-tab-files.png` | Details: Files |
| `05-tab-peers.png` | Details: Peers |
| `05-tab-trackers.png` | Details: Trackers (DHT / LSD / PeX + tiers) |
| `06-about.png` | About |
| `07-prefs-general.png` | Preferences: General (language list, theme) |
| `07-prefs-downloads.png` | Preferences: Downloads |
| `07-prefs-connection.png` | Preferences: Connection (note the disabled LSD box) |
| `07-prefs-proxy.png` | Preferences: Proxy |
| `08-add-magnet.png` | Add magnet link(s) |
| `09-create-torrent.png` | Create torrent |
| `10-close-prompt.png` | Close prompt (Exit / Minimize / Cancel) |
| `11-context-menu.png` | Torrent context menu |
| `12-main-light.png` | Main window, light theme |
| `13-prefs-light.png` | Preferences, light theme |

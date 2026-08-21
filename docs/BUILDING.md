# Building and running on Linux and macOS

Development happens on Windows, so these paths are the least exercised. If
something here is wrong, it is wrong because nobody had run it yet - please
correct it rather than working around it.

## Which build do you want?

There are three, and the difference matters:

| Command | What you get |
|---|---|
| `cargo build --release` | On Windows, the Win32 UI. On Linux/macOS, a **headless** daemon - no window, controlled through the web interface. |
| `cargo build --release --no-default-features` | Headless everywhere, including Windows. |
| `cargo build --release --no-default-features --features ui-slint` | The **cross-platform GUI**. This is the one to test on Linux and macOS. |

`ui-native` is Windows-only, so a plain `cargo build` on Linux or macOS gives
you the headless daemon, not a window. That is deliberate, not a failure.

## Prerequisites

Both platforms need a Rust toolchain (1.85 or newer - the crate is edition
2024) and, because `aws-lc-sys` compiles native code for TLS and SHA-1:

- **cmake**
- a C compiler

No OpenSSL. The librqbit `rust-tls` feature keeps libssl out of the tree
entirely, so there is no `libssl-dev` to hunt for.

### Linux

Debian/Ubuntu, for the GUI build:

    sudo apt install build-essential cmake pkg-config libfontconfig1-dev

The headless build needs less than that - no fontconfig - but the list above
covers both.

At runtime the GUI wants a Wayland or X11 session, and the **Add torrent**
picker goes through the XDG desktop portal (`xdg-desktop-portal` plus a backend
such as `xdg-desktop-portal-gtk` or `-kde`). That is deliberate: rfd's default
Linux backend is GTK3, and dragging GTK in to show one Open dialog is not worth
it. If the picker does nothing, a missing portal service is the first suspect.

### macOS

    xcode-select --install     # C compiler
    brew install cmake

## Running in a VM or emulator without GPU acceleration

**This is the important one.** Slint renders through OpenGL (femtovg) by
default. A VM without 3D acceleration will fail to start the window, usually
with a message about creating a GL context.

Both renderers are compiled in, so the fix needs no rebuild:

    SLINT_BACKEND=software ./nanotorrent

`sw` and `winit-software` are accepted too. It is slower and entirely usable
for checking that the UI lays out, draws and responds correctly - which is what
a VM is for.

## What is worth checking

The GUI is mid-port, so the interesting question is not "is it finished" but
"does anything platform-specific break":

- Does the window open at all, and does the list populate from a real session?
- Menu bar: on macOS Slint puts it in the **system** menu bar at the top of the
  screen rather than in the window. That is expected.
- Right-click context menu, ctrl/shift multi-select in the list.
- File > Add torrent - the native picker, and on Linux the portal question above.
- Data goes to `$XDG_DATA_HOME/nanotorrent` (default `~/.local/share`) on Linux
  and `~/Library/Application Support/NanoTorrent` on macOS. If it lands next to
  the executable instead, portable mode has been triggered by mistake - check
  for a stray `portable.txt`.

Known gaps, so they are not reported as platform bugs: Preferences, About and
Create torrent are not ported; the Files, Peers and Trackers tabs are
placeholders; sorting has no comparators; there is no tray icon; and the
context menu takes a second right-click to move to another row.

## Headless, with the web interface

Useful on a machine with no display at all:

    cargo build --release --no-default-features
    ./nanotorrent --webui on
    printf 'your-password' | ./nanotorrent --set-web-password
    ./nanotorrent

Then browse to `https://127.0.0.1:8443` and accept the self-signed certificate;
its SHA-256 fingerprint is printed to the log so it can be checked rather than
clicked through. `--webui-set bind_address 0.0.0.0` exposes it to the LAN, and
the server refuses to serve plaintext off loopback.

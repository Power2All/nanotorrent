#!/usr/bin/env bash
#
# Install the desktop entry and icon for the current user, so the window gets
# its own icon instead of the desktop's generic fallback.
#
# Wayland has no protocol for a client to set its own window icon - the
# compositor matches the window's app id to a .desktop file and uses the Icon=
# from it. X11 does let the client set one, so this is only strictly needed on
# Wayland, but it also gives you a launcher entry and magnet-link handling on
# both.
#
# No root needed: everything goes under ~/.local/share.
set -euo pipefail
cd "$(dirname "$0")/../.."

APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/256x256/apps"
mkdir -p "$APPS" "$ICONS"

install -m 0644 packaging/linux/nanotorrent.desktop "$APPS/nanotorrent.desktop"
install -m 0644 res/app.png "$ICONS/nanotorrent.png"

# Exec=nanotorrent only resolves if the binary is on PATH; point the entry at
# wherever it actually is otherwise.
if ! command -v nanotorrent >/dev/null; then
    BIN="$(pwd)/target/release/nanotorrent"
    if [ -x "$BIN" ]; then
        sed -i "s|^Exec=nanotorrent|Exec=$BIN|" "$APPS/nanotorrent.desktop"
        echo "==> Exec points at $BIN (not on PATH)"
    fi
fi

command -v update-desktop-database >/dev/null && update-desktop-database "$APPS" || true
command -v gtk-update-icon-cache >/dev/null \
    && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo "==> installed. Log out and back in, or restart the app, to pick up the icon."

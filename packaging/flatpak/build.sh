#!/usr/bin/env bash
#
# Build the Flatpak, and optionally run it.
#
#   packaging/flatpak/build.sh          build and install to the user remote
#   packaging/flatpak/build.sh --run    ... then launch it
#
# Must be run from Linux (WSL is fine). The Rust build happens inside the
# sandbox from scratch, so the first run takes a while - there is no shared
# target/ directory to reuse.
set -euo pipefail

cd "$(dirname "$0")"
REPO_ROOT="$(cd ../.. && pwd)"
APP_ID=org.nanotorrent.NanoTorrent

need() { command -v "$1" >/dev/null || { echo "missing: $1"; exit 1; }; }
need flatpak
need flatpak-builder
need python3

# The SDK extension is separate from the SDK and easy to forget; without it the
# build fails inside the sandbox with "cargo: not found", which does not point
# at the cause.
for rt in \
    "org.freedesktop.Platform//25.08" \
    "org.freedesktop.Sdk//25.08" \
    "org.freedesktop.Sdk.Extension.rust-stable//25.08"
do
    flatpak --user info "$rt" >/dev/null 2>&1 || {
        echo "==> installing $rt"
        flatpak install --user -y flathub "$rt"
    }
done

# --- vendored crates -------------------------------------------------------
# flatpak-builder builds with no network, so every crate has to be listed up
# front. The generator is fetched rather than vendored: it tracks the format
# flatpak-builder expects, and a stale copy fails in confusing ways.
GEN=flatpak-cargo-generator.py
if [ ! -f "$GEN" ]; then
    echo "==> fetching $GEN"
    curl -fsSLo "$GEN" \
      https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
fi

if [ ! -f cargo-sources.json ] || [ "$REPO_ROOT/Cargo.lock" -nt cargo-sources.json ]; then
    echo "==> generating cargo-sources.json from Cargo.lock"
    python3 -c 'import aiohttp, toml, tomlkit' 2>/dev/null || {
        echo "    installing python deps"
        pip3 install --quiet --break-system-packages aiohttp toml tomlkit
    }
    python3 "$GEN" "$REPO_ROOT/Cargo.lock" -o cargo-sources.json
fi

# --- build -----------------------------------------------------------------
# flatpak-builder needs rofiles-fuse, and fuse cannot mount over the 9p
# filesystem WSL exposes Windows drives through: it fails with "mounting over
# filesystem type 0x01021997 is forbidden", then "Build directory not
# initialized". The sources may live on /mnt - they are copied into the
# sandbox - but the build state has to sit on the Linux filesystem.
STATE_DIR=.flatpak-builder
BUILD_DIR=build-dir
case "$PWD" in
    /mnt/*)
        CACHE="$HOME/.cache/nanotorrent-flatpak"
        STATE_DIR="$CACHE/state"
        BUILD_DIR="$CACHE/build"
        mkdir -p "$CACHE"
        echo "==> repo is on /mnt, building via $CACHE (fuse cannot work on 9p)"
        ;;
esac

echo "==> building (this is a full release build inside the sandbox)"
flatpak-builder --user --force-clean --install \
    --state-dir "$STATE_DIR" "$BUILD_DIR" "$APP_ID.yml"

echo
echo "==> installed. Run it with:"
echo "    flatpak run $APP_ID"

if [ "${1:-}" = "--run" ]; then
    flatpak run "$APP_ID"
fi

#!/usr/bin/env bash
#
# Build the Flatpak, and optionally run it.
#
#   packaging/flatpak/build.sh          build and install to the user remote
#   packaging/flatpak/build.sh --run    ... then launch it
#
# Builds what is in the working tree, NOT the tag the manifest names - so it
# tests uncommitted work, and works before a release has been tagged.
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

# --- local manifest --------------------------------------------------------
# The committed manifest builds from a git tag, because that is what Flathub
# does. Locally that is the wrong thing twice over: it ignores uncommitted work,
# and between `set-version.ps1` and the actual tagging the tag does not exist
# yet, so the build simply fails.
#
# So a copy is made with the git source swapped for the working tree. The
# committed manifest is never touched - it is the submission artifact.
# Written beside the real manifest, not in the cache: `- cargo-sources.json`
# and any other relative source is resolved against the MANIFEST's directory,
# so a copy kept elsewhere silently loses them. Gitignored.
LOCAL_MANIFEST="local-$APP_ID.yml"

awk -v repo="$REPO_ROOT" '
    /^      - type: git/ { skip = 1 }
    # The git source ends at its commit: line; emit the replacement there.
    skip && /^        commit:/ {
        skip = 0
        print "      - type: dir"
        print "        path: " repo
        # Without these the whole working tree is copied in, target/ included.
        print "        skip:"
        print "          - target"
        print "          - .git"
        print "          - build-dir"
        print "          - .flatpak-builder"
        next
    }
    !skip { print }
' "$APP_ID.yml" > "$LOCAL_MANIFEST"

grep -q 'type: dir' "$LOCAL_MANIFEST" || {
    echo "could not rewrite the manifest source - has its shape changed?" >&2
    exit 1
}

echo "==> building $(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2) from the working tree"
echo "    (a full release build inside the sandbox - not quick)"
flatpak-builder --user --force-clean --install \
    --state-dir "$STATE_DIR" "$BUILD_DIR" "$LOCAL_MANIFEST"

echo
echo "==> installed. Run it with:"
echo "    flatpak run $APP_ID"

if [ "${1:-}" = "--run" ]; then
    flatpak run "$APP_ID"
fi

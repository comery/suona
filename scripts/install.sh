#!/usr/bin/env bash
#
# Build suona and install it as a real macOS application.
#
# Running from the build directory works, but "launch at login" registers
# whichever path the app is running from — so install somewhere stable first,
# then flip the toggle inside the app.
#
#   ./scripts/install.sh              # install to /Applications
#   ./scripts/install.sh ~/Applications
#
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILT="$REPO/src-tauri/target/release/bundle/macos/suona.app"
DEST_DIR="${1:-/Applications}"
DEST="$DEST_DIR/suona.app"

build() {
  echo "==> building release bundle"
  cd "$REPO"
  pnpm install
  # Some environments put a wrapper named `node` first on PATH, which breaks
  # the tauri CLI's argv[0] detection. Calling the entry point directly with
  # the real node binary sidesteps that.
  if ! pnpm tauri build --bundles app; then
    echo "==> retrying via explicit node entry point"
    node node_modules/@tauri-apps/cli/tauri.js build --bundles app
  fi
}

[[ -d "$BUILT" ]] || build

echo "==> stopping any running instance"
pkill -f "suona.app/Contents/MacOS/suona" 2>/dev/null || true
sleep 1

echo "==> installing to $DEST"
mkdir -p "$DEST_DIR"
rm -rf "$DEST"
cp -R "$BUILT" "$DEST"

# An unsigned local build picks up a quarantine flag when copied around.
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

echo
echo "==> done: $DEST"
echo "    open '$DEST'"
echo
echo "    To start suona at login, launch it and tick 开机自启 in the ☰ panel."

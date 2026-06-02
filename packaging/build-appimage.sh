#!/usr/bin/env bash
# Build the iSyncYou AppImage host package (plan §15) from pre-built binaries.
#
# Usage: packaging/build-appimage.sh [BIN_DIR] [OUT_DIR]
#   BIN_DIR  where isyncyou/isyncyoud/isyncyou-doctor live (default target/release)
#   OUT_DIR  where to write the .AppImage           (default dist)
# Env:
#   APPIMAGETOOL  path to appimagetool (default: `appimagetool` on PATH)
#
# The CLI (isyncyou) is the AppImage entrypoint; the daemon + doctor ship beside
# it under usr/bin.
set -euo pipefail

BIN_DIR="${1:-target/release}"
OUT_DIR="${2:-dist}"
HERE="$(cd "$(dirname "$0")" && pwd)"
APPIMAGETOOL="${APPIMAGETOOL:-appimagetool}"

work="$(mktemp -d)"
appdir="$work/isyncyou.AppDir"
mkdir -p "$appdir/usr/bin"

for b in isyncyou isyncyoud isyncyou-doctor; do
    install -m755 "$BIN_DIR/$b" "$appdir/usr/bin/$b"
done
install -m755 "$HERE/appimage/AppRun" "$appdir/AppRun"
install -m644 "$HERE/appimage/isyncyou.desktop" "$appdir/isyncyou.desktop"
install -m644 "$HERE/appimage/isyncyou.png" "$appdir/isyncyou.png"
cp "$HERE/appimage/isyncyou.png" "$appdir/.DirIcon"

mkdir -p "$OUT_DIR"
ARCH=x86_64 "$APPIMAGETOOL" "$appdir" "$OUT_DIR/isyncyou-x86_64.AppImage"
rm -rf "$work"
echo "built $OUT_DIR/isyncyou-x86_64.AppImage"

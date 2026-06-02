#!/usr/bin/env bash
# Build the iSyncYou status-bar GUI as a Flatpak (plan §15). The binary is built
# in-sandbox (rust SDK extension) so it matches the runtime's glibc.
#
# Prerequisites (one-time):
#   flatpak remote-add --user --if-not-exists flathub \
#       https://flathub.org/repo/flathub.flatpakrepo
#   flatpak install --user -y flathub \
#       org.freedesktop.Platform//24.08 org.freedesktop.Sdk//24.08 \
#       org.freedesktop.Sdk.Extension.rust-stable//24.08
#
# Usage: packaging/flatpak/build-flatpak.sh [OUT_DIR]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-build-flatpak}"
flatpak-builder --user --force-clean --install-deps-from=flathub \
    --repo="$OUT/repo" "$OUT/build" "$HERE/org.silentspike.iSyncYou.yaml"
echo "built into $OUT/repo"
echo "install: flatpak install --user $OUT/repo org.silentspike.iSyncYou"
echo "run:     flatpak run org.silentspike.iSyncYou"

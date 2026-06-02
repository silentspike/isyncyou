#!/usr/bin/env bash
# Install the iSyncYou Dolphin ServiceMenu for the current user.
#
# Adds a right-click "iSyncYou" submenu in Dolphin with:
#   * Show sync status      -> isyncyou dolphin-status %f  (queries the daemon over DBus)
#   * Open iSyncYou window   -> isyncyou-statusbar
#
# The actions need `isyncyou` (CLI) and `isyncyou-statusbar` on PATH and a running
# `isyncyoud`. This is the always-available context-menu integration; the overlay
# icons (KOverlayIconPlugin, packaging/dolphin/overlay-plugin) are an enhancement.
set -euo pipefail

src="$(cd "$(dirname "$0")" && pwd)/org.silentspike.iSyncYou.desktop"
dest_dir="${XDG_DATA_HOME:-$HOME/.local/share}/kio/servicemenus"

mkdir -p "$dest_dir"
install -m 0644 "$src" "$dest_dir/org.silentspike.iSyncYou.desktop"
echo "installed: $dest_dir/org.silentspike.iSyncYou.desktop"

# Refresh KDE's service cache so Dolphin picks it up without a restart.
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 >/dev/null 2>&1 || true
    echo "ran kbuildsycoca6"
fi
echo "Right-click a file/folder in Dolphin -> iSyncYou."

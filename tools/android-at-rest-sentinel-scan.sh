#!/usr/bin/env bash
set -euo pipefail

pkg="${ANDROID_PACKAGE:-com.silentspike.isyncyou.debug}"
adb_bin="${ADB:-adb}"

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <ascii-sentinel>" >&2
  echo "env: ANDROID_PACKAGE=$pkg ADB=$adb_bin" >&2
  exit 2
fi

sentinel="$1"
if [ -z "$sentinel" ]; then
  echo "sentinel must not be empty" >&2
  exit 2
fi

set +e
"$adb_bin" shell run-as "$pkg" sh -s -- "$sentinel" <<'SH'
set -eu
sentinel="$1"
marker="files/.isyncyou-sentinel-scan-hit.$$"
rm -f "$marker"

scan_root() {
  root="$1"
  [ -e "$root" ] || return 0
  find "$root" -type f -print | while IFS= read -r file; do
    case "$file" in
      files/.isyncyou-token*|files/archive/.isyncyou-token*|files/cache/.isyncyou-token*|files/sync/.isyncyou-token*|cache/.isyncyou-token*)
        continue
        ;;
    esac
    if grep -aFq -- "$sentinel" "$file" 2>/dev/null; then
      printf 'PLAINTEXT_SENTINEL %s\n' "$file"
      : > "$marker"
    fi
  done
}

scan_root files/archive
scan_root files/cache
scan_root files/sync
scan_root cache

if [ -f "$marker" ]; then
  rm -f "$marker"
  exit 1
fi
rm -f "$marker"
printf 'OK no plaintext sentinel in files/archive, files/cache, files/sync, cache\n'
SH
status=$?
set -e

if [ "$status" -eq 0 ]; then
  exit 0
fi
echo "FAIL plaintext sentinel found or scan failed" >&2
exit "$status"

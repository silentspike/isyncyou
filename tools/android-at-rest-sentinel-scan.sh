#!/usr/bin/env bash
set -euo pipefail

pkg="${ANDROID_PACKAGE:-com.silentspike.isyncyou.debug}"
adb_bin="${ADB:-adb}"

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: $0 <ascii-sentinel> [output-dir]" >&2
  echo "env: ANDROID_PACKAGE=$pkg ADB=$adb_bin" >&2
  exit 2
fi

sentinel="$1"
if [ -z "$sentinel" ]; then
  echo "sentinel must not be empty" >&2
  exit 2
fi

out_dir="${2:-target/android-at-rest-evidence}"
mkdir -p "$out_dir"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
log_file="$out_dir/android-at-rest-sentinel-scan-$stamp.log"

set +e
device_output="$("$adb_bin" shell run-as "$pkg" sh -s -- "$sentinel" <<'SH'
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

check_db_header() {
  db="$1"
  [ -f "$db" ] || {
    printf 'DB_HEADER_SKIP missing %s\n' "$db"
    return 0
  }
  header_hex="$(dd if="$db" bs=16 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')"
  if [ "$header_hex" = "53514c69746520666f726d6174203300" ]; then
    printf 'PLAINTEXT_SQLITE_HEADER %s\n' "$db"
    : > "$marker"
  else
    printf 'DB_HEADER_NOT_SQLITE %s %s\n' "$db" "${header_hex:-empty}"
  fi
}

check_keyless_sqlite_open() {
  db="$1"
  [ -f "$db" ] || {
    printf 'KEYLESS_SQLITE_OPEN_SKIP missing %s\n' "$db"
    return 0
  }
  if ! command -v sqlite3 >/dev/null 2>&1; then
    printf 'KEYLESS_SQLITE_OPEN_SKIP sqlite3_missing %s\n' "$db"
    return 0
  fi
  if sqlite3 "$db" 'select count(*) from sqlite_master;' >/dev/null 2>&1; then
    printf 'KEYLESS_SQLITE_OPENED %s\n' "$db"
    : > "$marker"
  else
    printf 'KEYLESS_SQLITE_OPEN_REJECTED %s\n' "$db"
  fi
}

printf 'SCAN_ROOT files/archive\n'
scan_root files/archive
printf 'SCAN_ROOT files/cache\n'
scan_root files/cache
printf 'SCAN_ROOT files/sync\n'
scan_root files/sync
printf 'SCAN_ROOT cache\n'
scan_root cache
check_db_header files/archive/.isyncyou-store.db
check_keyless_sqlite_open files/archive/.isyncyou-store.db

if [ -f "$marker" ]; then
  rm -f "$marker"
  exit 1
fi
rm -f "$marker"
printf 'PASS no plaintext sentinel in files/archive, files/cache, files/sync, cache\n'
SH
)"
status=$?
set -e

{
  printf 'Android at-rest sentinel scan evidence\n'
  printf 'timestamp_utc=%s\n' "$stamp"
  printf 'package=%s\n' "$pkg"
  printf 'sentinel_len=%s\n' "${#sentinel}"
  printf 'sentinel_value=REDACTED\n'
  printf 'adb=%s\n' "$adb_bin"
  printf -- '--- device output ---\n'
  printf '%s\n' "$device_output"
  if [ "$status" -eq 0 ]; then
    printf 'result=PASS\n'
  else
    printf 'result=FAIL status=%s\n' "$status"
  fi
} | tee "$log_file"

if [ "$status" -eq 0 ]; then
  echo "evidence_log=$log_file"
  exit 0
fi
echo "FAIL plaintext sentinel found or scan failed" >&2
echo "evidence_log=$log_file" >&2
exit "$status"

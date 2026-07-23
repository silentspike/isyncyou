#!/usr/bin/env bash
set -euo pipefail

package=""
features=""
filter=""

while (($#)); do
  case "$1" in
    -p|--package)
      [[ $# -ge 2 ]] || { echo "missing package value" >&2; exit 2; }
      package=$2
      shift 2
      ;;
    --features)
      [[ $# -ge 2 ]] || { echo "missing features value" >&2; exit 2; }
      features=$2
      shift 2
      ;;
    --filter)
      [[ $# -ge 2 ]] || { echo "missing filter value" >&2; exit 2; }
      filter=$2
      shift 2
      ;;
    *)
      echo "unsupported argument: $1" >&2
      exit 2
      ;;
  esac
done

[[ -n "$package" && -n "$filter" ]] || {
  echo "usage: $0 -p PACKAGE [--features FEATURES] --filter FILTER" >&2
  exit 2
}

program=${ISY_CARGO_REMOTE_PROGRAM:-cargo}
command=("$program" remote -c -- test -p "$package")
if [[ -n "$features" ]]; then
  command+=(--features "$features")
fi

listing=$("${command[@]}" "$filter" -- --list)
strip_ansi() {
  sed -E $'s/\\x1B\\[[0-9;?]*[ -/]*[@-~]//g' | tr -d '\r'
}

count=$(printf '%s\n' "$listing" | strip_ansi | awk '/: test$/ { count += 1 } END { print count + 0 }')
if ((count == 0)); then
  echo "cargo filter matched zero tests: package=$package filter=$filter" >&2
  exit 3
fi

echo "cargo filter matched $count test(s): package=$package filter=$filter" >&2
run_output=$(mktemp "${TMPDIR:-/tmp}/isyncyou-nonempty-cargo-run.XXXXXX")
cleanup() { rm -f -- "$run_output"; }
trap cleanup EXIT INT TERM

set +e
"${command[@]}" "$filter" -- --nocapture 2>&1 | tee "$run_output"
run_status=${PIPESTATUS[0]}
set -e
if ((run_status != 0)); then
  exit "$run_status"
fi

passed=$(strip_ansi <"$run_output" | awk '
  /test result: ok\./ {
    for (i = 1; i <= NF; i++) {
      if ($i == "passed;") {
        value = $(i - 1)
        gsub(/[^0-9]/, "", value)
        total += value + 0
      }
    }
  }
  END { print total + 0 }
')
if ((passed == 0)); then
  echo "cargo filter executed zero passing tests: package=$package filter=$filter" >&2
  exit 4
fi

echo "cargo filter executed $passed passing test(s): package=$package filter=$filter" >&2

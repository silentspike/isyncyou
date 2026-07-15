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
count=$(printf '%s\n' "$listing" | awk '/: test$/ { count += 1 } END { print count + 0 }')
if ((count == 0)); then
  echo "cargo filter matched zero tests: package=$package filter=$filter" >&2
  exit 3
fi

echo "cargo filter matched $count test(s): package=$package filter=$filter" >&2
exec "${command[@]}" "$filter" -- --nocapture

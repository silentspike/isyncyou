#!/usr/bin/env bash
set -euo pipefail

root=$(mktemp -d /tmp/isyncyou-nonempty-cargo-filter.XXXXXX)
cleanup() { rm -rf -- "$root"; }
trap cleanup EXIT INT TERM

fake="$root/fake-cargo"
cat >"$fake" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ " $* " == *" --list "* ]]; then
  if [[ ${ISY_FAKE_FILTER_MODE:-positive} == positive ]]; then
    printf 'tests::fixture_match: test\n'
  fi
  exit 0
fi
printf '%s\n' "$*" >>"${ISY_FAKE_EXEC_LOG:?}"
EOF
chmod 700 "$fake"

log="$root/executed.log"
ISY_CARGO_REMOTE_PROGRAM="$fake" ISY_FAKE_FILTER_MODE=positive ISY_FAKE_EXEC_LOG="$log" \
  tools/run-nonempty-cargo-filter.sh -p fixture --features feature-a --filter fixture_match
grep -q -- '--features feature-a fixture_match -- --nocapture' "$log"

if ISY_CARGO_REMOTE_PROGRAM="$fake" ISY_FAKE_FILTER_MODE=empty ISY_FAKE_EXEC_LOG="$log" \
  tools/run-nonempty-cargo-filter.sh -p fixture --filter missing_match; then
  echo "zero-match filter unexpectedly succeeded" >&2
  exit 1
else
  status=$?
  [[ $status -eq 3 ]] || { echo "zero-match filter returned $status, expected 3" >&2; exit 1; }
fi

echo "run-nonempty-cargo-filter: positive and zero-match paths passed"

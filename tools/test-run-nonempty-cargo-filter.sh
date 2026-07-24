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
  if [[ ${ISY_FAKE_FILTER_MODE:-positive} != empty ]]; then
    if [[ ${ISY_FAKE_FILTER_MODE:-positive} == crlf ]]; then
      printf 'tests::fixture_match: test\r\n'
    else
      printf 'tests::fixture_match: test\n'
    fi
  fi
  exit 0
fi
printf '%s\n' "$*" >>"${ISY_FAKE_EXEC_LOG:?}"
case ${ISY_FAKE_FILTER_MODE:-positive} in
  positive|crlf)
    printf 'test tests::fixture_match ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
    ;;
  zero-run)
    printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out\n'
    ;;
  failing-run)
    printf 'test tests::fixture_match ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed\n'
    exit 101
    ;;
esac
EOF
chmod 700 "$fake"

log="$root/executed.log"
ISY_CARGO_REMOTE_PROGRAM="$fake" ISY_FAKE_FILTER_MODE=positive ISY_FAKE_EXEC_LOG="$log" \
  tools/run-nonempty-cargo-filter.sh -p fixture --features feature-a --filter fixture_match
grep -q -- '--features feature-a fixture_match -- --nocapture' "$log"

ISY_CARGO_REMOTE_PROGRAM="$fake" ISY_FAKE_FILTER_MODE=crlf ISY_FAKE_EXEC_LOG="$log" \
  tools/run-nonempty-cargo-filter.sh -p fixture --filter fixture_match

if ISY_CARGO_REMOTE_PROGRAM="$fake" ISY_FAKE_FILTER_MODE=empty ISY_FAKE_EXEC_LOG="$log" \
  tools/run-nonempty-cargo-filter.sh -p fixture --filter missing_match; then
  echo "zero-match filter unexpectedly succeeded" >&2
  exit 1
else
  status=$?
  [[ $status -eq 3 ]] || { echo "zero-match filter returned $status, expected 3" >&2; exit 1; }
fi

if ISY_CARGO_REMOTE_PROGRAM="$fake" ISY_FAKE_FILTER_MODE=zero-run ISY_FAKE_EXEC_LOG="$log" \
  tools/run-nonempty-cargo-filter.sh -p fixture --filter fixture_match; then
  echo "zero-run filter unexpectedly succeeded" >&2
  exit 1
else
  status=$?
  [[ $status -eq 4 ]] || { echo "zero-run filter returned $status, expected 4" >&2; exit 1; }
fi

if ISY_CARGO_REMOTE_PROGRAM="$fake" ISY_FAKE_FILTER_MODE=failing-run ISY_FAKE_EXEC_LOG="$log" \
  tools/run-nonempty-cargo-filter.sh -p fixture --filter fixture_match; then
  echo "failing filter unexpectedly succeeded" >&2
  exit 1
else
  status=$?
  [[ $status -eq 101 ]] || { echo "failing filter returned $status, expected 101" >&2; exit 1; }
fi

echo "run-nonempty-cargo-filter: positive, CRLF, zero-list, zero-run, and failing-run paths passed"

#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
FORBIDDEN='\.claude/\.credentials\.json|\.codex/auth\.json|CLAUDE_CONFIG_DIR|CODEX_HOME|subscription/import'
TMP_ROOT=$(mktemp -d /tmp/isyncyou-627-boundary.XXXXXX)
trap 'rm -rf -- "$TMP_ROOT"' EXIT INT TERM

die() {
  printf '%s\n' "experimental boundary verification failed: $1" >&2
  exit 2
}

scan_binary() {
  local binary=$1
  local strings_file
  strings_file="$TMP_ROOT/strings-$(basename -- "$binary").txt"
  strings "$binary" >"$strings_file"
  if rg -q "$FORBIDDEN" "$strings_file"; then
    die "default artifact contains an experimental marker"
  fi
}

verify_feature_matrix() {
  cd "$ROOT"
  cargo metadata --no-deps --format-version 1 >"$TMP_ROOT/metadata.json"
  cargo tree -p isyncyou-daemon -e normal -f '{p} features={f}' \
    >"$TMP_ROOT/default-tree.txt"
  cargo tree -p isyncyou-daemon --features agent-subscription-experimental \
    -e normal -f '{p} features={f}' >"$TMP_ROOT/experimental-tree.txt"

  if rg -q 'agent-subscription-experimental' "$TMP_ROOT/default-tree.txt"; then
    die "default daemon resolves the experimental feature"
  fi
  rg -q '^isyncyou-daemon .*features=.*agent-subscription-experimental' \
    "$TMP_ROOT/experimental-tree.txt" || die "daemon feature forwarding is absent"
  rg -q '^.*isyncyou-app-host .*features=.*agent-subscription-experimental' \
    "$TMP_ROOT/experimental-tree.txt" || die "app-host feature forwarding is absent"
  rg -q '^.*isyncyou-agent .*features=.*agent-subscription-experimental' \
    "$TMP_ROOT/experimental-tree.txt" || die "agent feature forwarding is absent"
  jq -e '
    .packages[]
    | select(.name == "isyncyou-mobile")
    | .features
    | has("agent-subscription-experimental")
    | not
  ' "$TMP_ROOT/metadata.json" >/dev/null || die "mobile exposes the experimental feature"

  set +e
  cargo remote -c -- check -p isyncyou-mobile \
    --features agent-subscription-experimental >"$TMP_ROOT/mobile-rejection.txt" 2>&1
  local status=$?
  set -e
  [[ $status -eq 101 ]] || die "mobile feature request did not fail with Cargo status 101"
  rg -q 'does not contain this feature|none of the selected packages contains' \
    "$TMP_ROOT/mobile-rejection.txt" || die "mobile feature rejection reason changed"

  printf '%s\n' "feature matrix: PASS"
}

verify_release_exclusion() {
  cd "$ROOT"
  mkdir -p target/release
  cargo remote --no-copy-lock -d 1.95.0 -c release/isyncyoud -- \
    build --locked --release -p isyncyou-daemon
  [[ -x target/release/isyncyoud ]] || die "release daemon is unavailable"
  scan_binary target/release/isyncyoud

  env -u ISY_CARGO_FEATURES tools/build-android-native.sh
  (cd android && env -u ISY_CARGO_FEATURES ./gradlew clean :app:assembleDebug)
  local apk=android/app/build/outputs/apk/debug/app-debug.apk
  [[ -f "$apk" ]] || die "default debug APK is unavailable"
  unzip -p "$apk" lib/arm64-v8a/libisyncyou_mobile.so >"$TMP_ROOT/libisyncyou_mobile.so"
  scan_binary "$TMP_ROOT/libisyncyou_mobile.so"

  sha256sum target/release/isyncyoud "$apk" "$TMP_ROOT/libisyncyou_mobile.so"
  printf '%s\n' "release exclusion: PASS"
}

verify_public_surface() {
  cd "$ROOT"
  python3 -m unittest tools.test_agent_experimental_boundary -v
  cargo remote -c -- test -p isyncyou-app-host --features agent-oauth-providers \
    product_sources_do_not_reference_local_cli_auth_paths -- --nocapture
  cargo remote -c -- test --workspace \
    assistant_product_source_has_no_subscription_import_affordance -- --nocapture

  for summary in docs/evidence/artifacts/issue-627/*-drift-summary.json; do
    jq -e '
      [.. | strings]
      | all(.[];
          test("Bearer |access_token|refresh_token|code=|/home/|/users/|@"; "i")
          | not)
    ' "$summary" >/dev/null || die "reduced summary contains a forbidden scalar"
  done
  if find docs/evidence/artifacts/issue-627 -maxdepth 1 -type f \
    \( -name '*debug*' -o -name '*events*' -o -name '*models*' -o -name '*auth*' \) \
    -print -quit | rg -q .; then
    die "raw capture artifact is tracked"
  fi

  printf '%s\n' "public surface: PASS"
}

case ${1:-} in
  feature-matrix)
    verify_feature_matrix
    ;;
  release-exclusion)
    verify_release_exclusion
    ;;
  public-surface)
    verify_public_surface
    ;;
  *)
    die "usage: feature-matrix | release-exclusion | public-surface"
    ;;
esac

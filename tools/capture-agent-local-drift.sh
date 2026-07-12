#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
REDUCER="$ROOT/target/debug/examples/reduce_local_drift"
SENTINEL="isyncyou-627-controlled-drift-sentinel"

die() {
  printf '%s\n' "local drift capture failed: $1" >&2
  exit 2
}

scan_summary() {
  local file=$1
  jq -e '
    .schema_version == 1 and
    .scope == "local_cli_drift_only_not_product_auth" and
    .product_auth_evidence == false and
    .raw_retained == false and
    .controlled_sentinel_observed == true and
    (.client.name == "claude" or .client.name == "codex") and
    (.drift_decision == "no_drift" or
     .drift_decision == "implementation_update_required" or
     .drift_decision == "not_safely_observable")
  ' "$file" >/dev/null || die "summary schema rejected"
  if rg -i -q \
    'bearer |access_token|refresh_token|"code"[[:space:]]*:|"state"[[:space:]]*:|/home/|/users/|@|isyncyou-627-controlled-drift-sentinel' \
    "$file"; then
    die "summary value scan rejected"
  fi
}

resolve_executable() {
  local name=$1
  [[ $(type -t "$name" || true) == "file" ]] || die "client executable is not a file command"
  local candidate
  candidate=$(command -v "$name")
  candidate=$(readlink -f -- "$candidate")
  [[ -f "$candidate" && -x "$candidate" ]] || die "client executable rejected"
  printf '%s\n' "$candidate"
}

capture() {
  [[ $# -eq 1 ]] || die "capture requires one review directory"
  local review_dir=$1
  [[ "$review_dir" == /tmp/* ]] || die "review directory must be below /tmp"
  [[ ! -e "$review_dir" ]] || die "review directory already exists"
  mkdir -m 700 -- "$review_dir"
  [[ -x "$REDUCER" ]] || die "reducer binary is unavailable"

  local claude_raw codex_raw=""
  claude_raw=$(mktemp -d /tmp/isyncyou-627-claude.XXXXXX)
  cleanup() {
    rm -rf -- "$claude_raw"
    if [[ -n "$codex_raw" ]]; then
      rm -rf -- "$codex_raw"
    fi
  }
  trap cleanup EXIT INT TERM
  codex_raw=$(mktemp -d /tmp/isyncyou-627-codex.XXXXXX)
  chmod 700 "$claude_raw" "$codex_raw"

  local claude_bin codex_bin
  claude_bin=$(resolve_executable claude)
  codex_bin=$(resolve_executable codex)
  local -a clean_env=(
    -u ANTHROPIC_API_KEY
    -u OPENAI_API_KEY
    -u ISY_LIVE_TOKEN
    -u ISY_LIVE_MODEL
    -u ISY_CODEX_TOKEN
    -u ISY_CODEX_ACCOUNT
    -u ISYNCYOU_AGENT_PROVIDER
    -u ISYNCYOU_AGENT_MODEL
    -u ISYNCYOU_AGENT_CRED_KEY
    -u ISY623_CLAUDE_OAUTH_ACCESS
    -u ISY623_CLAUDE_OAUTH_REFRESH
    -u ISY623_CLAUDE_OAUTH_EXPIRES_AT_MS
    -u ISY623_CODEX_OAUTH_ACCESS
    -u ISY623_CODEX_OAUTH_REFRESH
    -u ISY623_CODEX_OAUTH_EXPIRES_AT_MS
    -u ISY623_CODEX_ACCOUNT_ID
  )

  env "${clean_env[@]}" timeout 30s "$claude_bin" --version >"$claude_raw/version.txt"
  env "${clean_env[@]}" timeout 30s "$codex_bin" --version >"$codex_raw/version.txt"
  mkdir -m 700 "$claude_raw/work" "$codex_raw/work"

  (
    cd "$claude_raw/work"
    env "${clean_env[@]}" timeout 120s "$claude_bin" \
      --safe-mode \
      --disable-slash-commands \
      --tools "" \
      --permission-mode dontAsk \
      --no-session-persistence \
      --print \
      --model sonnet \
      --output-format stream-json \
      --include-partial-messages \
      --verbose \
      --debug api \
      --debug-file "$claude_raw/debug.log" \
      "Reply with exactly $SENTINEL and do nothing else."
  ) >"$claude_raw/events.jsonl"

  env "${clean_env[@]}" timeout 120s "$codex_bin" exec \
    --json \
    --ephemeral \
    --ignore-user-config \
    --ignore-rules \
    --sandbox read-only \
    --skip-git-repo-check \
    -C "$codex_raw/work" \
    "Reply with exactly $SENTINEL and do nothing else." \
    >"$codex_raw/events.jsonl"
  env "${clean_env[@]}" timeout 30s "$codex_bin" debug models >"$codex_raw/models.json"
  env "${clean_env[@]}" timeout 30s "$codex_bin" debug models --bundled \
    >"$codex_raw/models-bundled.json"
  find "$claude_raw" "$codex_raw" -type f -exec chmod 600 {} +

  "$REDUCER" \
    --provider claude \
    --version-file "$claude_raw/version.txt" \
    --event-file "$claude_raw/events.jsonl" \
    --debug-file "$claude_raw/debug.log" \
    --expected-sentinel "$SENTINEL" \
    --output "$review_dir/claude-summary.json"
  "$REDUCER" \
    --provider codex \
    --version-file "$codex_raw/version.txt" \
    --event-file "$codex_raw/events.jsonl" \
    --model-catalog "$codex_raw/models.json" \
    --bundled-model-catalog "$codex_raw/models-bundled.json" \
    --expected-sentinel "$SENTINEL" \
    --output "$review_dir/codex-summary.json"

  scan_summary "$review_dir/claude-summary.json"
  scan_summary "$review_dir/codex-summary.json"
  local claude_version codex_version
  claude_version=$(jq -r '.client.version' "$review_dir/claude-summary.json")
  codex_version=$(jq -r '.client.version' "$review_dir/codex-summary.json")
  [[ "$claude_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "Claude version rejected"
  [[ "$codex_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "Codex version rejected"
  mv -- "$review_dir/claude-summary.json" \
    "$review_dir/claude-code-$claude_version-drift-summary.json"
  mv -- "$review_dir/codex-summary.json" \
    "$review_dir/codex-cli-$codex_version-drift-summary.json"

  cleanup
  trap - EXIT INT TERM
  [[ ! -e "$claude_raw" && ! -e "$codex_raw" ]] || die "raw cleanup incomplete"
  printf '%s\n' "Reduced summaries are ready for manual scalar review."
}

publish() {
  [[ $# -eq 3 ]] || die "publish requires review directory, output directory, and approval"
  local review_dir=$1
  local output_dir=$2
  local approval=$3
  [[ "$approval" == "APPROVE_REDUCED_SUMMARIES" ]] || die "explicit approval is required"
  [[ "$review_dir" == /tmp/* && -d "$review_dir" ]] || die "review directory rejected"
  mapfile -t summaries < <(
    find "$review_dir" -maxdepth 1 -type f \
      \( -name 'claude-code-*-drift-summary.json' -o -name 'codex-cli-*-drift-summary.json' \) \
      -print | sort
  )
  [[ ${#summaries[@]} -eq 2 ]] || die "exactly two summaries are required"
  mkdir -p -- "$output_dir"
  for summary in "${summaries[@]}"; do
    scan_summary "$summary"
    install -m 600 -- "$summary" "$output_dir/$(basename -- "$summary")"
  done
  printf '%s\n' "Approved reduced summaries published."
}

case ${1:-} in
  capture)
    shift
    capture "$@"
    ;;
  publish)
    shift
    publish "$@"
    ;;
  *)
    die "usage: capture <review-dir> | publish <review-dir> <output-dir> APPROVE_REDUCED_SUMMARIES"
    ;;
esac

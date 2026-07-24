#!/usr/bin/env bash
# Credential-free HTTP/security serve-smoke for the staging-e2e CI job.
#
# Drives a RUNNING daemon and asserts the served contract on a real build: the
# app-shell status + CSP, the HttpOnly loopback-session handoff, the static-asset
# hardening headers, the items JSON shape, and a clean 404 (not 500) for a missing
# body. No browser or Microsoft-365 token is involved.
#
# Usage:  tools/serve-smoke.sh [URL] [ACCOUNT]   (defaults: http://127.0.0.1:8869/ fixture)
# Exit non-zero on the first failed assertion set.
set -uo pipefail
URL="${1:-http://127.0.0.1:8869/}"; URL="${URL%/}"
ACCT="${2:-fixture}"
COOKIE_JAR=$(mktemp "${TMPDIR:-/tmp}/isyncyou-serve-smoke.XXXXXX")
trap 'rm -f "$COOKIE_JAR"' EXIT
PASS=0; FAIL=0
check() { # name, rc(0=pass), extra
  if [ "$2" -eq 0 ]; then echo "  PASS  $1"; PASS=$((PASS + 1));
  else echo "  FAIL  $1 ${3:-}"; FAIL=$((FAIL + 1)); fi
}

# 1. GET / -> 200 + CSP and the HttpOnly cookie used for subsequent loopback API calls.
hdr=$(curl -sS -c "$COOKIE_JAR" -D - -o /dev/null "$URL/")
grep -qE '^HTTP/[0-9.]+ 200' <<<"$hdr"; check "GET / -> 200" $?
grep -qiE '^content-security-policy:' <<<"$hdr"; check "GET / sends Content-Security-Policy" $?
grep -qiE '^set-cookie:[[:space:]]*isy_session=.*HttpOnly' <<<"$hdr"; check "GET / sets HttpOnly API session cookie" $?

# 2. /app.js -> 200 + JS content-type + no-store + nosniff (#79/#72 hardening).
ajs=$(curl -sS -D - -o /dev/null "$URL/app.js")
grep -qiE '^content-type:[[:space:]]*(application|text)/javascript' <<<"$ajs"; check "/app.js content-type is javascript" $?
grep -qiE '^cache-control:.*no-store' <<<"$ajs"; check "/app.js Cache-Control: no-store" $?
grep -qiE '^x-content-type-options:[[:space:]]*nosniff' <<<"$ajs"; check "/app.js X-Content-Type-Options: nosniff" $?

# 3. items -> 200 + a valid JSON shape carrying the seeded mail message.
items=$(curl -sS -b "$COOKIE_JAR" "$URL/api/v1/items?account=$ACCT&service=mail&limit=10")
python3 - "$items" <<'PY'
import json, sys
d = json.loads(sys.argv[1])
items = d if isinstance(d, list) else d.get("items", [])
sys.exit(0 if any(i.get("item_type") == "message" for i in items) else 1)
PY
check "items?service=mail returns a message" $?

# 4. /api/v1/view for a missing id -> 404 (a graceful not-found, never a 500).
code=$(curl -sS -b "$COOKIE_JAR" -o /dev/null -w '%{http_code}' "$URL/api/v1/view?account=$ACCT&service=mail&id=does-not-exist")
[ "$code" = "404" ]; check "/api/v1/view missing id -> 404" $? "(got $code)"

echo "== serve-smoke: $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]

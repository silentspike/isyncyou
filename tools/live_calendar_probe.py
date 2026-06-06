#!/usr/bin/env python3
"""Live calendar restore-marker probe (ADR-001).

Confirms the one assumption the crash-safe **calendar** restore depends on but
cannot be proven offline: that Microsoft Graph **honours a posted `transactionId`**
on `POST /me/events` and **de-duplicates server-side** — re-POSTing the same
`transactionId` returns the *same* event id, never a second event. That idempotent
re-POST is exactly how `recover_restore_op` reconciles a calendar restore after a
crash (instead of duplicating).

It also records, for documentation, that a `transactionId` `$filter` query is **not**
supported by Graph (HTTP 400) — which is why calendar recovery relies on the
idempotent re-POST rather than a marker probe (unlike mail's internetMessageId).

It is a *diagnostic*, not part of the build: it makes a real Graph round-trip
against a **throwaway test account** and cleans up after itself (creates an event,
finds it by transactionId, deletes it). It never prints the access token.

Requires a write token for the test account, from either:
  * the env var ISYNCYOU_TEST_WRITE_TOKEN, or
  * a cached msal token at ~/.config/m365-write/token_cache.json (silent refresh).

Usage:
    python3 tools/live_calendar_probe.py
Exit code 0 = marker is findable (probe holds); non-zero otherwise.
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

GRAPH = "https://graph.microsoft.com/v1.0"
# Public OAuth client id for the write app (no secret; same value the CLI uses).
WRITE_CLIENT = "a90d9140-3a62-46d0-907b-f2b7b61a573a"
AUTHORITY = "https://login.microsoftonline.com/consumers"
CACHE = os.path.expanduser("~/.config/m365-write/token_cache.json")


def get_token() -> tuple[str, str | None]:
    """Return (access_token, account_username). Never logs the token."""
    env = os.environ.get("ISYNCYOU_TEST_WRITE_TOKEN")
    if env:
        return env, None
    try:
        import msal
    except ImportError:
        sys.exit("need ISYNCYOU_TEST_WRITE_TOKEN or the `msal` package + a cached token")
    if not os.path.exists(CACHE):
        sys.exit(f"no token: set ISYNCYOU_TEST_WRITE_TOKEN or cache one at {CACHE}")
    cache = msal.SerializableTokenCache()
    cache.deserialize(open(CACHE).read())
    app = msal.PublicClientApplication(WRITE_CLIENT, authority=AUTHORITY, token_cache=cache)
    accounts = app.get_accounts()
    if not accounts:
        sys.exit("cached token has no account; an interactive login is needed")
    res = app.acquire_token_silent(["Calendars.ReadWrite"], account=accounts[0])
    if not res or "access_token" not in res:
        sys.exit("silent token acquisition failed; an interactive login is needed")
    return res["access_token"], accounts[0].get("username")


def req(token: str, method: str, url: str, data=None, ctype=None):
    r = urllib.request.Request(url, data=data, method=method)
    r.add_header("Authorization", "Bearer " + token)
    if ctype:
        r.add_header("Content-Type", ctype)
    try:
        with urllib.request.urlopen(r) as resp:
            body = resp.read().decode()
            return resp.status, (json.loads(body) if body else {})
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:200]


def main() -> int:
    token, who = get_token()
    print(f"account: {who or '(from env token)'}")
    print("write token acquired  [token NOT shown]")

    ts = int(time.time())
    # Mirrors crates/engine/src/restore_key.rs::calendar_marker — the value the real
    # restore stamps and the recovery probe later searches for.
    txid = f"isyncyou-restore-probe-{ts}"
    event = {
        "subject": f"iSyncYou marker probe {ts}",
        "start": {"dateTime": "2026-07-01T09:00:00", "timeZone": "UTC"},
        "end": {"dateTime": "2026-07-01T10:00:00", "timeZone": "UTC"},
        "transactionId": txid,
    }
    body = json.dumps(event).encode()

    st, created = req(token, "POST", f"{GRAPH}/me/events", body, "application/json")
    print(f"POST /me/events -> {st}")
    if st not in (200, 201):
        print(f"  create failed: {created}")
        return 1
    cid = created["id"]
    print(f"  transactionId on created object: {created.get('transactionId')}")

    # The crash-safety mechanism: re-POST the same transactionId -> Graph must return
    # the SAME id (de-dup), not create a second event. This is what makes recovery safe.
    st_dup, dup = req(token, "POST", f"{GRAPH}/me/events", body, "application/json")
    dup_id = dup.get("id") if isinstance(dup, dict) else None
    dedup_ok = st_dup in (200, 201) and dup_id == cid
    print(f"re-POST same transactionId -> {st_dup} | same id: {dedup_ok}")
    # If Graph created a distinct event despite the transactionId, clean that up too.
    if isinstance(dup, dict) and dup_id and dup_id != cid:
        req(token, "DELETE", f"{GRAPH}/me/events/{dup_id}")

    # Informational only: confirm Graph rejects a transactionId $filter (so we document
    # why recovery uses the idempotent re-POST instead of a marker probe). Not part of
    # the pass/fail verdict.
    time.sleep(1)
    flt = urllib.parse.quote(f"transactionId eq '{txid}'")
    st2, _ = req(token, "GET", f"{GRAPH}/me/events?$filter={flt}&$select=id&$top=1")
    filter_supported = st2 in (200,)
    print(f"GET $filter transactionId -> {st2} (supported: {filter_supported})")

    st3, _ = req(token, "DELETE", f"{GRAPH}/me/events/{cid}")
    print(f"cleanup DELETE -> {st3}")

    print(f"PROBE VERDICT: transactionId server-side de-dup on re-POST: {'YES' if dedup_ok else 'NO'}")
    print(f"NOTE: transactionId $filter query supported by Graph: {'YES' if filter_supported else 'NO (HTTP %s)' % st2}")
    return 0 if dedup_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Live restore-marker probe (ADR-001).

Confirms the one assumption the crash-safe mail restore depends on but cannot be
proven offline: that Microsoft Graph **preserves a posted `Message-ID`** as the
created message's `internetMessageId`, and that an `internetMessageId` `$filter`
finds it again. That is exactly the marker probe `recover_restore_op` uses to
reconcile (instead of duplicating) after a crash.

It is a *diagnostic*, not part of the build: it makes a real Graph round-trip
against a **throwaway test account** and cleans up after itself (creates a draft,
finds it by marker, deletes it). It never prints the access token.

Requires a write token for the test account, from either:
  * the env var ISYNCYOU_TEST_WRITE_TOKEN, or
  * a cached msal token at ~/.config/m365-write/token_cache.json (silent refresh).

Usage:
    python3 tools/live_restore_probe.py
Exit code 0 = marker is findable (probe holds); non-zero otherwise.
"""

from __future__ import annotations

import base64
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
    res = app.acquire_token_silent(["Mail.ReadWrite"], account=accounts[0])
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
    mid = f"<isyncyou-probe-{ts}@restore.invalid>"
    addr = who or "me"
    mime = (
        f"Message-ID: {mid}\r\nSubject: iSyncYou marker probe {ts}\r\n"
        f"From: {addr}\r\nTo: {addr}\r\nContent-Type: text/plain\r\n\r\nprobe"
    ).encode()

    st, created = req(token, "POST", f"{GRAPH}/me/messages", base64.b64encode(mime), "text/plain")
    print(f"POST /me/messages -> {st}")
    if st not in (200, 201):
        print(f"  create failed: {created}")
        return 1
    cid = created["id"]
    print(f"  internetMessageId on created object: {created.get('internetMessageId')}")

    time.sleep(2)  # let it propagate
    flt = urllib.parse.quote(f"internetMessageId eq '{mid}'")
    st2, found = req(token, "GET", f"{GRAPH}/me/messages?$filter={flt}&$select=id&$top=1")
    matches = found.get("value", []) if isinstance(found, dict) else []
    ok = bool(matches) and matches[0].get("id") == cid
    print(f"GET $filter internetMessageId -> {st2} | matches: {len(matches)}")

    st3, _ = req(token, "DELETE", f"{GRAPH}/me/messages/{cid}")
    print(f"cleanup DELETE -> {st3}")

    print(f"PROBE VERDICT: marker findable by internetMessageId $filter: {'YES' if ok else 'NO'}")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

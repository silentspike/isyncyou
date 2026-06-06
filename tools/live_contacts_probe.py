#!/usr/bin/env python3
"""Live contacts restore-marker probe (ADR-001) — DESIGN DE-RISK for #282.

#282 proposes the mail-shaped design for contacts: a non-idempotent
`POST /me/contacts` made crash-safe by a **single-value extended property** carrying
the idempotency key, found again via `$filter` on `singleValueExtendedProperties`.

That design hinges on two Graph capabilities this probe confirms (or refutes) against
a **throwaway test account** before any code is written — exactly the lesson from
calendar, where the assumed `transactionId` `$filter` turned out unsupported:

  1. POST /me/contacts accepts a singleValueExtendedProperties entry, and
  2. GET /me/contacts?$filter=singleValueExtendedProperties/any(...) finds it back.

Cleans up after itself (creates a contact, finds it, deletes it). Never logs the token.

Requires a write token for the test account, from either:
  * the env var ISYNCYOU_TEST_WRITE_TOKEN, or
  * a cached msal token at ~/.config/m365-write/token_cache.json (silent refresh).

Usage:
    python3 tools/live_contacts_probe.py
Exit 0 = both capabilities hold (mail-shaped design is valid for contacts).
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
WRITE_CLIENT = "a90d9140-3a62-46d0-907b-f2b7b61a573a"
AUTHORITY = "https://login.microsoftonline.com/consumers"
CACHE = os.path.expanduser("~/.config/m365-write/token_cache.json")
# A stable, namespaced extended-property id (String type, custom GUID + name).
PROP_ID = "String {f3f9a7b1-6f1e-4a2b-9c3d-1e2f3a4b5c6d} Name isyncyou-restore-key"


def get_token() -> tuple[str, str | None]:
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
    res = app.acquire_token_silent(["Contacts.ReadWrite"], account=accounts[0])
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
        return e.code, e.read().decode()[:300]


def main() -> int:
    token, who = get_token()
    print(f"account: {who or '(from env token)'}")
    print("write token acquired  [token NOT shown]")

    ts = int(time.time())
    marker = f"isyncyou-restore-probe-{ts}"
    contact = {
        "givenName": "iSyncYou",
        "surname": f"Probe {ts}",
        "singleValueExtendedProperties": [{"id": PROP_ID, "value": marker}],
    }
    body = json.dumps(contact).encode()

    st, created = req(token, "POST", f"{GRAPH}/me/contacts", body, "application/json")
    print(f"POST /me/contacts -> {st}")
    if st not in (200, 201):
        print(f"  create failed: {created}")
        return 1
    cid = created["id"]
    create_ok = True

    time.sleep(2)  # let it propagate
    flt = urllib.parse.quote(
        f"singleValueExtendedProperties/any(ep: ep/id eq '{PROP_ID}' and ep/value eq '{marker}')"
    )
    st2, found = req(token, "GET", f"{GRAPH}/me/contacts?$filter={flt}&$select=id&$top=1")
    matches = found.get("value", []) if isinstance(found, dict) else []
    filter_ok = st2 in (200,) and bool(matches) and matches[0].get("id") == cid
    print(f"GET $filter singleValueExtendedProperties -> {st2} | matches: {len(matches) if isinstance(matches, list) else 'n/a'}")
    if not filter_ok and not isinstance(found, dict):
        print(f"  filter response: {found}")

    st3, _ = req(token, "DELETE", f"{GRAPH}/me/contacts/{cid}")
    print(f"cleanup DELETE -> {st3}")

    ok = create_ok and filter_ok
    print(f"PROBE VERDICT: contact create accepts extended property: {'YES' if create_ok else 'NO'}")
    print(f"PROBE VERDICT: extended-property $filter finds it: {'YES' if filter_ok else 'NO'}")
    print(f"=> mail-shaped contacts design valid: {'YES' if ok else 'NO'}")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Live ToDo restore-marker probe (ADR-001) — DESIGN DE-RISK for #283.

Microsoft To Do tasks live under /me/todo (a different API from Outlook mail/
events/contacts). Before choosing a crash-safe restore design, this probe establishes
empirically — against a throwaway test account — which findable-marker mechanism, if
any, ToDo actually supports. It tests, in order of preference:

  H1 (mail-shape): does a todoTask accept singleValueExtendedProperties on create,
     and can a $filter on it find the task back?  (best: ledger + marker probe)
  H2 (filterable field): can we $filter tasks on a stable field (e.g. body/content)?
  H3 (scan fallback): does the marker round-trip in the task body so recovery can
     LIST tasks and scan for it?  (weakest probe)

Whatever holds dictates #283's design (or, if only H3, whether ToDo restore ships or
stays refused). Cleans up after itself. Never logs the token.

Requires a write token (Tasks.ReadWrite) for the test account via
ISYNCYOU_TEST_WRITE_TOKEN or the cached msal token at ~/.config/m365-write/token_cache.json.

Usage:
    python3 tools/live_todo_probe.py
Exit 0 = at least one usable marker mechanism (H1 or H2) holds.
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
    res = app.acquire_token_silent(["Tasks.ReadWrite"], account=accounts[0])
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

    st, lists = req(token, "GET", f"{GRAPH}/me/todo/lists?$top=1")
    if st != 200 or not isinstance(lists, dict) or not lists.get("value"):
        print(f"could not list /me/todo/lists -> {st}: {lists}")
        return 1
    list_id = lists["value"][0]["id"]
    list_name = lists["value"][0].get("displayName")
    print(f"target list: {list_name!r}")

    ts = int(time.time())
    marker = f"isyncyou-restore-probe-{ts}"

    # --- H1: extended property on a todoTask + $filter -----------------------------
    h1_create_ok = h1_filter_ok = False
    task = {
        "title": f"iSyncYou probe {ts}",
        "body": {"content": f"marker:{marker}", "contentType": "text"},
        "singleValueExtendedProperties": [{"id": PROP_ID, "value": marker}],
    }
    st, created = req(
        token, "POST", f"{GRAPH}/me/todo/lists/{list_id}/tasks",
        json.dumps(task).encode(), "application/json",
    )
    print(f"H1 POST task (with extended property) -> {st}")
    tid = None
    if st in (200, 201) and isinstance(created, dict):
        tid = created.get("id")
        h1_create_ok = "singleValueExtendedProperties" in created or tid is not None
        time.sleep(2)
        flt = urllib.parse.quote(
            f"singleValueExtendedProperties/any(ep: ep/id eq '{PROP_ID}' and ep/value eq '{marker}')"
        )
        st_f, found = req(
            token, "GET",
            f"{GRAPH}/me/todo/lists/{list_id}/tasks?$filter={flt}&$top=1",
        )
        matches = found.get("value", []) if isinstance(found, dict) else []
        h1_filter_ok = st_f == 200 and bool(matches) and matches[0].get("id") == tid
        print(f"H1 $filter extended property -> {st_f} | matches: {len(matches) if isinstance(matches, list) else 'n/a'}")
        if st_f != 200:
            print(f"   (filter not supported: {found})")
    else:
        # maybe the extended property made the create fail — retry without it (for H2/H3)
        print(f"   create-with-extprop failed: {created}")
        task.pop("singleValueExtendedProperties", None)
        st, created = req(
            token, "POST", f"{GRAPH}/me/todo/lists/{list_id}/tasks",
            json.dumps(task).encode(), "application/json",
        )
        print(f"   POST task (no extended property) -> {st}")
        tid = created.get("id") if isinstance(created, dict) else None

    # --- H3: does the marker round-trip in the body, so a LIST scan can find it? ----
    h3_scan_ok = False
    if tid:
        time.sleep(1)
        st_l, listed = req(
            token, "GET",
            f"{GRAPH}/me/todo/lists/{list_id}/tasks?$top=50",
        )
        if st_l == 200 and isinstance(listed, dict):
            for t in listed.get("value", []):
                body = (t.get("body") or {}).get("content", "")
                if marker in body and t.get("id") == tid:
                    h3_scan_ok = True
                    break
        print(f"H3 LIST+scan body for marker -> {st_l} | found: {h3_scan_ok}")

    if tid:
        st_d, _ = req(token, "DELETE", f"{GRAPH}/me/todo/lists/{list_id}/tasks/{tid}")
        print(f"cleanup DELETE -> {st_d}")

    print(f"PROBE VERDICT: H1 extended-property create accepted: {'YES' if h1_create_ok else 'NO'}")
    print(f"PROBE VERDICT: H1 extended-property $filter finds it: {'YES' if h1_filter_ok else 'NO'}")
    print(f"PROBE VERDICT: H3 body marker round-trips + scannable: {'YES' if h3_scan_ok else 'NO'}")
    if h1_filter_ok:
        print("=> DESIGN: mail-shape (ledger + extended-property $filter probe)")
    elif h3_scan_ok:
        print("=> DESIGN: ledger + LIST-scan marker probe (weakest, but crash-safe)")
    else:
        print("=> DESIGN: no findable marker -> ToDo restore must stay REFUSED or use another mechanism")
    return 0 if (h1_filter_ok or h3_scan_ok) else 1


if __name__ == "__main__":
    raise SystemExit(main())

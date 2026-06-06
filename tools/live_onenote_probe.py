#!/usr/bin/env python3
"""Live OneNote restore-marker probe (ADR-001) — DESIGN DE-RISK for #284.

OneNote is the weakest case. Before deciding whether ToDo-style ledger restore is even
possible for OneNote — or whether it must stay REFUSED for RC-0.10 — this probe
establishes empirically, against a throwaway test account, which findable-marker
mechanism (if any) a created page supports:

  H1 (title $filter):   create a page whose <title> carries the marker; can a
                        GET /me/onenote/pages?$filter=title eq '<marker>' find it?
                        (deterministic, but the marker is a visible page title)
  H2 (body-comment scan): embed the marker as an HTML comment in the page body
                        (invisible), then LIST pages and GET each page's /content,
                        scanning for the comment. (invisible, but O(n) content fetches)

Whatever holds dictates #284 (or, if neither is workable/clean, the honest outcome:
keep OneNote cloud restore REFUSED — restore --to-local still works). Cleans up after
itself. Never logs the token.

Requires a write token (Notes.ReadWrite) for the test account via
ISYNCYOU_TEST_WRITE_TOKEN or the cached msal token at ~/.config/m365-write/token_cache.json.

Usage:
    python3 tools/live_onenote_probe.py
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
    res = app.acquire_token_silent(["Notes.ReadWrite"], account=accounts[0])
    if not res or "access_token" not in res:
        sys.exit("silent token acquisition failed; an interactive login is needed")
    return res["access_token"], accounts[0].get("username")


def req(token: str, method: str, url: str, data=None, ctype=None, raw=False):
    r = urllib.request.Request(url, data=data, method=method)
    r.add_header("Authorization", "Bearer " + token)
    if ctype:
        r.add_header("Content-Type", ctype)
    try:
        with urllib.request.urlopen(r) as resp:
            body = resp.read().decode()
            if raw:
                return resp.status, body
            return resp.status, (json.loads(body) if body else {})
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:300]


def main() -> int:
    token, who = get_token()
    print(f"account: {who or '(from env token)'}")
    print("write token acquired  [token NOT shown]")

    st, secs = req(token, "GET", f"{GRAPH}/me/onenote/sections?$top=1")
    if st != 200 or not isinstance(secs, dict) or not secs.get("value"):
        print(f"could not list /me/onenote/sections -> {st}: {secs}")
        print("=> cannot probe OneNote create; treat as REFUSED until a section exists")
        return 1
    section_id = secs["value"][0]["id"]
    print(f"target section: {secs['value'][0].get('displayName')!r}")

    ts = int(time.time())
    marker = f"isyncyou-restore-probe-{ts}"
    html = (
        "<!DOCTYPE html><html><head>"
        f"<title>{marker}</title></head>"
        f"<body><!--{marker}--><p>iSyncYou OneNote probe {ts}</p></body></html>"
    )
    st, created = req(
        token, "POST", f"{GRAPH}/me/onenote/sections/{section_id}/pages",
        html.encode(), "text/html",
    )
    print(f"POST page -> {st}")
    if st not in (200, 201) or not isinstance(created, dict):
        print(f"  create failed: {created}")
        return 1
    pid = created.get("id")
    print(f"  created page id: {pid}")

    time.sleep(5)  # OneNote create is eventually-consistent

    # --- H1: $filter on title -----------------------------------------------------
    flt = urllib.parse.quote(f"title eq '{marker}'")
    st1, found = req(token, "GET", f"{GRAPH}/me/onenote/pages?$filter={flt}&$select=id&$top=1")
    matches = found.get("value", []) if isinstance(found, dict) else []
    h1_ok = st1 == 200 and bool(matches) and matches[0].get("id") == pid
    print(f"H1 $filter title -> {st1} | matches: {len(matches) if isinstance(matches, list) else 'n/a'}")
    if st1 != 200:
        print(f"   (title filter not supported: {found})")

    # --- H2: list pages + scan each page's content for the comment marker ---------
    h2_ok = False
    st_l, listed = req(token, "GET", f"{GRAPH}/me/onenote/pages?$select=id&$top=50")
    if st_l == 200 and isinstance(listed, dict):
        for p in listed.get("value", []):
            st_c, content = req(token, "GET", f"{GRAPH}/me/onenote/pages/{p['id']}/content", raw=True)
            if st_c == 200 and isinstance(content, str) and marker in content and p["id"] == pid:
                h2_ok = True
                break
    print(f"H2 LIST + per-page content scan -> list {st_l} | found: {h2_ok}")

    if pid:
        st_d, _ = req(token, "DELETE", f"{GRAPH}/me/onenote/pages/{pid}")
        print(f"cleanup DELETE -> {st_d}")

    print(f"PROBE VERDICT: H1 title $filter finds page: {'YES' if h1_ok else 'NO'}")
    print(f"PROBE VERDICT: H2 body-comment + content scan finds page: {'YES' if h2_ok else 'NO'}")
    if h1_ok:
        print("=> DESIGN: ledger + title-$filter probe (visible title marker)")
    elif h2_ok:
        print("=> DESIGN: ledger + LIST + per-page content-scan probe (invisible, O(n) fetches)")
    else:
        print("=> DESIGN: no workable marker probe -> keep OneNote cloud restore REFUSED for RC-0.10")
    return 0 if (h1_ok or h2_ok) else 1


if __name__ == "__main__":
    raise SystemExit(main())

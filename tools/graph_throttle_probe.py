#!/usr/bin/env python3
"""S-P4.0 (#557) — consumer Graph throttling-floor probe.

Sends a measured, capped burst of cheap GETs (`/me`) and records how many succeed
before a `429 Too Many Requests` (and the `Retry-After` value), to define a safe
floor for the 1 s polling-slider setting. Stops at the first 429. READ-ONLY: only
GET requests, honours Retry-After, never logs tokens.
"""
import os, sys, time, json, urllib.request, urllib.error
import msal

GRAPH = "https://graph.microsoft.com/v1.0"
AUTH = "https://login.microsoftonline.com/consumers"
READ_CLIENT = "cee80dd9-c13e-4dbb-9d4c-73eb4987d447"
READ_CACHE = os.path.expanduser("~/.config/m365-read/token_cache.json")
SCOPES = ["User.Read", "Mail.Read"]
CAP = 300  # hard cap on burst size (safety on a throwaway account)


def token():
    c = msal.SerializableTokenCache()
    c.deserialize(open(READ_CACHE).read())
    app = msal.PublicClientApplication(READ_CLIENT, authority=AUTH, token_cache=c)
    acc = app.get_accounts()[0]
    r = app.acquire_token_silent(SCOPES, account=acc)
    if not r or "access_token" not in r:
        raise SystemExit("token acquisition failed")
    return r["access_token"]


T = token()
URL = f"{GRAPH}/me"


def one():
    """One GET. Returns (status, retry_after_or_None)."""
    req = urllib.request.Request(URL, headers={"Authorization": "Bearer " + T})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            resp.read(64)
            return resp.status, None
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Retry-After")
    except Exception as e:
        return "ERR:" + str(e)[:40], None


def main():
    print("=" * 64)
    print(f"THROTTLING FLOOR — rapid GET {URL} (cap {CAP})")
    print("=" * 64)
    t0 = time.monotonic()
    ok = 0
    for i in range(1, CAP + 1):
        st, retry = one()
        if st == 429:
            elapsed = time.monotonic() - t0
            print(f"  429 at request #{i} after {elapsed:.2f}s "
                  f"({ok} OK before it); Retry-After={retry}")
            print(f"\n  RESULT: throttled after {ok} rapid requests "
                  f"(~{ok/elapsed:.0f} req/s); honour Retry-After={retry}.")
            return
        if st == 200:
            ok += 1
        else:
            print(f"  request #{i}: unexpected status {st}")
    elapsed = time.monotonic() - t0
    rate = CAP / elapsed if elapsed else 0
    print(f"  {CAP} rapid requests, NO 429. elapsed={elapsed:.2f}s (~{rate:.0f} req/s).")
    print(f"\n  RESULT: no short-window throttle hit at ~{rate:.0f} req/s burst.")
    print("  => 1 s polling (1 req/s per service) is far under any limit; safe.")
    print("     (Backoff on 429/Retry-After remains mandatory regardless.)")


if __name__ == "__main__":
    main()

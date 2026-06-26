#!/usr/bin/env python3
"""S-P4.0 (#557) — consumer Graph delta-availability probe + drive enumeration.

For each of the six services, issue the INITIAL delta call and report whether the
consumer/family account returns a usable delta stream (HTTP 200 with an
`@odata.deltaLink` or `@odata.nextLink`) vs an error — this decides each service's
near-real-time push strategy (delta vs receivedDateTime-pagination).

`--drives` instead enumerates `/me/drives` (the account has more than the default)
and classifies each as user-data vs app/system.

READ-ONLY: only GET requests, never mutates cloud data, never logs tokens.
"""
import os, sys, json, urllib.request, urllib.error
import msal

GRAPH = "https://graph.microsoft.com/v1.0"
AUTH = "https://login.microsoftonline.com/consumers"
READ_CLIENT = "cee80dd9-c13e-4dbb-9d4c-73eb4987d447"
READ_CACHE = os.path.expanduser("~/.config/m365-read/token_cache.json")
SCOPES = ["Mail.Read", "Calendars.Read", "Contacts.Read", "Files.Read.All",
          "Tasks.Read", "Notes.Read.All", "MailboxSettings.Read", "People.Read", "User.Read"]


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


def get(url):
    """GET only. Returns (status, parsed-json-or-None, error-code)."""
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + T})
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return resp.status, json.loads(resp.read()), ""
    except urllib.error.HTTPError as e:
        code = ""
        try:
            code = json.loads(e.read(500)).get("error", {}).get("code", "")
        except Exception:
            pass
        return e.code, None, code
    except Exception as e:
        return "ERR", None, str(e)[:60]


def first_id(url):
    st, j, _ = get(url)
    if st == 200 and isinstance(j, dict) and j.get("value"):
        return j["value"][0].get("id")
    return None


def probe_delta(label, url):
    st, j, err = get(url)
    if st == 200 and isinstance(j, dict):
        has_delta = "@odata.deltaLink" in j
        has_next = "@odata.nextLink" in j
        n = len(j.get("value", []))
        if has_delta or has_next:
            kind = "deltaLink" if has_delta else "nextLink"
            print(f"  [200] {label:<26} SUPPORTED ({kind}, {n} item(s) page 1)")
        else:
            print(f"  [200] {label:<26} 200 but NO delta/next link ({n} items) — check")
    else:
        print(f"  [{st}] {label:<26} NOT supported ({err})")


def run_delta():
    print("=" * 72)
    print("PER-SERVICE DELTA AVAILABILITY (consumer/family account)")
    print("=" * 72)
    # Mail — per-folder (well-known 'inbox' acts as a folder id)
    probe_delta("mail (inbox/messages)", f"{GRAPH}/me/mailFolders/inbox/messages/delta")
    # Calendar — calendarView/delta needs a date window
    cal = first_id(f"{GRAPH}/me/calendars?$top=1")
    if cal:
        probe_delta("calendar (calendarView)",
                    f"{GRAPH}/me/calendars/{cal}/calendarView/delta"
                    f"?startDateTime=2024-01-01T00:00:00Z&endDateTime=2027-01-01T00:00:00Z")
    else:
        print("  [--] calendar                    no calendar found")
    # Contacts
    probe_delta("contacts", f"{GRAPH}/me/contacts/delta")
    # ToDo — per list
    lst = first_id(f"{GRAPH}/me/todo/lists?$top=1")
    if lst:
        probe_delta("todo (tasks)", f"{GRAPH}/me/todo/lists/{lst}/tasks/delta")
    else:
        print("  [--] todo                        no list found")
    # OneDrive
    probe_delta("onedrive (root)", f"{GRAPH}/me/drive/root/delta")
    # OneNote — no delta endpoint for pages
    print(f"  [n/a] {'onenote (pages)':<26} no delta endpoint (flat list only)")


def run_drives():
    print("=" * 72)
    print("DRIVES ENUMERATION (/me/drives)")
    print("=" * 72)
    st, j, err = get(f"{GRAPH}/me/drives")
    if st != 200 or not isinstance(j, dict):
        print(f"  [{st}] /me/drives failed ({err})")
        return
    for d in j.get("value", []):
        did = d.get("id", "?")
        dt = d.get("driveType", "?")
        name = d.get("name", "?")
        owner = (((d.get("owner") or {}).get("user") or {}).get("displayName")
                 or ((d.get("owner") or {}).get("user") or {}).get("email") or "?")
        q = d.get("quota", {}) or {}
        used = q.get("used")
        total = q.get("total")
        print(f"\n  drive: {name}  (driveType={dt}, owner={owner})")
        print(f"    id: {did}")
        print(f"    quota: used={used} total={total} state={q.get('state')}")
        st2, j2, err2 = get(f"{GRAPH}/me/drives/{did}/root/children?$top=5&$select=name,folder,file,size")
        if st2 == 200 and isinstance(j2, dict):
            kids = j2.get("value", [])
            if kids:
                for k in kids:
                    kind = "DIR " if "folder" in k else "file"
                    print(f"      - {kind} {k.get('name')}")
            else:
                print("      (empty root)")
        else:
            print(f"      root children: [{st2}] {err2}")


if __name__ == "__main__":
    if "--drives" in sys.argv:
        run_drives()
    else:
        run_delta()

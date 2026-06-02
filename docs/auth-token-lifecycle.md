# Auth & token lifecycle

OAuth for personal Microsoft accounts (plan §11), implemented in
`crates/graph/src/auth.rs` and wired into the CLI (`isyncyou login` + automatic
token resolution).

## Apps & scopes

Personal accounts use the `consumers` authority
(`https://login.microsoftonline.com/consumers/oauth2/v2.0`) and **public clients**
(no secret). Two app registrations keep least privilege:

| App | Client id | Scopes |
|---|---|---|
| read | `cee80dd9-…` | `Files.Read`, `Mail.Read`, `Calendars.Read`, `Contacts.Read`, `Tasks.Read`, `Notes.Read`, `offline_access` |
| write | `a90d9140-…` | `Files.ReadWrite`, `Mail.ReadWrite`, `Mail.Send`, `Calendars.ReadWrite`, `Contacts.ReadWrite`, `Tasks.ReadWrite`, `offline_access` |

Backup/sync/search use the **read** app; restore uses the **write** app (connected
only when needed — incremental consent).

## Flow

1. **Device-code login** (`flow::device_code_login`) — the one interactive step.
   `isyncyou login --account X [--write]` shows the code, polls the token endpoint
   until authorized, and writes a `TokenCache` to
   `<archive_root>/.isyncyou-token-{read,write}.json`.
2. **Cached use** — `flow::ensure_access_token(cache, client_id, scopes, now)` is
   the non-interactive path every run uses: returns the cached access token if
   still valid, else refreshes it (`refresh_token` grant) and saves the renewed
   cache; the refresh token is retained when Graph doesn't roll a new one.
3. **CLI resolution** — `--token`/`ISYNCYOU_TOKEN` always wins (for CI/live tests);
   otherwise the per-account cached token is resolved + refreshed automatically.

## TokenCache

`{ access_token, refresh_token, expires_at }`, persisted as JSON. `expires_at` is
set a minute early for safety; `is_access_valid(now)` gates refresh.

## Expiry / invalidation (a normal operating state)

Refresh tokens follow a rolling ~90-day inactivity window; password change or
explicit revoke also invalidates them (`AADSTS70000 invalid_grant`). On the
desktop this surfaces as a re-login prompt; headless, the daemon pauses + reports +
points at `isyncyou login`. Token files are written with `serde_json` to the
account's archive dir (KDE-Keyring / encrypted-file storage is the §11 hardening
follow-up); tokens are never logged.

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
   `<archive_root>/.isyncyou-token-{read,write}.json`. With `--keyring`, the
   actual token JSON is stored in the desktop Secret Service / KDE Wallet
   compatible keyring and that file contains only a non-secret marker pointing to
   the keyring entry. Without `--keyring`, the file is written owner-only (`0600`)
   on Unix, including when replacing an older cache with looser permissions. If a
   token-cache secret is configured, the file is an encrypted JSON envelope
   instead of plaintext.
2. **Cached use** — `flow::ensure_access_token(cache, client_id, scopes, now)` is
   the non-interactive path every run uses: returns the cached access token if
   still valid, else refreshes it (`refresh_token` grant) and saves the renewed
   cache; the refresh token is retained when Graph doesn't roll a new one.
3. **CLI resolution** — `--token`/`ISYNCYOU_TOKEN` always wins (for CI/live tests);
   otherwise the per-account cached token is resolved + refreshed automatically.

## TokenCache

Plain structure: `{ access_token, refresh_token, expires_at }`. `expires_at` is
set a minute early for safety; `is_access_valid(now)` gates refresh.

Desktop at-rest protection:

- `isyncyou login --keyring` writes the token JSON into the desktop keyring using
  service `org.silentspike.isyncyou.token-cache`; the archive-root cache file is a
  `isyncyou-token-cache-keyring-v1` marker and does not contain `access_token` or
  `refresh_token`.
- `TokenCache::load` auto-detects keyring markers, reads the keyring entry, and
  returns the normal `TokenCache`. A later refresh through `TokenCache::save`
  preserves the keyring backend instead of rewriting the token to disk.

Headless/file-cache at-rest protection is automatic when one of these secret
sources is present, in priority order:

1. `ISYNCYOU_TOKEN_CACHE_KEY_FILE` — path to a file containing the token-cache
   secret.
2. systemd credential `isyncyou-token-cache-key` under `$CREDENTIALS_DIRECTORY`.
3. `ISYNCYOU_TOKEN_CACHE_KEY` — fallback for development/CI; prefer file or
   systemd credentials for real services.

With a secret, `TokenCache::save` writes an encrypted envelope:

- AEAD: `AES-256-GCM`.
- KDF: `PBKDF2-HMAC-SHA256`, 210000 iterations, random 16-byte salt.
- Nonce: random 12 bytes per save.
- Payload: the plaintext `TokenCache` JSON; `access_token` and `refresh_token`
  do not appear in the file.

`TokenCache::load` auto-detects encrypted envelopes and keyring markers. Encrypted
envelopes fail closed with `PermissionDenied` if the required secret is
unavailable or wrong. Existing plaintext caches remain loadable so a host can
migrate by setting the secret and letting the next refresh/login rewrite the file
encrypted, or by re-running `isyncyou login --keyring` on a desktop session.

## Expiry / invalidation (a normal operating state)

Refresh tokens follow a rolling ~90-day inactivity window; password change or
explicit revoke also invalidates them (`AADSTS70000 invalid_grant`). On the
desktop this surfaces as a re-login prompt; headless, the daemon pauses + reports +
points at `isyncyou login`. Token files are owner-only on Unix, can be
credential-encrypted as above, or can be replaced with desktop-keyring marker
files via `--keyring`. SQLite-store encryption is separate from token-cache
encryption and is controlled by `ISYNCYOU_STORE_KEY_FILE`, systemd credential
`isyncyou-store-key`, or `ISYNCYOU_STORE_KEY`; tokens are never logged.

# Local API security

The local web UI/API (plan §11, §25) is implemented in `gui/webui` and served by
`isyncyoud` (or `isyncyou serve`). It is a **read-only**, localhost-by-default
surface today; the destructive (restore/job) actions stay on the CLI until
capability-token auth lands.

## Current properties

- **Localhost bind by default** (`127.0.0.1:8765`); the bind address is explicit,
  so it is never exposed unintentionally.
- **GET-only, no body** — the router rejects non-GET with `405`; there are no
  destructive GETs (listing/search/body only).
- **Inert body serving** — `GET /api/v1/body` forces a non-executable content type:
  `.json` → `application/json`, everything else (incl. `.eml`/`.html`) →
  `text/plain`, plus `X-Content-Type-Options: nosniff`. A backed-up mail/page is
  rendered inertly; the browser never runs embedded scripts or loads trackers.
- **Path-traversal guard** — a body's resolved path must canonicalize to inside the
  account's `archive_root`, else `400`; ids come from our own store and are
  hash-sharded, but the guard is defense-in-depth.
- **No secrets in responses** — item endpoints serialize metadata only
  (`has_body` flag, never the token cache or absolute paths).

- **Rendered viewer is escape-safe + CSP-locked** — `GET /api/v1/view` renders
  our own canonical JSON with every value HTML-escaped and a raw `.eml` as escaped
  source, and the response carries a strict `Content-Security-Policy`
  (`default-src 'none'; …`) so the page loads/runs nothing. See
  [html-viewer-security.md](html-viewer-security.md).

## Endpoints

`GET /` (UI), `GET /api/v1/accounts`, `/settings` (whitelisted sync config +
account roots, no secrets), `/status?account` (per-service counts),
`/items?account&service`, `/item?…&id`, `/body?…&id` (inert bytes),
`/view?…&id` (safe rendered page), `/search?account&q` (names + indexed mail
bodies). Bad params → `400`, unknown account → `404`.

## Planned hardening (plan §11 — not yet implemented)

- Unix-socket default (file-permission gated) with HTTP strictly opt-in.
- Per-install secret + Origin/Host checks + CSRF protection.
- Separate **capability tokens** (read-GUI / destructive-CLI / remote-admin) for
  the restore/job/settings actions when they move into the web UI.
- Remote access only via mTLS / pairing + token rotation + an audit log of
  restore/delete/config operations.
- A sanitized mail viewer (CSP, blocked external resources, safe `cid:` mapping)
  layered over the inert body endpoint.

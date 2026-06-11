# Local API security

The local web UI/API (plan §11, §25) is implemented in `gui/webui` and served by
`isyncyoud` (or `isyncyou serve`). On Unix it defaults to an owner-only
Unix-domain socket (`$XDG_RUNTIME_DIR/isyncyou.sock`, or a temp fallback) and uses
TCP only when `--tcp` is explicitly passed. Read endpoints are plain `GET`;
destructive actions are `POST` only and require the matching per-process
capability token injected by the daemon.

## Current properties

- **Unix socket default on Unix** — `isyncyoud` and `isyncyou serve` choose an
  owner-only Unix-domain socket unless `--tcp` is passed.
- **Loopback-only TCP opt-in** (`--tcp --bind 127.0.0.1:8765`);
  `gui/webui::serve` rejects non-loopback bind addresses such as `0.0.0.0` or
  `[::]` before opening a listener, so TCP is never exposed beyond the local host
  accidentally.
- **TCP Host/Origin boundary** — TCP requests must carry a loopback `Host`
  (`localhost`, `127.0.0.1`, or `[::1]`, with optional port). A non-local
  `Origin` is rejected before routing, including for read endpoints.
- **Owner-only Unix socket** (`--socket` override available); `gui/webui::serve_unix`
  removes stale socket files and sets mode `0600`.
- **No destructive GETs** — listing/search/body/view/settings/activity are `GET`.
  Cloud restore and scheduled-sync controls are `POST` only.
- **Capability-token guarded destructive POSTs** — `POST /api/v1/restore` and
  `POST /api/v1/sync/{pause,resume,now}` require `X-Capability-Token`. Restore
  and scheduled-sync controls use separate per-process tokens, so the scheduler
  token cannot authorize a cloud restore and the restore token cannot pause/resume
  sync. Without the injected daemon handler, restore POST returns `404`; with the
  wrong token it returns `401`.
- **Durable restore audit trail** — after the capability token and parameters are
  accepted, `POST /api/v1/restore` writes an `audit:restore` activity entry to the
  account store before invoking the destructive handler, then writes `ok` or
  `error` after the handler returns. Audit summaries include the service/item/new
  cloud id but never the capability token.
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
`/view?…&id` (safe rendered page), `/open-external?url=…` (CSP-locked
confirmation page for archived-mail `http(s)` links), `/search?account&q`
(names + indexed mail bodies), `/activity?account`, `/sync/state`. Destructive
endpoints:
`POST /api/v1/restore?account&service&id`,
`POST /api/v1/sync/{pause,resume,now}`. Bad params → `400`, unknown account →
`404`.

## Remote access — local-only **by design** (decided 2026-06-11)

The API intentionally has **no remote surface**, and none is planned. The
listener never binds beyond loopback (`serve()` rejects non-loopback binds), and
the Unix socket is owner-only. This is a deliberate architecture decision, not a
gap:

- The target audience runs iSyncYou on the machine they sit at (a desktop
  client, like the OneDrive client it replaces). Comparable tools
  (abraunegg/onedrive, rclone's default posture) ship no remote API either.
- The rare headless-server operator already has a better-audited channel than
  anything we could hand-build: `ssh -L 8765:127.0.0.1:8765 host` (then open
  `http://localhost:8765`), or any self-hosted VPN. SSH keys plus decades of
  hardening beat a home-grown mTLS/pairing/rotation stack.
- "No open port" is the strongest security posture available; building remote
  auth would create the very attack surface it then has to defend
  (risk-register **R6 is accepted by design** on this basis).

The former plan-§11 remote items (mTLS/pairing, remote-admin capability, token
rotation) were closed as not-planned with this rationale (story S-P3.1). If a
genuine multi-user server demand ever materializes, the decision can be
revisited — against real requirements, not speculative ones.

## Planned hardening (still open, local scope)

- A stricter CSRF story for the TCP loopback transport beyond the current
  Host/Origin boundary and capability-token guard. Restore POSTs already write a
  durable per-account audit trail; future delete/config endpoints need the same
  audit hook.

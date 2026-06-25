# ADR-003 — Direct-Graph token model (mobile live view)

- **Status:** Accepted (design). Governs #80 (the live view talking to Microsoft Graph
  directly, so the mobile app works over cellular without a reachable daemon).
- **Date:** 2026-06-22
- **Related:** ADR-002 (request bodies), ADR-004 (per-mode CSP), ADR-005 (Android build /
  on-device OAuth), [auth token lifecycle](../auth-token-lifecycle.md),
  [local API security](../local-api-security.md).

---

## Context

Today the web UI reads all live/archived data through the daemon (`/api/v1/items` from
the local store; a few live endpoints proxy Graph server-side). For the mobile/APK
client to show live mail/calendar/etc. over cellular, the JS must call
`graph.microsoft.com` **directly** — which means a Graph access token has to be present
in the client.

Capability tokens already live in the served `app.js`, but a **Graph bearer token is in
a different risk class**: it authorizes real Microsoft 365 access, not just a local
daemon POST. The boundary between the trusted local daemon-web mode and the
token-bearing direct mode must be explicit, and the proof must not expose more than it
needs.

## Decision

- **Two distinct modes.** "Direct mode" is opt-in and separate from the normal
  daemon-web shell. The daemon-web shell never receives a Graph token.
- **Read-only proof.** The first step injects **only a read-scope access token**
  (`__GRAPH_READ_TOKEN__`), and only when direct mode is explicitly enabled. The
  write/restore token and any refresh token are **never** placed in the JS.
- **Hygiene.** Short-lived access token only; served with `Cache-Control: no-store`;
  never logged, never written to `localStorage`, never captured in screenshots.
- **Production token source.** For the standalone APK the token comes from **on-device
  OAuth** (system browser, see ADR-005 / RFC 8252), not from daemon injection. Daemon
  injection is the bridge for the daemon-served proof only.
- **Direct writes** (using a write token in the client) stay deferred until the read
  path is proven and the exposure is reviewed; writes meanwhile keep going through the
  cap-token daemon routes.

## Consequences

- **+** Unlocks a real cellular live view; the existing UI renders unchanged once the
  adapter returns the same item shape (see #80 plan, ADR honesty on coverage state).
- **−** A short-lived, read-only Graph token is exposed to same-origin JS. This is the
  accepted mobile-client architecture (every native M365 app stores a token on device);
  it is bounded by read-only scope, short TTL, and no refresh token in the client.
- **−** Requires the per-mode CSP of ADR-004 so the common mode is not widened.

## Superseded for #89 (2026-06-25)

The standalone APK (#89) chose a different architecture: it **embeds the real Rust
engine in the app process** and serves the existing web UI over loopback
(`crates/mobile` + `crates/app-host::build_live_router`), instead of a daemon-less
direct-Graph JS client. The UI calls 48 `/api/v1/*` endpoints with no data-source
seam, and the server-side HTML sanitization + item shaping would have to be
reimplemented (and re-secured) in JS — a thinner product that also discards the backup
engine. The embed path reuses everything and exposes **no** Graph token to JS.
**This ADR's direct-Graph-JS token model is therefore not used by #89** (retained only
as the abandoned direct-mode design). #89's on-device model is the existing device-code
login + the per-process session-token gate (REQ-AND-012/014).

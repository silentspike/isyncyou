# ADR-004 — Per-mode Content-Security-Policy

- **Status:** Accepted (design). Pairs with ADR-003 (direct-Graph token).
- **Date:** 2026-06-22
- **Related:** ADR-003, [HTML viewer security](../html-viewer-security.md),
  [local API security](../local-api-security.md).

---

## Context

The app shell ships a strict CSP — `APP_SHELL_CSP` in `gui/webui/src/lib.rs` has
`connect-src 'self'`, so the page can only fetch same-origin. The direct-Graph mode
(ADR-003) needs the JS to reach `https://graph.microsoft.com` (and
`https://login.microsoftonline.com` for token refresh).

Relaxing `connect-src` **globally** would widen the attack surface of the normal
daemon-web mode — which never calls Graph from the browser — for no benefit. CSP is a
core defence here (a third party already audited it), so the relaxation must be scoped
to exactly the mode that needs it.

## Decision

- **Daemon-web shell keeps `connect-src 'self'`.** Unchanged for the common, local mode.
- **Direct / Android shell uses a separate CSP** that adds `https://graph.microsoft.com`
  and `https://login.microsoftonline.com` to `connect-src`, and nothing else beyond the
  existing strict directives.
- **Header vs meta.** The daemon delivers CSP as an HTTP header (`html_with_csp`). The
  bundled standalone APK serves the UI from an asset origin with no HTTP header, so its
  CSP is delivered via `<meta http-equiv="Content-Security-Policy">` in `index.html`.
- Each mode's CSP is asserted by its own test (the existing
  `app_shell_carries_strict_csp_header` stays the guard for the common mode).

## Consequences

- **+** The common mode keeps the minimal surface that was audited; only the explicitly
  token-bearing mode can reach Graph.
- **−** Two CSP variants to maintain and test; a meta-CSP for the APK is weaker than a
  header (cannot express `frame-ancestors`), acceptable for a self-contained app.

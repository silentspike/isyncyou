# ADR-002 — Request bodies for the local API

- **Status:** Accepted (design). Prerequisite for any structured/large write (mail
  follow-up flag, OneNote edit, on-device auth).
- **Date:** 2026-06-22
- **Related:** [local API security](../local-api-security.md), ADR-003 (direct-Graph token), [REQ-SEC](../requirements/security.yml).

---

## Context

`ApiRequest` (`gui/webui/src/lib.rs`) carries only method, path, decoded query pairs
and the `X-Capability-Token` header — **there is no request body**. The HTTP front end
(`gui/webui/src/serve.rs`) builds a request without ever reading a body. As a result
every write today smuggles its payload through **query parameters**, including large
HTML: the inline mail composer sends the full reply body as `?body=<html>` (`mail_send`
/ `mail_reply`).

That is wrong for anything non-trivial:

- **URL length** — full OneNote page HTML or a long mail body can exceed practical
  request-line / proxy limits and silently truncate.
- **Exposure** — payloads in the URL leak into logs, history, and `Referer`; a
  capability/Graph token must never travel this way.
- **Shape** — structured writes (follow-up flag with dates, account/auth actions) want
  JSON, not flat string pairs.

A body-reading server is also a new denial-of-service surface, so the decision must
pin down limits and ordering, not just "read the body".

## Decision

Add `body: Vec<u8>` and `content_type: Option<String>` to `ApiRequest`. `serve.rs`
reads the body under these rules:

1. **Content-Length required** for body-bearing methods; absent ⇒ treat as empty.
2. **Hard per-route size limit** (small default, e.g. 256 KiB; larger only where a route
   needs it). Exceeding ⇒ **HTTP 413**, body not buffered past the limit.
3. **Reject `Transfer-Encoding: chunked`** (respond 411/400) — we do not stream.
4. **Authorize before reading.** Host/Origin checks and the capability-token check run
   **before** the body is consumed, so an unauthenticated or cross-origin client can
   never push a large body into the daemon.
5. **`application/json`** is the content type for structured writes; handlers parse it
   with the existing validation pattern.
6. **Migration.** Large query-param write routes (mail send/reply, OneNote create/edit,
   account/auth) move to JSON bodies. The query path is kept only where a payload is
   genuinely small and already shipped, and is removed once callers + tests are moved,
   so the UI and tests never break in the same step.

## Consequences

- **+** Correct, scalable writes; no HTML or tokens in URLs or logs; room for structured
  payloads (dates, nested objects).
- **+** DoS surface is bounded by early auth + a hard size cap + chunked-reject.
- **−** Touches `ApiRequest`, `serve.rs`, and every migrated route plus its tests; must
  land with new tests for the limit (413), the chunked-reject, and the auth-before-read
  ordering.

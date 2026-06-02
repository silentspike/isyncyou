# HTML viewer security

How archived items are shown in the browser UI without becoming an attack
surface (plan §13 "HTML-Viewer-Security", §25 viewers). Implemented in
`gui/webui/src/view.rs` and served by `GET /api/v1/view` in `gui/webui/src/lib.rs`.

## Threat model

Archived content is **untrusted**: a backed-up mail can carry `<script>`,
event-handler attributes, tracking pixels (remote `img`), `javascript:` links, or
a `meta refresh`. Rendering it naively in the user's own browser would run that
markup in the localhost origin. The viewer must show the content **inertly**.

## Two independent layers

1. **Safe by construction (escaping).** The structured services
   (calendar / contacts / todo / onenote) are rendered from **our own canonical
   Graph JSON**, not from attacker-controlled markup. Every value is
   HTML-escaped (`& < > " '`) before it enters the fixed page skeleton via
   `view::escape`, so no untrusted markup can ever become live DOM. A raw `.eml`
   (or any non-JSON body) is shown as **escaped source** (`view::source_page`) —
   verbatim text, never interpreted — capped at 512 KiB on a UTF-8 char boundary
   so a pathological message can't bloat the page.

2. **Content-Security-Policy (containment).** Every viewer response carries a
   strict CSP **header** (set by `ApiResponse::html_locked`, emitted by
   `format_http`) and an equivalent `<meta http-equiv>` in the page:

   ```
   default-src 'none'; style-src 'unsafe-inline'; img-src 'none';
   base-uri 'none'; form-action 'none'; frame-ancestors 'none'
   ```

   So even if a value somehow carried markup, the browser would load nothing, run
   no script, fetch no image (no tracking pixels), submit no form, and could not
   be framed. Only the page's own inline stylesheet is permitted. Header values
   are CRLF-stripped before emission, so a value can never inject extra headers.

## Path safety

`/api/v1/view` resolves the body through the shared `read_archived` helper: the
canonicalized file path must stay under the account's `archive_root`, else `400`.
Ids come from our own hash-sharded store, but the guard is defense-in-depth
against `..`/symlink traversal. Shared with `/api/v1/body`.

## What is deliberately NOT done yet

A **rich, sanitized HTML mail renderer** — parsing a message's own `text/html`
body and rendering it through an allowlist (so the mail looks like mail, with
inline `cid:`/`data:` images, neutralized external resources, `rel=noopener`
links) — is out of scope here. Doing it safely needs a battle-tested HTML
sanitizer/parser dependency (e.g. `ammonia` + `html5ever`), a real addition to
the otherwise dependency-light crate; hand-rolling one would be an XSS risk. Until
that dependency decision is made, mail is viewable as inert escaped source via
`/api/v1/view` and as inert bytes via `/api/v1/body`. Tracked under #25.

## Tests

`gui/webui/src/view.rs` unit tests cover escaping of markup/quotes, each renderer
(event/contact/task/page/generic), the source-page cap on a char boundary, and
the embedded CSP meta. A router integration test
(`view_renders_safe_html_with_csp_and_escapes_untrusted_values`) feeds a
`<script>`-payload subject and asserts it is escaped (not live) and that the
strict CSP **header** is present.

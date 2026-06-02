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

## Sanitized HTML mail rendering

A mail message's own `text/html` body is rendered through an **allowlist
sanitizer** — `ammonia` (over `html5ever`), the battle-tested choice (hand-rolling
an HTML sanitizer would be an XSS risk). `gui/webui/src/view.rs::sanitize_mail_html`
configures it to:

- drop `<script>`, event-handler attributes, `<iframe>`/`<object>`/`<style>` and
  other dangerous elements (ammonia's allowlist default), and
- restrict URL schemes to `cid:` / `data:` / `mailto:` only — so remote image
  `src` (tracking pixels), `javascript:` links and remote stylesheets are stripped;
  links keep their text and gain `rel="noopener noreferrer nofollow"`.

The mail page is served with [`MAIL_CSP`] (`default-src 'none'; img-src data:; …`)
as header + `<meta>`, so even a slipped-past remote URL cannot fetch. The HTML
part is extracted from the archived `.eml` by `connectors::extract_html` (MIME
walk, transfer-decoding); a plain-text-only mail falls back to escaped source.

### Still open

`cid:` inline images are not yet resolved to their MIME parts (they render as
broken images); a safe external-link **open dialog** (plan §13) is not built — for
now external links are neutralized to plain text.

## Tests

`gui/webui/src/view.rs` unit tests cover escaping of markup/quotes, each renderer
(event/contact/task/page/generic), the source-page cap on a char boundary, and
the embedded CSP meta. A router integration test
(`view_renders_safe_html_with_csp_and_escapes_untrusted_values`) feeds a
`<script>`-payload subject and asserts it is escaped (not live) and that the
strict CSP **header** is present.

# Issue #643 Progressive Search Verification

## Scope

This report verifies Issue #643 against immutable implementation commit
`65e96ddfe8cd05be62b2fae54b78ec5665cef882`. It covers the three-stage search
producer, the closed public event contract, recovery and budget behavior, real
StoreArchive wiring, and bounded desktop and default-APK product rows. It makes
no staging, main, RC, release, or Issue #644 completion claim.

The binding Issue #643 contract comment `5119956130` supersedes historical
partial-completion comments.

## Result

PASS. The Linux host gates, protected `windows-latest` handle tests, controlled
desktop product row, and physical default-APK product row all passed against the
implementation commit. `REQ-AGENT-017` is implemented and is linked to the
machine-checked Issue #643 manifest.

The final physical turn emitted `names`, `bodies`, and `deep` in order. The first
two stages completed and the deep stage closed truthfully as `skipped` for the
observed candidate/budget outcome. The host then emitted terminal `complete`,
returned one resolvable source, and exposed no body excerpt. A terminal skipped
stage is part of the reviewed public contract and is not reported as a completed
deep-body read.

## Contract Verification

- Search publishes the complete `names`, `bodies`, `deep` queued plan before work.
- Stage progress includes bounded current-item presence and coalesced scan counts.
- Public events contain no query, account, continuation, candidate authority,
  private body path, or body excerpt.
- Names and body FTS do not open archived body files.
- Deep reads require authenticated model-selected candidates and stop at the
  record, time, byte, token, body-read, event, and provider-step limits.
- Cancellation and deadline errors remain typed and no partial result is emitted
  after cancellation.
- Every turn exit closes open stages; sink loss is not misreported as delivered.
- Recovery validates encrypted V1 fixtures before V2 conversion, keeps structured
  sources, and fences old-harness journals before executor I/O.
- WebUI, SSE, probe tooling, and Android consume the same closed public event
  contract.
- Product feature graphs include StoreArchive retrieval; the minimal stub cannot
  satisfy product readiness.

## Verification Gates

- Protected PR checks passed on the implementation commit: workspace tests,
  workspace Clippy, formatting, coverage, MSRV, cargo-deny, actionlint, Gitleaks,
  dependency review, JavaScript, language policy, requirements/evidence,
  Semgrep, and `windows-progressive-archive` on `windows-latest`.
- The real Windows job executed the required native no-follow/reparse-point test
  filters; it was not substituted by Linux cross-compilation.
- Focused package totals recorded by the frozen host run were: store 65, core 102,
  agent 465 plus integration targets, app-host 546 passed with 6 ignored, WebUI
  316, and mobile 32.
- The deterministic FakeProvider row found a metadata-selected, keyword-less item
  missed by name/body FTS and returned a resolvable source.
- The managed desktop product row used official product Claude OAuth, emitted the
  ordered activity, returned one resolvable source, committed terminal state, and
  replayed the request idempotently.
- The final clean default Android APK passed native build, unit tests, lint,
  assembly, all 24 default instrumentation tests, hook exclusion, and one normal
  Assistant product turn on a physical Pixel.
- UI smoke passed 65 assertions; the progressive-search device-probe unit suite
  passed 8 tests.

## Artifact Identity

- Implementation commit:
  `65e96ddfe8cd05be62b2fae54b78ec5665cef882`
- Managed daemon SHA-256:
  `aa3493a510add6cb263e14097b6ed8f65b38970b832a4c2554e0dfe1c946fb15`
- Final default debug APK SHA-256:
  `3bb39954ec3893a5e06fe4f3137b69f986110914ca19b98904ef6b7ab29391f3`

No committed evidence contains a device serial, provider identity, account or
email address, raw query, sender/title, source ID/path, body content, OAuth
state/code, provider request, raw ToolResult, or token. Temporary authentication
and mail readbacks were deleted and the device lock was released without logging
out either controlled product account.

## Landing Boundary

PR #820 targets `dev` and uses `Closes #643`. The evidence closeout commit must
pass the protected exact-head checks before merge. Promotion workflows remain
disabled; no cascade, tag, RC, release dispatch, or release artifact belongs to
this issue.

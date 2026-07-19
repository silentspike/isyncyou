# Issue #628 Agent Epic Pre-RC Verification

## Identity and boundary

The implementation commit is
`4df8d89ec2d71c838e234b2a0d9388eb4412c59c`, with tree
`f5bf24eaa7403699a758cdccc9abcb782c789eaf`. The pre-RC manifest pins only
that existing commit. It does not contain a candidate commit/tree, release
commit, or self-reference.

This package verifies the implemented product integration before review and
release. It does not claim that a PR was merged, a promotion ran, an RC was
published, or Issue #628 was closed.

## Verified implementation

- Session V2 uses an encrypted authenticated authoritative manifest, atomic
  manifest-CAS heads, bounded records, and provider-generation fencing.
- UUID reuse is checked through the authoritative index even when the direct
  route/scope key misses. Product mutation receipts survive restart, and the
  local session index serializes read-modify-write updates.
- Quiet turns renew their lease. A conflicted release clears only its exact
  authoritative lease and cannot clear a replacement holder.
- Cancellation blocks late publication, and the host emits completion only
  after durable terminal state.
- Pairing claim/install/finalize recovery is idempotent. The prior physical
  Pairing V2 observation remains recorded without its code or session secret;
  all affected recovery and lease paths pass at the exact implementation head.
- Strict JSON, route body limits, session/capability gates, mutation chunks,
  no-store responses, URL-secret exclusion, and redacted UI output pass.
- The UI smoke passes 61 assertions across desktop and mobile, including
  Reader/Writer separation, pre-sign-in session import, GPT-5.6 model/effort
  selection, source rendering, terminal confirmation controls, and autoscroll.

## Runtime observations

Real Claude and ChatGPT product OAuth each completed a read-only StoreArchive
turn with at least one source. Those turns ran at
`018dfa5255271f802d3ea05b96b396bec2662c66`; the only change from that commit
to the pinned implementation commit is a `cfg(test)`-only mutex in
`crates/core/src/envelope.rs`. The exact implementation APK was then rebuilt,
installed with product data preserved, and read back as connected with no
failed runs.

The clean default APK is pinned by SHA-256 in `default-apk-matrix.json`. It
contains no network/job/credential test-hook or experimental-subscription
marker. Hook-only observations remain separate and are not used as provider
OAuth evidence.

The merged Issue #645 manifest still validates with 28 entries. Issue #628
uses its provider operation lease and rejects recovery under a changed
credential generation; it does not relabel the prior physical two-account
Switch as a new Issue #628 run.

## Gates

The exact implementation commit passes remote workspace tests and Clippy,
remote formatting, Cargo deny, Actionlint, Shellcheck, the nonempty-filter
self-test, JavaScript syntax, Python probe/workflow tests, UI smoke,
traceability, Android unit/lint/instrumentation gates, default APK assembly,
and the product boundary scans. The full remote workspace test completed with
exit code 0 after the global body-key test race was serialized.

No account identity, OAuth value, callback query, token, cookie, device serial,
raw provider frame, raw platform log, prompt/answer body, or ToolResult is
included in this package.

## Remaining release work

The next phase is approval-gated: create the candidate evidence commit, obtain
explicit push/PR approval, pass protected `dev` review and CI, obtain separate
promotion approval for the candidate cascade, then obtain a fresh approval for
the exact RC dispatch. Final-RC evidence and the second evidence cascade happen
only after publication. No stable tag belongs to Issue #628.

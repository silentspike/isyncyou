# Issue #628 Agent Epic Pre-RC Verification

## Identity and boundary

The implementation commit is
`61651929d970fb778f05de245b5edc07a48d420d`, with tree
`dd00134d7a66bbb100a615e38570ef77160f39e3`. The pre-RC manifest pins only
that existing commit. It does not contain a candidate commit/tree, release
commit, final evidence commit, or self-reference.

This package verifies the product integration before protected review and RC
publication. It does not claim that a PR was merged, a promotion ran, an RC was
published, or Issue #628 was closed.

## Verified implementation

- V2 sessions use an encrypted authenticated authoritative manifest, atomic
  manifest-CAS publication, bounded records, durable turn admission, and
  provider-generation fencing.
- Mutable requests bind one canonical UUID to a typed route, durable scope, and
  normalized semantic payload before execution. Sensitive route responses are
  never retained in global receipts.
- Provider steps and recovery checkpoints are durable and bounded. Ambiguous
  outbound calls do not replay, quiet turns renew their lease, cancellation
  discards late results, and the host emits completion only after terminal
  publication.
- Pairing V2 is user-presence-gated, one-time, and crash recoverable. Its remote
  cleanup authority remains encrypted until conditional deletion succeeds or
  remote absence is proven.
- Strict JSON, route body limits, session/capability gates, sealed mutation
  chunks, no-store responses, URL-secret exclusion, and redacted UI output pass.
- The UI smoke passes 62 assertions across desktop and mobile, including
  Reader/Writer separation, session import, transport-failure recovery, source
  rendering, terminal confirmation controls, and autoscroll.

## Runtime observations

Real Claude and ChatGPT product OAuth each completed a read-only StoreArchive
turn with a resolvable source on the exact implementation commit. The controlled
Reader and Writer M365 roles remained connected, and desktop/mobile OneDrive
quota observations matched across all five compared fields.

A fresh Pairing V2 transfer moved one selected encrypted session from desktop
to the default Pixel build after native user presence. Android rehydrated the
history and completed a new nonempty assistant turn without an error, pending
action, or tool call. A fresh desktop process then observed the same
authoritative session grow by one three-record turn group.

The clean default APK and the separate hook APK are pinned by SHA-256 in their
matrix files. Hook-only tests are not provider or product evidence. The default
APK is rebuilt after hook testing and excludes all deliberate hook and
experimental-subscription markers.

## Predecessor evidence

The merged #624, #625, and #626 manifests remain the live-operation baselines;
the exact implementation commit reruns the integration and idempotency
regressions without creating unnecessary new destructive cloud fixtures. The
#639, #640, and #645 manifests validate, and their merge commits are ancestors
of the candidate.

Historical manifests for #618, #621, #623, and #627 contain six test names that
were superseded by later contracts. They are not rewritten retroactively.
`dependency-and-scope.json` records each old-to-current mapping, and every
current replacement test passed in the exact-head workspace run.

## Gates

The exact implementation commit passes the full remote workspace and feature
test matrices, workspace and feature Clippy, remote formatting, Cargo deny,
Actionlint, Shellcheck, the nonempty-filter self-test, JavaScript/Python
contracts, UI smoke, traceability, Android JVM/lint/instrumentation, pinned
Semgrep, Gitleaks, and product boundary scans.

No account identity, OAuth value, callback query, token, cookie, device serial,
raw provider frame, raw platform log, prompt/answer body, pairing code, or
ToolResult is included in this package.

## Remaining release work

Protected `dev` review and CI remain outstanding. Promotion, RC dispatch,
published-artifact verification, the final evidence cascade, and explicit issue
closure remain separately approval-gated. No stable tag belongs to Issue #628.

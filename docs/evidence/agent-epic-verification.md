# Issue #628 Agent Epic Pre-RC Verification

## Identity and boundary

The implementation commit is
`54374b8fd121832238e2ad2f7dd8df67945ba67c`, with tree
`2f7ea5b0ebde774643838ab33aa7296d17059b65`. This empty refreeze commit
identifies the exact post-review implementation tree after the protected staging
gate fixes. The pre-RC manifest pins only
that existing commit. It does not contain a candidate commit/tree, release
commit, final evidence commit, or self-reference.

This package verifies the product integration before RC publication. Protected
review and the manual fallback promotion reached `dev`, `staging`, and `main`
with the same implementation tree. It does not claim that an RC was published
or Issue #628 was closed.

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
turn with a resolvable source on
`a2b42aa83e83afeb09fc47d88c29c9b2f8a3c53d`. That observation is reused
without relabeling because provider OAuth, the custom harness, StoreArchive, and
turn source are unchanged in the refreeze tree. The controlled Reader and Writer
M365 roles remained connected, and desktop/mobile OneDrive quota observations
matched across all five compared fields.

The physical Pairing V2 and cross-device continuation observation remains pinned
to `61651929d970fb778f05de245b5edc07a48d420d`. It is reused rather than
relabeled. The later Pairing V2 change replaces an initialized temporary buffer
plus `SystemRandom::fill` with `ring::rand::generate` using the same
`SystemRandom` source; protocol, state transitions, AAD, transfer payload, and
publication are unchanged. All 26 exact-tree pairing tests and the protected
staging Android build and emulator smoke pass.

The refreeze tree produced a clean default APK with SHA-256
`e07599cef819445921314b5687c744f27b12785e97161366f412a779a9c2c698`.
Both default-APK boundary scans pass. That exact APK was installed on the
physical Pixel with `adb install -r`; the installed APK hash matched, existing
app data remained available, and a cold launch reached the Assistant surface
without a crash or ANR. The check performed no login, logout, revoke, account
switch, or other account-lifecycle action. Earlier broad physical default/hook
observations remain separately pinned and are not relabeled. Hook-only tests are
not provider or product evidence.

## Predecessor evidence

The merged #624, #625, and #626 manifests remain the live-operation baselines.
The full-matrix observation reran the integration and idempotency regressions
without creating unnecessary new destructive cloud fixtures; the exact
refreeze tree then passed the protected CI matrix for its bounded delta. The
#639, #640, and #645 manifests validate, and their merge commits are ancestors
of the candidate.

Historical manifests for #618, #621, #623, and #627 contain six test names that
were superseded by later contracts. They are not rewritten retroactively.
`dependency-and-scope.json` records each old-to-current mapping. Every current
replacement test passed in the full-matrix observation and remains in unchanged
source covered by the protected refreeze-tree CI.

## Gates

The full remote workspace, Clippy, formatting, Android, UI, and live matrix is
pinned to the preceding implementation observation. The refreeze delta is
bounded to CI action pins, the Pairing V2 random-array call form, and the
loopback-session-aware serve smoke. The exact refreeze tree passes the 26 pairing
tests, native/default APK build, default marker scans, traceability, protected
dev/staging/main CodeQL and secret scans, staging E2E/DAST, Android build/emulator,
release build, and main vulnerability scan.

No account identity, OAuth value, callback query, token, cookie, device serial,
raw provider frame, raw platform log, prompt/answer body, pairing code, or
ToolResult is included in this package.

## Remaining release work

RC dispatch, published-artifact verification, the final evidence cascade, and
explicit issue closure remain separately approval-gated. No stable tag belongs
to Issue #628.

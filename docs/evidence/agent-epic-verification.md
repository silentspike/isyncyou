# Issue #628 Agent Epic Verification

## Current Status

The current local implementation head is
`4deb1d91cb906e0d2e3527c8c3cba2bc583fae42` with tree
`0a932b50457f29b88137d9c1623be620fbeb1cb5`. The release-blocking live
matrix and frozen-head gate run are complete. This head is the immutable
`IMPLEMENTATION_COMMIT` for the pre-RC package.

The implementation, workspace, feature, format, static, Android build, and
targeted physical regression gates pass. REQ-AGENT-016 is `implemented` by this
validated evidence closeout. No candidate tree, pull request, promotion, RC, or
release claim exists.

## Verified Gates

- Every required filtered Cargo family listed and executed at least one matching
  test on the exact implementation commit.
- Workspace tests and workspace Clippy pass with warnings denied on the remote
  Rust builder.
- Product and experimental Agent feature suites and feature Clippy pass.
- Format, Cargo deny, pinned Actionlint, Shellcheck, Python, JavaScript, npm, UI
  smoke, traceability, Android JVM tests, lint, and predecessor boundary scans pass.
- The exact current commit passed the managed desktop Codex row with admission
  under 250 ms, same-request turn reuse, one committed terminal, transcript
  rehydration, and a resolvable source. The probe now validates Git object identity,
  an optional bounded loopback Android CDP target, and authoritative request status
  after every terminal outcome.
- The exact current commit also passed the managed desktop Claude row after the
  product OAuth completed through the strict manual-code route. Turn admission was
  under 250 ms, the same request reused one turn, terminal state committed,
  transcript rehydrated, and all 19 bounded source references resolved through
  both item listing and view.
- The default APK is separated from the hook APK and contains no #628/#645 hook
  marker.
- Claude, ChatGPT, Microsoft Reader, and Microsoft Writer were independently ready
  on the prior physical evidence commit and remained present after the current
  default APK reinstall; this is not relabeled as a fresh current-head OAuth pass.
- Real Pairing V2 completed through Android device-credential confirmation on the
  prior physical evidence commit. The
  imported shared session continued with both providers on Android and converged
  back to the managed desktop runtime.
- The current APK proves that an absent bridge stream produces one transport
  error, zero events, and no synthetic terminal success. Only a persisted host
  terminal event may complete a turn.
- A controlled cached hostile-content fixture was read by Claude and ChatGPT.
  Both turns completed without a pending action or mutation; only closed result
  facts are retained.
- The current default APK exposes Stop only after stream readiness. A real stopped
  turn ended as `cancelled` in the UI without a connection-loss message, error, or
  pending action.
- The deterministic confirmation matrix rejects wrong, mismatched, expired, and
  replayed tokens; Pending-Cancel removes authority, duplicate/late native callbacks
  cannot open a second prompt, and lease loss rejects late publication. Combined
  with the physical running-turn cancellation, Row F leaves zero mutation.
- Four controlled desktop operation effects completed through the confirmation
  contract. The created mail draft, restored mail, direct share permission, and
  temporary drive item were each explicitly removed; post-cleanup Graph readback
  proved all four absent and left zero unreverted effects.
- The default APK completed the real ChatGPT Reconnect command. Status was
  non-ready after old-credential revocation and before browser login, then returned
  connected with the new credential. A subsequent normal product turn completed
  with 25 citations, 21 read-only tool steps, no error, and no PendingAction.
- The validated #625/#626 native-job baseline remains applicable: #628 changed no
  product Worker, scheduler, notification, lease, or job execution behavior. The
  only job-path diff is a test fixture adaptation to the typed transport error.
  Current default instrumentation, device-credential confirmation, Keystore, and
  hook exclusion pass; the inherited physical baseline covers visible foreground
  backup, notification-denial recovery, restart dedupe, and reverted restore-cloud.
- Shared-session Row J combines the real Pairing V2 and cross-device continuation
  with a current connected shared-session turn. Non-empty remote filters passed
  four Manifest-CAS/lease tests, four provider-crash tests, one full multi-step
  reconstruction test, and three repeatable-read recovery tests.
- All three #627 boundary modes pass, including daemon/default-APK exclusion.

## Pre-RC Matrix

| Row | Scope | State | Closed reason |
|---|---|---|---|
| A | Claude desktop OAuth and retrieval | PASS | `claude_managed_cited_turn_and_retry_reuse` |
| B | Codex desktop OAuth and retrieval | PASS | `codex_managed_cited_turn_and_retry_reuse` |
| C | Claude Android OAuth and long turn | PASS | `current_default_product_runtime_cited_turn_and_guard_pass` |
| D | Codex Android OAuth and long turn | PASS | `current_default_product_runtime_cited_turn_and_guard_pass` |
| E | Prompt-injection containment | PASS | `controlled_hostile_fixture_contained_for_both_providers` |
| F | Cancel and confirmation negatives | PASS | `physical_turn_cancel_plus_deterministic_confirmation_negative_matrix_pass` |
| G | Confirmed desktop operations | PASS | `controlled_confirmed_operations_and_verified_reverts_pass` |
| H | Hook-APK deterministic recovery | PASS | `current_hook_instrumentation_passed_and_default_restored` |
| I | Default-APK native operations/jobs | PASS | `unchanged_native_job_baseline_plus_current_default_regression_pass` |
| J | Desktop-to-Android shared session | PASS | `pairing_cross_device_current_turn_lease_and_crash_recovery_pass` |
| K | #645 bounded lifecycle regression | PASS | `default_apk_codex_reconnect_and_cited_turn_pass` |
| L | #627 product/debug exclusion | PASS | `experimental_fallback_excluded` |
| M | Desktop non-Agent regression | PASS | `managed_daemon_m365_list_and_view_passed` |

A host test or hook APK never substitutes for a blocked real provider,
confirmation, lifecycle, job, or mutation/revert row.

## Artifact Identity

| Artifact | SHA-256 | Claim boundary |
|---|---|---|
| Default debug APK | `436156a6d76e48c945b20e87f4447240a0d33eb7bbbd736151e52ac21120cc8e` | Clean exact-head build restored after hooks; all hook markers absent |
| Combined hook APK | `f642cfe859a9ae7e924fa3040324044a0036aa53cae1e7173af979debf7d5d6f` | Exact-head deterministic hook instrumentation only; not a product APK |

## Remaining Boundary

All pre-RC live rows A-M, frozen-head gates, manifest validation, requirement
traceability, and staged secret/diff checks are PASS. The local remaining work is
the candidate evidence commit. Push, PR, both cascades, RC dispatch, final-RC
evidence, and explicit issue closure remain separate approval-gated phases.

No account identity, OAuth value, token, callback value, cookie, device serial,
raw platform log, raw provider frame, prompt body, answer body, or ToolResult is
recorded here.

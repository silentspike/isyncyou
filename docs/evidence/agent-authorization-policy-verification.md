# Agent Authorization Policy Verification

Issue: #642, S-AG.17, REQ-AGENT-019.
Status: **LOCAL AND DEVICE GATES PASS; protected PR and review remain pending.**
Implementation commit: `8d6e79fb48bad2afbccff4888579803b20fe3041`.
Implementation tree: `03f1e6d02a6b4f6cfb8847d60f3ff841f3dd2605`.
Date: 2026-10-04. Landing target: `dev` only.

## Proven Scope

One exhaustive `ToolAction::policy()` maps six immediate actions to their
existing read/local-materialization recovery policies and four confirmed effects
to `NeverRepeat`. Registration, atomic durable consumption, host validation,
native challenge, authenticated stream transport, fixed public result and closed
audit projections retain one authority system. A consumed ambiguous effect or
terminal projection failure never authorizes re-execution.

The owner-requested confirmation-latency correction commits the terminal encrypted
local outbox before responding, without waiting for OneDrive transcript sync.
Existing maintenance and startup recovery retry that projection. The actual
encrypted-store restart regression covers success/failure, retained outbox,
consumed-token rejection and unavailable cloud transport. It does not claim that
network effects themselves complete instantaneously.

The exact-commit product-feature workspace aggregate passed, together with
warnings-denied Clippy, remote formatting, dependency policy, minimal-mobile
tests/Clippy, browser regressions, controller tests and pinned security scans.
[Host gates](artifacts/issue-642/host-gates.txt) contain actual commands, remote job
IDs, counts and the remote aggregate log digest. Optional skips do not prove
physical acceptance.

The branch also contains separately committed, owner-authorized dependency/build,
OAuth, model-catalog and mail-cache corrections. Its 74 changed implementation
paths are not represented as authorization-only changes. No release was performed.

## Acceptance Map

| Criterion | Current proof | Remaining |
|---|---|---|
| AC-1 | H1 exhaustive authorization/recovery matrix and compiler-exhaustive policy | Independent review/CI |
| AC-2 | H1 immediate-read and stop-before-effect runtime tests; P1 real approved effect | Independent review/CI |
| AC-3 | H1 closed schemas reject remembered/always-allow fields | Independent review/CI |
| AC-4 | H1 owner/hash/policy/expiry/cancel/replay/restart negatives and production HTTP ambiguity regression | Independent review/CI |
| AC-5 | H1 native challenge/handle negatives, default-APK tests, P1 approval and P2 denial | Independent review/CI |
| AC-6 | H2 FakeProvider hostile-body authority containment | No probabilistic safety claim |
| AC-7 | H1 stream/audit/public-result privacy tests, H3 containment and redacted physical facts | Independent review/CI |
| AC-N | Exhaustive schema/runtime tests and product feature graphs | Independent review/CI |

H1 is [the runtime policy matrix](artifacts/issue-642/authorization-policy-matrix.json).
H2 is [deterministic injection containment](artifacts/issue-642/prompt-injection-containment.json).
H3 is [the bound browser reports](artifacts/issue-642/ui-smoke.json).
Feature graphs do not alone prove runtime authority. Fakes do not prove physical
native approval or live cloud effects.

## Default APK

[APK evidence](artifacts/issue-642/default-apk.json) binds the native manifest and
installed APK to the implementation commit. APK signature, 16 KiB alignment,
default-hook and experimental-marker checks passed. The exact installed base
hash was independently checked before submission. Current product/Graph identity
checks verified the configured provider and both Microsoft roles.

The final Gradle gate exited zero with 71 unchanged JVM test results, lint and
fresh default-APK instrumentation. Twelve device tests executed successfully.
Six optional hook/benchmark/strong-biometric preconditions were skipped. The
Gradle XML represents their `AssumptionViolatedException` entries as failures;
the first private report reducer rejected them before their exception classes
were inspected. This is not permission to classify arbitrary failures as skips.

The final runner explicitly used the leave-installed override. No reset,
uninstall or reauthentication was requested in this attempt. Key-file continuity
was not recorded after the first report reducer rejected the XML, so the package
does not assert uninterrupted key-byte preservation across instrumentation.
Post-run product readiness and approved-context checks are separately verified.

## Physical Boundary

The fresh P1 used one real provider request in a fresh product session. An
actually rendered synthetic unread item was visibly selected; Graph and
StoreArchive verified it unread. The controller registered cleanup before
submission and checked the visible approved account immediately before the
effectful path. The real provider produced the required PendingAction and the
actual native user-presence prompt was observed.

P1 verified actual approval, a confirmed UI card with no retained attempt, a
completed shared operation projection, a closed completed audit and the read
state through both Graph and StoreArchive. P2 verified cancellation of the actual
system prompt through Back, a still-pending card with the cancellation message,
no new confirmation audit, an unconfirmed shared operation projection and an
unchanged unread state through both independent paths.

The accepted rows came from separate runs on the same unchanged implementation
APK. An earlier approval expired; a subsequent second prompt was accepted rather
than denied. Neither failed observation is used as accepted denial evidence.
Only P2 was repeated after diagnosing that actual state; P1 was not repeated.
No fake PendingAction, native arming bypass, model-tool forcing, account reset,
reauthentication or artifact rebuild supplied these observations.

After a controller connection failure, cleanup was independently retried and
verified both targets unread before restoring the captured foreground and
releasing the device lock. At that readback the targets were already unread;
the evidence therefore does not claim a new revert response that was not recorded.
[Physical observations](artifacts/issue-642/pixel-native-confirmation.json) and
[restoration](artifacts/issue-642/graph-and-store-revert.json) remain separate.
Successful cleanup cannot manufacture a missing scenario result.

## Closeout Boundary

REQ-AGENT-019 is `planned` in the immutable tested implementation and changes to
`implemented` only in this separate evidence closeout. The
[manifest](issue-642-manifest.json) pins that already existing implementation,
not its own future commit. Artifact hashes and requirement/manifest validation
precede the evidence commit. Push/PR requires explicit owner approval; protected
CI, independent approval, dev merge and issue closure are not claimed.
No staging/main, promotion, RC, tag or release action belongs to this issue.

[Artifact digests](artifacts/issue-642/artifact-sha256.json) cover every reduced
artifact and synthetic screenshot without hashing the index into itself. Before
commit, every referenced digest was compared to the actual stored bytes.

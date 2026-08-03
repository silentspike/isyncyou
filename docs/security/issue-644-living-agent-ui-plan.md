# Issue #644 Implementation Plan: Living Agent UI

**Issue:** [#644 - S-AG.19](https://github.com/silentspike/isyncyou/issues/644)
**Parent epic:** [#614 - In-App M365 Agent](https://github.com/silentspike/isyncyou/issues/614)
**Planning workspace:** `/work/isyncyou-agent1`
**Implementation worktree:** `/work/isyncyou-agent644`
**Plan date:** 2026-08-03
**Plan status:** Implementation in progress after the Task 0 gate passed against the
merged #643 producer contract
**Landing boundary:** One protected PR to `dev`; no promotion, staging/main cascade,
tag, RC, release workflow, or go-live action belongs to #644.

## 1. Purpose

#644 improves the existing Assistant tab so users can see truthful, live operational
progress instead of a static waiting state:

1. show a bounded activity plan for a real multi-step action;
2. render stage state, counters, current work, and incremental results from typed
   public stream events;
3. show operational thinking feedback without exposing model reasoning;
4. preserve exact provider token order while batching DOM writes;
5. use bounded animation that remains fully usable with reduced motion;
6. apply the same visual language to one already-supported non-search operation;
7. avoid a plan for direct single-step responses.

This is a frontend-focused story. It consumes work reported by the host; it does not
invent work from prompts and does not change execution authority.

## 2. Scope Boundary

### 2.1 In scope

Production changes are limited to:

- `gui/webui/src/app.js`;
- `gui/webui/src/app.css`;
- a focused real-asset browser smoke under `tools/`;
- narrow requirement, changelog, security-boundary, and evidence updates.

The UI owns:

- one bounded turn-local activity reducer;
- closed display catalogs for trusted activity and tool kinds;
- activity-plan, stage, result, thinking, and streamed-answer rendering;
- result dedupe and `add`/`enrich` updates;
- token-frame batching;
- follow-mode autoscroll;
- reduced-motion, responsive, and accessibility behavior.

### 2.2 Explicitly out of scope

#644 does not add or modify:

- progressive search orchestration or producer events, owned by #643;
- durable elapsed timing, timing routes, timing receipts, control-store schemas,
  admission migration, or PendingAction timing, tracked by #813;
- optimistic pre-hydration placeholders or provisional-turn adoption, tracked by
  #814;
- `SessionRecordV2`, request journals, cloud-session persistence, or cross-device
  activity hydration;
- provider transport, OAuth, account lifecycle, confirmation authority, Graph
  mutation behavior, or native permission prompts;
- #641 device-bridge behavior;
- frontend dependencies, analytics, external fonts, or external network calls;
- release or promotion automation.

Activity plans, counters, incremental result cards, and animation are ephemeral
turn-local presentation state. Durable history remains the existing #628 contract:
user/final Assistant text, citations, sanitized usage, and redacted operation state.

## 3. Live Snapshot

This snapshot was refreshed on 2026-08-03 at the Task 0 branch gate. Volatile
GitHub and workflow state must still be refreshed before push and landing.

| Item | State |
|---|---|
| `origin/dev` | `bbca96bfaca7de087bc00409fd6c15e96efee382` |
| #621 / #622 / #628 | CLOSED |
| #643 | CLOSED; PR #820 merged into `dev` as `bbca96bfaca7de087bc00409fd6c15e96efee382` |
| #644 | OPEN; Task 0 changes `status:backlog` to `status:in-progress` after its commit |
| #813 | OPEN, optional low-priority durable timing follow-up |
| #814 | OPEN, optional low-priority provisional-send follow-up |
| Open PRs | #826-#830; workflow-only Dependabot changes, no #644-owned path overlap |
| Promotion workflows | `disabled_manually` |
| `feature/ag-644` | Created locally from the exact `origin/dev`; absent on origin |
| `/work/isyncyou-agent644` | Isolated implementation worktree |
| Requirement | #644 owns `REQ-AGENT-018` |

The dirty historical `/work/isyncyou-agent1` checkout is planning context only. It
must not be cleaned, reset, switched, or reused for implementation.

## 4. Existing Product Baseline

The current source already provides much of the visual foundation:

- a three-dot thinking indicator exists;
- `search_stage` and `partial_result` have UI handlers;
- result cards and streamed Assistant text already render;
- PendingAction cards and their authority behavior belong to #622;
- a global reduced-motion rule exists;
- the real embedded `app.js` and `app.css` can be exercised by the existing browser
  smoke tooling.

The remaining #644 gaps are narrow:

- the whole search plan is not shown when the activity starts;
- stage state is stringly and incomplete;
- duplicate/replayed partial results can duplicate cards;
- stage/result state is mutated directly in DOM handlers rather than one reducer;
- token handling rewrites and scrolls on every event;
- autoscroll pulls a reader back to the bottom;
- motion, NEW-state lifetime, reduced-motion equivalence, and generic non-search
  behavior need focused verification.

#644 improves these frontend behaviors. It does not rebuild the Agent lifecycle.

## 5. Hard Start Gate

Before any #644 branch or implementation edit:

```bash
cd /work/isyncyou-agent1
git fetch origin --prune
git status --short --branch
git rev-parse origin/dev
gh issue view 614 --repo silentspike/isyncyou \
  --json number,title,state,body,labels,url
gh issue view 643 --repo silentspike/isyncyou \
  --json number,title,state,body,labels,url
gh issue view 644 --repo silentspike/isyncyou \
  --json number,title,state,body,labels,url
gh issue view 813 --repo silentspike/isyncyou \
  --json number,title,state,labels,url
gh issue view 814 --repo silentspike/isyncyou \
  --json number,title,state,labels,url
gh pr list --repo silentspike/isyncyou --state open \
  --json number,title,headRefName,baseRefName,isDraft,mergeable,url
gh api repos/silentspike/isyncyou/actions/workflows/promote.yml --jq .state
gh api repos/silentspike/isyncyou/actions/workflows/promote-watchdog.yml --jq .state
```

Required results:

- #643 is closed through a merged PR;
- its merge commit is an ancestor of fresh `origin/dev`;
- the merged #643 event and serialization tests are readable;
- no open PR changes a #644-owned path;
- promotion workflows remain disabled and no promotion/release run is active;
- `feature/ag-644` and `/work/isyncyou-agent644` do not already exist;
- #813 and #814 remain separate and are not pulled into #644.

Create the worktree non-destructively:

```bash
git fetch origin --prune
test -z "$(git branch --list feature/ag-644)"
test -z "$(git ls-remote --heads origin feature/ag-644)"
test ! -e /work/isyncyou-agent644
git worktree add -b feature/ag-644 /work/isyncyou-agent644 origin/dev
cd /work/isyncyou-agent644
test -z "$(git status --porcelain)"
```

Copy only this reviewed plan into the isolated worktree. Do not copy the historical
worktree or its unrelated evidence and plan files. Set #644 to
`status:in-progress` only after the gate succeeds.

No push or PR is allowed without the explicit approval required by the landing
phase.

## 6. Producer Contract

#643 owns the exact wire schema. Task 0 must read the merged Rust serialization
tests and update this section only if the final names differ. #644 must consume one
documented schema, not support both legacy and new shapes indefinitely.

The expected public activity event contains:

```json
{
  "event": "stage_progress",
  "schema_version": 1,
  "activity_id": "opaque",
  "activity_kind": "archive_search",
  "stage": "names",
  "status": "running",
  "scanned": 120,
  "total": null,
  "current_item": "bounded display text or null",
  "hits": 8,
  "coverage_complete": null,
  "budget_reached": null,
  "continuation_available": null
}
```

The expected incremental-result event contains:

```json
{
  "event": "partial_result",
  "schema_version": 1,
  "activity_id": "opaque",
  "stage": "names",
  "sequence": 0,
  "items": [
    {
      "result_key": "opaque",
      "change": "add",
      "service": "mail",
      "item_id": "opaque source id",
      "name": "bounded display text",
      "item_type": "message",
      "display_path": null,
      "sender": null,
      "body_available": true,
      "source": {
        "service": "mail",
        "item_id": "opaque source id",
        "label": "bounded display text"
      }
    }
  ]
}
```

Consumer rules:

- accept only the merged schema version and closed enums;
- bind events to the active local session, turn request, turn, and stream;
- never infer an activity kind or stage catalog from prompt text;
- treat labels, sender, display paths, and names as untrusted display text;
- use DOM text APIs only;
- ignore unknown event types and fields rather than rendering raw fallback JSON;
- reject provider-private search fields such as account, query, continuation,
  candidate handles, `deep_context`, or body excerpts in public events;
- use `(service, item_id)` for source identity and existing same-origin viewer
  navigation; never use `display_path` as identity or authority;
- do not persist activity events in transcript state.

## 7. Frontend Design

### D1. One turn-local reducer

All activity state changes pass through one pure reducer:

```text
reduceAssistantActivity(previous, normalizedEvent) -> next
```

The state contains only:

```text
TurnActivityState
  identity: session_id + turn_request_id + turn_id + stream_id
  phase: starting | running | pending_confirmation | complete | error | cancelled
  thinking: preparing | provider | working | hidden
  activities: ordered map<activity_id, ActivityState>
  results: ordered map<activity_id + result_key, ResultState>
  expected_partial_sequence: map<activity_id, integer>
  token_buffer
  follow_mode
  unseen_content
  terminal_reason
```

It contains no timer, request authority, provider frame, raw ToolResult, token,
account identity, prompt copy beyond the normal transcript, or durable session
record.

Bounds:

| Resource | Limit |
|---|---:|
| Activities per turn | 4 |
| Stages per activity | exactly 3 closed stages |
| Accepted stage updates per activity / turn | 256 / 1,024 |
| Partial-result batches per activity / turn | 64 / 256 |
| Unique result cards per turn | 200 |
| Activity/result identifier | exactly 22 unpadded base64url characters |
| Current item | 160 UTF-8 bytes |
| Result/source label | merged producer bound |
| Diagnostic counters | 16 closed saturating counters |

When a detail bound is reached, stop accepting more detail and show one bounded
summary. Continue processing error, pending, cancellation, and terminal events so
resource pressure cannot leave the UI permanently busy.

### D2. Monotonic stage and sequence handling

Allowed stage transitions:

```text
queued -> running -> complete
queued -> skipped
queued -> cancelled
running -> complete
running -> failed
running -> cancelled
```

Terminal stage states never reopen. Counters never decrease. Unknown totals remain
unknown; the UI does not invent percentages.

For partial results:

- `sequence == expected` applies the batch and increments expected;
- `sequence < expected` is a transport replay and is ignored;
- `sequence > expected` marks result detail as needing reconciliation and ignores
  the gap batch;
- `add` creates one result;
- `enrich` updates an existing `result_key` without moving it;
- unknown enrich targets are rejected;
- source identity is also deduped defensively.

Because activity state is advisory presentation, #644 does not add cryptographic
event digests or a durable event ledger.

### D3. Thinking is operational, not hidden reasoning

The thinking indicator starts when the existing admitted-turn placeholder is
created. #814, not #644, owns any future move before session hydration.

Allowed text comes from closed local mappings such as:

- Preparing;
- Thinking;
- Searching names;
- Searching archived content;
- Reading selected items;
- Waiting for confirmation.

No provider reasoning field, chain-of-thought, raw event name, or model-internal
state is displayed.

### D4. Activity plans

`archive_search` has the fixed three-stage catalog delivered by #643:

```text
Names and subjects
Archived content
Selected deep reads
```

The full plan appears after the first valid `archive_search` activity event, before
expensive stage completion. A stage row shows an icon, label, optional bounded
counter/current item, and text status.

For generic non-search acceptance, use a closed display adapter over already
validated public `tool_call`, `tool_result`, `confirmation_required`, and terminal
events. Support existing operations such as:

```text
backup         Prepare -> Await confirmation -> Run backup
restore-cloud Prepare -> Await confirmation -> Run restore
```

The adapter is display-only. It cannot call a mutation route, mint confirmation,
or predict success. One real backup or restore flow is sufficient for #644
acceptance. #641 photo-to-mail is not a close gate.

A direct final answer, `read`, or `list` does not create an empty plan.

### D5. Results

Result cards:

- render from normalized public fields only;
- use `result_key` for update identity and `(service, item_id)` for source identity;
- preserve initial order during enrich;
- show NEW only for a bounded initial lifetime;
- open sources only through the existing same-origin viewer;
- never place untrusted values in HTML, CSS, URL, class, or dataset construction;
- truncate visible text with stable layout while keeping accessible text bounded.

Result expansion is a direct user action and need not animate layout. Automatic
arrival motion uses only opacity and transform.

### D6. Token rendering

Append incoming token text to an in-memory buffer and flush at most once per
`requestAnimationFrame`.

Requirements:

- preserve exact token order and final text bytes;
- do not add artificial character-by-character delays;
- update one text node, not the whole message subtree;
- flush synchronously before terminal rendering;
- show a caret only while tokens are arriving;
- stop the caret on tool work, pending confirmation, complete, error, or
  cancellation;
- do not announce every token to screen readers.

### D7. Follow-mode scrolling

Automatic scrolling remains enabled only while the user is near the bottom of the
Assistant scroller. When the user scrolls away:

- keep their reading position;
- count unseen updates without retaining event payloads;
- show an accessible jump-to-latest icon button;
- resume follow mode only after the user returns to the bottom or activates the
  button.

Programmatic scrolling must not move keyboard focus.

### D8. Motion, responsive behavior, and accessibility

Use the existing visual language and no new dependency.

Required motion:

- bounded thinking pulse;
- spinner to checkmark transition;
- staggered result entry;
- short NEW emphasis;
- token caret.

`prefers-reduced-motion: reduce` disables decorative motion immediately while
preserving all text, state, results, controls, and interactions.

Accessibility:

- status is not conveyed by color alone;
- activity and stage structure has meaningful semantics;
- live announcements are throttled to stage/terminal changes;
- current-item and token updates are not repeatedly announced;
- icon-only controls have accessible names and visible focus;
- stream updates never steal focus;
- long labels do not overlap counters or controls.

Desktop and mobile layouts use stable tracks and bounded text. No nested cards are
introduced.

### D9. Security and privacy

- CSP remains `connect-src 'self'`.
- No external script, font, analytics, image, or animation dependency is added.
- Dynamic event values use `textContent` or equivalent DOM text APIs.
- Unknown values never become class names, inline styles, URLs, or fallback HTML.
- Public UI and evidence contain no token, account identity, email address,
  callback value, provider frame, raw ToolResult, raw error, or device serial.
- Progress is display-only and carries no mutation authority.

### D10. Performance

Use one representative real-renderer fixture:

- 500 token events;
- 100 progress events;
- 200 unique results plus duplicates/enrichments;
- one complete search plan;
- one terminal event.

Verify:

- token DOM writes are materially fewer than token events;
- automatic animations use opacity/transform;
- no #644 long task above 100 ms in the controlled desktop run;
- controlled cumulative layout shift stays below 0.1;
- no timer, observer, listener, or animation frame remains after teardown;
- repeated create/terminal/teardown cycles do not grow state or DOM.

Use a separate pure-reducer boundary test for exact caps plus one over-limit value.
Do not animate thousands of adversarial events merely to prove a numerical bound.

## 8. Implementation Tasks

### Task 0. Freeze dependency and scope

1. Run section 5.
2. Read the merged #643 public serialization and host reachability tests.
3. Update only the expected event field names if the merged contract differs.
4. Re-read the corrected #644 body and follow-ups #813/#814.
5. Create the isolated worktree.
6. Copy and commit this plan as the initial #644 contract.
7. Add `REQ-AGENT-018` as `planned`.
8. Set `status:in-progress`.

Abort if #643 is not merged, the merged public event lacks fields required by
AC-1, an open PR changes an owned path, or the follow-up work has been folded back
into #644 without explicit owner approval.

### Task 1. Add the reducer

Expected file:

```text
gui/webui/src/app.js
```

Implement:

- strict event normalization;
- active-turn identity binding;
- closed search and generic action catalogs;
- monotonic stage transitions and counter rules;
- partial-result sequence handling;
- result dedupe and enrich;
- terminal freeze and stale-event rejection;
- resource caps and bounded diagnostics;
- teardown on terminal/session switch/route replacement.

Required tests:

- `living_ui_reducer_accepts_only_matching_active_turn_events`
- `living_ui_reducer_requires_merged_activity_schema`
- `living_ui_reducer_stage_transitions_are_monotonic`
- `living_ui_reducer_never_reopens_terminal_stage`
- `living_ui_reducer_rejects_counter_regression_and_unknown_enums`
- `living_ui_reducer_ignores_replayed_partial_sequence`
- `living_ui_reducer_marks_partial_sequence_gap_for_reconciliation`
- `living_ui_reducer_applies_add_then_enrich_without_reordering`
- `living_ui_reducer_dedupes_results_by_source_identity`
- `living_ui_reducer_keeps_terminal_events_after_detail_cap`
- `living_ui_reducer_rejects_private_search_fields`
- `living_ui_reducer_erases_state_on_turn_teardown`

### Task 2. Build the activity and result renderer

Expected files:

```text
gui/webui/src/app.js
gui/webui/src/app.css
```

Implement:

- immediate full search plan after the first trusted activity event;
- truthful queued/running/complete/failed/skipped/cancelled rows;
- bounded counters and current-item copy;
- stable result cards with add/enrich/NEW behavior;
- closed generic backup/restore plan adapter;
- direct single-step rendering without a plan;
- operational thinking labels only;
- existing PendingAction card behavior unchanged.

Required tests:

- `living_ui_search_shows_complete_plan_from_first_activity_event`
- `living_ui_stage_rows_render_all_terminal_states_truthfully`
- `living_ui_unknown_total_never_renders_false_percentage`
- `living_ui_results_use_text_nodes_and_existing_source_viewer`
- `living_ui_result_enrich_preserves_card_order`
- `living_ui_new_state_expires_without_removing_result`
- `living_ui_backup_or_restore_uses_closed_display_only_plan`
- `living_ui_single_step_turn_has_no_plan`
- `living_ui_thinking_copy_contains_no_reasoning_or_raw_event_text`
- `living_ui_pending_card_authority_and_terminal_controls_are_unchanged`

### Task 3. Improve token, scroll, motion, and accessibility behavior

Expected files:

```text
gui/webui/src/app.js
gui/webui/src/app.css
```

Implement D6-D8 without changing turn admission or hydration ordering.

Required tests:

- `living_ui_tokens_flush_once_per_frame_and_preserve_exact_text`
- `living_ui_terminal_flushes_pending_tokens_before_done`
- `living_ui_caret_stops_on_pending_error_cancel_and_complete`
- `living_ui_autoscroll_stops_when_reader_leaves_bottom`
- `living_ui_jump_to_latest_restores_follow_without_focus_loss`
- `living_ui_reduced_motion_preserves_content_and_interaction`
- `living_ui_status_is_not_conveyed_by_color_alone`
- `living_ui_stream_updates_do_not_announce_every_token_or_move_focus`
- `living_ui_long_labels_do_not_overlap_compact_or_mobile_layout`

### Task 4. Add one focused browser smoke

Create:

```text
tools/agent-living-ui-smoke.mjs
```

Reuse existing real-asset fixture infrastructure without changing #622 evidence.
Exercise:

1. three-stage search with counters, current item, duplicate/enriched results,
   tokens, and complete terminal;
2. thinking before first output;
3. one backup or restore PendingAction plan;
4. direct single-step response with no plan;
5. reduced motion with equivalent content;
6. user scroll-away and jump-to-latest;
7. error, cancellation, stale event, and terminal teardown;
8. hostile/long display strings;
9. representative performance fixture;
10. exact reducer caps plus one over-limit value.

Capture only:

- desktop running and terminal screenshots;
- one compact/mobile terminal screenshot;
- reduced-motion screenshot;
- closed assertion counts and performance counters;
- console errors, CSP violations, and failed/non-self requests.

Do not create a separate evidence-schema framework when the existing repository
manifest checker can validate these artifacts.

### Task 5. Verify production integration

After deterministic tests pass:

| ID | Flow | Required observation |
|---|---|---|
| L1 | Real controlled #643 archive search | Full plan, live counters/current item, deduped results, streamed final text, resolvable citations |
| L2 | Direct factual turn | Thinking and streamed text; no plan |
| L3 | Controlled backup or restore request | Generic plan reaches PendingAction; cancel causes no mutation, or confirmed controlled effect occurs once and is reverted |
| L4 | Reduced-motion L1 | Same stages, results, controls, text, and citations without decorative motion |
| L5 | Scroll away during L1 | Reading position remains; jump-to-latest restores follow |

One bounded physical default-APK regression is required because the WebUI assets
ship inside the APK:

```bash
device-lock acquire ag-644 "Living Agent UI default-APK verification"
trap 'device-lock release ag-644' EXIT INT TERM
(cd android && env -u ISY_CARGO_FEATURES ./gradlew clean :app:assembleDebug)
sha256sum android/app/build/outputs/apk/debug/app-debug.apk
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
```

Then:

1. verify the exact APK contains no test-hook marker;
2. run one controlled real #643 search;
3. verify plan, progress, results, tokens, citations, and background/foreground
   rendering without duplication;
4. preserve only redacted closed observations and screenshots;
5. release the device lock through the installed trap.

No hook APK, timer restart, parallel PendingAction, cross-installation, or schema
migration matrix belongs to #644.

### Task 6. Add requirement, documentation, and evidence

Update narrowly:

```text
docs/requirements/agent.yml
docs/security/agent-threat-model.md
CHANGELOG.md
docs/evidence/issue-644-manifest.json
docs/evidence/artifacts/issue-644/
```

`REQ-AGENT-018` covers only:

- typed public activity rendering;
- bounded turn-local reducer state;
- thinking/stage/result/token presentation;
- generic supported multi-step presentation;
- reduced-motion/accessibility equivalence;
- no plan for single-step turns.

Keep it `planned` in the implementation commit. Change it to `implemented` only
after deterministic, live, and default-APK evidence exists.

The evidence package contains:

```text
docs/evidence/issue-644-manifest.json
docs/evidence/artifacts/issue-644/ui-smoke.json
docs/evidence/artifacts/issue-644/desktop-running.png
docs/evidence/artifacts/issue-644/desktop-terminal.png
docs/evidence/artifacts/issue-644/mobile-terminal.png
docs/evidence/artifacts/issue-644/reduced-motion.png
docs/evidence/artifacts/issue-644/live-search.json
docs/evidence/artifacts/issue-644/live-non-search.json
docs/evidence/artifacts/issue-644/default-apk.json
```

Evidence records the immutable implementation commit and artifact hashes. It
contains no personal content, account identity, OAuth data, provider frame, raw
ToolResult, device serial, or raw log.

## 9. Verification Strategy

Before editing, load the complete `gui/webui/src/app.js`,
`gui/webui/src/app.css`, the public #643 activity-event contract, the direct
reducer/render callers, and the focused UI smoke/tests into context. Implement
the reducer state, rendering, styling, and test-tool changes as one coherent
frontend block, or as the fewest substantial blocks needed to keep each block
reviewable.

Do not run the entire repository suite after every small edit.

### 9.1 Development loop

After one coherent frontend implementation block:

```bash
node --check gui/webui/src/app.js
node tools/agent-living-ui-smoke.mjs \
  --out /tmp/issue-644-ui-smoke
```

Repeat only the focused failing assertion while iterating.

### 9.2 Final local gate

Run once after the implementation is frozen:

```bash
node --check gui/webui/src/app.js
npm ci
node tools/agent-living-ui-smoke.mjs \
  --out docs/evidence/artifacts/issue-644/ui-smoke
cargo remote -c -- test -p isyncyou-webui --all-targets -- --nocapture
cargo remote -c -- clippy -p isyncyou-webui --all-targets -- -D warnings
python3 tools/check_traceability.py
python3 tools/check_evidence.py \
  --manifest docs/evidence/issue-644-manifest.json
git diff --check
```

Use the repository package directory if `package-lock.json` moves. Remote Cargo
and Android/Gradle must not overlap.

Because #644 changes no Rust production code, local full-workspace Rust, app-host
feature matrices, Cargo deny, actionlint, and unrelated Android instrumentation are
not duplicated as #644-specific evidence. The protected `pr-dev` workflow remains
the authoritative full-repository CI gate. Run broader local gates only if the
actual diff expands beyond the owned paths or focused checks expose a cross-package
regression.

Before each commit:

```bash
git diff --check
git status --short
```

After staging only owned files:

```bash
gitleaks git --staged --redact --no-banner
git diff --cached --name-only
git diff --cached --check
```

## 10. Acceptance Mapping

| Issue AC | Proof |
|---|---|
| AC-1 | Deterministic and real #643 search show full plan, checkmarks, counters, deduped result cards, and streamed final answer |
| AC-2 | Thinking state and exact frame-batched token rendering in smoke and real direct turn |
| AC-3 | One real supported backup or restore flow uses the same display language without changing authority |
| AC-4 | Reduced-motion smoke and real search preserve complete content and interaction |
| AC-5 | Browser smoke records zero console/CSP/non-self network failures |
| AC-N | Direct final answer and simple read render without an empty activity plan |

## 11. Expected File Inventory

Expected production changes:

```text
gui/webui/src/app.js
gui/webui/src/app.css
```

Expected support changes:

```text
gui/webui/src/lib.rs (test-only source contract assertions)
tools/agent-living-ui-smoke.mjs
tools/agent-ui-smoke.mjs (test-only fixture export; direct #622 smoke behavior unchanged)
docs/security/issue-644-living-agent-ui-plan.md
docs/requirements/agent.yml
docs/security/agent-threat-model.md
CHANGELOG.md
docs/evidence/issue-644-manifest.json
docs/evidence/artifacts/issue-644/*
```

Unexpected and blocking without explicit scope approval:

```text
gui/webui/src/serve.rs
crates/app-host/src/*
crates/agent/src/*
crates/mobile/src/*
android/app/src/*
.github/workflows/*
```

If a merged #643 defect requires producer work, fix #643 separately rather than
silently expanding #644.

## 12. Commit Plan

Use at most four focused commits:

1. `docs(agent): define living agent ui contract`
2. `feat(webui): render bounded assistant activity`
3. `test(webui): verify living agent ui behavior`
4. `docs(agent): record living ui evidence`

The first commit adds this plan and `REQ-AGENT-018` as planned. The final evidence
commit marks the requirement implemented only after real proof exists.

## 13. Risks and Rollback

| Risk | Mitigation |
|---|---|
| UI invents work | Plans come only from closed trusted event/action catalogs |
| Duplicate events duplicate cards | Turn-local sequence handling and source/result dedupe |
| Missing event creates false completion | Monotonic reducer never backfills unreported success |
| Untrusted text enters HTML or URLs | DOM text APIs and existing source viewer only |
| Token bursts cause jank | One text node and frame-batched writes |
| Reader is pulled to bottom | Near-bottom follow mode and jump-to-latest |
| Motion hides information | Reduced-motion equivalence and semantic text state |
| Generic plan gains authority | Adapter is display-only; existing confirmation path remains authoritative |
| Scope expands into backend timing | #813 owns timing; #814 owns provisional hydration |
| APK uses stale assets | Build/install/hash one exact implementation-commit default APK |

Rollback is frontend-only:

1. disable or revert the new reducer/renderer;
2. fall back to the existing #622 transcript and PendingAction UI;
3. leave #643 producer events harmlessly ignored by the old renderer;
4. do not roll back #643, session schemas, provider code, or confirmation authority.

No database or protocol rollback is required because #644 adds none.

## 14. Landing

After all scoped acceptance evidence passes:

1. freeze `IMPLEMENTATION_COMMIT`;
2. generate redacted evidence against exactly that commit;
3. validate evidence, traceability, staged diff, and Gitleaks;
4. obtain explicit push/PR approval;
5. push `feature/ag-644`;
6. open one PR to `dev` with `Closes #644`;
7. set `status:review`;
8. wait for every required `pr-dev` check and review;
9. verify promotion workflows remain disabled and no release run is active;
10. merge through the protected PR path;
11. confirm #644 closed and no promotion/release artifact was created.

#644 does not enable promotion workflows and does not create a staging/main PR,
tag, RC, release dispatch, or published artifact.

## 15. Definition of Done

- [ ] #643 is merged and its merge commit is an ancestor of the fresh #644 base.
- [ ] #813 and #814 remain separate optional follow-ups.
- [ ] The diff stays within the expected file inventory.
- [ ] One turn-local bounded reducer owns activity state.
- [ ] Search shows the complete plan from the first trusted activity event.
- [ ] Stage status and counters are monotonic and truthful.
- [ ] Result add/enrich/replay is stable and deduped.
- [ ] Source navigation uses only the existing `(service,item_id)` viewer contract.
- [ ] Thinking copy is operational and exposes no model reasoning.
- [ ] A supported backup or restore flow uses the same display language without new authority.
- [ ] Direct single-step turns render no activity plan.
- [ ] Token writes are frame-batched and final text remains byte-exact.
- [ ] Autoscroll respects deliberate reading and offers jump-to-latest.
- [ ] Reduced motion preserves every state, result, control, and terminal outcome.
- [ ] Accessibility and responsive layouts pass focused checks.
- [ ] CSP remains self-only and no external frontend dependency is added.
- [ ] No activity/result state is added to durable or cross-device history.
- [ ] Focused browser performance and exact reducer-bound tests pass.
- [ ] One real #643 search and one real supported non-search flow pass.
- [ ] One exact-commit default APK passes the bounded physical search regression.
- [ ] `REQ-AGENT-018` becomes implemented only after evidence exists.
- [ ] The evidence manifest is valid, redacted, and pinned to the implementation commit.
- [ ] Protected `pr-dev` checks are green.
- [ ] The PR lands to `dev` and closes #644.
- [ ] Promotion workflows remain disabled and no release artifact is created.

## 16. Explicit Non-Claims

Completion does not claim:

- access to or display of model chain-of-thought;
- durable or cross-restart elapsed timing;
- optimistic pre-hydration message submission;
- cross-device activity-plan or result-card hydration;
- implementation of #643 search orchestration;
- implementation of #641 device bridging;
- new mutation or confirmation authority;
- that every future action has a plan without a closed adapter;
- that animation proves provider or Graph success;
- that #644 authorizes a future Agent go-live or release.

# OneDrive Mobile Verification - plan diff + E2E evidence (#646 / #660)

This is the close-out evidence document for epic #646 and story #660. It maps the REV-4
OneDrive-mobile modes plan (`~/.claude/plans/onedrive-mobile-modes.md`, Phases 1-6) to the
code now shipped on `origin/dev`, then records the real Pixel 8 Pro end-to-end evidence.

Current execution branch: `feature/om-660` from `origin/dev` `62ebdc3`.

No-RC directive: #660 ends at `main` plus this committed verification report. Do not run
`release.yml`, do not create `v1.0.0-rc.*`, and do not create a stable tag for this close-out.
The older GitHub issue text still says "RC"; this document follows the user's binding
2026-07-06 No-RC directive.

## Gate

- #656 is CLOSED (`2026-07-05T18:08:20Z`), verified with `gh issue view 656 --repo silentspike/isyncyou`.
- #659 is CLOSED (`2026-07-06T10:10:47Z`), verified with `gh issue view 659 --repo silentspike/isyncyou`.
- `origin/dev` tip at branch creation: `62ebdc3`.
- On-device endpoint for this run: `10.0.0.115:35619` under `device-lock om-660`.

## Plan-Diff Matrix

| REV-4 phase | Story / merge evidence | Shipped runtime path | Tests / evidence class | #660 status |
|---|---|---|---|---|
| Phase 1: Mode 1 online listing | #647/#648/#649; commits `bed4190`, `4b7396b`, `fd8b403` | `crates/graph/src/http.rs::list_children` uses paged Graph reads through `get_json_paged`; `crates/engine/src/onedrive_live.rs::OneDriveLister`; `gui/webui/src/lib.rs` routes `GET /api/v1/onedrive/children` and `GET /api/v1/onedrive/open`; `crates/app-host/src/lib.rs::DaemonOneDriveList` / `DaemonOneDriveOpen`; `gui/webui/src/app.js::driveLoad`, `driveMapChild`, `driveFileUrl`, `driveOpenFile` mobile branch. | Unit tests include `list_children_pages_over_200_items`, `list_children_retries_via_central_policy_then_pages`, `list_children_writes_no_store_row`; on-device proof is required in AC2.1. | Code mapped; on-device E2E pending. |
| Phase 2: mode config, effective mode, UI | #650/#651/#652; commits `bed4190`, `c5e1b23`, `534fcb2` | `crates/core/src/onedrive_mode.rs::OneDriveMode` / `OneDriveModes::effective_mode`; `crates/core/src/config.rs::onedrive_modes` and `Config::effective_mode`; `gui/webui/src/lib.rs` routes `GET/POST /api/v1/onedrive/mode` and enriches children with `effective_mode`; `crates/app-host/src/lib.rs::DaemonOneDriveMode` persists settings; `gui/webui/src/app.js::driveEffMode`, `renderDriveModeBar`, `driveModePill`, `setFolderMode`, and storage display. | Unit tests include `effective_mode_deepest_ancestor_wins`, `onedrive_modes_round_trip_and_default_when_omitted`, `onedrive_modes_validation_rejects_invalid_entries`, `onedrive_mode_post_persists_and_get_reflects`; on-device persistence proof is required in AC2.2. | Code mapped; on-device E2E pending. |
| Phase 3: Mode 2 scoped sync + ledger | #653/#654; commits `bed4190`, `1e900b3` | `crates/connectors/src/scope.rs::owning_scope` and `scopes_from_modes`; `crates/connectors/src/onedrive.rs::incremental_sync_scoped` persists per-folder delta cursors and resolves scope overlap; `crates/store/src/lib.rs` cloud-write ledger; `crates/engine/src/onedrive_write.rs` idempotent create/rename/move/delete/upload/replace ledger; `gui/webui/src/lib.rs` routes `/api/v1/onedrive/{create,rename,move,delete}`. | Unit tests include `deepest_active_ancestor_wins_on_overlap`, scoped-delta tests under `crates/connectors/src/onedrive.rs`, `onedrive_write_cap_gate_and_dispatch`, and ledger recovery tests in `onedrive_write.rs`; on-device Mode 2 proof is required in AC2.3. | Code mapped; on-device E2E pending. |
| Phase 4: Mode 3 offline + writeback | #655 plus #656 fixes; commits `b828a42`, `8c73107`, `793cbc9` | `crates/engine/src/lib.rs::offline_sync_once` runs boot recovery, scoped delta, `materialize_downloads_scoped`, and scoped local create/modify/delete writeback over the ledger; `crates/mobile/src/lib.rs::run_offline_pass` invokes it from the Android refresh loop; `crates/connectors/src/onedrive.rs::materialize_downloads_scoped` writes to `sync_root` with policy and progress; `/api/v1/onedrive/{transfers,policy}` is exposed by `gui/webui/src/lib.rs`. | Unit tests include materialization/progress/cancel tests, `recovery_skips_a_missing_local_body_op_without_aborting_the_batch`, `transfers_progress_cancel_and_policy_endpoints`; on-device airplane/writeback/restart proof is required in AC2.4. | Code mapped; on-device E2E pending. |
| Phase 5: Android edit | 5a #657 / 5b #658; commits `84599ec`, `374788b` | Upload/replace: `gui/webui/src/serve.rs` decodes request bodies, `gui/webui/src/lib.rs` routes `/api/v1/onedrive/{upload,replace}`, `crates/app-host/src/lib.rs::DaemonOneDriveWrite::{upload,replace}` stages bytes and calls `upload_via_ledger` / `replace_via_ledger`, `crates/graph/src/http.rs::upload_to_parent` handles root upload. SAF: `android/app/src/main/kotlin/com/silentspike/isyncyou/OneDriveDocumentsProvider.kt` exposes live children and RAM/proxy-fd opens; manifest registers the provider. | Unit tests include `onedrive_upload_replace_dispatch_and_gates`, `onedrive_upload_replace_are_biometric_gated_on_mobile`, `upload_to_parent_targets_root_or_item_content`; on-device upload/replace/root and SAF proof is required in AC2.6/AC2.8. | Code mapped; on-device E2E pending. |
| Phase 6: rest-features + E2E | #656/#659/#660; commits `8c73107`, `793cbc9`, `62ebdc3` | Transfer UI: `gui/webui/src/app.js::startDriveTransfersPoll`, `renderTransfersPanel`, `driveModeChip`; transfer controls are gate-exempt from the store gate but session/cap-gated. Management: `crates/connectors/src/onedrive.rs::{dematerialize_one,download_one,resolve_conflict,cleanup_offline_to_online}`; `crates/engine/src/lib.rs::{free_up_for,download_now_for,resolve_conflict_for,cleanup_offline_to_online_for,list_conflicts_for}`; `gui/webui/src/lib.rs` routes `/onedrive/{free-up,download-now,conflicts,conflict/resolve,cleanup}`; `gui/webui/src/app.js::driveManageSection` / conflict center. | Unit tests include `free_up_and_download_now_roundtrip`, `materialize_skips_paused_and_resumes`, `shared_progress_cancel_is_one_shot`, `shared_progress_retry_now_unpauses_and_clears_backoff`, `cleanup_offline_to_online_drops_safe_keeps_unsynced`, `onedrive_manage_endpoints_cap_gate_and_dispatch`, `onedrive_manage_biometric_gating_on_mobile`; on-device proof is required in AC2.9. | Code mapped; on-device E2E pending. |

## Epic Findings To Re-Verify

These findings are part of the close-out evidence because the feature caught real defects during
device-level execution, not only happy paths:

| Finding | Impact | Shipped mitigation | #660 re-check |
|---|---|---|---|
| F-A: stale pending body cloud-write aborted offline pass | A missing staged body could make recovery stop before later valid ops, blocking offline materialization. | `crates/engine/src/onedrive_write.rs::cloud_write_body_source_missing` and `recovery_skips_a_missing_local_body_op_without_aborting_the_batch` mark the missing body op terminally failed without aborting the batch. | Pending in AC2.4 restart/recovery row. |
| F-B / Bug2: transfer polling was store-gate blocked | Transfer UI could not update during a blocking offline pass. | `gui/webui/src/lib.rs` gate-exempts `GET /api/v1/onedrive/transfers` and transfer control POSTs from the store gate while keeping session/cap gates. | Pending in AC2.3/AC2.4 live transfer rows. |
| F-C: progress bar was one-shot | Materialization showed no moving byte progress until completion. | `crates/graph/src/http.rs::get_bytes_with_progress` / `download_content_with_progress`; `materialize_downloads_scoped` calls `download_with_progress` and advances `SharedProgress`. | Pending in AC2.4 live progress row. |
| #659 free-up data-loss guard | Free-up must remove only the local materialized body, never create a local-delete signal that deletes the cloud copy. | `dematerialize_one` keeps the row listable and sets `content_state=cached`, `body_state=missing`; `free_up_and_download_now_roundtrip` asserts `scan_local_deletes` does not include the freed item. | Pending in AC2.4/AC2.9 Graph survival row. |
| #655 / #657 root upload | Empty parent id used to build malformed Graph upload URL for drive root. | `GraphClient::upload_to_parent` branches empty parent to `/me/drive/root:/{name}:/content`; test `upload_to_parent_targets_root_or_item_content`. | Pending in AC2.6 root upload row. |
| Stale RC wording | Issue #660 and `CONTRIBUTING.md` text can still imply RC-on-main despite No-RC directive. | This document records No-RC as binding; `CONTRIBUTING.md` must be fixed in AC3. | Pending in AC3. |

## On-Device E2E Matrix

Evidence folder for this run: `docs/evidence/artifacts/onedrive-mobile-660/` (to be created during
device prep). Each row must contain on-device evidence plus Graph/store cross-checks. Tokens must never
be printed or committed.

| Row | Scenario | Required proof | Status |
|---|---|---|---|
| AC2.1 | Mode 1 online live root/subfolder browse; on-demand open; no store write | screenshot/CDP, Graph child ids, store `count_by_service` check | PENDING |
| AC2.2 | Mode config toggle and effective-mode inheritance; restart persistence | screenshot/CDP, config/store read after restart | PENDING |
| AC2.3 | Mode 2 sync metadata cache and lazy body into `cache_root`; transfer panel | CDP/store/file-system proof, transfer JSON | PENDING |
| AC2.4 | Mode 3 offline materialization, airplane read, writeback, restart recovery, free-up cloud survival | screenshots/CDP, airplane command proof, Graph version proof, revert proof | PENDING |
| AC2.5 | Cloud create/rename/move/delete with biometric delete | CDP, BiometricPrompt dumpsys, Graph verify/revert | PENDING |
| AC2.6 | Upload/replace and root-upload regression | CDP/file picker or binary post, Graph id/eTag/version proof, revert proof | PENDING |
| AC2.7 | Share link with biometric gate and permission delete | BiometricPrompt dumpsys, Graph permission JSON, DELETE permission proof | PENDING |
| AC2.8 | SAF DocumentsProvider | system picker screenshot/dumpsys, live children, proxy-fd open proof | PENDING |
| AC2.9 | Rest features: free-up, download-now, pause/retry/cancel, conflict center, rollback, cleanup | screenshots/CDP, Graph survival proof, store/file-system proof | PENDING |

## Host / Desktop Regression Matrix

| Check | Command / proof | Status |
|---|---|---|
| Workspace tests | `cargo remote -c -- test --workspace` | PENDING |
| Clippy | `cargo remote -c -- clippy --workspace --all-targets -- -D warnings` | PENDING |
| Remote fmt parity | `cargo-remote-fmt --check` | PENDING |
| WebUI syntax | `node --check gui/webui/src/app.js` | PENDING |
| Desktop OneDrive unchanged | Spot-check `/api/v1/items` + `/view` on desktop daemon path | PENDING |
| No RC release action | Confirm no `release.yml` run or tag is created for #660 | PENDING |

## Task-3 Evidence

Commands used to create the AC1 mapping:

- `git log --oneline --decorate --max-count=90 origin/dev`
- `gh issue view 646 --repo silentspike/isyncyou --json number,title,state,body,url,labels`
- `gh issue view 656 --repo silentspike/isyncyou --json number,title,state,closed,closedAt,url`
- `gh issue view 659 --repo silentspike/isyncyou --json number,title,state,closed,closedAt,url`
- `gh issue view 660 --repo silentspike/isyncyou --json number,title,state,body,url,labels,assignees`
- `rg -n` over `crates`, `gui`, `android`, `bin`, and `docs`
- Targeted `nl -ba ... | sed -n ...` reads of the runtime files cited in the matrix.

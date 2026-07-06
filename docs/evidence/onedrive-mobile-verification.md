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
| Phase 1: Mode 1 online listing | #647/#648/#649; commits `bed4190`, `4b7396b`, `fd8b403` | `crates/graph/src/http.rs::list_children` uses paged Graph reads through `get_json_paged`; `crates/engine/src/onedrive_live.rs::OneDriveLister`; `gui/webui/src/lib.rs` routes `GET /api/v1/onedrive/children` and `GET /api/v1/onedrive/open`; `crates/app-host/src/lib.rs::DaemonOneDriveList` / `DaemonOneDriveOpen`; `gui/webui/src/app.js::driveLoad`, `driveMapChild`, `driveFileUrl`, `driveOpenFile` mobile branch. | Unit tests include `list_children_pages_over_200_items`, `list_children_retries_via_central_policy_then_pages`, `list_children_writes_no_store_row`; on-device proof is recorded in AC2.1. | Code mapped; AC2.1 on-device E2E PASS. |
| Phase 2: mode config, effective mode, UI | #650/#651/#652; commits `bed4190`, `c5e1b23`, `534fcb2` | `crates/core/src/onedrive_mode.rs::OneDriveMode` / `OneDriveModes::effective_mode`; `crates/core/src/config.rs::onedrive_modes` and `Config::effective_mode`; `gui/webui/src/lib.rs` routes `GET/POST /api/v1/onedrive/mode` and enriches children with `effective_mode`; `crates/app-host/src/lib.rs::DaemonOneDriveMode` persists settings; `gui/webui/src/app.js::driveEffMode`, `renderDriveModeBar`, `driveModePill`, `setFolderMode`, and storage display. | Unit tests include `effective_mode_deepest_ancestor_wins`, `onedrive_modes_round_trip_and_default_when_omitted`, `onedrive_modes_validation_rejects_invalid_entries`, `onedrive_mode_post_persists_and_get_reflects`; on-device persistence proof is recorded in AC2.2. | Code mapped; AC2.2 on-device E2E PASS. |
| Phase 3: Mode 2 scoped sync + ledger | #653/#654; commits `bed4190`, `1e900b3` | `crates/connectors/src/scope.rs::owning_scope` and `scopes_from_modes`; `crates/connectors/src/onedrive.rs::incremental_sync_scoped` persists per-folder delta cursors and resolves scope overlap; `crates/store/src/lib.rs` cloud-write ledger; `crates/engine/src/onedrive_write.rs` idempotent create/rename/move/delete/upload/replace ledger; `gui/webui/src/lib.rs` routes `/api/v1/onedrive/{create,rename,move,delete}`. | Unit tests include `deepest_active_ancestor_wins_on_overlap`, scoped-delta tests under `crates/connectors/src/onedrive.rs`, `onedrive_write_cap_gate_and_dispatch`, and ledger recovery tests in `onedrive_write.rs`; on-device Mode 2 proof is recorded in AC2.3. | Code mapped; AC2.3 on-device E2E PASS. |
| Phase 4: Mode 3 offline + writeback | #655 plus #656 fixes; commits `b828a42`, `8c73107`, `793cbc9` | `crates/engine/src/lib.rs::offline_sync_once` runs boot recovery, scoped delta, `materialize_downloads_scoped`, and scoped local create/modify/delete writeback over the ledger; `crates/mobile/src/lib.rs::run_offline_pass` invokes it from the Android refresh loop; `crates/connectors/src/onedrive.rs::materialize_downloads_scoped` writes to `sync_root` with policy and progress; `/api/v1/onedrive/{transfers,policy}` is exposed by `gui/webui/src/lib.rs`. | Unit tests include materialization/progress/cancel tests, `recovery_skips_a_missing_local_body_op_without_aborting_the_batch`, `transfers_progress_cancel_and_policy_endpoints`; on-device airplane/writeback/restart proof is required in AC2.4. | Code mapped; on-device E2E pending. |
| Phase 5: Android edit | 5a #657 / 5b #658; commits `84599ec`, `374788b` | Upload/replace: `gui/webui/src/serve.rs` decodes request bodies, `gui/webui/src/lib.rs` routes `/api/v1/onedrive/{upload,replace}`, `crates/app-host/src/lib.rs::DaemonOneDriveWrite::{upload,replace}` stages bytes and calls `upload_via_ledger` / `replace_via_ledger`, `crates/graph/src/http.rs::upload_to_parent` handles root upload. SAF: `android/app/src/main/kotlin/com/silentspike/isyncyou/OneDriveDocumentsProvider.kt` exposes live children and RAM/proxy-fd opens; manifest registers the provider. | Unit tests include `onedrive_upload_replace_dispatch_and_gates`, `onedrive_upload_replace_are_biometric_gated_on_mobile`, `upload_to_parent_targets_root_or_item_content`; on-device upload/replace/root and SAF proof is required in AC2.6/AC2.8. | Code mapped; on-device E2E pending. |
| Phase 6: rest-features + E2E | #656/#659/#660; commits `8c73107`, `793cbc9`, `62ebdc3` | Transfer UI: `gui/webui/src/app.js::startDriveTransfersPoll`, `renderTransfersPanel`, `driveModeChip`; transfer controls are gate-exempt from the store gate but session/cap-gated. Management: `crates/connectors/src/onedrive.rs::{dematerialize_one,download_one,resolve_conflict,cleanup_offline_to_online}`; `crates/engine/src/lib.rs::{free_up_for,download_now_for,resolve_conflict_for,cleanup_offline_to_online_for,list_conflicts_for}`; `gui/webui/src/lib.rs` routes `/onedrive/{free-up,download-now,conflicts,conflict/resolve,cleanup}`; `gui/webui/src/app.js::driveManageSection` / conflict center. | Unit tests include `free_up_and_download_now_roundtrip`, `materialize_skips_paused_and_resumes`, `shared_progress_cancel_is_one_shot`, `shared_progress_retry_now_unpauses_and_clears_backoff`, `cleanup_offline_to_online_drops_safe_keeps_unsynced`, `onedrive_manage_endpoints_cap_gate_and_dispatch`, `onedrive_manage_biometric_gating_on_mobile`; on-device proof is required in AC2.9. | Code mapped; on-device E2E pending. |

## Epic Findings To Re-Verify

These findings are part of the close-out evidence because the feature caught real defects during
device-level execution, not only happy paths:

| Finding | Impact | Shipped mitigation | #660 re-check |
|---|---|---|---|
| F-A: stale pending body cloud-write aborted offline pass | A missing staged body could make recovery stop before later valid ops, blocking offline materialization. | `crates/engine/src/onedrive_write.rs::cloud_write_body_source_missing` and `recovery_skips_a_missing_local_body_op_without_aborting_the_batch` mark the missing body op terminally failed without aborting the batch. | Pending in AC2.4 restart/recovery row. |
| F-B / Bug2: transfer polling was store-gate blocked | Transfer UI could not update during a blocking offline pass. | `gui/webui/src/lib.rs` gate-exempts `GET /api/v1/onedrive/transfers` and transfer control POSTs from the store gate while keeping session/cap gates. | PASS in AC2.3: `task7-mode2-transfer-panel-fixed.json` shows `GET /onedrive/transfers` polling while `/onedrive/open` downloads a 24 MB file, with panel text moving from 0% to 100%. |
| F-C: progress bar was one-shot | Materialization showed no moving byte progress until completion. | `crates/graph/src/http.rs::get_bytes_with_progress` / `download_content_with_progress`; `materialize_downloads_scoped` calls `download_with_progress` and advances `SharedProgress`. | Pending in AC2.4 live progress row. |
| #659 free-up data-loss guard | Free-up must remove only the local materialized body, never create a local-delete signal that deletes the cloud copy. | `dematerialize_one` keeps the row listable and sets `content_state=cached`, `body_state=missing`; `free_up_and_download_now_roundtrip` asserts `scan_local_deletes` does not include the freed item. | Pending in AC2.4/AC2.9 Graph survival row. |
| #655 / #657 root upload | Empty parent id used to build malformed Graph upload URL for drive root. | `GraphClient::upload_to_parent` branches empty parent to `/me/drive/root:/{name}:/content`; test `upload_to_parent_targets_root_or_item_content`. | Pending in AC2.6 root upload row. |
| Stale RC wording | Issue #660 and `CONTRIBUTING.md` text can still imply RC-on-main despite No-RC directive. | This document records No-RC as binding; `CONTRIBUTING.md` must be fixed in AC3. | Pending in AC3. |
| F-D: #660 Mode-2 open skipped cache | Sync-mode mobile open still served a live Graph body without materializing the body into `cache_root`, so AC2.3 lazy-body proof failed. | `b3e572c` updates `DaemonOneDriveOpen` to serve local OneDrive bodies first, download sync-mode misses into `cache_root`, update store body state, and emit transfer progress. Targeted remote regression `onedrive_open_serves_cached_sync_body_before_graph_lookup` passed. | PASS in AC2.3: `sync-lazy-2.txt` moved from no local body to `content_state=cached`, `body_location=cache`, `body_state=available`, `sync_state=clean`; file exists under app `files/cache/mode-sync/`. |
| F-E: #660 mobile OneDrive toolbar collapsed | A real device screenshot showed deep breadcrumbs squeezing the toolbar into an unusable narrow column on the phone. | `755f147` keeps mobile OneDrive breadcrumbs on a single horizontal scroll row and lets the action toolbar wrap below it. Rebuilt/reinstalled debug APK SHA-256 `d3267c2fd7eae862d001cbd6a0bf8058232ad74d3ff78986640e8665072bf96e`. | PASS in AC2.3: `task7-mode2-layout-fixed.png` and `task7-mode2-transfer-active-fixed.png` show breadcrumbs, sort, upload, verify, view toggle, mode bar, transfer panel, and file grid without overlap. |

## On-Device Prep

Task 4 completed on 2026-07-06 against Pixel 8 Pro over `adb connect 10.0.0.115:35619`,
with `device-lock om-660` held.

Evidence:

- Debug APK rebuilt with `cd android && ./gradlew :app:assembleDebug`; APK:
  `android/app/build/outputs/apk/debug/app-debug.apk`, SHA-256
  `40d695946691f4198cfedf2af8fd512dbd5e20d7033913728bb4d4403de43327`.
- APK installed with `adb install -r`; app launched as `com.silentspike.isyncyou.debug`,
  live PID `7311`.
- M365 write token silently re-minted from `~/.config/m365-write/token_cache.json` with
  `force_refresh=True`, provisioned to
  `/data/user/0/com.silentspike.isyncyou.debug/files/archive/.isyncyou-token-write.json`
  via `run-as`; only `expires_at` was printed (`1783342350`, about 59 minutes left at
  provisioning time).
- CDP verified through the live PID socket `webview_devtools_remote_7311` with
  `Runtime.evaluate` (`suppress_origin=True`); app reported `account=me`, `MOBILE=true`,
  and active caps for `onedriveMode`, `onedrivewrite`, `onedriveManage`, `transfers`, and
  `share`.
- Graph fixture root created for this run: `isy-om660-20260706-135511`, id
  `892B68CBF4A7C544!s243029df950d49938d6a3e7199c5873b`; child fixture folders:
  `mode-online`, `mode-sync`, `mode-offline`, `ops-source`, and `ops-dest`.
- Graph fixture files created: `online-open.txt`, `sync-lazy.txt`, `offline-read.txt`,
  `freeup-guard.txt`, and `progress-24mb.bin` (`25,165,824` bytes; SHA-256
  `3bbb171e9101245cf763bba6146cc317bc9c681182f7afd5a94e33ea3f3ff5f0`).
- Device state confirmed `mWakefulness=Awake`; foreground activity confirmed
  `com.silentspike.isyncyou.debug/com.silentspike.isyncyou.MainActivity`.
- Screenshot evidence: `artifacts/onedrive-mobile-660/task4-overview.png`.
- Structured fixture evidence:
  `artifacts/onedrive-mobile-660/task4-fixture.json` and
  `artifacts/onedrive-mobile-660/task4-graph-root-children.json`.

The mobile store was not wiped for a synthetic clean slate. It already contained 6 OneDrive rows
from earlier device work at prep time. AC2.1 therefore verifies Mode-1 online browse by proving the
OneDrive store count is unchanged before/after live browsing, rather than deleting unrelated
existing mobile cache state.

## On-Device E2E Matrix

Evidence folder for this run: `docs/evidence/artifacts/onedrive-mobile-660/`. Each row must
contain on-device evidence plus Graph/store cross-checks. Tokens must never be printed or committed.

| Row | Scenario | Required proof | Status |
|---|---|---|---|
| AC2.1 | Mode 1 online live root/subfolder browse; on-demand open; no store write | screenshot/CDP, Graph child ids, store `count_by_service` check | PASS - `task5-mode1-online.json`, `task5-graph-crosscheck.json`, `task5-mode1-open.png` |
| AC2.2 | Mode config toggle and effective-mode inheritance; restart persistence | screenshot/CDP, config/store read after restart | PASS - `task6-mode-config-before-restart.json`, `task6-mode-config-after-restart.json`, screenshots |
| AC2.3 | Mode 2 sync metadata cache and lazy body into `cache_root`; transfer panel | CDP/store/file-system proof, transfer JSON | PASS - `task7-mode2-sync-lazy.json`, `task7-mode2-layout-fixed.json`, `task7-mode2-transfer-panel-fixed.json`, screenshots |
| AC2.4 | Mode 3 offline materialization, airplane read, writeback, restart recovery, free-up cloud survival | screenshots/CDP, airplane command proof, Graph version proof, revert proof | PENDING |
| AC2.5 | Cloud create/rename/move/delete with biometric delete | CDP, BiometricPrompt dumpsys, Graph verify/revert | PENDING |
| AC2.6 | Upload/replace and root-upload regression | CDP/file picker or binary post, Graph id/eTag/version proof, revert proof | PENDING |
| AC2.7 | Share link with biometric gate and permission delete | BiometricPrompt dumpsys, Graph permission JSON, DELETE permission proof | PENDING |
| AC2.8 | SAF DocumentsProvider | system picker screenshot/dumpsys, live children, proxy-fd open proof | PENDING |
| AC2.9 | Rest features: free-up, download-now, pause/retry/cancel, conflict center, rollback, cleanup | screenshots/CDP, Graph survival proof, store/file-system proof | PENDING |

### AC2.1 Mode 1 Online Evidence

On-device CDP drove the mobile WebView through the actual OneDrive explorer:

- `go("onedrive")` rendered the live root; `Drive.items` included fixture root
  `isy-om660-20260706-135511` with `effective_mode=online`.
- `driveOpen(root)` browsed into the fixture root and listed `mode-online`, `mode-sync`,
  `mode-offline`, `ops-source`, and `ops-dest`, all from live Graph `/children`.
- `driveOpen(mode-online)` listed `online-open.txt`; `driveOpenFile()` opened it in the
  mobile in-app iframe through `/api/v1/onedrive/open`.
- Iframe text was `OM660 online open fixture`, matching the Graph fixture file
  `892B68CBF4A7C544!se0748f2fe81c4973899c57a520cef5fc`.
- Store proof: `/api/v1/items?account=me&service=onedrive&limit=1` returned `total=7`
  before and `total=7` after the live browse/open; `storeDelta=0`.
- Graph cross-check: `task5-graph-crosscheck.json` records the same fixture root,
  child folder IDs, and opened file size (`26` bytes) without committing account metadata.

Artifacts:

- `artifacts/onedrive-mobile-660/task5-mode1-online.json`
- `artifacts/onedrive-mobile-660/task5-graph-crosscheck.json`
- `artifacts/onedrive-mobile-660/task5-mode1-open.png`

### AC2.2 Mode Config Evidence

On-device CDP used the mobile WebView's own mode setter (`setFolderMode`) with
`CAP.onedriveMode`:

- Set `mode-sync` (`892B68CBF4A7C544!s52ac141d4b63421eb7586d07d884aee6`) to `sync`.
- Set `mode-offline` (`892B68CBF4A7C544!s0eefa6bd13314bd59a1d48f84a86bb8e`) to `offline`.
- Before restart, the root fixture listing rendered `mode-sync` as explicit `sync`,
  `mode-offline` as explicit `offline`, and the unconfigured siblings as inherited/default
  `online`.
- Opening `mode-sync` showed `sync-lazy.txt` with `effective_mode=sync`, proving child/file
  effective-mode inheritance from the folder mode.
- After `adb shell am force-stop com.silentspike.isyncyou.debug` and relaunch, the WebView was
  reattached to live socket `webview_devtools_remote_7906`; `/api/v1/onedrive/mode` still
  returned the same folder-mode map and `sync-lazy.txt` still rendered with
  `effective_mode=sync`.

Artifacts:

- `artifacts/onedrive-mobile-660/task6-mode-config-before-restart.json`
- `artifacts/onedrive-mobile-660/task6-mode-config-before-restart.png`
- `artifacts/onedrive-mobile-660/task6-mode-config-after-restart.json`
- `artifacts/onedrive-mobile-660/task6-mode-config-after-restart.png`

### AC2.3 Mode 2 Sync Evidence

On-device execution used the real mobile WebView on the Pixel 8 Pro and the same fixture folder
configured as `sync` in AC2.2:

- Fixture folder: `mode-sync`
  (`892B68CBF4A7C544!s52ac141d4b63421eb7586d07d884aee6`), inherited by files as
  `effective_mode=sync`.
- Lazy body file: `sync-lazy-2.txt`
  (`892B68CBF4A7C544!sfe1cee4b61784d2bb59754ad77f0b12b`), Graph size `31` bytes.
- Before open, store/file-system proof showed no local body for `sync-lazy-2.txt`:
  `has_body=false`, `content_state=null`, `body_location=null`, `body_state=null`,
  `sync_state=remote_dirty`, and no matching file under app `files/cache` or `files/sync`.
- The mobile in-app viewer opened the file through `/api/v1/onedrive/open`; CDP read the iframe
  text as `OM660 sync lazy second fixture`.
- After open, the store row showed `has_body=true`, `content_state=cached`,
  `body_location=cache`, `body_state=available`, `sync_state=clean`, and the file existed at
  `/data/user/0/com.silentspike.isyncyou.debug/files/cache/mode-sync/sync-lazy-2.txt`.
- The targeted regression for the finding found during this row passed:
  `cargo remote -c -- test -p isyncyou-app-host onedrive_open_serves_cached_sync_body_before_graph_lookup -- --nocapture`.

Transfer panel proof used a larger Graph file, `sync-transfer-fetch-24mb.bin`
(`892B68CBF4A7C544!sd7da2cbfe8a945758baf1f7c8e97cd74`, `25,165,824` bytes,
SHA-256 `98f846dc6f21ec0781e4033d643a96fda43818e8e79439008aaa5f57c003f354`):

- `free-up` first evicted the local body only: before the re-open, the store row had
  `content_state=cached`, `body_location=none`, `body_state=missing`, and `has_body=false`.
- Opening the file through the mobile OneDrive open path triggered a lazy download into
  `cache_root`.
- The live transfer panel was visible in the app while polling `/api/v1/onedrive/transfers`;
  snapshots recorded `0 B / 24.0 MB - 0%`, `1.6 MB / 24.0 MB - 6%`,
  `19.7 MB / 24.0 MB - 82%`, and `24.0 MB / 24.0 MB - 100%`.
- After completion, the row returned to `body_location=cache`, `body_state=available`,
  `has_body=true`, and the fetched byte count was `25,165,824`.

Additional #660 finding fixed during this row:

- The first active-transfer screenshot exposed a real mobile layout defect: deep OneDrive
  breadcrumbs squeezed the toolbar into a narrow column. Commit `755f147` fixed the mobile CSS
  by making breadcrumbs one horizontal scroll row and allowing the action toolbar to wrap below
  it. The rebuilt/reinstalled debug APK has SHA-256
  `d3267c2fd7eae862d001cbd6a0bf8058232ad74d3ff78986640e8665072bf96e`.
- `task7-mode2-layout-fixed.json` measured the fixed layout on-device:
  breadcrumb width `495px`, toolbar height `75px`, breadcrumb bottom `41.8px`, toolbar bottom
  `91.4px`, and body top `153.7px`; no toolbar/body overlap.
- `task7-mode2-layout-fixed.png` and `task7-mode2-transfer-active-fixed.png` visually show the
  fixed OneDrive layout with the transfer panel visible.

Artifacts:

- `artifacts/onedrive-mobile-660/task7-graph-sync-lazy-2.json`
- `artifacts/onedrive-mobile-660/task7-graph-sync-transfer-fetch.json`
- `artifacts/onedrive-mobile-660/task7-mode2-sync-lazy.json`
- `artifacts/onedrive-mobile-660/task7-mode2-sync-lazy-open.png`
- `artifacts/onedrive-mobile-660/task7-mode2-layout-fixed.json`
- `artifacts/onedrive-mobile-660/task7-mode2-layout-fixed.png`
- `artifacts/onedrive-mobile-660/task7-mode2-transfer-panel-fixed.json`
- `artifacts/onedrive-mobile-660/task7-mode2-transfer-active-fixed.png`
- `artifacts/onedrive-mobile-660/task7-mode2-transfer-after-fixed.png`

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

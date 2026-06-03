# v0.1 Acceptance Gate — A1–A10 (Phase 1: OneDrive bidirectional MVP)

> **Status: PASS (engine).** Every hard acceptance criterion A1–A10 (plan §19) has a
> dedicated, passing test, the data-loss/chaos matrix is covered, and the engine has been
> verified live against the throwaway test account `testuser@example.com`.
> The one remaining carve-out — the *fully assembled daemon + GUI* end-to-end run
> (install → login → tray → chaos) — depends on the native status-bar/tray work (#16 / #56)
> and needs a display server to drive the window; it is not blocked by any unmet A-criterion.

This document is the single place that maps each criterion to its concrete evidence. Run
the suite yourself with `cargo test --workspace` (deterministic) and, with a test-account
token, the `live_*` tests (see *Live verification* below).

## How the gate is structured

- **Deterministic criteria tests** — `crates/acceptance/tests/mvp.rs`, one `aN_*` test per
  criterion, exercising the real engine crates (no mocks of our own logic; only the Graph
  HTTP boundary is faked where a criterion is purely about local behaviour).
- **Chaos / data-loss matrix** — `crates/acceptance/tests/chaos.rs` (12 cases), the failure
  scenarios from plan §20 / [`test-chaos-matrix.md`](test-chaos-matrix.md).
- **Live engine tests** — env-gated (`ISYNCYOU_TEST_TOKEN` / `ISYNCYOU_TEST_WRITE_TOKEN`)
  tests that drive real Microsoft Graph endpoints against the disposable account; CI without
  a token skips them.

## A1–A10 evidence matrix

| # | Criterion | Test (`crates/acceptance/tests/mvp.rs`) | What it proves | Status |
|---|-----------|------------------------------------------|----------------|--------|
| **A1** | No path/name data loss | `a1_no_path_or_name_data_loss` (:18) | Forbidden cloud chars (`< > : " \| ? *`, trailing space/dot) round-trip `to_cloud`→`to_local` losslessly incl. Unicode; reserved names detected; the persistent `MappingTable` is reversible both ways. | ✅ PASS |
| **A2** | Mass-delete guard, both directions | `a2_delete_guard_both_directions` (:62) | Absolute and fraction caps block in `LocalToCloud` **and** `CloudToLocal`; small batches proceed; tiny libraries are exempt from the fraction rule (`min_total`). | ✅ PASS |
| **A3** | ETag/If-Match — no silent overwrite | `a3_etag_precondition_no_silent_overwrite` (:85) | An upload precondition failure resolves to *re-evaluate*, never a blind overwrite; a real content/content divergence resolves to *keep-both* in headless mode. | ✅ PASS |
| **A4** | `410 Gone` → reconciliation | `a4_gone_triggers_reconciliation` (:102) | A 410 mid-delta forces a resync from a fresh snapshot; stale pre-410 items are discarded, the resync result wins — **not** a blind delete-all. | ✅ PASS |
| **A5** | Upload-resume survives process kill | `a5_upload_resume_survives_kill` (:131) | After "losing" all session state, a fresh `UploadSession` told the server's `nextExpectedRanges` resumes from the correct offset and drives to completion. | ✅ PASS |
| **A6** | Download/commit safe-restart | `a6_atomic_write_safe_restart` (:162) | `atomic_write` (tmp + rename) makes a reader never see a partial file; overwrites are atomic; no `.tmp` is left behind → an interrupted write is safe to restart. | ✅ PASS |
| **A7** | inotify overflow → full rescan | `a7_inotify_overflow_forces_rescan` (:185) | A `QueueOverflow` event flags overflow and drops the now-incomplete coalescer buffer, so the engine cannot trust it and must rescan. | ✅ PASS |
| **A8** | Disk-full → paused, no corruption | `a8_disk_full_is_red` (:202) | `SelfCheck` reports `Red("disk…")` below the free-space floor (engine pauses); a healthy machine reports `Green`. | ✅ PASS |
| **A9** | Trash/archive separate from sync root | `a9_archive_separate_from_sync_root` (:233) | Config validation refuses `archive_root == sync_root` (`must differ`), so trash/archive writes can never land inside the synced tree. Runtime trash placement also asserted in `onedrive.rs` (`!trash_root.starts_with(&sync_root)`). | ✅ PASS |
| **A10** | Crash recovery (journal) | `a10_crash_recovery_journal` (:259) | An operation begun but not committed survives a journal reopen ("crash") as *pending*; committing clears it; a clean run leaves nothing pending. | ✅ PASS |

```
$ cargo test --workspace
test a1_no_path_or_name_data_loss ... ok          test a6_atomic_write_safe_restart ... ok
test a2_delete_guard_both_directions ... ok       test a7_inotify_overflow_forces_rescan ... ok
test a3_etag_precondition_no_silent_overwrite ... ok  test a8_disk_full_is_red ... ok
test a4_gone_triggers_reconciliation ... ok       test a9_archive_separate_from_sync_root ... ok
test a5_upload_resume_survives_kill ... ok        test a10_crash_recovery_journal ... ok
```

## Chaos / data-loss matrix (`crates/acceptance/tests/chaos.rs`, 12 cases — all PASS)

Scenarios from [`test-chaos-matrix.md`](test-chaos-matrix.md), each asserting the engine
degrades safely (no data loss, no DB corruption, recovers to green):

- **410 resync** — `chaos_410_on_first_request_resyncs`, `chaos_410_resync_to_empty_snapshot_discards_stale`
- **Upload resume** — `chaos_resume_from_zero_completes`, `chaos_resume_honors_server_offset_ahead`
- **inotify overflow** — `chaos_overflow_as_first_event_forces_rescan`, `chaos_overflow_midstream_drops_everything`, `chaos_overflow_baseline_clean_batch_is_trusted`
- **Disk-full** — `chaos_disk_full_boundary_exact_minimum_is_ok`, `chaos_disk_full_combines_with_auth_failure`, `chaos_disk_full_recovers_to_green`
- **Atomic write / journal** — `chaos_atomic_write_failure_leaves_no_partial`, `chaos_journal_partial_commit_across_crash`

## Live verification against the test account

Run on `testuser@example.com` (disposable, full CRUD) with tokens minted from the cached
dev token caches; every test uses unique prefixes and tears down after itself. Captured this
session:

| Direction / capability | Test | Live result |
|---|---|---|
| Cloud → local **download** | `onedrive::live_materialize_downloads` | `downloaded=3 dirs=9 failed=0` |
| Local → cloud **create** | `onedrive::live_local_create_uploads_to_cloud` | new local file → cloud item `…!s368a0423…` |
| Local → cloud **modify** (If-Match) | `onedrive::live_local_modify_replaces_cloud_content` | cloud content replaced for `…!sbf311a32…` |
| Local → cloud **delete** | `onedrive::live_local_delete_removes_from_cloud` | item removed from cloud |
| Cloud → local **delete → trash** (A9 runtime) | `onedrive::live_remote_delete_moves_local_to_trash` | remote delete moved 1 local item to trash |
| **Delta** (stateful cursor) | `graph::http::live_onedrive_delta` | 13 items, cursor 189 chars |
| **Upload session** (chunked + If-Match) | `graph::http::live_onedrive_upload_roundtrip` | uploaded 1,100,000 bytes → real item, cleaned up |

```
$ ISYNCYOU_TEST_TOKEN=… ISYNCYOU_TEST_WRITE_TOKEN=… \
    cargo test -p isyncyou-connectors -p isyncyou-graph --features http live_ -- --test-threads=1
test result: ok. 22 passed; 0 failed (connectors) ; ok. 2 passed; 0 failed (graph onedrive)
```

## Verdict

All ten hard acceptance criteria **A1–A10 pass deterministically**, the **chaos matrix is
green**, and the **bidirectional OneDrive engine is verified live** against the test account
in every direction (create / modify / delete / download / trash) plus stateful delta and
chunked resumable upload. The Phase-1 engine meets the v0.1 gate.

**Carve-out (not a failure):** the *assembled-product* end-to-end walk — install the AppImage,
run the daemon, sign in through the GUI/tray, watch a live sync + a chaos injection in the
running tray — depends on the native status-bar/tray work (**#16 / #56**) and needs a display
server to drive the window. The headless render of that UI is verified separately via the
own-renderer PNG snapshots (plan §24).

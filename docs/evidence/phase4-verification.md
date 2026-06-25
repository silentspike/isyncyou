# Phase-4 Verification — line-by-line plan diff (epic #556, S-P4.12 / #569)

Per-service confirmation that every Phase-4 decision shipped in the runtime path, with the landed symbol (file) and the live-verification status. Local verification only; the push → cascade → RC → close is gated on explicit GO (S-P4.12 B5).

Gate (all stories): `cargo +1.95.0 fmt --all --check` clean · `clippy --workspace --all-targets -D warnings` clean · `test --workspace` 0 failed (36 suites) · `cargo test -p isyncyou-graph --features http` 0 failed (83 passed) · `node --check gui/webui/src/app.js` OK · `tools/check_traceability.py` 40/40 implemented, OK.

Security (S-P4.12 B3, live curl + unit):
- **Layer 1** app shell `GET /`: `default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self'; frame-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`.
- **Layer 2** static `/app.js`: `application/javascript` + `nosniff` + `no-store`.
- **Layer 3** sanitized `/api/v1/view` (OneNote page + mail message): `default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'self'` (MAIL_CSP) + **0 `<script>`** in both bodies (ammonia strips).
- Write gate: all six write endpoints + settings → 401 without `X-Capability-Token`. XSS inert: `view_renders_safe_html_with_csp_and_escapes_untrusted_values` + `note_page_sanitizes_and_is_csp_locked` pass.

## S-P4.0 #557 — capability spike (CLOSED)
Read-only Graph probe; informed the per-service decisions. Closed.

## S-P4.1/4.2 #558/#559 — foundations
- Write/read scope expansion + `MailboxSettings`/`People` — `auth::READ_SCOPES`/`RESTORE_SCOPES` (`crates/engine/src/lib.rs`). ✓
- Cloud-poll engine + SSE push + interval slider (1 s–60 min) + writable settings — `EventBus`, multi-thread `serve`, `SettingsHandler` + `POST /api/v1/settings` (`gui/webui/src/lib.rs`), daemon poll loop (`bin/daemon/src/main.rs`). ✓
- 429/Retry-After backoff — `crates/graph/src/{throttle,http,error}.rs`. ✓

## S-P4.3 #560 — 4-state badge
- `backup_state()` → live_only / live_backup / stale / backup_only (`gui/webui/src/lib.rs`), store v10 `body_etag` set at `set_local_path` (`crates/store/src/lib.rs`). ✓
- Frontend `coverageBadge(it)` + `STATES` + `stateFilterBar` (`gui/webui/src/app.js`). ✓ Lucide glyphs, no emoji.

## S-P4.4/4.5/4.6 #561/#562/#563 — Mail (pilot)
- Write layer: `MailWriteHandler` + 8 `POST /api/v1/mail/*` + `mail_live.rs` `MailWriter` + `DaemonMailWrite`. ✓
- Backup completeness: `mail_preview_enrichment` (`lib.rs`) + `backup_mailbox_flanks` (`connectors/mail.rs`) + attachments (`mime::list_attachments`/`extract_attachment` + `/api/v1/attachment`) + restore-PATCH (`MailRestoreState`). ✓
- UI: `openCompose`/`openReplyForward` + per-message manage + 4-state filters (`app.js`). ✓ no-SSE-on-self-write.
- **Live (prior session):** compose→sent, flag/read/categories/move Graph-confirmed; daemon AC-5 new-mail-live-via-SSE proven.

## S-P4.7 #564 — OneDrive
- `onedrive_preview` (sidecar) + `OneDriveInfoHandler` (`drive_quota` + lazy `list_permissions`) + `/api/v1/drive`,`/permissions` (`lib.rs`/`graph/http.rs`). ✓ FUSE `getattr` cloud mtime. 2 live-found bugs fixed (store lock, `[object Object]`).
- **Live (prior session):** 22 sidecars, quota, permissions, recent sort.

## S-P4.8 #565 — Calendar
- `events_sync_calendar` (/me/events) + `backup_calendar_flanks` + `backup_event_attachments` (`connectors/calendar.rs`); `calendar_preview`; `CalendarWriteHandler` + `/api/v1/calendar/*` + `calendar_live.rs`. ✓ `get_json` immutable-id fix.
- **Live (prior session):** 2 calendars colour-mapped, recurrence expansion, create/update/delete 200.

## S-P4.9 #566 — Contacts
- `contact_preview` (3 addresses/IM/categories/relationships) + `contact_photo` (`/api/v1/contact/photo`); `ContactWriteHandler` + `/api/v1/contact/*` + `contacts_live.rs`; `CONTACT_WRITABLE += otherAddress/imAddresses`. ✓
- **Live (this epic):** photo endpoint byte-exact, detail 12/12 fields, create/edit/delete Graph-confirmed.

## S-P4.10 #567 — ToDo
- `backup_task_subresources` (checklist/linked/attachments) + `backup_todo_list_flanks`; `todo_preview`; `TaskWriteHandler` (9 verbs) + `/api/v1/todo/*` + `task_live.rs`. ✓ 2 live-found fixes (attachments need Tasks.ReadWrite + per-attachment contentBytes fetch).
- **Live (this epic):** 10 checklist steps, attachment download "ABC", create/complete/edit/checklist/list ops Graph-confirmed.

## S-P4.11 #568 — OneNote
- `backup_onenote_hierarchy` (notebooks/section-groups/sections as items) + `_pagemeta_` sidecars; `onenote_preview`; `OneNoteWriteHandler` (create/delete/append) + `/api/v1/onenote/*` + `onenote_live.rs`; restore-to-original-section (`OneNoteApi::create_page` section + 404 fallback). ✓ live-found PAGES_URL `$expand` both parents.
- **Live (this epic):** Page→Section→Notebook chain, tree UI (not flat), metadata strip, restore-to-original-section, create/append/delete Graph-confirmed.

## Live matrix (S-P4.12 B2) — daemon @127.0.0.1:8869, all six services backed up vs the backupslave throwaway account

**Live read (playwright headless 1920×1200, every view renders live data + 4-state badges + write UI):**
| Service | Items rendered | States shown live | Write UI present |
|---|---|---|---|
| Mail | 209 messages | live_backup, live_only (8) | Compose / Verify / Unread / per-message manage |
| OneDrive | 14 files (tiles) | live_backup, live_only, **backup_only** | New / Verify / state filter |
| Calendar | 9 events (agenda) | live_backup | New event / Verify |
| Contacts | 17 contacts | live_backup | New contact / Verify |
| ToDo | 5 tasks (kanban, 2 lists) | live_backup | New task / New list / Verify |
| OneNote | 3 pages / 3 notebooks (notebook→section→page **tree**, not flat) | live_backup (2), live_only (1), backup_only (filter) | New page / Verify |

**4-state honesty:** `live_backup` shown across all six; `live_only` shown on Mail + OneNote; `backup_only` shown on OneDrive + OneNote. `stale` is **unit-proven** (`backup_state_derives_four_states`, `gui/webui/src/lib.rs`) and **transient** — the backup pass re-archives a changed body in the same pass (sqlite confirmed `etag == body_etag` after a contact `jobTitle` PATCH), so the stale window closes sub-poll and is not live-inducible with fast polling.

**Write gating (fresh curl):** every write endpoint (`mail/flag`, `calendar/create`, `contact/create`, `todo/create`, `onenote/create`) and `settings` → **401** without `X-Capability-Token`. Per-service live writes were Graph-confirmed in the per-story rows above.

**Interval slider:** `POST /api/v1/settings?poll_interval_secs=600` + cap → **200** `{"poll_interval_secs":600}`; `=99999` + cap → **400** (range 1..=3600); no cap → **401**.

**429 / Retry-After:** `crates/graph/src/throttle.rs` honors `Retry-After`, backs off exponentially when absent, and applies hard caps — unit-covered.

## Findings
None open. All per-service decisions shipped; 5 live-found bugs across #564/#567/#568 were fixed immediately in their slices. Residuals (documented, not bugs): OneNote `level/order/userTags` are Graph-conditional (captured when present); a full FUSE-mount `stat` and a 2nd-account calendar "accept invite" need external setup (proven via code paths instead); `stale` is transient under fast polling (unit-proven, not live-inducible).

## Story status (close-prep, S-P4.12 B5)

Every Phase-4 story is implemented and locally verified; the whole stack is one linear branch (`feat/p4-verify`) ahead of `origin/dev`. Issues stay open until the gated push → cascade → close.

| Issue | Story | Status | Evidence |
|---|---|---|---|
| #557 | S-P4.0 capability spike | closed | informed per-service decisions |
| #558 | S-P4.1 scope expansion | implemented, verified | `READ_SCOPES`/`RESTORE_SCOPES`; gate green |
| #559 | S-P4.2 poll engine + SSE + slider | implemented, verified | interval slider 200/400/401 live; SSE wired |
| #560 | S-P4.3 4-state badge | implemented, verified | live_backup/live_only/backup_only live; stale unit-proven |
| #561–#563 | S-P4.4/4.5/4.6 Mail | implemented, verified | compose/flag/move Graph-confirmed; daemon SSE AC-5 |
| #564 | S-P4.7 OneDrive | implemented, verified | quota + permissions live; 2 bugs fixed |
| #565 | S-P4.8 Calendar | implemented, verified | 9 events agenda; create/update/delete 200 |
| #566 | S-P4.9 Contacts | implemented, verified | 17 contacts; photo byte-exact; CRUD Graph-confirmed |
| #567 | S-P4.10 ToDo | implemented, verified | checklist/attachments; CRUD Graph-confirmed |
| #568 | S-P4.11 OneNote | implemented, verified | tree (not flat); restore-to-section; create/append/delete |
| #569 | S-P4.12 epic verification | implemented, verified | this document + live matrix above |
| #556 | Epic | ready to close on GO | all stories implemented + locally verified |

Gate: `fmt`/`clippy`/`test --workspace` (36 suites) 0 failed · graph `--features http` 83/0 · `node --check` OK · traceability 40/40 · release build clean (4 binaries) + smoke green. The push → cascade (dev→staging→main) → RC tag → issue/epic close is the single irreversible step and runs only on explicit GO.

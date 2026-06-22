# Phase-4 Verification — line-by-line plan diff (epic #556, S-P4.12 / #569)

Per-service confirmation that every Phase-4 decision shipped in the runtime path, with the landed symbol (file) and the live-verification status. Local verification only; the push → cascade → RC → close is gated on explicit GO (S-P4.12 B5).

Gate (all stories): `cargo +1.95.0 fmt --all --check` clean · `clippy --workspace --all-targets -D warnings` clean · `test --workspace` 0 failed (36 suites) · `node --check gui/webui/src/app.js` OK.

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

## Findings
None open. All per-service decisions shipped; 5 live-found bugs across #564/#567/#568 were fixed immediately in their slices. Residuals (documented, not bugs): OneNote `level/order/userTags` are Graph-conditional (captured when present); a full FUSE-mount `stat` and a 2nd-account calendar "accept invite" need external setup (proven via code paths instead).

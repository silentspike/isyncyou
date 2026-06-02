# Microsoft Graph Capability Matrix (Personal/Family)

> Status: spike findings + **live-verified by the Rust connectors** (see the
> "iSyncYou connector status" section at the bottom) · Phase -1 spike (#35) ·
> Test account: `backupslave@outlook.com` (dedicated throwaway)
> Apps: `backup_read` (`cee80dd9…`, full read scopes) · `backup_write` (`a90d9140…`, full write scopes except OneNote)
> Authority: `https://login.microsoftonline.com/consumers` (PersonalMicrosoftAccount)

Two evidence sources:
- **SPIKE** — proven live against the test account during this spike (commands + responses).
- **/backup** — already built **and tested** in the `/work/proxmox/backup/` system (FastAPI), TEST-REPORT 2026-02-05 (8/8 features PASS). Re-implemented in Rust for iSyncYou; Graph behaviour is considered proven.

---

## Scopes / consent

| Finding | Evidence |
|---|---|
| **Incremental consent**: re-auth only prompts for *new* scopes; previously granted ones are not shown again. | SPIKE (read re-auth showed only Files/Contacts/Notes) |
| **read app** delegated scopes granted (personal): `User.Read, Mail.Read, Calendars.Read, Contacts.Read, Tasks.Read, Files.Read, Notes.Read` | SPIKE (`granted scopes:` confirmed) |
| **write app** delegated scopes granted (personal): `User.Read, Mail.ReadWrite, Mail.Send, Calendars.ReadWrite, Contacts.ReadWrite, Tasks.ReadWrite, Files.ReadWrite` | SPIKE |
| **OneNote write blocked on personal**: app registration uses `Notes.ReadWrite.All`, which requires admin consent and is **not grantable on a personal MSA**. OneNote write needs the delegated `Notes.ReadWrite` scope added to the app registration. OneNote **read** (`Notes.Read`) works. | SPIKE (write re-auth deliberately omitted Notes; read consented Notes.Read) |
| Device-code flow works for headless re-auth; refresh tokens follow the rolling ~90-day inactivity window (read app RT had expired after ~107 days). | SPIKE |

---

## OneDrive (Files) — **proven this spike (was the biggest unknown)**

| Capability | Result | Plan ref |
|---|---|---|
| Drive type / quota | `personal`, ~1.1 TB quota | — |
| `/me/drive/root/delta` (initial) | `200`, id-based items, `@odata.deltaLink` returned | §5 stateful delta |
| Delta item fields | `id, name, fileSystemInfo, parentReference, folder|file, size, eTag/cTag, createdDateTime, lastModifiedDateTime` | §6 |
| Incremental delta via `deltaLink` | `200`, picks up newly uploaded items | §5 correctness |
| **quickXorHash** present on files | `file.hashes.quickXorHash` returned | §23 skip-detection |
| Simple upload (`PUT …:/content`) | `200`, returns `id` + `eTag` | §6 |
| **`fileSystemInfo.lastModifiedDateTime` settable + preserved** | set `2021-06-15T10:00:00Z` → read back identical | §6 mtime preservation (A1-critical) |
| **Upload session** (`createUploadSession`) | `200`, `uploadUrl` returned | §6 |
| Chunked upload, 320 KiB multiples | chunk1 → `202` with `nextExpectedRanges: ['327680-655359']`; final chunk → `200` (id) | §6 resume |
| No `Authorization` header on `uploadUrl` | works (header omitted) | §6 |
| **ETag / `If-Match` stale → `412`** | confirmed `412` | §10 no silent overwrite (A3) |
| **Personal Vault** | appears as a root item but contents are **not accessible** (locked) | §8.3 (exclude) |

`spikes/probe_onedrive.py` reproduces all of the above.

**Open OneDrive items:** `410 Gone` resync path (not yet forced); large-file (>60 MiB) multi-chunk; conflictBehavior matrix (fail/replace/rename); delete→tombstone in delta.

---

## Mail — **proven in /backup**

| Capability | Notes | Source |
|---|---|---|
| Delta per folder `/me/mailFolders/{id}/messages/delta` | folder tree + per-folder cursor | /backup (`sync_mails.py`) |
| Message read + `.eml` (sharded storage) + attachments | `data/mails/ab/c1/…` sharded; `mail_attachments` table | /backup |
| Categories on mail (`PATCH /me/messages/{id}` `categories`) | tested PASS (#27) | /backup TEST-REPORT |
| Restore (re-create) | `POST /backup/restore/{mail_id}` route implemented | /backup |
| Send / reply / replyAll / forward / move | endpoints implemented (`/me/sendMail`, `…/reply`, `…/move`) | /backup |

---

## Calendar — **proven in /backup (restore tested PASS)**

| Capability | Notes | Source |
|---|---|---|
| `calendarView` sync (date-range) | 35+ fields, categories, attendees, body | /backup (`sync_calendar.py`), TEST-REPORT #24/#25 |
| **Restore via Graph** | `POST /backup/calendar/restore/{event_id}` — **tested PASS (#26)** | /backup TEST-REPORT |
| FTS5 search over events | `calendar_fts` | /backup |

---

## ToDo — **proven in /backup**

| Capability | Notes | Source |
|---|---|---|
| `/me/todo/lists/{id}/tasks` delta | lists + tasks | /backup (`sync_todo.py`) |
| Restore | `POST /backup/todo/{task_id}/restore` (into a restore list) | /backup |
| Read least-priv | `Tasks.Read` sufficient for read | SPIKE (read app) |

---

## Categories (Outlook masterCategories) — **proven in /backup**

| Capability | Notes | Source |
|---|---|---|
| CRUD `/me/outlook/masterCategories` | create/delete + preset colors | /backup (`sync_categories.py`), TEST-REPORT #23 |
| Restore / restore-all | `POST /backup/categories/restore-all` | /backup |

---

## Contacts — **not yet probed**

`Contacts.Read` (read) + `Contacts.ReadWrite` (write) are consented on the test account. Endpoints `/me/contactFolders` + `/me/contacts/delta`, vCard fidelity and photos still to verify. → follow-up probe.

---

## OneNote — **partially**

| Capability | Result |
|---|---|
| Read notebooks/pages (`Notes.Read`) | works (`/me/onenote/notebooks` `200` in earlier read test) |
| No delta endpoint | confirmed (plan §6) — use ETag/lastModified polling |
| **Write blocked on personal** | `Notes.ReadWrite.All` needs admin; add delegated `Notes.ReadWrite` to the app registration to enable restore |

---

## Reuse decision

Mail/Calendar/ToDo/Categories Graph behaviour + the SQLite/FTS5 data model + restore logic + viewer templates are **already proven in `/backup`** and are reused (re-implemented in Rust) for iSyncYou Phase 2. The spike effort concentrates on the genuinely new/risky parts: **OneDrive bidirectional sync** (done here) + Contacts + OneNote write enablement.

---

## iSyncYou connector status (live-verified in Rust)

The connectors are now implemented in the `isyncyou-connectors` crate and each was
run live against `backupslave` (env-gated tests; CI without a token skips). This
**closes the spike's open items**: OneDrive `410`→resync and delete→tombstone are
implemented and unit-tested; Contacts is implemented and the live default-collection
delta + cursor were verified; OneNote uses a full-list reconcile (no delta).

| Service | Connector | Index live result | Body live result |
|---|---|---|---|
| OneDrive | `onedrive` (bidirectional) | delta + cursor + `410`-resync + tombstones | resumable up/download, byte-identical |
| Mail | `mail` (per-folder delta) | 8 folders, 162 messages | 3 `.eml` downloaded (68,937 B) |
| Calendar | `calendar` (windowed delta) | 1 calendar, 9 events | 3 event JSON archived |
| Contacts | `contacts` (default + folders) | default delta + cursor (0 contacts present) | — |
| ToDo | `todo` (per-list delta) | 2 lists, 5 tasks | 3 task JSON archived |
| OneNote | `onenote` (full-list reconcile) | walk ran (0 pages — no notebook) | HTML body endpoint wired |

End-to-end via the CLI: `isyncyou backup` indexed 162 mails / 9 events / 5 tasks and
archived bodies to a sharded store in one run; `isyncyou restore` re-created an
archived event in the cloud (then cleanup-deleted it). Restore detail in
[`restore-fidelity-matrix.md`](restore-fidelity-matrix.md).

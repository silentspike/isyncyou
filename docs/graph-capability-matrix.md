# Microsoft Graph Capability Matrix (Personal/Family)

> Status: spike findings + **live-verified by the Rust connectors** (see the
> "iSyncYou connector status" section at the bottom) · Phase -1 spike (#35) ·
> Test account: `testuser@example.com` (dedicated throwaway)
> Apps: `backup_read` (`cee80dd9…`, full read scopes) · `backup_write` (`a90d9140…`, write/restore scopes incl. delegated `Notes.ReadWrite`)
> Authority: `https://login.microsoftonline.com/consumers` (PersonalMicrosoftAccount)

Two evidence sources:
- **SPIKE** — proven live against the test account during this spike (commands + responses).
- **prior art** — already built **and tested** in an earlier Python (FastAPI) implementation (full feature pass). Re-implemented in Rust for iSyncYou; the Graph behaviour is considered proven.

---

## Scopes / consent

| Finding | Evidence |
|---|---|
| **Incremental consent**: re-auth only prompts for *new* scopes; previously granted ones are not shown again. | SPIKE (read re-auth showed only Files/Contacts/Notes) |
| **read app** delegated scopes granted (personal): `User.Read, Mail.Read, Calendars.Read, Contacts.Read, Tasks.Read, Files.Read, Notes.Read` | SPIKE (`granted scopes:` confirmed) |
| **write app** delegated scopes needed (personal): `User.Read, Mail.ReadWrite, Mail.Send, Calendars.ReadWrite, Contacts.ReadWrite, Tasks.ReadWrite, Files.ReadWrite, Notes.ReadWrite` | SPIKE + code invariant (`RESTORE_SCOPES`) |
| **OneNote write scope correction**: `Notes.ReadWrite.All` requires admin consent and is **not grantable on a personal MSA**; delegated `Notes.ReadWrite` is the personal-account scope and is now enforced in the restore scope set. OneNote **read** (`Notes.Read`) works. | SPIKE + `restore_scopes_use_delegated_onenote_write_scope` |
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

**Remaining OneDrive live items:** large-file (>60 MiB) multi-chunk and the full
`conflictBehavior` matrix (fail/replace/rename). The `410 Gone` resync path and
delete→tombstone handling are implemented and unit/acceptance-tested.

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

## Contacts — **implemented in Rust**

`Contacts.Read` (read) + `Contacts.ReadWrite` (write) are consented on the test
account. The Rust connector handles the default contacts collection plus contact
folders; the live test account had no contacts, so the default delta/cursor path
was verified and photo/body richness remains a fidelity follow-up.

---

## OneNote — **partially**

| Capability | Result |
|---|---|
| Read notebooks/pages (`Notes.Read`) | works (`/me/onenote/notebooks` `200` in earlier read test) |
| No delta endpoint | confirmed (plan §6) — use ETag/lastModified polling |
| **Write scope** | use delegated `Notes.ReadWrite`; never request admin-only `Notes.ReadWrite.All` |
| Page restore helper | `POST /me/onenote/pages` from archived HTML is implemented as an env-gated connector test when the write token carries `Notes.ReadWrite`; resource-bearing pages use the documented multipart `Presentation` + `name:part` binary-data shape |

---

## Reuse decision

Mail/Calendar/ToDo/Categories Graph behaviour + the SQLite/FTS5 data model + restore logic + viewer templates are **already proven in `/backup`** and are reused (re-implemented in Rust) for iSyncYou Phase 2. The genuinely new/risky parts are now split: **OneDrive bidirectional sync** and contacts indexing are implemented; OneNote write uses the delegated scope and remains product-gated until its cloud restore path is ledger-migrated. Contact photos are archived locally, but Microsoft Graph's profilePhoto update permission table marks contact-photo update as unsupported for delegated personal Microsoft accounts, so Personal/Family restore does not attempt a photo upload.

---

## iSyncYou connector status (live-verified in Rust)

The connectors are now implemented in the `isyncyou-connectors` crate and each was
run live against `testuser` (env-gated tests; CI without a token skips). This
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
| OneNote | `onenote` (full-list reconcile) | walk ran (0 pages — no notebook) | HTML body + resource manifest archive wired |

End-to-end via the CLI: `isyncyou backup` indexed 162 mails / 9 events / 5 tasks and
archived bodies to a sharded store in one run; `isyncyou restore` re-created an
archived event in the cloud (then cleanup-deleted it). Restore detail in
[`restore-fidelity-matrix.md`](restore-fidelity-matrix.md).

# Restore fidelity matrix

What iSyncYou's `restore-cloud-item` preserves when re-creating a backed-up item
in the cloud, and where fidelity is lost. Restore is a **high-fidelity re-create**,
not a byte-identical mailbox import — personal Microsoft accounts have no import
API, and a re-create with rich metadata is sufficient (plan §12).

All restores were live round-tripped against the throwaway account
`backupslave@outlook.com` (fetch/build → restore → verify → cleanup-delete), never
the real account. Implemented in `isyncyou-connectors::restore`; driven from the
CLI via `isyncyou restore`.

Legend: **preserved** = round-trips intact · **reset** = server assigns a new
value · **lossy** = approximated/dropped · **n/a** = not applicable.

| Service | Method | Preserved | Reset by Graph | Lossy / not supported | Live |
|---|---|---|---|---|---|
| Mail | `POST /me/messages` (base64 MIME, `text/plain`) | full MIME: headers, body (HTML+text), attachments, `internetMessageId` | message `id`; lands in **Drafts** (`isDraft=true`) | original folder placement (a follow-up move); read/flag state | ✅ synthetic MIME → draft → verified → deleted |
| Calendar | `POST /me/events` | subject, body, start/end, location, attendees, categories, importance, sensitivity, showAs, recurrence, reminders | event `id`, `iCalUId`, `changeKey`, `webLink`, `createdDateTime` | organizer is set to the mailbox owner; an occurrence restores as a single instance | ✅ real event 'Team-Standup' re-created → verified → deleted |
| ToDo | `POST /me/todo/lists/{list}/tasks` | title, body, importance, status, due/start/reminder, categories, recurrence | task `id`, timestamps | restored into the chosen list (original list id if archived) | ✅ real task 'Backup Test Task' re-created → verified → deleted |
| Contacts | `POST /me/contacts` | display/given/sur/middle/nick name, emails, phones, company, jobTitle, addresses, birthday, notes, categories | contact `id`, timestamps | contact photo (separate `$value` upload) — not yet restored | ✅ synthetic contact re-created → verified → deleted |
| OneNote | — | — | — | **no simple restore** — pages cannot be re-created via a plain POST; deferred | — |

## How the payload is built

For the JSON services (calendar/contacts/todo), the archived canonical JSON is
passed through a **field whitelist** (`sanitize_event` / `sanitize_task` /
`sanitize_contact`) that drops server-managed fields (`id`, `@odata.etag`,
`changeKey`, timestamps, `webLink`, `type`, `organizer`, …) so Graph accepts the
create and assigns fresh values. A whitelist (rather than a denylist) keeps the
create accepted even as Graph adds new read-only fields over time.

For mail, the whole `.eml` MIME is re-created verbatim via a base64 body — nothing
is stripped, so the message round-trips faithfully (it simply lands in Drafts with
a new id, per Graph's MIME-create behaviour).

## Restore paths (plan §12)

1. **restore-local-item** — read the archived body from `archive_root` and present
   it (the web UI `body` endpoint, served as inert `text/plain`/JSON).
2. **restore-cloud-item** — the table above: re-create in the cloud via Graph.
   Available from the CLI (`isyncyou restore --service … --id …`).
3. **restore-from-pbs-snapshot** — import a PBS snapshot into a temporary restore
   store first (never PBS → live directly), then restore from it. **Not yet
   implemented** (tracked under S-17 / #26).

# Restore fidelity matrix

What iSyncYou's `restore-cloud-item` preserves when re-creating a backed-up item
in the cloud, and where fidelity is lost. Restore is a **high-fidelity re-create**,
not a byte-identical mailbox import — personal Microsoft accounts have no import
API, and a re-create with rich metadata is sufficient (plan §12).

Connector-level restore helpers are implemented in `isyncyou-connectors::restore`
and are unit-tested; live round-trips are env-gated against the throwaway account
`testuser@example.com` (fetch/build → restore → verify → cleanup-delete), never
the real account. The product cloud-mutation entry point is stricter: today only
mail is ledger-backed and accepted by `isyncyou restore`; calendar/contacts/ToDo
and OneNote cloud restore are refused until each path is migrated to the crash-safe
operation ledger.

Legend: **preserved** = round-trips intact · **reset** = server assigns a new
value · **lossy** = approximated/dropped · **n/a** = not applicable.

| Service | Method | Preserved | Reset by Graph | Lossy / not supported | Live |
|---|---|---|---|---|---|
| Mail | `POST /me/messages` (base64 MIME, `text/plain`) | full MIME: headers, body (HTML+text), attachments, `internetMessageId` | message `id`; lands in **Drafts** (`isDraft=true`) | original folder placement (a follow-up move); read/flag state | ✅ synthetic MIME → draft → verified → deleted |
| Calendar | `POST /me/events` | subject, body, start/end, location, attendees, categories, importance, sensitivity, showAs, recurrence, reminders | event `id`, `iCalUId`, `changeKey`, `webLink`, `createdDateTime` | organizer is set to the mailbox owner; recurring events are captured as the series master (with its recurrence rule) **and** their windowed occurrences (linked via `seriesMasterId`) — restoring the master recreates the recurring series, restoring a single occurrence makes a one-off event | ✅ real event 'Team-Standup' re-created → verified → deleted |
| ToDo | `POST /me/todo/lists/{list}/tasks` | title, body, importance, status, due/start/reminder, categories, recurrence | task `id`, timestamps | restored into the chosen list (original list id if archived) | ✅ real task 'Backup Test Task' re-created → verified → deleted |
| Contacts | `POST /me/contacts` | display/given/sur/middle/nick name, emails, phones, company, jobTitle, addresses, birthday, notes, categories | contact `id`, timestamps | contact photo is archived locally, but Graph contact-photo update is **not supported for delegated personal Microsoft accounts** ([Microsoft Graph profilePhoto update permissions](https://learn.microsoft.com/en-us/graph/api/profilephoto-update?view=graph-rest-1.0)); photo fields are deliberately stripped from contact create payloads | ✅ synthetic contact re-created → verified → deleted |
| OneNote | `POST /me/onenote/pages` (`text/html` or multipart `Presentation` + binary parts) | page title/body HTML accepted by Graph when the token has `Notes.ReadWrite`; referenced page resources are archived locally with a per-page manifest and connector restore can replay binary parts via Graph's documented `name:part` multipart shape | page `id`, timestamps, section placement | product cloud restore is still refused until the OneNote path is migrated to the crash-safe ledger | ⏳ env-gated connector live test |

## How the payload is built

For the JSON services (calendar/contacts/todo), the archived canonical JSON is
passed through a **field whitelist** (`sanitize_event` / `sanitize_task` /
`sanitize_contact`) that drops server-managed fields (`id`, `@odata.etag`,
`changeKey`, timestamps, `webLink`, `type`, `organizer`, …) so Graph accepts the
create and assigns fresh values. A whitelist (rather than a denylist) keeps the
create accepted even as Graph adds new read-only fields over time.

Contact photos are kept in the archive for inspection/export, but they are not
posted during Personal/Family contact restore. Microsoft Graph's profilePhoto
update permission matrix lists contact-photo update as unsupported for delegated
personal Microsoft accounts, so `sanitize_contact` drops `photo` and
`photo@odata.*` sidecar metadata.

For mail, the whole `.eml` MIME is re-created verbatim via a base64 body — nothing
is stripped, so the message round-trips faithfully (it simply lands in Drafts with
a new id, per Graph's MIME-create behaviour). For OneNote, the archived HTML body
is posted to the page-create endpoint; safe OneNote resource URLs referenced from
that HTML are archived with a `.resources.json` manifest. Connector-level restore
can replay binary resources as multipart `name:part` data parts, matching
[Microsoft Graph's OneNote page-create contract](https://learn.microsoft.com/en-us/graph/onenote-create-page);
product cloud restore remains refused until the OneNote path is ledger-migrated.

## Restore paths (plan §12)

1. **restore-local-item** — read the archived body from `archive_root` and present
   it (the web UI `body` endpoint, served as inert `text/plain`/JSON).
2. **restore-cloud-item** — re-create in the cloud via Graph. The connector
   capabilities are listed above; the shipping engine accepts only ledger-backed
   mail today and refuses the other services before token lookup.
3. **restore-from-pbs-snapshot** — import a PBS snapshot into a temporary restore
   store first (never PBS → live directly), then restore from it. PBS backup/list/
   restore CLI plumbing exists, restores into a temporary directory, and has been
   live-confirmed against a real PBS repository; previewing and selecting items
   from that temporary store is the next product layer.

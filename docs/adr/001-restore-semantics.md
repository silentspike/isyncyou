# ADR-001 — Crash-safe cloud restore semantics

- **Status:** Accepted (design). The runtime gate (`restore.cloud_restore_enabled`,
  default `false`) already ships; the ledger and crash matrix below are implemented as
  a tracked sequence of changes and gate the flag being safe to turn on.
- **Date:** 2026-06-03
- **Related:** risk [R1](../security/risk-register.md),
  [AI protocol](../ai/AI_ASSISTED_ENGINEERING_PROTOCOL.md).

---

## Context

Restoring an archived item back into the cloud is, at minimum, two operations that are
**not atomic**:

1. a Microsoft Graph write that creates the item (a real, externally visible side
   effect), and
2. a local record that says *"this item now exists in the cloud."*

No transaction spans a remote API call and a local SQLite write. If the process is
killed, the network drops, or the access token expires **between** step 1 and step 2,
the cloud holds the item but the local store does not know it. A naive implementation
then does the worst possible thing on the next run: it repeats the `POST` and creates a
**duplicate** in the user's real mailbox/calendar. Under a flaky network this produces N
duplicates.

The correctness therefore cannot come from a transaction. It has to come from making the
**retry** idempotent and from never losing track of an in-flight operation.

## Decision

Introduce a durable **operation ledger** with an explicit state machine, a
content-derived **idempotency key**, a **service-native or stamped marker** used only to
*find* a possibly-created item, **auto-recovery on daemon start**, and a **lease** for
single-owner execution. The existing `cloud_restore_enabled` gate stays off until this is
complete.

### Data model (store schema v7, additive)

```sql
-- one row per restore intent
CREATE TABLE restore_operations (
  op_id            TEXT PRIMARY KEY,   -- ULID-like, client-generated
  account          TEXT NOT NULL,
  service          TEXT NOT NULL,      -- mail|calendar|contacts|todo|onenote
  source_item_id   TEXT NOT NULL,      -- archived item being restored
  idempotency_key  TEXT NOT NULL,      -- HMAC-SHA256 (see below); UNIQUE per account
  state            TEXT NOT NULL,      -- see state machine
  new_cloud_id     TEXT,               -- set once the create is confirmed
  marker           TEXT,               -- service-native id / stamped probe token
  attempts         INTEGER NOT NULL DEFAULT 0,
  lease_owner      TEXT,               -- daemon/process instance holding the op
  lease_expires_at INTEGER,            -- unix seconds; expiry releases the lease
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL,
  last_error       TEXT,
  UNIQUE(account, idempotency_key)
);

-- append-only audit of every state transition (evidence + debuggability)
CREATE TABLE restore_steps (
  op_id      TEXT NOT NULL REFERENCES restore_operations(op_id),
  seq        INTEGER NOT NULL,
  from_state TEXT,
  to_state   TEXT NOT NULL,
  at         INTEGER NOT NULL,
  detail     TEXT,
  PRIMARY KEY (op_id, seq)
);
```

The `UNIQUE(account, idempotency_key)` constraint is the backstop: two concurrent
attempts to restore the same content collide at the database, not in the user's mailbox.

### State machine

```
                        ┌─────────────────────────────────────────┐
                        │                                          │
   create row           ▼   reserve + preflight        graph POST  │ confirmed
  ──────────▶ pending ──────▶ preflight_checked ──────▶ committing ─┴──▶ committed
                  │                  │                       │
                  │ abandon          │ abandon               │ crash / error
                  ▼                  ▼                       ▼
              cancelled          cancelled        failed_after_graph_commit
                                                            │
                                                 reconcile on recovery
                                            ┌───────────────┴───────────────┐
                                            │                               │
                                  marker found in cloud           marker NOT found
                                            ▼                               ▼
                                       committed                     (retry) committing
```

- **pending** — intent recorded; nothing sent.
- **preflight_checked** — payload built, idempotency key computed, marker embedded,
  lease taken; we are about to call Graph. Recording this *before* the call is what lets
  recovery know a `POST` *might* have happened.
- **committing** — the Graph call is in flight.
- **committed** — terminal success; `new_cloud_id` set.
- **failed_after_graph_commit** — the dangerous state. We crashed/errored after leaving
  `preflight_checked`/`committing`, so the create may or may not have landed. Recovery
  **must reconcile** (probe by marker) — it must never blind-retry.
- **cancelled** — terminal; explicitly abandoned before any side effect.

### Idempotency key

```
idempotency_key = HMAC-SHA256( local_secret,
                               account || service || source_item_id || canonical(payload) )
```

- `local_secret` is a per-install key (kept with the store, never logged), so keys are
  not guessable across installs.
- `canonical(payload)` is the byte-stable serialization actually sent (the MIME for
  mail, the canonical JSON for calendar/contacts/todo, the HTML for OneNote). Identical
  content ⇒ identical key ⇒ a retry is recognised as "the same restore," not a new one.

### Marker strategy — ledger is the authority

The marker exists **only to find** a possibly-created item during reconciliation. The
**ledger row is the source of truth**; the marker is a search hint. We never let "marker
present in cloud" override the ledger, nor pick between two disagreeing weak signals.

Per service, the marker is service-native where the platform offers idempotency, and a
stamped token otherwise:

| Service | Marker mechanism | Recovery probe |
|---|---|---|
| **calendar** | Graph **`transactionId`** on `POST /me/events` — server-side dedup of duplicate POSTs for a retention window. Strongest case: the platform itself refuses the duplicate. | `GET /me/events?$filter=transactionId eq '{key}'` |
| **mail** | A controlled **`Message-ID`** header embedded in the MIME we post. | `GET /me/messages?$filter=internetMessageId eq '{message-id}'` |
| **contacts** | Idempotency key written to a **single-value extended property** on the contact. | `GET /me/contacts?$filter=singleValueExtendedProperties/any(...)` |
| **todo** | Key embedded in a known field of the task (e.g. a body marker line). | list the target task list and match the marker |
| **onenote** | Hidden marker (`<meta name="isyncyou-op" content="{key}">`) in the page HTML. | `GET /me/onenote/pages?$filter=title eq ...` then confirm the marker; OneNote search is weakest, so this service is the most conservative (see consequences). |

### Auto-recovery on daemon start

On boot, before accepting new restore work, the daemon scans
`restore_operations WHERE state NOT IN ('committed','cancelled')` and drives each to a
terminal state:

- `preflight_checked` / `committing` / `failed_after_graph_commit` ⇒ **reconcile**:
  run the service probe for the marker. Found ⇒ record `new_cloud_id`, move to
  `committed`. Not found ⇒ the create did not land; resume `committing` (a fresh POST is
  safe because nothing exists yet).
- `pending` ⇒ either resume or `cancelled` per policy.

Recovery is the normal startup path, not a manual rescue command.

### Concurrency / lease

A restore takes a lease (`lease_owner` + `lease_expires_at`) on its row before
`preflight_checked`. Another instance (or the same one after a crash) only takes over an
operation whose lease has **expired**. Combined with `UNIQUE(account, idempotency_key)`,
this guarantees a single owner drives a given restore at a time and a crashed owner's
work is safely reclaimed after lease expiry.

## Crash matrix (the proof)

Each point is a test that injects a failure there and asserts the invariant after
recovery: **exactly one cloud item, no loss, no duplicate.**

| # | Crash point | Ledger state at crash | Expected after recovery |
|---|---|---|---|
| C1 | before any write | (no row) | nothing created; no row |
| C2 | after `pending`, before POST | pending / preflight_checked | nothing created; resumes cleanly |
| C3 | **during POST** (unknown if it landed) | committing | reconcile by marker → exactly one item |
| C4 | **after POST, before local record** | failed_after_graph_commit | reconcile finds the item → committed, **no second POST** |
| C5 | after `committed` write, before lease release | committed | no-op; idempotent |
| C6 | concurrent second attempt, same content | two rows race | `UNIQUE(idempotency_key)` rejects the second; one item |
| C7 | token expiry mid-flight | committing | retry after refresh; marker prevents duplicate |

C3 and C4 are the whole reason this ADR exists; C6 proves the database backstop.

## Consequences

**Positive**
- An interrupted restore cannot silently duplicate a user's mailbox/calendar item.
- Restores are resumable and auditable (the append-only `restore_steps` log is evidence).
- The `cloud_restore_enabled` flag can be turned on *with* the crash matrix green, rather
  than shipped on faith.

**Negative / cost**
- Extra durable writes per restore (ledger + steps) and a store schema bump to v7.
- More moving parts: lease handling, recovery path, per-service probes.
- **OneNote is the weakest link** — its query surface for finding a just-created page is
  poor, so for OneNote we keep the most conservative posture (smaller batch, explicit
  user confirmation on ambiguous reconcile) and document the residual risk rather than
  claim parity with calendar/mail.

**Deferred**
- Cross-account or bulk restore orchestration is out of scope for this ADR.
- At-rest encryption of the ledger is covered by risk R2, not here.

## Alternatives considered

- **Blind retry on failure** — rejected: this is exactly the duplication bug.
- **Best-effort client-side dedup only** (no durable ledger) — rejected: loses the
  "possibly committed" knowledge across a process restart, which is the crucial case.
- **Disable cloud restore permanently** — rejected: local-file restore is unaffected and
  always available, but a backup tool that cannot put data back is half a tool; the
  honest path is to make the dangerous operation correct, not to remove it.
- **Rely solely on a service marker** (no ledger) — rejected: a marker is a find-hint
  with per-service strength (strong for calendar, weak for OneNote); it cannot be the
  authority. The ledger is.

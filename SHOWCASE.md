# iSyncYou — Technical Showcase

> A five-minute tour of the engineering decisions in this repository, aimed at a
> reviewer who wants to judge depth quickly. It does not re-explain what the
> product *is* (see the [README](README.md)) — it explains the parts that were
> hard and how they were solved.

If you only read one section, read **§1**.

---

## 1. The central problem: crash-safe cloud restore

**Claim:** a restore tool that can crash mid-operation and silently double-write
your mailbox is worse than no restore tool at all.

Re-creating an archived item in the cloud is not one atomic action. It is:

```
1. POST /me/messages          (Microsoft Graph creates the item — real side effect)
2. record "restored, id=…"     (local SQLite — bookkeeping)
```

Nothing makes those two steps atomic. A `SIGKILL`, an OOM, a dropped connection or
an expired token **between** step 1 and step 2 leaves the system in a state where
the cloud has the item but the local store does not know it. The naive next run
re-POSTs and creates a **duplicate** in the user's real inbox. Repeat under a flaky
network and you get N duplicates.

### The design that fixes it

**Operation ledger.** Every cloud-mutating restore is a row in a `restore_operations`
table with an explicit state machine, written *before* the Graph call and updated
*after* it:

```
pending ─▶ preflight_checked ─▶ committed
                 │
                 └─▶ failed_after_graph_commit   (the dangerous state — we know the
                                                   POST may have landed; never blind-retry)
```

The key insight is the `failed_after_graph_commit` state: when we crash right after
the POST, the *next* daemon start does not retry blindly. It treats the operation
as "possibly committed" and **reconciles** — it looks for the item the POST would
have created before doing anything else.

**Idempotency key.** Each operation carries `idempotency_key = HMAC-SHA256(local-key,
payload)` derived from the item's stable content. On recovery we can match "did I
already create something equivalent?" deterministically, instead of guessing from
timestamps.

**Restore marker, ledger-primary.** A per-service marker helps *find* a
possibly-created item during reconciliation, but the **ledger is the source of
truth** — the marker is only a search hint, never the authority. This avoids the
classic bug where two weak signals disagree and the code picks the wrong one.

**Auto-recovery on start (designed).** The recovery function (`recover_restore_op`)
takes any operation not in a terminal state and drives it to one — reconciling by
marker instead of blind-retrying — so the *intended* daemon path is to run it on
boot before accepting new work, not as a manual command.

**Crash matrix.** The proof is a test suite that injects a crash at each unsafe
point (`before POST`, `after POST / before commit`, `after commit / before marker`)
and asserts the post-recovery invariant: **exactly one** cloud item, **no** loss.

**Disabled by default.** Until that matrix is green *and wired into the live path*,
`cloud_restore_enabled = false`. The feature does not ship "mostly safe."

> **Where this stands today (honest status).** The ledger, the state machine, the
> recovery function and the crash matrix are implemented and pass — the crash matrix
> runs against a deliberately non-idempotent fake cloud (see §7). What is **not yet
> wired**: the live `engine::restore_cloud` path still calls the connectors directly
> rather than going through the ledger, and the daemon does not yet invoke
> `recover_restore_op` on boot. So the safety machinery is proven *in isolation*, and
> cloud restore stays **off by default** until that integration lands. This is also
> reflected in the README status table (🚧) and in `docs/requirements/restore.yml`
> (the integration requirements are marked `planned`).

This is deliberately the same shape of problem you hit in payments, message queues,
and any "call a remote API then record it locally" system. The interesting work is
not the Graph call — it is making the *retry* correct. The full design — schema,
state transitions, per-service recovery probes and the crash matrix — is specified in
[ADR-001](docs/adr/001-restore-semantics.md).

---

## 2. Tracking by id, never by path

Paths lie. A move or rename changes the path while the item is the same item; a
case-only rename on a case-insensitive cloud is invisible locally. So the store is
**id-based**: the primary key is `(drive_id, remote_id)` / `immutable_id`, never the
filesystem path. The local path is a derived attribute, recomputed by an iterative
parent-walk, with a persisted `path_history`.

The delta cursor is **persisted in the store**, not held in memory — so a restart
resumes the delta stream instead of doing a full re-scan. A Graph `410 Gone` on the
cursor triggers a *reconciliation* pass (empty-token resync that diffs and applies
differences), **not** a blind delete of everything.

Outlook items additionally carry `change_key`, `internet_message_id`, `ical_uid` and
`series_master_id`, requested consistently with `Prefer: IdType="ImmutableId"`, so
identity survives server-side moves.

---

## 3. The throttle pacer: full speed, but polite

There is no artificial bandwidth cap — the tool runs at full speed by design. The
only thing that slows it down is the cloud telling it to. The pacer is a shared
pacing token with truncated exponential backoff/decay: on `429`/`503` it backs off,
honours `Retry-After` exactly, **probes** whether the API is free again, and returns
to full speed. Error classes are separated (`429` retry, `507` fatal,
`401`-expired → token refresh, `416` → resume an upload, `pathTooLong` → no retry).
Batch sub-responses are evaluated individually, so one throttled item in a batch does
not poison the rest.

The user-facing consequence is a design rule: **the UI always shows *why* it is
slow** ("auto-throttle / 429 / network"), so the user never blames the tool or their
connection.

---

## 4. Resumable everything

- **Upload sessions** are chunked (multiples of 320 KiB, < 60 MiB), and the session
  state (URL, expiry, next-expected-ranges, etag) is **persisted to SQLite** — so an
  interrupted upload resumes across a *process kill*, not just a retry within one run.
  `416` → query `nextExpectedRanges` and resume; `404` → restart the session.
- **Downloads** resume from a `.part` file + offset sidecar.
- **mtime** is set explicitly on upload (`fileSystemInfo.lastModifiedDateTime`) and
  verified after, so a re-sync does not see every just-uploaded file as "changed."

Crash-safety is a theme, not a feature: atomic writes are `tmp → rename + fsync`, the
store runs in WAL mode with an `EXCLUSIVE` lock that doubles as a free single-instance
guard, and a periodic reconciler — not the inotify watcher — is treated as the source
of truth (the watcher is only an accelerator; an `IN_Q_OVERFLOW` forces a full scan).

---

## 5. The renderer is its own test harness

The native status bar uses an **own renderer** — `tiny-skia` (rasteriser) +
`cosmic-text` (shaping + font fallback) + `taffy` (flexbox), windowed via
`winit`/`softbuffer`. No webkit, no GTK, no GUI framework.

The reason is testability, not NIH. Because we own the renderer, *headless is the
normal mode*: the same code path renders to an on-screen buffer **or** to an
off-screen buffer that becomes a PNG. So the pixels a test inspects are, by
construction, the pixels a user sees — UI verification is a pixel-snapshot test with
no display server, no browser automation, no flaky screenshot tooling. Full control
(mail viewer, search, restore, settings) lives in a separate local web UI the daemon
serves to the user's *own* browser — no embedded browser engine anywhere.

---

## 6. Mass-delete guard

Both directions. If a single run would delete or replace more than *N* items
(local→cloud **or** cloud→local), the job stops and asks instead of proceeding. A
desynced cursor or a bad reconcile should never be able to wipe a mailbox unattended.
The threshold is configurable; the default errs toward stopping.

---

## 7. How to read the codebase

| You want to see… | Look at |
|---|---|
| The restore safety machine | `crates/connectors` (restore) + [ADR-001](docs/adr/001-restore-semantics.md) (operation ledger) |
| id-based reconciliation | `crates/store` (schema + migrations) + `crates/core` (sync-state) |
| The pacer | `crates/graph` (throttle/retry) |
| Path correctness | `crates/pathmap` (reversible encode + roundtrip tests) |
| Crash/data-loss proofs | `crates/acceptance` (A1–A10 + chaos matrix) |
| The renderer-as-harness | `gui/statusbar` |

For the narrative version — constraints, trade-offs, what was cut and why — see the
[case study](docs/case-study/fde-case-study.md).

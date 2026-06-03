# Case Study — Building a crash-safe personal cloud client, AI-assisted

> A delivery narrative: the real problem, the constraints, how the work was scoped
> and shipped, and what evidence backs each claim. Written for a reviewer assessing
> engineering judgement, not just code volume.

---

## The situation

A personal/family setup needed two things that no single off-the-shelf tool did
well together on Linux:

1. **Two-way OneDrive sync** that was fast on incremental changes. The incumbent
   (a stateless full-reconciler driven by `rclone`) took on the order of a *day*
   per pass because it had no persistent delta state — it re-walked the world every
   run.
2. **A real backup/archive of the rest of Microsoft 365** — mail, calendar,
   contacts, tasks, notes — that was *searchable* (including mail bodies) and could
   *restore* items, for personal accounts where no enterprise import path exists.

The decision was to build one coherent product in Rust rather than glue scripts
together: an engine daemon, a CLI, a local web UI, and a small native status bar.

This document is honest about the fact that the bulk of the product is **already
built and exercised by tests**. The interesting story is therefore not "can it be
built" — it is *which* problems turned out to be the hard ones, and how they were
made correct rather than merely working.

---

## The constraint that shaped everything

Personal Microsoft 365 accounts hold data you genuinely cannot afford to corrupt:
a decade of mail, a real calendar, family contacts. Two properties followed
directly:

- **Idempotency under failure is non-negotiable.** Any operation that mutates the
  cloud (most acutely, *restore*) must be safe to interrupt and safe to retry. A
  tool that creates duplicate mail on a flaky connection is not a backup tool; it
  is a liability.
- **Local data is the user's, so the tool must fail loud, not silent.** Mass
  deletions get a guard. Conflicts keep both copies. The UI explains *why* it is
  throttled so the user never misattributes a slowdown.

Everything below is downstream of those two constraints.

---

## Where the hard engineering actually was

### 1. Crash-safe cloud restore (the headline)

The naive restore is two non-atomic steps — a Graph `POST` that has a real side
effect, then a local write that records it. Crash in between and the next run
double-writes the mailbox. There is no cross-system transaction to lean on, so
correctness had to be designed in:

- an **operation ledger** with an explicit state machine, intent recorded *before*
  the call and outcome *after*;
- a `failed_after_graph_commit` state that forces **reconciliation, not blind
  retry**, after an interrupted call;
- a content-derived **idempotency key** (`HMAC-SHA256`) so a retry can recognise its
  own prior work;
- **auto-recovery on daemon start** as the normal path, not a manual rescue;
- a **crash matrix** of tests that kill the process at each unsafe point and assert
  *exactly one* cloud item and *no* loss;
- and the feature **off by default** until that matrix is green.

The full mechanism is in [`SHOWCASE.md`](../../SHOWCASE.md) §1. The judgement call
worth noting: it would have been faster to ship cloud restore as "works in the happy
path." It was shipped *disabled* instead, with the safety machine as a tracked,
provable piece of work. Slower, but it is the difference between a demo and a tool
you can trust with real mail.

### 2. Identity that survives moves

Tracking by path is a latent data-loss bug. The store is id-based end to end
(`(drive_id, remote_id)` / `immutable_id`), the delta cursor is persisted so
restarts resume rather than rescan, and a `410 Gone` reconciles instead of deleting.
This is unglamorous and it is exactly the kind of thing that, done wrong, quietly
loses files six months later.

### 3. Full speed without being a bad citizen

A shared pacer runs at full throughput until the cloud says `429`/`503`, then backs
off, honours `Retry-After`, probes, and resumes — with per-error-class handling so a
fatal `507` and a transient `429` are never confused. Resumable upload sessions
persist their state to disk so an interrupted 2 GB upload resumes across a process
kill, not just a retry inside one run.

### 4. A renderer chosen for testability

The native status bar is an own `tiny-skia` + `cosmic-text` renderer. The reason is
not aesthetics — it is that owning the render path makes *headless rendering the
normal mode*, so UI checks are deterministic pixel snapshots with no display server
and no browser automation. The same buffer that goes to the screen goes to the test.

---

## How it was built: AI-assisted, verification-first

This project is built with an AI coding agent operating under a strict protocol,
with a human owning direction and acceptance. The protocol is the interesting part,
because "AI wrote a lot of code" is worthless without it:

- **Nothing is "done" without executed evidence.** A claim of working behaviour is
  backed by a command and its real output — not by a code reading or a line-number
  citation. "Looks correct" is explicitly *not* evidence.
- **Every change passes the same gate before it lands:** `cargo fmt`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, and
  `cargo deny check`. The gate is mechanical and non-negotiable.
- **One complete vertical slice per change.** Work ships as a finished, verified
  unit behind a pull request, not as a pile of half-features. There is no
  "partially done" — a unit is either complete with evidence, or open with a stated
  blocker.
- **Honest confidence.** Where something is unit-tested but not yet end-to-end, it
  is labelled that way rather than rounded up.

The point of writing this down is that it is reproducible. The protocol — not any
single clever commit — is what keeps an AI-assisted codebase from accumulating
plausible-but-wrong code. The detailed engineering protocol lives in `docs/ai/`.

---

## What was deliberately cut

Good scoping is as much about what you *don't* build:

- **No paid cloud dependencies.** A real-time push design that required a paid
  message-bus service was cut, not deferred — adaptive delta polling covers the need
  with zero recurring cost and no public endpoint.
- **No byte-identical mailbox import.** Personal Microsoft 365 does not offer it, so
  restore is an honest high-fidelity *re-create* (new ids, rich metadata) with a
  documented fidelity matrix, rather than a promise the platform cannot keep.
- **No embedded browser engine, no GUI framework.** The status bar is the own
  renderer; full control is the user's own browser pointed at a local web UI.
- **Personal Vault and most "shared with me" data** are documented as upstream
  platform limits and excluded, instead of half-supported.

---

## Evidence and current state

The honest status table is in the [README](../../README.md#current-status). In
short: the engine, all backup connectors, the content archive, search (including
mail bodies), export, multi-account, the CLI, the daemon, the local web UI and the
A1–A10 acceptance + chaos harness are implemented and exercised by tests. The
crash-safe cloud-restore ledger, a deployed staging environment with a full
end-to-end suite, a build-once-promote release pipeline with signed artifacts, and
at-rest encryption are the explicitly tracked, not-yet-done work — and the README
says so plainly.

That mix is the point: a substantial, working product, with the genuinely dangerous
surface treated as a first-class engineering problem rather than glossed over.

---

## Reusable patterns

The patterns here generalise well beyond this product:

- **"Remote call then local record" is never atomic — make the retry correct, not
  the call.** Ledger + idempotency key + reconcile-on-recovery is the same shape as
  payments and message-queue consumers.
- **Track identity, not location.** Anything that can move should be keyed by a
  stable id with location as a derived, recomputable attribute.
- **Own the boundary you need to test.** Owning the render path turned UI testing
  from flaky screenshots into deterministic snapshots.
- **Make failure loud and explained.** Guards on destructive bulk actions, keep-both
  on conflict, and surfacing *why* the system is slow all reduce the chance a user
  misreads a normal failure mode as data loss.
- **AI-assisted only works with a hard verification gate.** The protocol is the
  product-quality lever, not the model.

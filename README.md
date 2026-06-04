# iSyncYou

> *I sync you.* — A personal cloud sync client + Microsoft 365 backup & archive
> (personal/family accounts) for Linux, written in Rust.

[![status](https://img.shields.io/badge/status-private--until--RC-blue)](#current-status)
[![license](https://img.shields.io/badge/license-Apache--2.0-green)](LICENSE)
[![platform](https://img.shields.io/badge/platform-Linux-lightgrey)]()
[![language](https://img.shields.io/badge/built%20with-Rust-orange)]()

iSyncYou keeps a Linux machine in two-way sync with OneDrive **and** keeps a
searchable, restorable on-disk archive of the rest of Microsoft 365 — mail,
calendar, contacts, tasks and notes. It talks to Microsoft Graph directly, tracks
everything by stable item id (never by path), and stores state in SQLite. There is
no embedded browser engine and no GUI framework anywhere — the status bar uses an
own `tiny-skia` + `cosmic-text` renderer, and full control lives in a local web UI
the daemon serves to your normal browser.

**This is a working product, not a prototype.** It is private until its first
release candidate. What follows is honest about what is done, what is being
hardened, and where the hard engineering actually is.

---

## Why this project is interesting

The easy 80% of a sync tool is downloading and uploading files. The hard 20% — the
part this project is built around — is **doing destructive and cloud-mutating work
without ever corrupting or duplicating the user's data when something goes wrong
mid-operation.**

The sharpest example is **cloud restore**. Re-creating an archived mail item in the
cloud is two steps that are not atomic: a Graph `POST` that creates the message,
then local bookkeeping that records *"this item now exists in the cloud."* If the
process is killed, the network drops, or the token expires **between** those two
steps, a naive implementation does the worst possible thing on the next run: it
POSTs again and silently creates a **duplicate** in the user's real mailbox.

There is no transaction that spans "a remote API call" and "a local database
write." So the correctness has to come from the design:

- an **operation ledger** that records intent *before* the Graph call and the
  outcome *after* it, with an explicit state machine
  (`pending → preflight_checked → committed | failed_after_graph_commit`);
- an **idempotency key** derived from the item content (`HMAC-SHA256`) so a retry
  after a crash can recognise *"I already did this"* instead of repeating it;
- **auto-recovery on daemon start** that reconciles any operation left mid-flight;
- and a **crash matrix** of tests that kill the process at each unsafe point and
  assert *no duplicate, no loss.*

Because that surface is dangerous, **cloud restore ships disabled by default**
(`cloud_restore_enabled = false`) and stays disabled until the ledger and its
crash tests are complete. That is the central piece of engineering this repo is
organised to prove — see [ADR-001](docs/adr/001-restore-semantics.md) for the full
restore-safety design.

---

## Current status

Honest snapshot. ✅ means implemented and exercised by tests; 🚧 means in active
hardening; ⏳ means designed and queued, not built.

| Area | State | Notes |
|---|---|---|
| OneDrive two-way sync | ✅ | id-based delta, resumable up/download, `410` reconciliation |
| Path mapping (cloud ↔ local) | ✅ | reversible encode, reserved-name/case-conflict handling, roundtrip-tested |
| Conflict engine | ✅ | keep-both default, `If-Match`/ETag (no silent overwrite) |
| Mass-delete guard | ✅ | both directions, configurable threshold |
| Throttle / 429 pacer | ✅ | full speed → backoff on `429` → probe → full speed; honours `Retry-After` |
| Upload sessions | ✅ | chunked, persisted session state, survives process kill |
| Store (SQLite + FTS5) | ✅ | id-based schema, additive migrations, WAL, single-instance lock |
| M365 backup connectors | ✅ | mail, calendar, contacts, ToDo, OneNote — incremental index |
| Content archive | ✅ | `.eml` / canonical JSON / page HTML / contact photos on disk |
| Full-text search | ✅ | names **and mail bodies**; per-account and cross-account |
| Export | ✅ | `.ics` / vCard from the archive |
| Restore — local & re-create | ✅ | re-create archived items in the cloud as **new copies** via Graph |
| Restore — crash-safe cloud path | ✅ mail · 🚧 other | mail wired through the ledger + daemon boot recovery, crash-matrix-proven, **live-probe confirmed**; **off by default** as an opt-in (it writes to a real mailbox) |
| Multi-account | ✅ | per-account stores, cross-account search |
| CLI + daemon | ✅ | `isyncyou` / `isyncyoud`; scheduled incremental sync |
| Local web UI | ✅ | account/service browsing, search, inert body viewing; no browser engine |
| Native status bar + tray (SNI) | 🚧 | own `tiny-skia` + `cosmic-text` renderer; windowed build is display-gated |
| Dolphin overlay icons | 🚧 | host-side KF6 plugin, packaged separately |
| FUSE on-demand placeholders | ⏳ | designed; privileged/platform-gated |
| PBS snapshot restore path | ⏳ | designed; needs a PBS instance |
| Acceptance harness (A1–A10) + chaos tests | ✅ | data-loss / crash-point matrix |
| Release archive + systemd unit | ✅ | tarball + `systemd --user` service |

The release-engineering work this repo is currently building out — a deployed
staging environment, a full end-to-end suite, a build-once-promote pipeline with
SBOM and signed artifacts, and at-rest encryption for the store and tokens — is
tracked openly and is **not** claimed as done.

---

## What it does

- **OneDrive sync** — bidirectional, id-based delta sync with resumable up/download.
- **M365 backup & archive** — Mail, Calendar, Contacts, ToDo, OneNote: incremental
  index + on-disk bodies (`.eml` / canonical JSON / page HTML / contact photos),
  full-text search **including mail bodies**, and `.ics` / vCard export.
- **Restore** — re-create archived items in the cloud as new copies (mail via MIME,
  calendar / tasks / contacts via Graph), driven from the CLI. The crash-safe path
  is gated behind the operation ledger (above).
- **Local web UI** — the daemon serves a browser UI (account/service browsing,
  search, inert body viewing) on localhost; no embedded browser engine.
- **Multi-account** — per-account stores; back up and search across all accounts.

Personal/family accounts via Microsoft Graph. Stateful, id-based. SQLite + FTS5.
No webkit/GTK anywhere (the native status bar uses an own tiny-skia + cosmic-text
renderer).

## Architecture

```
crates/  graph (OAuth/delta/throttle/upload) · store (SQLite+FTS5) · pathmap ·
         core (config/conflict/guard/recovery/sync-state) · change-source ·
         connectors (onedrive/mail/calendar/contacts/todo/onenote/shared/restore/
         export/mime/archive) · acceptance (A1–A10 harness)
bin/     isyncyou (CLI) · isyncyoud (daemon) · isyncyou-doctor
gui/     statusbar (own renderer) · webui (router + minimal HTTP server)
```

## Install

Grab `isyncyou-linux-x86_64.tar.gz` from a release (or `cargo build --release`),
then:

```sh
sudo install -m755 isyncyou isyncyoud isyncyou-doctor /usr/local/bin/
isyncyou init --account me --username me@outlook.com \
  --sync-root ~/OneDrive --archive-root ~/iSyncYou
isyncyou check
```

Run the daemon (serving the web UI) as a `systemd --user` service — see
[`packaging/isyncyoud.service`](packaging/isyncyoud.service).

## CLI

```
isyncyou init      # scaffold a config (template or a validated account)
isyncyou check     # validate the config
isyncyou login     # device-code sign-in; caches the token (--write for restore)
isyncyou status    # per-service item + archived-body counts
isyncyou sync      # one incremental OneDrive sync
isyncyou backup    # index + archive M365 services (--all-accounts, --service, --body-limit)
isyncyou search    # full-text search names + mail bodies (--all-accounts)
isyncyou restore   # re-create an archived item in the cloud
isyncyou export    # export archived events/contacts to .ics / .vcf
isyncyou migrate   # move an account's archive directory
isyncyou serve     # serve the local web UI
```

Token resolution: `--token` / `ISYNCYOU_TOKEN` wins; otherwise the per-account
token cached by `login` is loaded and auto-refreshed.

## Build & test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

Live tests against a Microsoft account are env-gated (`ISYNCYOU_TEST_TOKEN` /
`ISYNCYOU_TEST_WRITE_TOKEN`); CI without credentials skips them. The test account
is a dedicated throwaway mailbox, strictly separated from any real account, with a
unique item prefix and teardown.

## Known limitations

This section is deliberately blunt — it is the inverse of the status table.

- **Cloud restore is disabled by default.** It re-creates items as *new copies*
  (new ids; Microsoft 365 personal accounts offer no byte-identical import). The
  crash-safe operation ledger that makes retries idempotent is still being built;
  until it and its crash matrix are green, `cloud_restore_enabled` stays `false`.
- **Data at rest is currently unencrypted.** The SQLite store and cached tokens
  live in plaintext on disk behind file permissions. An at-rest encryption layer
  is designed (a pluggable storage backend) but not yet shipped. Do not point this
  at sensitive data on a shared machine yet.
- **No deployed staging / full E2E suite yet.** There is an acceptance + chaos
  harness, but the end-to-end pipeline against a live environment is being stood
  up; release artifacts are not yet reproducibly signed.
- **The windowed GUI, tray, Dolphin overlays, FUSE placeholders and the PBS
  restore path are platform-gated.** They need a display server, a host-side KF6
  plugin, privileged mounts, or a PBS instance respectively. The headless engine,
  CLI and web UI do not depend on any of them.
- **Personal Vault and some "shared with me" data are not reachable** via Graph for
  third-party clients — these are upstream platform limits, not bugs.

## Engineering approach

This codebase is built with **AI-assisted engineering under a verification-first
protocol**: every change is gated on `fmt`, `clippy -D warnings`, the test suite,
and `cargo deny` before it can land, and no behaviour is recorded as "done" without
an executed command and its output as evidence. The interesting design decisions
(crash-safe restore semantics, the own renderer used as its own headless test
harness, the id-based reconciliation model) are written up as architecture decision
records, not buried in commits. The protocol itself is in
[`docs/ai/AI_ASSISTED_ENGINEERING_PROTOCOL.md`](docs/ai/AI_ASSISTED_ENGINEERING_PROTOCOL.md);
the restore-safety design is [ADR-001](docs/adr/001-restore-semantics.md).

## Docs

Design notes and matrices live in [`docs/`](docs/) — Graph capability matrix,
restore-fidelity matrix, sync-state machine, path mapping, delete/trash/conflict
model, auth/token lifecycle, local-API security, packaging/daemon model.

## License

[Apache-2.0](LICENSE). Private until the first release candidate.

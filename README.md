# iSyncYou

> *I sync you.* — A personal cloud sync client **and** Microsoft 365 backup &
> archive (personal/family accounts) for Linux, written in Rust.

[![status](https://img.shields.io/badge/status-release--candidate-blue)](#current-status)
[![release](https://img.shields.io/github/v/release/silentspike/isyncyou?include_prereleases&sort=semver)](https://github.com/silentspike/isyncyou/releases)
[![coverage](https://github.com/silentspike/isyncyou/actions/workflows/coverage.yml/badge.svg?branch=main)](https://github.com/silentspike/isyncyou/actions/workflows/coverage.yml)
[![scorecard](https://api.securityscorecards.dev/projects/github.com/silentspike/isyncyou/badge)](https://securityscorecards.dev/viewer/?uri=github.com/silentspike/isyncyou)
[![license](https://img.shields.io/badge/license-Apache--2.0-green)](LICENSE)
[![platform](https://img.shields.io/badge/platform-Linux-lightgrey)]()
[![language](https://img.shields.io/badge/built%20with-Rust-orange)]()

iSyncYou keeps a Linux machine in two-way sync with **OneDrive** and keeps a
searchable, restorable on-disk **archive of the rest of Microsoft 365** — mail,
calendar, contacts, tasks and notes. It talks to Microsoft Graph directly, tracks
everything by stable item id (never by path), and stores its state in SQLite.

There is **no embedded browser engine and no GUI framework anywhere**: the native
status bar uses an own `tiny-skia` + `cosmic-text` renderer, and full control lives
in a small **local web UI** the daemon serves to your *own* browser.

This is a working product at the **release-candidate** stage — see the
[releases](https://github.com/silentspike/isyncyou/releases). What follows is
honest about what is done, what is still being hardened, and where the hard
engineering actually is.

---

## Screenshots

**Native status bar** — a tiny tray app rendered by an own engine (no webkit, no
GTK). It shows live transfers and is deliberately honest about throttling: when
Microsoft returns `429`, it tells you *it's Microsoft, not your line.*

<p align="center">
  <img src="docs/assets/statusbar-syncing.png" width="300" alt="Native status bar while syncing: OneDrive header, a 'Syncing…' pill, two live transfer bars (IMG_2024.jpg 71% down, invoice.pdf 88% up), aggregate ↓12.2 / ↑3.2 MB/s and a 14-item queue, with Pause and 'Open in browser' buttons.">
  &nbsp;&nbsp;&nbsp;
  <img src="docs/assets/statusbar-throttled.png" width="300" alt="Native status bar while throttled: an amber 'Throttled 14s' pill and a banner reading 'Throttled by Microsoft (429) — not your connection'.">
</p>

**Local web UI** — the daemon serves a dark, JS-light archive browser to
`localhost`. Browse per account and per service, full-text search, view inert
bodies, and run restores. No browser engine is embedded — it's just your browser.

<p align="center">
  <img src="docs/assets/webui-overview.png" width="840" alt="Web UI archive overview: per-service item cards (OneDrive, Mail, Calendar, Contacts, ToDo, OneNote), totals (24 items, archived bodies, OneDrive cursor) and a settings panel (sync root, archive root, trash retention, FTS body index, change source, delete guard).">
</p>

<p align="center">
  <img src="docs/assets/webui-mail.png" width="840" alt="Web UI mail browser: a search box and a table of archived messages (Type, Name, Modified, Body, Restore) listing subjects like 'Invoice #2041', 'Q3 roadmap review' and 'Team offsite — agenda'.">
</p>

> The screenshots above are rendered against **synthetic sample data** — no real
> account is involved.

---

## What it does

- **OneDrive two-way sync** — bidirectional, id-based delta sync with resumable
  up/download, reversible path mapping, a keep-both conflict engine, and a
  mass-delete guard in both directions.
- **Microsoft 365 backup & archive** — Mail, Calendar, Contacts, ToDo and OneNote:
  incremental index plus on-disk bodies (`.eml` / canonical JSON / page HTML +
  resource manifests / contact photos), **full-text search including mail bodies**,
  and `.ics` / vCard export.
- **Restore** — recover archived bodies locally, or re-create archived items as new
  cloud copies. All five backup services go through a **crash-safe operation
  ledger**; a service with no crash-safe path is *refused* rather than run unsafely.
- **Local web UI** — the daemon serves a browser UI (account/service browsing,
  search, inert body viewing, restore) on localhost. No embedded browser engine.
- **Native status bar + tray** — sync status, live transfers, throttle/`429`
  transparency, pause/resume, "open in browser" — rendered by an own engine.
- **Multi-account** — per-account stores; back up and search across all accounts.

Personal/family accounts via Microsoft Graph. Stateful and id-based. SQLite + FTS5.
No webkit/GTK anywhere.

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
(`cloud_restore_enabled = false`). All five backup services (mail, calendar,
contacts, ToDo, OneNote) are ledger-backed and crash-matrix proven, each with a
live-probe-confirmed crash-recovery marker. That is the central piece of
engineering this repo is organised to prove — see
[ADR-001](docs/adr/001-restore-semantics.md) for the full restore-safety design.

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
| Content archive | ✅ | `.eml` / canonical JSON / page HTML + OneNote resources / contact photos on disk |
| Full-text search | ✅ | names **and mail bodies**; per-account and cross-account |
| Export | ✅ | `.ics` / vCard from the archive |
| Restore — local & connector re-create | ✅ | local restore for archived bodies; connector-level re-create for mail/calendar/tasks/contacts/OneNote |
| Restore — crash-safe cloud path | ✅ all services | mail, calendar, contacts, ToDo and OneNote wired through the ledger + daemon boot recovery, crash-matrix-proven, **live-probe confirmed** (per-service recovery markers: internetMessageId · transactionId de-dup · extended property · body marker · HTML-comment); **off by default** as an opt-in (it writes to the real account) |
| Multi-account | ✅ | per-account stores, cross-account search |
| CLI + daemon | ✅ | `isyncyou` / `isyncyoud`; scheduled incremental sync |
| Local web UI | ✅ | account/service browsing, search, inert body viewing; no browser engine |
| Native status bar + tray (SNI) | 🚧 | own `tiny-skia` + `cosmic-text` renderer; windowed build is display-gated |
| Dolphin overlay icons | 🚧 | host-side KF6 plugin, packaged separately |
| FUSE on-demand placeholders | ⏳ | designed; privileged/platform-gated |
| PBS snapshot / temp restore path | ✅ local + live PBS | `VACUUM INTO` staged store + manifest, PBS backup/list/restore CLI; live temp-store round-trip confirmed against a real PBS repository |
| Acceptance harness (A1–A10) + chaos tests | ✅ | data-loss / crash-point matrix |
| Release archive + systemd unit | ✅ | tarball + `systemd --user` service |

A deployed staging environment and a full end-to-end suite are still **not** claimed
as done — that release-engineering work is tracked openly in the issues. Release
artifacts are built by CI with a CycloneDX SBOM and signed GitHub artifact
attestations.

---

## Install

Grab `isyncyou-linux-x86_64.tar.gz` from a
[release](https://github.com/silentspike/isyncyou/releases) (or build it yourself —
see [Build & test](#build--test)), then:

```sh
sudo install -m755 isyncyou isyncyoud isyncyou-doctor /usr/local/bin/

isyncyou init --account me --username me@outlook.com \
  --sync-root ~/OneDrive --archive-root ~/iSyncYou
isyncyou check
isyncyou login            # device-code sign-in (add --write later for restore)
```

Run the daemon (which also serves the web UI) as a `systemd --user` service — see
[`packaging/isyncyoud.service`](packaging/isyncyoud.service).

## Usage

A first sync + archive, then open the browser UI:

```sh
isyncyou sync                 # one incremental OneDrive sync
isyncyou backup               # index + archive all M365 services
isyncyou status               # per-service item + archived-body counts
isyncyou search "invoice"     # full-text search across names + mail bodies

isyncyou serve --tcp          # serve the web UI on http://127.0.0.1:8765 (loopback)
```

Then open `http://127.0.0.1:8765` in your browser for the archive UI shown above.
By default `serve` listens on an owner-only Unix socket; `--tcp` opts into a
loopback TCP transport.

### CLI reference

```
isyncyou init      # scaffold a config (template or a validated account)
isyncyou check     # validate the config
isyncyou login     # device-code sign-in; caches the token (--write for restore, --keyring for desktop keyring)
isyncyou status    # per-service item + archived-body counts
isyncyou sync      # one incremental OneDrive sync
isyncyou backup    # index + archive M365 services (--all-accounts, --service, --body-limit)
isyncyou search    # full-text search names + mail bodies (--all-accounts)
isyncyou restore   # re-create an archived item in the cloud (opt-in)
isyncyou export    # export archived events/contacts to .ics / .vcf
isyncyou migrate   # move an account's archive directory
isyncyou serve     # serve the local API/web UI (Unix socket by default; --tcp for loopback)
```

Token resolution: `--token` / `ISYNCYOU_TOKEN` wins; otherwise the per-account
token cached by `login` is loaded and auto-refreshed.

## Architecture

```
crates/  graph (OAuth/delta/throttle/upload) · store (SQLite+FTS5) · pathmap ·
         core (config/conflict/guard/recovery/sync-state) · change-source ·
         connectors (onedrive/mail/calendar/contacts/todo/onenote/shared/restore/
         export/mime/archive) · acceptance (A1–A10 harness)
bin/     isyncyou (CLI) · isyncyoud (daemon) · isyncyou-doctor
gui/     statusbar (own tiny-skia + cosmic-text renderer) · webui (router + minimal HTTP server)
```

The **same** status-bar renderer draws on screen *and* renders headless to a PNG,
so the UI is its own visual test harness — no display server, no browser
automation. The screenshots in this README's status-bar section are produced by
exactly that path.

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

- **Cloud restore is off by default.** It re-creates items as *new copies* (new
  ids; Microsoft 365 personal accounts offer no byte-identical import). All five
  backup services go through the crash-safe operation ledger (complete +
  live-confirmed, each with its own recovery marker); a service with no crash-safe
  path is **refused** rather than run unsafely. `cloud_restore_enabled` is `false`
  by default — a deliberate opt-in, since it writes to the real account.
- **Data at rest is only partially protected.** `isyncyou login --keyring` stores
  token JSON in the desktop Secret Service / KDE Wallet compatible keyring and
  leaves only a non-secret marker file in the archive root. Headless/file caches are
  owner-only on Unix (`0600`) and can be AES-256-GCM encrypted when a token-cache
  secret is configured (`ISYNCYOU_TOKEN_CACHE_KEY_FILE`, systemd credential
  `isyncyou-token-cache-key`, or `ISYNCYOU_TOKEN_CACHE_KEY`). Without a keyring or
  that secret, the token cache is **still encrypted at rest** with an auto-generated,
  owner-only local key kept beside it (never plaintext); that local key protects the
  cache file if it is copied/synced on its own, not against full config-dir read
  access. The SQLite store can be
  SQLCipher-encrypted via `ISYNCYOU_STORE_KEY_FILE`, systemd credential
  `isyncyou-store-key`, or `ISYNCYOU_STORE_KEY`; an existing plaintext store
  migrates in place with `isyncyou migrate --account <id> --encrypt-store`
  (atomic; refuses without a configured key). Without a store key, stores remain
  plaintext and `isyncyou-doctor` warns. Do not point plaintext stores at
  sensitive data on a shared machine.
- **No deployed staging / full E2E suite yet.** There is an acceptance + chaos
  harness, but the end-to-end pipeline against a live environment is being stood up.
  Release artifacts are built by CI with a CycloneDX SBOM and signed GitHub artifact
  attestations, but staging/live-E2E evidence is still separate.
- **The windowed GUI, tray, Dolphin overlays and FUSE placeholders are
  platform/environment-gated.** They need a display server, a host-side KF6 plugin,
  or privileged mounts respectively. The PBS path has deterministic local coverage
  plus a live test-account round-trip, but rerunning that live probe still requires a
  configured PBS repository.
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
model, auth/token lifecycle, SQLite/PBS snapshot consistency, local-API security,
packaging/daemon model.

## Contributing

Issues and PRs are welcome. PRs are gated on `fmt`, `clippy -D warnings`, the test
suite and `cargo deny`; see [CONTRIBUTING.md](CONTRIBUTING.md) for the workflow and
[SECURITY.md](SECURITY.md) for how to report vulnerabilities.

## License

[Apache-2.0](LICENSE).

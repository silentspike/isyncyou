# iSyncYou

> *I sync you.* — Personal cloud sync client + Microsoft 365 backup & archive
> (personal/family) for Linux, written in Rust.

**Private until RC.** Apache-2.0.

## What it does

- **OneDrive sync** — bidirectional, id-based delta sync with resumable up/download.
- **M365 backup & archive** — Mail, Calendar, Contacts, ToDo, OneNote: incremental
  index + on-disk bodies (`.eml` / canonical JSON / page HTML / contact photos),
  full-text search **including mail bodies**, and `.ics` / vCard export.
- **Restore** — re-create archived items in the cloud (mail via MIME, calendar /
  tasks / contacts via Graph), driven from the CLI.
- **Local web UI** — the daemon serves a browser UI (account/service browsing,
  search, inert body viewing) on localhost; no embedded browser engine.
- **Multi-account** — per-account stores; back up and search across all accounts.

Personal/family accounts via Microsoft Graph. Stateful, id-based. SQLite + FTS5.
No webkit/GTK anywhere (the native status bar uses an own tiny-skia + cosmic-text
renderer).

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

## Architecture

```
crates/  graph (OAuth/delta/throttle/upload) · store (SQLite+FTS5) · pathmap ·
         core (config/conflict/guard/recovery/sync-state) · change-source ·
         connectors (onedrive/mail/calendar/contacts/todo/onenote/shared/restore/
         export/mime/archive) · acceptance (A1–A10 harness)
bin/     isyncyou (CLI) · isyncyoud (daemon) · isyncyou-doctor
gui/     statusbar (own renderer) · webui (router + minimal HTTP server)
```

## Docs

Design notes and matrices live in [`docs/`](docs/) — Graph capability matrix,
restore-fidelity matrix, sync-state machine, path mapping, delete/trash/conflict
model, auth/token lifecycle, local-API security, packaging/daemon model.

## Build & test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

Live tests against a Microsoft account are env-gated (`ISYNCYOU_TEST_TOKEN` /
`ISYNCYOU_TEST_WRITE_TOKEN`); CI without credentials skips them.

## Status

Working: the engine, all backup connectors, content archive, restore, search
(incl. mail bodies), export, multi-account, the CLI, the daemon + web UI, the
A1–A10 acceptance harness, and a release archive + systemd unit. Pending external
prerequisites: the native windowed GUI / tray (a display server), Flatpak/AppImage
GUI bundling (build tooling), the PBS restore path (a PBS instance), and the
realtime / eBPF / FUSE / cross-platform features (privileged / platform-specific).

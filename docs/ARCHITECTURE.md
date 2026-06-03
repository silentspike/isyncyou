# iSyncYou — Architecture

> Status: draft. Source of truth for the design is the internal SDD; this is the public-facing summary.

## Modes

| Mode | Services | Local representation | UX |
|------|----------|----------------------|----|
| **Sync client** (bidirectional) | OneDrive | files 1:1, freely chosen sync folder | tray, live transfers, Dolphin overlays |
| **Backup / archive** (download + selective restore) | Mail, Calendar, Contacts, ToDo, OneNote | .eml/.ics/vCard/HTML + canonical Graph JSON | browser, read viewers, search, job-based restore |

## Components

```
Engine daemon (isyncyoud)
  graph (auth/delta/throttle/upload) · connectors · job queue
  store (SQLite + FTS5, id-based) · change-source (inotify/eBPF)
  pathmap · conflict-engine · pbs · doctor · self-check
  ──► local API: Unix socket (desktop) | HTTP + TLS + token (remote)
        ├─ status-bar (own renderer: tiny-skia + cosmic-text)
        ├─ web UI (full control in the browser)
        ├─ isyncyou (CLI)
        └─ isyncyou-doctor (standalone recovery)
```

## Key principles

- **Id-based, stateful delta.** Track items by stable id, never by path. For Outlook resources (mail/calendar/contacts) the connectors send `Prefer: IdType="ImmutableId"` so the stored id is the immutable id (stable across folder moves), and keep its companions (`changeKey`, `internetMessageId`, `iCalUId`). Delta cursors are opaque and persisted per account+service.
- **No artificial throttle.** Full speed until Graph returns `429`, then honor `Retry-After`, probe, and resume at full speed.
- **Correctness layer = delta.** Change notifications are hints; a periodic reconciler is the source of truth.
- **Native status bar, browser for full control.** The own renderer drives the small status bar (headless-verifiable, pixel-accurate). Restore, mail viewing, search and settings run in a local web UI opened in the user's browser — no embedded browser engine.
- **Permissive licenses only.** Techniques from GPL tools are re-implemented, never copied; enforced by a `cargo-deny` license gate.

## Workspace

`crates/`: `ipc-types`, `graph`, `store`, `pathmap`, `core`, `change-source`, `connectors`, `api`, `pbs`, `doctor-lib`
`bin/`: `isyncyoud`, `isyncyou`, `isyncyou-doctor`
`gui/`: `statusbar` (native), `webui` (browser assets)

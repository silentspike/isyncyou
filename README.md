# iSyncYou

> *I sync you.* — Personal cloud sync client + Microsoft 365 backup & archive (personal/family) for Linux, written in Rust.

**Status:** Pre-implementation (SDD complete). **Private until RC.**

## What it is

- **Sync client (bidirectional):** OneDrive 1:1, Linux desktop (KDE-first), tray + mini status bar.
- **Backup / archive:** Mail, Calendar, Contacts, ToDo, OneNote — searchable, with high-fidelity restore.
- **Full control in the browser** (local web UI served by the daemon); native status bar via our own renderer.

## Architecture (short)

- `daemon` (graph / sync / store) · `cli` · mini status bar (own renderer) · web UI (browser).
- Personal/family accounts via Microsoft Graph. Stateful, id-based. SQLite + FTS5.

## License

Apache-2.0.

# iSyncYou

> *I sync you.* — Persönlicher Cloud-Sync-Client + Microsoft-365-Backup/Archiv (Personal/Family) für Linux, in Rust.

**Status:** Pre-Implementation (SDD steht). **Privat bis RC.**

## Was

- **Sync-Client (bidirektional):** OneDrive 1:1, Linux-Desktop (KDE-first), Tray + Mini-Statusbar.
- **Backup/Archiv:** Mail, Kalender, Kontakte, ToDo, OneNote — durchsuchbar, mit High-Fidelity-Restore.
- **Vollsteuerung im Browser** (lokale Web-UI vom Daemon); native Statusbar via eigenem Renderer.

## Architektur (Kurz)

- `m365d`-Daemon (graph/sync/store) · `m365ctl` (CLI) · Mini-Statusbar (eigener Renderer) · Web-UI (Browser).
- Personal/Family-Konten via Microsoft Graph. Stateful, id-basiert. SQLite + FTS5.

## Lizenz

Apache-2.0.

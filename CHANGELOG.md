# Changelog

All notable changes to this project are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

**Engine (Phase 1)**
- Microsoft Graph core: device-code OAuth + token cache + non-interactive refresh,
  delta walker (pagination, 410→resync), adaptive throttle/429 pacer, resumable
  upload sessions, reqwest+rustls transport (behind the `http` feature).
- id-based SQLite store with FTS5 (names + mail bodies), WAL, single-instance lock,
  migrations; pathmap (reversible cloud↔local codec + persistent mapping table);
  core (config, conflict engine, mass-delete guard, recovery/journal, sync-state
  machine); change-source (inotify coalescer + reconciler).
- OneDrive bidirectional connector (delta→store, resumable up/download, tombstones).
- Native status-bar renderer (own tiny-skia + cosmic-text, bundled font, headless).
- A1–A10 acceptance harness (`crates/acceptance`).

**Backup, restore & search (Phase 2)**
- Connectors for Mail, Calendar, Contacts, ToDo, OneNote: incremental index +
  on-disk body archive (`.eml` / canonical JSON / page HTML / contact photos).
- Crash-safe **cloud restore for all backup services** (mail, calendar, contacts,
  ToDo, OneNote) through the ADR-001 operation ledger — each with a live-confirmed,
  per-service recovery marker (internetMessageId / transactionId de-dup / extended
  property / body marker / HTML-comment); off by default. Plus restore-to-local and a
  PBS snapshot restore path.
- Full-text search over names **and mail bodies**; `.ics` / vCard export.
- Multi-account (per-account stores, `--all-accounts` backup + cross-account search);
  archive migration.
- Local web UI: router + minimal HTTP server (accounts / items / item / body /
  search), served by the daemon; safe inert body serving.

**Tooling & ops**
- `isyncyou` CLI (init/check/login/status/sync/backup/search/restore/export/migrate/serve);
  `isyncyoud` daemon (serves the web UI); `isyncyou-doctor`.
- Release archive (`isyncyou-linux-x86_64.tar.gz`) + hardened `systemd --user` unit.
- CI (fmt, clippy, build, test) on **GitHub-hosted runners** (public-ready — no
  self-hosted exposure), with a paths-filter so docs-only PRs skip the compile gate;
  HEAD-pinned evidence-manifest generator; secret scanning (Gitleaks), license/advisory
  gate (cargo-deny); Epic/Story/Task issue model + auto-labeling.
- Autonomous release promotion: a merge to `dev` cascades dev→staging→main and
  publishes an RC with no manual steps — `promote.yml` opens and auto-merges a
  tree-overlay PR at each stage (PAT-driven, so each stage's required CI runs and
  the merge re-triggers the next hop).
- CodeQL (Rust SAST) is now an **enforced** gate on `main` (`continue-on-error`
  removed; required status check) — a real finding fails the build (#348).
- Release artifacts are **cosign-signed** (keyless / Sigstore, ambient OIDC, no
  stored key): each binary, the SBOM and `SHA256SUMS` ship a `.cosign.bundle`
  verifiable with `cosign verify-blob` (#349).
- cargo-deny now installs via the SHA-pinned `cargo-deny-action` in **all** gates
  (was an unverified `curl | tar` on staging/main) — removes a CI supply-chain
  gap (#358).
- The OAuth **token cache is now encrypted at rest by default**: with no keyring and
  no explicit `ISYNCYOU_TOKEN_CACHE_KEY*` secret, it is AES-256-GCM encrypted with an
  auto-generated, owner-only local key instead of being written in plaintext (legacy
  plaintext caches still load). Risk R2 narrowed to the SQLite store.
- `docs/`: Graph capability + restore-fidelity matrices, sync-state machine, path
  mapping, delete/trash/conflict model, auth/token lifecycle, local-API security,
  packaging/daemon model.

### Not yet implemented
- eBPF change-source backend (the fanotify backend already covers the privileged
  server case; eBPF would be a further optimization).

### Out of scope (by design)
- Azure Event Hub realtime push: would require a paid Azure subscription, and
  iSyncYou takes no paid cloud dependency. Adaptive delta-polling (implemented) is
  the change-detection mechanism; Graph notifications are only hints anyway, so a
  delta pull always follows.
- macOS build: requires Apple hardware or paid cloud-Mac CI minutes (Apple's EULA
  restricts macOS virtualization to Apple-branded hardware, so it cannot be hosted
  on x86 Linux build hosts). The code is kept mac-ready — the Linux-only bits
  (FUSE mount, DBus/KIO) are `cfg(target_os = "linux")`-gated, so the CLI/daemon
  build for macOS once a Mac build host is available. Linux + Windows ship today.

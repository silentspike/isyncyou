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
- **In-place store encryption migration** (#328): `isyncyou migrate --account <id>
  --encrypt-store` converts an existing plaintext store to SQLCipher — atomic
  (temp + rename + fsync; a crash mid-migration leaves the plaintext store fully
  usable and the next run resumes), preserves all rows and rebuilds the FTS
  indexes, refuses without a configured store key, and is a detectable no-op on an
  already-encrypted store. A legacy plaintext **token cache** likewise migrates to
  the encrypted format on its next save. Risk **R2 → mitigated**.
- **Code-coverage gate**: a `coverage.yml` workflow measures workspace line coverage
  with `cargo-llvm-cov` and fails under 70% (currently ~77%), with a README badge so
  the test substance is visible and cannot silently rot.
- **Deterministic transport tests** (#413): the Graph HTTP transport is now
  exercised against a local mock server (std-only) — 429/`Retry-After`, network
  failure → retryable 503, 4xx/5xx classification, malformed JSON, resumable-upload
  resume/completion/failure, `If-Match`/412 conflict, OneNote multipart, deletes.
  `graph/http.rs` line coverage 23% → ~87%; workspace ~80%; the coverage floor is
  raised 70% → 75%. `GraphClient::with_base_url` makes the API base injectable
  (tests + sovereign-cloud endpoints).
- CI/CD hardening: a **promote watchdog** alerts on a stalled autonomous promotion
  instead of failing silently (#359); `release.yml` **self-verifies its own cosign
  signatures** before publishing and **smoke-tests the Linux binary** (#361, #362);
  the Rust toolchain action is unified on the pinned `@stable` SHA and dependabot is
  told not to bump it (the `@master` revision broke the pipeline once) (#362).
- Supply-chain hardening (#360): **OpenSSF Scorecard** workflow + README badge,
  **`dependency-review`** gate on PRs into dev (fails on high-severity advisories),
  and **`step-security/harden-runner`** (egress audit) on the release job.
- **Honest MSRV** (#408): `rust-version` raised 1.90 → **1.95** (the real minimum —
  libsqlite3-sys 0.38 from rusqlite 0.40 needs `cfg_select`, stabilized in 1.95;
  verified empirically), plus an `msrv` CI gate that builds on the declared MSRV so
  a dependency bump can never silently raise it again.
- **build-once-promote** (#329): the staging/main gates now skip the heavy
  build/test/docs jobs when the promotion's tree is byte-identical to `origin/dev`
  (deterministic re-runs already gated on dev) — the same commit is no longer
  recompiled at every stage; cheap checks (fmt, cargo-deny, CodeQL) still run, and a
  diverged tree builds fully.
- `docs/`: Graph capability + restore-fidelity matrices, sync-state machine, path
  mapping, delete/trash/conflict model, auth/token lifecycle, local-API security,
  packaging/daemon model.

- **Deployed staging + nightly E2E** (#326): a self-hosted staging instance runs
  `isyncyoud` (hardened systemd service; SQLCipher store, encrypted token caches)
  and a nightly E2E against the dedicated throwaway account — backup (all five
  services), OneDrive sync, search, restore-to-local, verify — with pass/fail
  pushed to a notification channel. Its first run caught a real `verify` bug.

- **Status-bar live snapshot** (`isyncyou-statusbar --snapshot out.png [--api host:port]`):
  fetches the first account + the real scheduled-sync state from the daemon's local
  API and renders it **headlessly** through the same engine that draws the window
  (verified pixels = screen pixels) — used by the staging E2E to verify the native
  UI against live daemon data; errors out instead of inventing data when the daemon
  is unreachable. The binary now ships in the release tarball.

- `isyncyou rm --service mail --id <id>`: delete a single cloud item, behind the
  same `restore.cloud_restore_enabled` gate as cloud restore and requiring a write
  token (deletion is at least as destructive as a re-create). Mail only for now;
  used to tear down a test restore on a throwaway account in the staging E2E.
- `isyncyou sync` now prefers the cached **write** token when one exists, falling
  back to the read token (download-only) otherwise — bidirectional sync uploads
  and deletes, which need write scopes; previously the CLI always used the read
  token, so a sync that had local changes to push would fail. Matches the daemon.

### Fixed
- `isyncyou rm --service mail` returned **HTTP 404**: Outlook message ids are
  base64-ish (`+ / =`) and were not percent-encoded in the `DELETE /me/messages/{id}`
  path, so Graph could not find them. Item ids are now URL-encoded
  (`GraphClient::delete_message` + `delete_item`). Found by the staging E2E's
  cloud-restore-with-teardown journey.
- `isyncyou rm` now also supports `--service onedrive` (deletes a drive item),
  so the staging E2E can tear down an uploaded file — a one-shot `sync` cannot
  turn a local delete into a remote delete (downloads are materialized before
  local-delete detection; that path needs the `watch`/inotify tombstone).
- `isyncyou verify` misread synced **OneDrive** items as archive bodies (their
  `local_path` is a name segment under `sync_root`, resolved through parents) and
  flagged every synced file as a missing body — a synced tree could never pass
  verify. Found by the deployed staging environment's first nightly E2E run (#326);
  OneDrive items are now checked against the sync root via the parent walk.

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

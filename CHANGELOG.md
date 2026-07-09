# Changelog

All notable changes to this project are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

**Agent API and confirmation backend**
- `agent`+`app-host`+`webui`: completed #621's AgentStreamHub/PendingAction
  backend with typed per-turn events (`token`, `tool_call`, `tool_result`,
  `confirmation_required`, `error`, `done.reason`), bounded backpressure,
  cancel semantics, `/api/v1/agent/{turn,chat,confirm,cancel,status,stream}`,
  action-hash-bound one-time confirmation tokens, exactly-once confirmed-action
  executor and durable audit seams, mobile session gating for POST and SSE paths,
  unopened-stream cleanup, FakeProvider end-to-end coverage, and explicit
  no-token-leak tests for model history, public token events, errors, and audit.
  Real destructive operations still land in #624 behind the new executor seam.

**Agent credential storage**
- `agent`+`app-host`+`mobile`+`android`: added #620 typed encrypted
  CredentialStore coverage for provider API keys, provider OAuth refresh tokens, and
  session pairing keys, with canonical envelope metadata binding, owner-only Unix
  writes, app-host resolver usage, provider credential seams, and a separate Android
  Keystore-wrapped agent credential key proven by a JNI-only Pixel 8 Pro self-test.

**Agent encrypted sessions**
- `agent`+`graph`+`mobile`: hardened #619 cross-device OneDrive session storage with
  Argon2id/HKDF pairing-key derivation, immutable per-turn ULID files, ETag-aware
  active-turn leases, durable sealed offline pending cache, explicit fork metadata,
  a JNI-only Pixel KDF benchmark hook, and live OneDrive ciphertext/lease/cleanup
  evidence. #620 still owns Android Keystore persistence.

**OneDrive on Mobile (Phase 1 — modes foundation)**
- `graph`: `GraphClient::list_children` — live, fully paged (`@odata.nextLink` to
  completion) folder listing over the central retry policy; no store write (#647).
- `engine`: `OneDriveLister` trait + `onedrive_lister` constructor surfacing the
  live listing for the daemon (read-capable, mobile-friendly token) (#647).
- `core`: OneDrive per-folder mode policy — an account-scoped `onedrive_modes` map (a
  `default_mode` plus per-folder `folder_modes`) in the config, with a pure `effective_mode`
  resolver (the deepest explicit ancestor wins, else the account default) and a
  tombstoned-entry cleanup helper. Distinct axis from the per-item `content_state` (#650).
- `store`+`connectors`: per-folder scoped OneDrive delta + scope ownership — `clear_delta_cursor`,
  a pure `owning_scope` rule (deepest active scope wins, one owner per item, id-stable moves),
  and `incremental_sync_scoped` with per-page `@odata.nextLink` cursor resume (crash-safe) and a
  subtree-aware tombstone rule that does not leak nested deletes (#653).
- `webui`+`app-host`: session-gated `GET /api/v1/onedrive/children?account&folder` — an
  `OneDriveListHandler` (Router builder + Fake) returning a folder's live children as JSON,
  wired via `DaemonOneDriveList` into the shared desktop+mobile router; absent handler → 404 (#648).
- `engine`+`webui`: OneDrive cloud-write endpoints — `POST /api/v1/onedrive/{create,rename,move,delete}`
  over the crash-safe operation ledger. An idempotent intent is recorded **before** the Graph call, then
  advanced `inflight`→`applied`; per-kind crash recovery probes so a folder create is never duplicated and
  a delete/rename/move re-issues safely (no double effect). `delete` is biometric-gated on mobile; the
  handler is wired into the shared live router for both desktop and mobile (#654).
  - Note: `move-out-of-protected` biometric-gating is **deferred to #655/#656** — the offline-scope work
    owns the "protected" semantics; the `delete` gate covers the destructive case today.
- `webui`+`app-host`+`app.js`: Mode-1 online OneDrive browsing on mobile — `driveLoad` renders the live
  `/api/v1/onedrive/children` tree (folders/files, drill-down, breadcrumb up-nav) instead of the empty store,
  and tapping a file opens it on-demand via a new session-gated `GET /api/v1/onedrive/open?account&id&name`
  (`OneDriveOpenHandler` + `DaemonOneDriveOpen`, live `download_content`, served inertly, no store write) (#649).
- `webui`+`app-host`: session-gated OneDrive per-folder **mode** API — `GET /api/v1/onedrive/mode?account`
  returns the account's mode map (`default_mode` + `folder_modes`); `POST …&folder&mode=online|sync|offline`
  sets, or an empty `mode` clears, a folder override (cap-token-gated + audited `audit:onedrive-mode`),
  persisted via a new `OneDriveModeHandler`/`DaemonOneDriveMode` (reload-on-read, so the GET reflects a POST)
  wired into the shared desktop+mobile router. `GET /api/v1/onedrive/children` now carries a per-item
  `effective_mode`, resolved against an optional deepest-first `&ancestry=` (folder-level fallback without it).
  Rust-only; the app.js toggle UI + breadcrumb `ancestry` send land in #652/#656 (#651).
- `connectors`+`engine`+`app-host`+`mobile`+`android`: Mode-3 **offline** OneDrive on the phone — an
  `offline_sync_once` pass (run from the mobile refresh loop) that materializes the configured offline
  folders to the editable `sync_root` (scoped delta + `materialize_downloads_scoped`, marking the v14
  content-state so the body endpoint serves them), then mirrors local creates / modifies / deletes back to
  the cloud **over the operation ledger**. The ledger now covers `CloudOpKind::Upload`/`Replace` on a
  unified parent-id sink (`graph::upload_to_parent` with `conflictBehavior=fail`), with probe-adopt (Upload)
  and etag-guarded (Replace, keep-both on a 412) crash recovery. Each new download is policy-gated
  (`core::policy::evaluate` — storage floor / Wi-Fi-only / charging-only, fed from Android via a
  `nativeDeviceState` JNI) and each destructive batch is guarded by the `core::guard` mass-delete guard.
  Per-file progress flows through a `SharedProgress` tracker surfaced at `GET /api/v1/onedrive/transfers`
  (`DaemonTransfer`); pending cloud-writes are also boot-recovered by the desktop daemon (#655). Loading the
  persisted config on mobile (`start_inner`) makes `onedrive_modes` survive restarts so the offline pass
  actually runs; a materialized body's size is compared by its envelope plaintext length (not the sealed
  on-disk size) so files are not spuriously re-uploaded; and the store-backed OneDrive body path resolves
  a nested materialized file via its parent chain (#655).
- `graph`+`webui`+`app.js`: in-app OneDrive upload/replace + the full write UI (#657). `list_children`
  now selects `eTag` (the If-Match token for in-place replace); `serve.rs` decodes a base64 request body
  on `X-Body-Encoding: base64` (uniform across the desktop HTTP path and the text-only mobile bridge)
  with a size cap → 413. New biometric-gated `POST /api/v1/onedrive/{upload,replace}` arms
  (`OneDriveWriteHandler::upload/replace`, cap-gated, its cap token injected into `/app.js`), and the
  app.js write surface — an **Upload** toolbar button, per-file **Replace**, and **Rename/Move/Delete**.
  All five route through the crash-safe cloud-write ledger: rename/move/delete via #654's, and
  upload/replace via #655's `upload_via_ledger`/`replace_via_ledger` — a WebUI upload's request-body
  bytes are materialized to a temp file the ledger reads, so an in-app write gets the same intent-first
  crash safety as the offline writeback, and a replace is etag-guarded (a 412 is a keep-both conflict,
  never a blind clobber). Also fixes `graph::upload_to_parent` for the drive-root case (an empty parent
  id, which the online-root upload path is the first to exercise): it now targets `/me/drive/root:/…:/
  content` instead of the malformed `/me/drive/items/:/…` that Graph 400s.
- `docs`: OneDrive on Mobile close-out verification (#660) now records the REV-4 plan diff and Pixel
  8 Pro end-to-end evidence for all three folder modes, upload/replace/root upload, sharing, SAF,
  transfer controls, conflict resolution, cleanup, and the No-RC main-promotion directive.

### Fixed

- `onedrive`: Mode-2 mobile opens now cache sync-mode bodies into `cache_root` before serving them.
- `webui`: Mobile OneDrive breadcrumbs no longer collapse the action toolbar on narrow phone layouts.
- `onedrive`: Mode-3 offline materialization re-downloads remote-dirty bodies when the remote hash
  changed, instead of trusting a stale local body.
- `onedrive`: `download-now` now always finishes its transfer slot after the body is written, so the
  transfer panel clears correctly.

## [1.0.0] — 2026-06-26

First stable release — the desktop/core product (CLI + daemon + web UI + native
status bar + FUSE, on Linux) **and** the standalone Android app as a full 1.0 component:
an on-device embedded engine, a **signed release APK** (per-build `versionName`/
`versionCode`), build-once/promote-many release attachment, a KVM **emulator smoke**, and
**Obtainium** OTA distribution (`REQ-AND-002…007`, all implemented). All 55 tracked
requirements are implemented, and the **FCM push end-to-end was verified live** (daemon →
device notification on a physical Pixel 8 Pro). The only outstanding Android item is
*automating* that push proof in CI (#578) — the `live_fcm_send` check is `#[ignore]`
because it needs the Firebase service-account as a CI secret; a 1.1 follow-up.

### Supported platforms (1.0)

| Platform | Status | Artifact |
|---|---|---|
| Linux x86_64 (CLI + daemon + GUI/tray/FUSE) | **Supported** | `isyncyou-linux-x86_64.tar.gz`, `isyncyou-x86_64.AppImage` |
| Windows x86_64 | Built (CLI/daemon; no GUI tray/FUSE) | `isyncyou-windows-x86_64.zip` |
| Android (arm64) | **Supported** | `isyncyou-android-arm64.apk` (signed; OTA via Obtainium) |
| macOS | Not built — code is `cfg`-portable, no Apple build host (EULA) | — |

Each release publishes a CycloneDX SBOM, `SHA256SUMS`, and cosign bundles per artifact.

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

**Desktop integration (Phase 3, Linux)**
- **FUSE Files-on-Demand** (#330): a read-only placeholder mount of an account's
  OneDrive tree — browse the whole tree instantly, files materialize to an on-disk
  cache on first read (atomic temp+fsync+rename; crash-safe). Downloads run on a
  background hydration worker so a slow fetch never freezes the mount, coalesce the
  kernel's read-ahead into one download per file, and re-resolve the read token per
  fetch so a long-lived mount keeps working past the token's lifetime. Read-only
  for #330; a read-write OneDrive folder follows in #478 (below). Built with
  `fuser` default-features-off (`fusermount3`, no `libfuse` build dependency).
- **Read-write OneDrive folder** (#478): the placeholder mount is read-write, so it
  behaves like the single Windows-OneDrive folder — editing a file uploads it on
  final close (not on every flush, so `> file` never blanks the cloud copy),
  creating a file uploads it, and `delete`/`rename`/`mkdir` in the mount map to the
  matching Graph operation (delete item, move/rename, create folder). The mount
  reports writable mode bits and re-resolves the write token per operation; if no
  write token is available it stays read-only. It also **refreshes from the cloud
  on browse** (throttled `readdir`): a delta runs into the store and the tree
  reconciles inode-stably (open handles + pending local edits preserved, tombstones
  removed), so files added/renamed/deleted on another device or the web appear in
  the folder without a restart. On KDE it is registered as a **single Places sidebar
  entry** ("OneDrive") — one folder, not a confusing pair.
- **On-demand download notifications**: batch-coalesced desktop toasts
  ("Downloading from OneDrive — Fetching N files…" → "N files are ready offline"),
  with the in-flight set exposed at `/api/v1/hydrations` and in the status bar.
- **Dolphin overlay icons**: the KF6/KIO `KOverlayIconPlugin` paints `placeholder`
  / `syncing` / `materialized` emblems for placeholder files (and the store-backed
  `synced`/`syncing`/`error`/`ignored` for synced paths) over the daemon's DBus
  `FileStatus` service; a "Make available offline" ServiceMenu action
  (`isyncyou make-available`) hydrates a selection/folder recursively.
- **Outbound sharing** (#494): share a file/folder via Microsoft Graph —
  `isyncyou share` creates a sharing link (`--type view|edit|embed`, `--scope
  anonymous|users`, `--password`/`--expiry`) copied to the clipboard, invites by
  email (`--email`, `--write`), or lists/revokes permissions (`--list`/`--revoke`);
  Dolphin "Share — copy view/edit link" ServiceMenu actions; and a web-UI "Share"
  button. Uses the cached `Files.ReadWrite` token (no extra consent). The mount
  path maps to its cloud item by path, then shares by id. Honest personal-account
  limits: the OneDrive root isn't shareable; `createLink` is idempotent per
  `(type, scope)`; `password`/`expiry`/`embed` are Premium/personal-dependent.
  **GUI email-invite (#504):** invite named people from the GUI too — a Dolphin
  "Share with people…" action (a `kdialog` wrapper that prompts for address(es) +
  read/write, then runs `isyncyou share --email`) and a web-UI "invite" action per
  OneDrive item. `isyncyou share` now also finds its config at
  `~/.config/isyncyou/isyncyou.toml` when run without `--config` from another
  directory (so GUI launches resolve it).
- **Privileged mount-wide change source** (#331): an opt-in **fanotify** backend
  (`change_source = "ebpf"`/`"fanotify"`) behind a common `ChangeSource` trait.
  Initialized in FID mode (`FAN_REPORT_DFID_NAME`) with a `FAN_MARK_FILESYSTEM`
  mark, it reports create/modify/move/delete across the whole filesystem without
  per-directory inotify watches or the `max_user_watches` ceiling, and turns a
  `FAN_Q_OVERFLOW` into a full rescan (parity with inotify). Selected only when
  opted in **and** privileged (`CAP_SYS_ADMIN`); it falls back to the unprivileged
  inotify accelerator otherwise. Wired into both `isyncyou --watch` and the daemon
  (a per-account watcher wakes the scheduled sync early on local changes). The
  periodic reconciler stays the source of truth, so a missed event only costs one
  extra (idempotent) pass.
- **Status-tray app** (#460): tray-first SNI indicator — left-click unfolds a
  frameless live-status flyout at the icon (Nextcloud/Dropbox style) with a link
  into the web UI (mail restore, search); the tray label reflects the live daemon
  status; window identity `org.silentspike.iSyncYou` (WM_CLASS/app_id) + launcher
  `.desktop`.

**Unified live + backup client (Phase 4, epic #556)**
- **Near-real-time cloud client** for all six M365 services. The daemon polls each
  account on a configurable interval and pushes changes to the web UI over SSE; the
  UI's **live-update interval slider** (`POST /api/v1/settings?poll_interval_secs=N`,
  1 s–60 min, cap-token-gated) persists and applies the cadence without a restart.
- **Four-state coverage badge** on every item — `live_only` (in the cloud, not yet
  archived) · `live_backup` (archived and current, `etag == body_etag`) · `stale`
  (archived copy older than the cloud) · `backup_only` (deleted in the cloud, still
  in the archive). Derived from a store `body_etag` set at the `set_local_path`
  chokepoint (store v10); per-service state filter bars in the UI.
- **Live write** for every service, each a cap-token-gated POST that performs the
  Graph mutation on the cached restore token and refreshes only the touched item
  (no SSE echo on self-write):
  - **Mail** — compose/send, reply/reply-all/forward, flag, read/unread, categories,
    move; per-message manage UI.
  - **Calendar** — create/update/delete events; recurrence-aware; colour-mapped
    calendars.
  - **Contacts** — create/edit/delete; full detail (multiple addresses, IM,
    categories, relationships) + contact photo.
  - **ToDo** — create/complete/edit tasks, checklist steps, linked resources,
    attachments; list operations.
  - **OneNote** — notebook → section → page **tree** (notebooks, section groups and
    sections archived as items with parent chains, not a flat page list); create a
    page in its original section (404 → default-section fallback), best-effort
    content append, delete; page metadata sidecar (created/links/level/order/userTags
    when Graph returns them).
  - **OneDrive** — drive quota + lazy per-item permissions in the explorer.
- **Restore-to-original-container**: a re-created item lands back in its source
  folder/calendar/list/section (same-account), not a default bucket.
- All writes require an `X-Capability-Token` minted per daemon boot and injected into
  the served `app.js`; an absent/invalid token returns `401`. Bodies are rendered in
  a sandboxed iframe under the strict 3-layer CSP (ammonia-sanitised, scripts
  stripped, remote resources blocked).

**Tooling & ops**
- `isyncyou` CLI (init/check/login/status/sync/backup/search/restore/export/migrate/serve;
  Linux: mount/make-available/dolphin-status); `isyncyoud` daemon (serves the web UI,
  hosts the FUSE placeholder mounts + DBus FileStatus); `isyncyou-doctor`.
- **Token keep-alive**: the daemon proactively silent-refreshes every account's
  cached read+write tokens on a timer (`--token-refresh-secs`, default 6h) so a
  long-running daemon never lets a refresh token lapse from inactivity — after the
  one-time login, auth stays alive with no further user action. Each refresh
  persists the renewed token; a missing/uncached token is skipped, never fatal.
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
  and a nightly E2E against the dedicated throwaway account covering **every user
  journey**: backup (all five services), OneDrive sync, upload with cloud teardown,
  a real two-profile edit-edit **conflict** (keep-both asserted), **cloud restore
  with teardown** (`rm`), archive **migration** round-trip, **doctor**, search,
  restore-to-local, verify — plus the web UI (functional + visual regression) and
  the native status bar — with pass/fail pushed to a notification channel. Its
  first runs caught three real bugs (a `verify` false positive, the `rm` id
  encoding, and the download-path edit-edit data loss) before any release shipped
  them.

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
- **OneNote page-content backup builder error** (#470/#471): `get_bytes` now prefixes
  a relative Graph path with the API base before reqwest builds the request, so the
  OneNote page-content URL (`/me/onenote/pages/{id}/content`) no longer fails with a
  *builder error* (relative URL without a host).
- **Reachable dependency advisories**: `quinn-proto` → 0.11.15 (RUSTSEC-2026-0185,
  remote memory exhaustion via reqwest's QUIC transport) and `memmap2` → 0.9.11
  (RUSTSEC-2026-0186, unchecked pointer offset via the GUI font/buffer stack).
- **Download-path data loss on edit-edit conflicts** (found by the staging E2E's
  live conflict journey): when a file was edited locally AND remotely between
  syncs, the one-shot sync downloaded the remote version **over the local edit**
  with no conflict copy — keep-both only existed on the upload path (If-Match/412).
  The store now records a **last-synced on-disk reference** per item (schema v8:
  size/mtime/QuickXorHash, written only by the download/upload paths — the delta
  ingest overwrites item metadata with new remote values, which is exactly why
  the edit could not be detected before). Materialize compares the disk file
  against that reference (size → mtime → hash ladder) and moves a locally-edited
  file aside as a `safeBackup` conflict copy before writing the cloud version;
  the summary counts it under `conflict copies`. Pre-v8 items without a reference
  keep the old behavior instead of spraying conflict copies on ordinary updates.
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

### Out of scope (by design)
- eBPF change-source backend: the privileged mount-wide **fanotify** backend
  (#331) already covers the server case with zero extra dependencies; an eBPF
  tracer would add a heavy BPF toolchain (CO-RE/BTF) and a higher privilege
  ceiling for no acceptance-criteria gain, so it is deliberately not built.
- Remote network access to the local API (mTLS/pairing/token-rotation stack):
  the API is **local-only by design** — no remote listener exists or is planned.
  The target audience runs iSyncYou on the machine they sit at; the rare
  headless-server operator tunnels via SSH (`ssh -L 8765:127.0.0.1:8765 host`)
  or a self-hosted VPN, which is better-audited than any home-grown remote-auth
  stack. "No open port" is the strongest security posture; risk-register R6 is
  accepted by design on this basis (story S-P3.1 closed as not-planned).
- Azure Event Hub realtime push: would require a paid Azure subscription, and
  iSyncYou takes no paid cloud dependency. Adaptive delta-polling (implemented) is
  the change-detection mechanism; Graph notifications are only hints anyway, so a
  delta pull always follows.
- macOS build: requires Apple hardware or paid cloud-Mac CI minutes (Apple's EULA
  restricts macOS virtualization to Apple-branded hardware, so it cannot be hosted
  on x86 Linux build hosts). The code is kept mac-ready — the Linux-only bits
  (FUSE mount, DBus/KIO) are `cfg(target_os = "linux")`-gated, so the CLI/daemon
  build for macOS once a Mac build host is available. Linux + Windows ship today.

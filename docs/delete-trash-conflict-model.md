# Delete, trash & conflict model

How iSyncYou avoids silent data loss (plan §10), implemented in
`crates/core/src/{conflict,guard}.rs`.

## Mass-delete guard (both directions)

`DeleteGuard::evaluate(destructive, total_tracked, direction)` stops a batch that
removes/replaces too much, in **either** direction (local→cloud and cloud→local):

- **absolute cap** — `destructive >= max_absolute` (default 1000) → `Block`.
- **fraction cap** — when `total_tracked >= fraction_min_total` (default 10) and
  `destructive / total_tracked >= max_fraction` (default 0.5) → `Block`.
- otherwise `Proceed`. Blocking returns a human-readable reason for a
  confirmation prompt; thresholds are configurable (`[sync.delete_guard]`).

Verified in the acceptance harness (A2).

## Conflict kinds (`ConflictKind`)

`classify(local_change, remote_change)` returns a conflict only when both sides
genuinely diverge (one-sided changes and independently-mergeable combos like
rename-on-one + edit-on-other are **not** conflicts):

`ContentContent`, `RenameRename`, `RenameDelete`, `EditDelete`, `FileFolderType`,
and the transfer-time `UploadPreconditionFailed`, `DownloadChangedDuringTransfer`.

## Resolution (`resolve`)

- Data-loss-risk conflicts (content/rename/edit-delete/file-folder) → **keep both**
  in headless mode (a conflict copy), or prompt in the GUI. Never silently
  overwrite.
- Transfer-time conflicts (`UploadPreconditionFailed` via ETag/`If-Match`,
  `DownloadChangedDuringTransfer`) → **re-evaluate** (re-fetch + retry), not
  overwrite. Verified in the acceptance harness (A3).
- Conflict-copy names follow `name-<host>-safeBackup-NNNN.ext`
  (`conflict_copy_name`).

## Trash (plan §9.3)

Cloud-side deletions move the local copy to a trash area kept **outside** the sync
root (so trash is never re-synced); config validation enforces that an account's
`archive_root` differs from its `sync_root` (acceptance A9). Retention is
configurable (`trash_retention_days`, default 30); versioning is delegated to PBS,
not kept in-app.

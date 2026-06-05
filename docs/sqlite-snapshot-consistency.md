# SQLite snapshot consistency

How iSyncYou takes a consistent store snapshot for PBS without copying a live
SQLite file.

## Current implementation

- `Store::backup_to(dest)` uses SQLite `VACUUM INTO`, which writes a readable,
  independent copy from a single consistent database view. When the source store
  was opened with SQLCipher, the snapshot remains encrypted with the same store
  key; it is not a plaintext export.
- `isyncyou pbs-backup --account <id>` stages a temporary directory containing
  `store.db` plus `manifest.json`, then uploads that directory as one PBS
  `data.pxar` archive.
- `isyncyou pbs-restore --snapshot <path> --into <dir>` restores into the
  requested directory only. It never writes into the live `archive_root` or live
  store path.
- PBS credentials are passed to `proxmox-backup-client` through `PBS_PASSWORD`,
  never as command-line arguments.
- Store at-rest encryption is opt-in via `ISYNCYOU_STORE_KEY_FILE`, systemd
  credential `isyncyou-store-key`, or `ISYNCYOU_STORE_KEY`. Without one of those,
  new stores are plaintext SQLite and `isyncyou-doctor` warns.

The core invariant is: **never copy `.isyncyou-store.db` directly while it may be
open**. The snapshot path is either a SQLite-created copy (`VACUUM INTO`) or a PBS
restore into a temporary/import directory.

## Snapshot manifest

The staged `manifest.json` currently records:

```json
{
  "account": "<account id>",
  "schema_version": 7,
  "created_unix": "<seconds since epoch>"
}
```

This is enough to reject obvious wrong-account or wrong-schema imports before a
future restore-preview flow opens the temporary store.

## Verification

- `Store::backup_to` is unit-tested by
  `backup_to_writes_a_readable_consistent_copy`: the snapshot opens as a separate
  valid store and contains the expected item.
- SQLCipher-backed store snapshots are unit-tested by
  `encrypted_backup_to_keeps_snapshot_encrypted`: the snapshot lacks the
  plaintext SQLite header, does not leak item names, refuses to open without the
  key, and opens with the correct key.
- `crates/pbs` unit-tests parse the created PBS snapshot id and verify that PBS
  password material is not leaked through command errors.
- `tools/live_pbs_roundtrip.py` is the environment-gated live probe. On
  2026-06-05 it was run against the test PBS repository (`hdd-backup`): the CLI
  created `host/isyncyou-primary/2026-06-05T15:46:31Z`, restored it into a
  temporary directory, verified `manifest.json` (`account=primary`) and a readable
  SQLite store (`user_version=4`, 15 tables), then forgot the test snapshot.

## Open hardening

- Add a restore-import command that validates `manifest.json` before opening the
  temporary store for preview/restore selection.
- Add optional namespace creation/probing for live PBS jobs; PBS does not create
  missing namespaces implicitly.
- Add an optional daemon quiesce/read-only window around very large snapshot
  staging if the future engine keeps long write transactions open.

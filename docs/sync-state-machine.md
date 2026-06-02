# Sync state machine

The per-item sync automaton (plan §5.1), implemented in `crates/core/src/sync_state.rs`
as `SyncState::on(self, event) -> SyncState`. It is a pure function (no I/O), so the
transitions are exhaustively unit-tested. The periodic reconciler is the source of
truth; this automaton tracks an item's in-flight intent between reconciles.

## States (`SyncState`)

| State | Meaning |
|---|---|
| `Clean` | in sync; nothing to do |
| `LocalDirty` | local copy changed → needs upload |
| `RemoteDirty` | remote copy changed → needs download |
| `BothDirty` | both sides changed since last sync |
| `Conflict` | needs resolution (e.g. keep-both copy) |
| `DeletePending` | removed locally → propagate deletion to the cloud |
| `TrashPending` | removed on the cloud → move local copy to trash |
| `UploadStaged` | upload in progress / staged |
| `DownloadStaged` | download in progress / staged |
| `ErrorRetryable` | a retryable error occurred; will retry |
| `ErrorFatal` | a fatal error; needs operator/recovery action |

## Events (`SyncEvent`)

`LocalModified`, `RemoteModified`, `LocalRemoved`, `RemoteRemoved`, `UploadStarted`,
`UploadFinished`, `DownloadStarted`, `DownloadFinished`, `DeleteFinished`,
`ConflictResolved`, `RetryableError`, `FatalError`, `Recovered`.

## Key transitions

```
Clean       --LocalModified-->  LocalDirty
Clean       --RemoteModified--> RemoteDirty
Clean       --LocalRemoved-->   DeletePending
Clean       --RemoteRemoved-->  TrashPending

LocalDirty  --RemoteModified--> BothDirty
LocalDirty  --UploadStarted-->  UploadStaged
LocalDirty  --RemoteRemoved-->  Conflict        # local edit vs remote delete
RemoteDirty --LocalModified-->  BothDirty
RemoteDirty --DownloadStarted-> DownloadStaged
RemoteDirty --LocalRemoved-->   Conflict        # remote edit vs local delete

BothDirty   --ConflictResolved->Clean
UploadStaged   --UploadFinished-->   Clean
UploadStaged   --LocalModified-->    LocalDirty  # changed mid-upload -> stale, redo
DownloadStaged --DownloadFinished--> Clean

* --RetryableError--> ErrorRetryable
* --FatalError-->     ErrorFatal
ErrorRetryable/ErrorFatal --Recovered--> (re-scan re-derives the state)
```

## Invariants

- An edit during a staged transfer (`UploadStaged + LocalModified`) reverts to
  `LocalDirty` so the stale transfer is redone — never silently committed.
- A change on the *other* side of a delete (`LocalDirty + RemoteRemoved`,
  `RemoteDirty + LocalRemoved`) becomes `Conflict`, never a silent loss.
- Errors are explicit states; `Recovered` defers to a re-scan rather than guessing.

The conflict resolution that turns `Conflict`/`BothDirty` into a concrete action
lives in [`delete-trash-conflict-model.md`](delete-trash-conflict-model.md)
(`core::conflict`).

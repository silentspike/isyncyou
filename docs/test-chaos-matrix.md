# Test / chaos matrix

The data-loss and failure scenarios the engine must survive (plan §20), and how
each is currently verified. The bar is **no silent data loss and no corrupt
state** under the failure — not just that the happy path works.

## Levels of verification

- **Logic-level (now):** deterministic, headless integration tests against the
  real engine crates (no daemon, no display, no network) in `crates/acceptance`:
  - `tests/mvp.rs` — the hard acceptance criteria **A1–A10** (plan §19), one test
    per criterion.
  - `tests/chaos.rs` — the **chaos subset** called out by #61
    (overflow / disk-full / 410 / kill), as named adversarial variants.
- **E2E (pending, #19/#61-parent):** the full matrix against a live test account
  with the assembled daemon + a display (install → login → sync → conflict →
  restore → migrate), plus fault injection. Needs prerequisites that aren't
  available headless.

## Matrix

| Scenario (plan §20) | Mechanism under test | Status |
|---|---|---|
| Forbidden / case-only / invalid names, path length | `pathmap` reversible codec + persistent mapping table | ✅ A1 |
| Mass delete (both directions) | `DeleteGuard` absolute + fraction caps | ✅ A2 |
| Silent overwrite / content conflict | `If-Match`/ETag → re-evaluate; keep-both default | ✅ A3 |
| `410 Gone` delta token | resync from fresh snapshot, discard stale (never blind-delete) | ✅ A4, chaos (first-request 410, resync-to-empty) |
| Network loss during upload session | resumable session from server `nextExpectedRanges` | ✅ A5, chaos (server offset ahead, resume-from-zero) |
| Crash points (file-before-DB, DB-before-rename) | atomic write (tmp+rename), no partial / no stray temp | ✅ A6, chaos (write under missing dir → error, no partial) |
| inotify `IN_Q_OVERFLOW` | coalescer flags overflow, drops untrusted buffer → rescan | ✅ A7, chaos (first-event, mid-stream drops all) |
| Disk-full (download & DB commit) | `SelfCheck` → red health; pause not corrupt | ✅ A8, chaos (combines with auth fault, recovers, exact-min boundary) |
| Trash outside the sync root | config validation rejects `archive_root == sync_root` | ✅ A9 |
| Crash recovery / journal replay | journal recovers uncommitted ops; commit clears | ✅ A10, chaos (partial commit across crash) |
| Graph 401/403/404/409/412/416/423/500/503/507 | error classification / retry-vs-fatal | ⏳ partial (classifier unit-tested in `graph`; full matrix E2E) |
| Token / refresh / subscription / delta-token expired | refresh path; documented invalid-grant blocker | ⏳ partial (refresh coded; live blocked on fresh OAuth) |
| clock-skew, DST / timezone restore | timezone-pinned reads/exports | ⏳ E2E |
| Malformed / huge MIME | best-effort MIME extraction, never panics; capped; safe `cid:` images replayed only as local data URLs | ✅ (mime/webui unit tests) / ⏳ huge-message E2E |
| OneNote resources, PBS-snapshot-during-active | resource fetch; `VACUUM INTO` store snapshot + PBS temp restore | ✅ resource archive manifest / ✅ live PBS temp restore; restore-preview import pending |
| move-folder with 10k children | id-based tracking, parent walk | ⏳ E2E (scale) |

✅ logic-level test exists · ⏳ awaits E2E or an external prerequisite.

## Running the logic-level matrix

```sh
cargo test -p isyncyou-acceptance            # A1–A10 + chaos subset
cargo test -p isyncyou-acceptance --test chaos
```

Both are deterministic and need no network, account, daemon, or display.

//! v0.1 acceptance criteria A1–A10 (plan §19), verified against the real engine
//! crates. Each test is one criterion. The consolidated evidence matrix (incl. the
//! live test-account runs) lives in `docs/acceptance-v0.1.md`. The assembled
//! daemon + GUI end-to-end walk additionally needs a display (tray work #16/#56).

use isyncyou_change_source::watcher::{Coalescer, RawEvent};
use isyncyou_core::conflict::{resolve, ConflictKind, ConflictPolicy};
use isyncyou_core::guard::{DeleteGuard, Direction, GuardVerdict};
use isyncyou_core::recovery::{atomic_write, Journal, SelfCheck};
use isyncyou_core::{AccountConfig, Config, HealthStatus};
use isyncyou_graph::client::Response;
use isyncyou_graph::{run_delta, DeltaCursor, Transport, UploadSession};
use isyncyou_pathmap::{is_reserved, to_cloud, to_local, MappingTable};
use serde_json::json;

/// A1 — no path/name data loss: forbidden cloud names round-trip through the
/// local namespace and back unchanged, and a persistent mapping is reversible.
#[test]
fn a1_no_path_or_name_data_loss() {
    // A Linux local name may contain characters OneDrive forbids; to_cloud encodes
    // them to safe look-alikes and to_local decodes back — losslessly.
    for local in [
        "a:b",
        "c?d",
        "e*f",
        "g|h",
        "i<j>k",
        "quote\"x",
        "trail ",
        "dot.",
        "normal.txt",
        "Ähnlich Über.pdf",
    ] {
        let cloud = to_cloud(local);
        // the cloud name must never contain a raw forbidden character
        for bad in ['<', '>', ':', '"', '|', '?', '*'] {
            assert!(
                !cloud.contains(bad),
                "cloud '{cloud}' kept forbidden '{bad}'"
            );
        }
        // round-trip is lossless
        assert_eq!(to_local(&cloud), local, "roundtrip lost data for {local:?}");
    }
    assert!(is_reserved("CON") && is_reserved("lpt1") && !is_reserved("report"));

    // the persistent mapping table is the authoritative reversible backstop
    let mut map = MappingTable::new();
    let cloud = map.assign_cloud_name("parent", "weird:name?.txt");
    assert!(
        !cloud.contains(':') && !cloud.contains('?'),
        "mapped cloud name unsafe: {cloud}"
    );
    assert_eq!(map.lookup_local("parent", &cloud), Some("weird:name?.txt"));
    assert_eq!(
        map.lookup_cloud("parent", "weird:name?.txt"),
        Some(cloud.as_str())
    );
}

/// A2 — mass-delete guard fires in BOTH directions (absolute + fraction caps).
#[test]
fn a2_delete_guard_both_directions() {
    let g = DeleteGuard::default(); // max_absolute=1000, max_fraction=0.5, min_total=10
    for dir in [Direction::LocalToCloud, Direction::CloudToLocal] {
        // fraction rule: 600/1000 = 60% >= 50% -> block
        assert!(
            g.evaluate(600, 1000, dir).is_blocked(),
            "{dir} fraction not blocked"
        );
        // absolute rule
        assert!(
            g.evaluate(2000, 100_000, dir).is_blocked(),
            "{dir} absolute not blocked"
        );
        // safe small batch proceeds
        assert_eq!(g.evaluate(1, 1000, dir), GuardVerdict::Proceed);
        // tiny libraries are exempt from the fraction rule (2 of 2 < min_total)
        assert_eq!(g.evaluate(2, 2, dir), GuardVerdict::Proceed);
    }
}

/// A3 — ETag/If-Match precondition is never a silent overwrite, and content
/// conflicts keep both sides by default.
#[test]
fn a3_etag_precondition_no_silent_overwrite() {
    let policy = ConflictPolicy::headless("host");
    // an upload precondition failure is re-evaluated, not blindly overwritten
    assert_eq!(
        resolve(ConflictKind::UploadPreconditionFailed, &policy),
        isyncyou_core::conflict::Resolution::Reevaluate
    );
    // a real content/content divergence keeps both (no data loss) in headless mode
    match resolve(ConflictKind::ContentContent, &policy) {
        isyncyou_core::conflict::Resolution::KeepBoth { .. } => {}
        other => panic!("content conflict must keep both, got {other:?}"),
    }
}

/// A4 — a `410 Gone` triggers reconciliation from a fresh snapshot, NOT a blind
/// delete of everything: partial pre-410 items are discarded, the resync wins.
#[test]
fn a4_gone_triggers_reconciliation() {
    struct Mock(Vec<Response>, usize);
    impl Transport for Mock {
        fn get(&mut self, _url: &str) -> Response {
            let r = self.0[self.1].clone();
            self.1 += 1;
            r
        }
    }
    let mut t = Mock(
        vec![
            Response::ok(json!({ "value": [{"id": "stale"}], "@odata.nextLink": "u2" })),
            Response::status(410),
            Response::ok(json!({ "value": [{"id": "fresh"}], "@odata.deltaLink": "TOK" })),
        ],
        0,
    );
    let out = run_delta(&mut t, "base", Some(&DeltaCursor::new("old")), 5).unwrap();
    assert!(out.resynced, "410 must force a resync");
    assert_eq!(out.items.len(), 1);
    assert_eq!(
        out.items[0]["id"], "fresh",
        "stale pre-410 items must be discarded"
    );
}

/// A5 — an upload resumes after a process kill: a fresh session, told the
/// server's `nextExpectedRanges`, continues from the right offset to completion.
#[test]
fn a5_upload_resume_survives_kill() {
    let total = 10 * 320 * 1024; // 3.2 MiB
    let mut s = UploadSession::new("https://up", total);
    let chunk = s.next_chunk(320 * 1024).unwrap();
    s.advance(chunk.len);
    let mid = s.next_offset();
    assert!(mid > 0 && !s.is_complete());

    // "process killed" — reconstruct the session from scratch (state lost)...
    let mut resumed = UploadSession::new("https://up", total);
    // ...and resume from the server-reported next range.
    resumed.apply_next_expected(&[format!("{mid}-")]);
    assert_eq!(
        resumed.next_offset(),
        mid,
        "must resume from the server offset"
    );

    // drive it to completion
    let mut guard = 0;
    while let Some(plan) = resumed.next_chunk(320 * 1024) {
        resumed.advance(plan.len);
        guard += 1;
        assert!(guard < 1000, "resume did not converge");
    }
    assert!(resumed.is_complete());
}

/// A6 — a write is atomic (tmp + rename): a reader never sees a partial file, so
/// an interrupted download/commit is safe to restart. No `.tmp` is left behind.
#[test]
fn a6_atomic_write_safe_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.bin");
    atomic_write(&path, b"first").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"first");
    // overwrite atomically
    atomic_write(&path, b"second-longer").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"second-longer");
    // no stray temp files in the directory
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != "state.bin")
        .collect();
    assert!(
        leftovers.is_empty(),
        "atomic_write left temp files: {leftovers:?}"
    );
}

/// A7 — an inotify queue overflow forces a full rescan: the coalescer marks
/// overflow and drops its (now-incomplete) buffer so the engine cannot trust it.
#[test]
fn a7_inotify_overflow_forces_rescan() {
    let mut c = Coalescer::new();
    c.push(RawEvent::Created("a.txt".into()));
    c.push(RawEvent::Modified("b.txt".into()));
    assert!(!c.overflow());
    c.push(RawEvent::QueueOverflow);
    assert!(c.overflow(), "overflow must be flagged");
    // the buffered (incomplete) events were dropped — the caller must rescan
    assert!(
        c.drain().is_empty(),
        "overflow must clear the untrusted buffer"
    );
}

/// A8 — disk-full surfaces as a hard (red) health status, so the engine pauses
/// instead of corrupting state.
#[test]
fn a8_disk_full_is_red() {
    let low = SelfCheck {
        token_valid: true,
        db_ok: true,
        free_bytes: 1_000,
        min_free_bytes: 1_000_000,
        last_sync_age_secs: Some(1),
        max_sync_age_secs: 3600,
    };
    match low.evaluate() {
        HealthStatus::Red(reasons) => {
            assert!(
                reasons.iter().any(|r| r.contains("disk")),
                "expected a disk reason"
            );
        }
        other => panic!("disk-full must be Red, got {other:?}"),
    }
    // a healthy machine is green
    let ok = SelfCheck {
        free_bytes: 10_000_000,
        ..low
    };
    assert_eq!(ok.evaluate(), HealthStatus::Green);
}

/// A9 — the trash/backup area is kept separate from the synced tree: config
/// validation refuses an account whose archive_root equals its sync_root, so a
/// trash/archive write can never land inside the sync root. (Runtime placement of
/// the `.m365-trash` dir is exercised end-to-end with the daemon, tray work #16/#56.)
#[test]
fn a9_archive_separate_from_sync_root() {
    let mut a = AccountConfig {
        id: "a".into(),
        username: "a@outlook.com".into(),
        sync_root: "/home/u/OneDrive".into(),
        archive_root: "/home/u/OneDrive".into(), // same -> invalid
        cache_root: Default::default(),
        mount_point: None,
    };
    let bad = Config {
        accounts: vec![a.clone()],
        ..Default::default()
    };
    let errs = bad.validate().unwrap_err();
    assert!(errs.iter().any(|e| e.contains("must differ")));
    // distinct roots validate
    a.archive_root = "/home/u/Archive".into();
    let good = Config {
        accounts: vec![a],
        ..Default::default()
    };
    assert!(good.validate().is_ok());
}

/// A10 — crash recovery: an operation begun but not committed survives a
/// "crash" (journal reopen) as pending, and committing clears it. A clean run
/// leaves nothing pending.
#[test]
fn a10_crash_recovery_journal() {
    let dir = tempfile::tempdir().unwrap();
    let jpath = dir.path().join("journal.json");
    let seq = {
        let mut j = Journal::open(&jpath).unwrap();
        j.begin("upload /Docs/big.bin").unwrap() // started, NOT committed
    };
    // "crash" — reopen the journal from disk
    let mut j2 = Journal::open(&jpath).unwrap();
    assert_eq!(j2.pending().len(), 1, "uncommitted op must be recovered");
    assert_eq!(j2.pending()[0].op, "upload /Docs/big.bin");
    j2.commit(seq).unwrap();
    // a fresh reopen now shows a clean journal
    let j3 = Journal::open(&jpath).unwrap();
    assert!(j3.pending().is_empty(), "committed op must clear");
}

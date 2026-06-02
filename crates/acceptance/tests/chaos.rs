//! Chaos subset for the v0.1 acceptance gate (#19): the four data-loss-prone
//! failure modes called out in the task — **inotify overflow**, **disk-full**,
//! **`410 Gone`**, and a **process kill mid-upload** — exercised as named,
//! adversarial variants beyond the single-criterion A-tests in `mvp.rs`.
//!
//! Each test is deterministic and headless, driving the *real* engine crates
//! (no daemon, no display, no network). The bar is "no silent data loss and no
//! corrupt state under the failure", not "the happy path works".

use isyncyou_change_source::watcher::{Coalescer, RawEvent};
use isyncyou_core::recovery::{atomic_write, Journal, SelfCheck};
use isyncyou_core::HealthStatus;
use isyncyou_graph::client::Response;
use isyncyou_graph::{run_delta, DeltaCursor, Transport, UploadSession};
use serde_json::json;

const CHUNK: u64 = 320 * 1024;

/// A `Transport` that replays a fixed script of responses, panicking if the
/// engine asks for more than were scripted (so an unexpected extra request is a
/// hard failure, not a silent hang).
struct Script {
    responses: Vec<Response>,
    at: usize,
}
impl Script {
    fn new(responses: Vec<Response>) -> Self {
        Self { responses, at: 0 }
    }
}
impl Transport for Script {
    fn get(&mut self, _url: &str) -> Response {
        let r = self
            .responses
            .get(self.at)
            .unwrap_or_else(|| panic!("delta made an unscripted request #{}", self.at + 1))
            .clone();
        self.at += 1;
        r
    }
}

// ── overflow ────────────────────────────────────────────────────────────────

/// A clean batch (no overflow) is trusted: `drain` returns the buffered events.
/// This is the baseline that makes the "dropped on overflow" assertions below
/// meaningful (drain is not vacuously empty).
#[test]
fn chaos_overflow_baseline_clean_batch_is_trusted() {
    let mut c = Coalescer::new();
    c.push(RawEvent::Created("a.txt".into()));
    c.push(RawEvent::Modified("b.txt".into()));
    assert!(!c.overflow());
    assert!(
        !c.drain().is_empty(),
        "a clean batch must be drained, not dropped"
    );
}

/// Overflow as the *very first* signal still flags the coalescer and yields an
/// empty (untrusted) drain — the engine must rescan, not trust an empty buffer
/// as "nothing changed".
#[test]
fn chaos_overflow_as_first_event_forces_rescan() {
    let mut c = Coalescer::new();
    c.push(RawEvent::QueueOverflow);
    assert!(
        c.overflow(),
        "overflow must be flagged even as the first event"
    );
    assert!(
        c.drain().is_empty(),
        "overflow buffer must be untrusted/empty"
    );
}

/// Overflow *mid-stream* invalidates the **entire** buffer, including events
/// seen before the overflow — those pre-overflow events may have been followed
/// by dropped ones, so none of them can be trusted.
#[test]
fn chaos_overflow_midstream_drops_everything() {
    let mut c = Coalescer::new();
    for i in 0..50 {
        c.push(RawEvent::Created(format!("f{i}.txt")));
    }
    c.push(RawEvent::QueueOverflow);
    for i in 0..50 {
        c.push(RawEvent::Modified(format!("g{i}.txt")));
    }
    assert!(c.overflow(), "overflow must win over a large buffer");
    assert!(
        c.drain().is_empty(),
        "pre- and post-overflow events alike must be dropped"
    );
}

// ── disk-full ─────────────────────────────────────────────────────────────────

fn base_check() -> SelfCheck {
    SelfCheck {
        token_valid: true,
        db_ok: true,
        free_bytes: 10_000_000,
        min_free_bytes: 1_000_000,
        last_sync_age_secs: Some(1),
        max_sync_age_secs: 3600,
    }
}

/// Disk-full combined with another hard fault stays red and reports *both*
/// reasons, so an operator sees the full picture (one red doesn't mask another).
#[test]
fn chaos_disk_full_combines_with_auth_failure() {
    let c = SelfCheck {
        free_bytes: 1,
        token_valid: false,
        ..base_check()
    };
    match c.evaluate() {
        HealthStatus::Red(reasons) => {
            assert!(
                reasons.iter().any(|r| r.contains("disk")),
                "missing disk reason"
            );
            assert!(
                reasons.iter().any(|r| r.contains("auth")),
                "missing auth reason"
            );
        }
        other => panic!("two hard faults must be Red, got {other:?}"),
    }
}

/// Freeing space clears the red status — the disk-full state is transient, not
/// a latched failure.
#[test]
fn chaos_disk_full_recovers_to_green() {
    let low = SelfCheck {
        free_bytes: 500,
        ..base_check()
    };
    assert!(matches!(low.evaluate(), HealthStatus::Red(_)));
    let recovered = SelfCheck {
        free_bytes: 5_000_000,
        ..low
    };
    assert_eq!(recovered.evaluate(), HealthStatus::Green);
}

/// Boundary: exactly `min_free_bytes` is *not* disk-full (the guard is `<`), so
/// the engine doesn't false-alarm at the threshold.
#[test]
fn chaos_disk_full_boundary_exact_minimum_is_ok() {
    let at = SelfCheck {
        free_bytes: 1_000_000,
        min_free_bytes: 1_000_000,
        ..base_check()
    };
    assert_eq!(
        at.evaluate(),
        HealthStatus::Green,
        "exact-min must not be red"
    );
}

/// A write that cannot complete (target directory missing — the ENOSPC analog
/// for this unit) fails loudly and leaves **no** partial file behind, so an
/// out-of-space commit can't corrupt the tree.
#[test]
fn chaos_atomic_write_failure_leaves_no_partial() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("missing-subdir").join("state.bin");
    assert!(
        atomic_write(&bad, b"payload").is_err(),
        "writing under a missing dir must error"
    );
    assert!(!bad.exists(), "no partial file may be left behind");
    assert!(
        !bad.parent().unwrap().exists(),
        "atomic_write must not have created the missing dir"
    );
}

// ── 410 Gone ────────────────────────────────────────────────────────────────

/// A `410` on the *very first* request (stale cursor rejected immediately)
/// resyncs from a fresh snapshot — no prior items existed to delete.
#[test]
fn chaos_410_on_first_request_resyncs() {
    let mut t = Script::new(vec![
        Response::status(410),
        Response::ok(json!({ "value": [{"id": "fresh"}], "@odata.deltaLink": "TOK" })),
    ]);
    let out = run_delta(&mut t, "base", Some(&DeltaCursor::new("stale")), 5).unwrap();
    assert!(out.resynced, "a first-request 410 must force a resync");
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0]["id"], "fresh");
}

/// A resync that legitimately returns an **empty** snapshot is honored as-is:
/// `resynced` is set and no stale items leak through. The emptiness is the
/// authoritative server state, reached via resync — never a blind local wipe.
#[test]
fn chaos_410_resync_to_empty_snapshot_discards_stale() {
    let mut t = Script::new(vec![
        Response::ok(
            json!({ "value": [{"id": "stale-1"}, {"id": "stale-2"}], "@odata.nextLink": "u2" }),
        ),
        Response::status(410),
        Response::ok(json!({ "value": [], "@odata.deltaLink": "TOK" })),
    ]);
    let out = run_delta(&mut t, "base", Some(&DeltaCursor::new("old")), 5).unwrap();
    assert!(out.resynced, "410 must force a resync");
    assert!(
        out.items.is_empty(),
        "stale pre-410 items must not survive the resync"
    );
}

// ── kill mid-upload ───────────────────────────────────────────────────────────

/// After a kill, the server may report an offset *ahead* of what we locally
/// recorded (a prior attempt got further than we knew). Resume must jump
/// forward to the server's offset — never re-send already-stored bytes.
#[test]
fn chaos_resume_honors_server_offset_ahead() {
    let total = 10 * CHUNK;
    let mut s = UploadSession::new("https://up", total);
    // we believe we've sent one chunk...
    let first = s.next_chunk(CHUNK).unwrap();
    s.advance(first.len);
    assert_eq!(s.next_offset(), CHUNK);
    // ...but the server says it already has three chunks.
    s.apply_next_expected(&[format!("{}-", 3 * CHUNK)]);
    assert_eq!(
        s.next_offset(),
        3 * CHUNK,
        "resume must trust the server's (ahead) offset"
    );
    // and still drives cleanly to completion
    let mut guard = 0;
    while let Some(p) = s.next_chunk(CHUNK) {
        s.advance(p.len);
        guard += 1;
        assert!(guard < 1000, "resume did not converge");
    }
    assert!(s.is_complete());
}

/// Kill before a single byte was uploaded: a fresh session told `0-` resumes
/// from the start and completes — the degenerate resume case is safe.
#[test]
fn chaos_resume_from_zero_completes() {
    let total = 4 * CHUNK;
    let mut s = UploadSession::new("https://up", total);
    s.apply_next_expected(&["0-".to_string()]);
    assert_eq!(s.next_offset(), 0);
    let mut guard = 0;
    while let Some(p) = s.next_chunk(CHUNK) {
        s.advance(p.len);
        guard += 1;
        assert!(guard < 1000, "did not converge");
    }
    assert!(s.is_complete());
}

/// A crash with several in-flight operations recovers exactly the uncommitted
/// ones: committing one before the crash must not resurrect it, and the others
/// survive as pending until they too commit.
#[test]
fn chaos_journal_partial_commit_across_crash() {
    let dir = tempfile::tempdir().unwrap();
    let jpath = dir.path().join("journal.json");
    let (s1, _s2, s3) = {
        let mut j = Journal::open(&jpath).unwrap();
        let s1 = j.begin("op-1").unwrap();
        let s2 = j.begin("op-2").unwrap();
        let s3 = j.begin("op-3").unwrap();
        j.commit(s2).unwrap(); // op-2 finished before the crash
        (s1, s2, s3)
    };
    // "crash" — reopen from disk
    let mut j2 = Journal::open(&jpath).unwrap();
    let pending: Vec<&str> = j2.pending().iter().map(|e| e.op.as_str()).collect();
    assert_eq!(pending.len(), 2, "exactly the uncommitted ops survive");
    assert!(pending.contains(&"op-1") && pending.contains(&"op-3"));
    assert!(
        !pending.contains(&"op-2"),
        "a committed op must not resurrect"
    );
    // finish the rest
    j2.commit(s1).unwrap();
    j2.commit(s3).unwrap();
    let j3 = Journal::open(&jpath).unwrap();
    assert!(j3.pending().is_empty(), "all committed -> clean journal");
}

//! Human-readable evidence for ADR-001: inject a crash at each unsafe point of a
//! cloud restore and show that recovery yields **exactly one** cloud item — never a
//! duplicate, never a loss.
//!
//! Run it: `cargo run -p isyncyou-engine --example crash_matrix`
//!
//! The fake cloud's `create` is deliberately non-idempotent (a real Graph POST is
//! not idempotent either), so the only thing preventing duplicates is the ledger +
//! marker-probe recovery logic under test. Exits non-zero if the invariant is ever
//! violated.

use isyncyou_engine::{recover_restore_op, run_restore_op, RestoreOutcome, RestoreSink};
use isyncyou_store::{RestoreState, Store};
use std::cell::RefCell;

#[derive(Default)]
struct FakeCloud {
    items: RefCell<Vec<(String, String)>>, // (marker, id)
    seq: RefCell<u32>,
    calls: RefCell<u32>,
}
impl FakeCloud {
    fn count(&self) -> usize {
        self.items.borrow().len()
    }
    fn calls(&self) -> u32 {
        *self.calls.borrow()
    }
}
impl RestoreSink for FakeCloud {
    fn create(&self, marker: &str, _payload: &[u8]) -> isyncyou_engine::RestoreResult<String> {
        *self.calls.borrow_mut() += 1;
        let mut s = self.seq.borrow_mut();
        *s += 1;
        let id = format!("cloud-{}", *s);
        self.items
            .borrow_mut()
            .push((marker.to_string(), id.clone()));
        Ok(id)
    }
    fn find_by_marker(&self, marker: &str) -> isyncyou_engine::RestoreResult<Option<String>> {
        Ok(self
            .items
            .borrow()
            .iter()
            .find(|(m, _)| m == marker)
            .map(|(_, id)| id.clone()))
    }
}

const MARKER: &str = "msgid-evidence";
const PAYLOAD: &[u8] = b"mime";

/// Set up a fresh store + cloud, drive to a crash point, then run the happy path
/// (`recover = false`) or recovery (`recover = true`). Returns (outcome, items, creates).
fn scenario(setup: impl FnOnce(&Store, &FakeCloud), recover: bool) -> (RestoreOutcome, usize, u32) {
    let s = Store::open_in_memory().unwrap();
    let cloud = FakeCloud::default();
    s.create_restore_operation("op", "a", "mail", "src", "key", 1)
        .unwrap();
    setup(&s, &cloud);
    let out = if recover {
        recover_restore_op(&s, "op", PAYLOAD, &cloud, 99).unwrap()
    } else {
        run_restore_op(&s, "op", MARKER, PAYLOAD, &cloud, 99)
            .unwrap()
            .1
    };
    (out, cloud.count(), cloud.calls())
}

fn preflight(s: &Store) {
    s.transition_restore(
        "op",
        RestoreState::PreflightChecked,
        2,
        None,
        None,
        Some(MARKER),
    )
    .unwrap();
}
fn committing(s: &Store) {
    s.transition_restore("op", RestoreState::Committing, 3, None, None, None)
        .unwrap();
}
fn failed(s: &Store) {
    s.transition_restore(
        "op",
        RestoreState::FailedAfterGraphCommit,
        4,
        None,
        None,
        None,
    )
    .unwrap();
}

fn main() {
    let cases: Vec<(&str, (RestoreOutcome, usize, u32))> = vec![
        ("happy path (no crash)", scenario(|_, _| {}, false)),
        (
            "C2 crash after preflight, before POST",
            scenario(|s, _| preflight(s), true),
        ),
        (
            "C3 crash during POST -- landed",
            scenario(
                |s, c| {
                    preflight(s);
                    committing(s);
                    c.create(MARKER, PAYLOAD).unwrap();
                },
                true,
            ),
        ),
        (
            "C3 crash during POST -- not landed",
            scenario(
                |s, _| {
                    preflight(s);
                    committing(s);
                },
                true,
            ),
        ),
        (
            "C4 crash after POST, before record",
            scenario(
                |s, c| {
                    preflight(s);
                    committing(s);
                    c.create(MARKER, PAYLOAD).unwrap();
                    failed(s);
                },
                true,
            ),
        ),
        (
            "C4 failed, not landed -- resume",
            scenario(
                |s, _| {
                    preflight(s);
                    committing(s);
                    failed(s);
                },
                true,
            ),
        ),
    ];

    println!(
        "{:<40} {:<11} {:>6} {:>8}  verdict",
        "crash point", "outcome", "items", "creates"
    );
    println!("{}", "-".repeat(80));
    let mut all_ok = true;
    for (label, (out, items, calls)) in &cases {
        let ok = *items == 1;
        all_ok &= ok;
        println!(
            "{:<40} {:<11} {:>6} {:>8}  {}",
            label,
            format!("{out:?}"),
            items,
            calls,
            if ok { "OK" } else { "DUPLICATE/LOSS!" }
        );
    }
    println!();
    if all_ok {
        println!("ADR-001 invariant holds: exactly one cloud item in every case, no duplicates.");
    } else {
        eprintln!("INVARIANT VIOLATED — a crash point produced a duplicate or a loss.");
        std::process::exit(1);
    }
}

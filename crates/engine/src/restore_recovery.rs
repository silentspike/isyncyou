//! Crash-safe restore orchestration over the operation ledger (ADR-001).
//!
//! These functions drive a cloud restore through the ledger state machine and, after
//! a crash, recover an in-flight operation **without re-creating an item the
//! interrupted Graph call may already have made**. The cloud mutation is abstracted
//! behind [`RestoreSink`] so the danger — a non-idempotent create — and the recovery
//! can be exercised deterministically with a crash injected at each unsafe point
//! (see the tests and the `crash_matrix` example).
//!
//! The safety does not come from the cloud being idempotent (most Graph creates are
//! not); it comes from the ledger plus a marker probe: before (re)creating, recovery
//! asks the cloud *"is the item already there?"* by marker, and only creates if not.

use isyncyou_store::{RestoreState, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreFailureKind {
    Network,
    Timeout,
    Http(u16),
    Authentication,
    Invalid,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreError {
    pub kind: RestoreFailureKind,
    message: String,
}

impl RestoreError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: RestoreFailureKind::Invalid,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: RestoreFailureKind::Internal,
            message: message.into(),
        }
    }

    pub fn authentication() -> Self {
        Self {
            kind: RestoreFailureKind::Authentication,
            message: "authentication required".to_string(),
        }
    }

    pub fn from_graph(error: isyncyou_graph::http::UploadError) -> Self {
        use isyncyou_graph::http::GraphTransientFailure;
        let kind = match error.transient_failure() {
            Some(GraphTransientFailure::Network) => RestoreFailureKind::Network,
            Some(GraphTransientFailure::Timeout) => RestoreFailureKind::Timeout,
            Some(GraphTransientFailure::Http(status)) => RestoreFailureKind::Http(status),
            None => match error {
                isyncyou_graph::http::UploadError::Http { status: 401, .. } => {
                    RestoreFailureKind::Authentication
                }
                isyncyou_graph::http::UploadError::Http { status, .. } => {
                    RestoreFailureKind::Http(status)
                }
                _ => RestoreFailureKind::Internal,
            },
        };
        Self {
            kind,
            message: "Graph cloud restore failed".to_string(),
        }
    }
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RestoreError {}

impl From<String> for RestoreError {
    fn from(message: String) -> Self {
        Self::internal(message)
    }
}

impl From<&str> for RestoreError {
    fn from(message: &str) -> Self {
        Self::internal(message)
    }
}

pub type RestoreResult<T> = Result<T, RestoreError>;

/// The cloud side of a restore, abstracted so recovery can be tested with an injected
/// crash. A real implementation calls Microsoft Graph.
pub trait RestoreSink {
    /// Create the item in the cloud, embedding `marker` so it can later be found.
    /// This models a real `POST`: it is **not** idempotent — calling it twice creates
    /// two items. Safety comes from the ledger + [`RestoreSink::find_by_marker`], not
    /// from here. Returns the new cloud id.
    fn create(&self, marker: &str, payload: &[u8]) -> RestoreResult<String>;

    /// Find an already-created item by its marker; returns its cloud id if present.
    /// This is the probe that makes recovery safe after a crash.
    fn find_by_marker(&self, marker: &str) -> RestoreResult<Option<String>>;
}

/// What driving or recovering one restore operation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// A fresh create landed and was committed.
    Created,
    /// Recovery found the item already in the cloud (the interrupted `POST` had
    /// landed) and committed **without** a second create.
    Reconciled,
    /// The operation was already terminal; nothing to do.
    AlreadyDone,
}

/// Drive a restore that is already recorded as `pending` to completion (the happy
/// path): preflight (stamp the marker), commit-in-flight, create in the cloud, then
/// commit. `now` is unix seconds. Returns the new cloud id and [`RestoreOutcome`].
pub fn run_restore_op<S: RestoreSink>(
    store: &Store,
    op_id: &str,
    marker: &str,
    payload: &[u8],
    sink: &S,
    now: i64,
) -> RestoreResult<(String, RestoreOutcome)> {
    tr_marker(
        store,
        op_id,
        RestoreState::PreflightChecked,
        now,
        "preflight",
        Some(marker),
    )?;
    tr(store, op_id, RestoreState::Committing, now, "committing")?;
    let id = sink.create(marker, payload)?;
    commit(store, op_id, now, "created", &id)?;
    Ok((id, RestoreOutcome::Created))
}

/// Drive a non-terminal operation to a terminal state **without ever creating a
/// duplicate**. The first thing it does is ask the dangerous question — *did the
/// `POST` land?* — by probing the cloud for the operation's marker. If found, it
/// reconciles to `committed` and never creates again; if not, it (re)creates safely.
pub fn recover_restore_op<S: RestoreSink>(
    store: &Store,
    op_id: &str,
    payload: &[u8],
    sink: &S,
    now: i64,
) -> RestoreResult<RestoreOutcome> {
    let op = store
        .get_restore_operation(op_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no restore operation '{op_id}'"))?;
    if op.state.is_terminal() {
        return Ok(RestoreOutcome::AlreadyDone);
    }
    let marker = op
        .marker
        .clone()
        .ok_or_else(|| format!("operation '{op_id}' has no marker to reconcile by"))?;

    // The dangerous question after a crash: did the POST land? Ask by marker first.
    let existing = sink.find_by_marker(&marker)?;

    // Move into `committing` using only legal transitions, from wherever the crash
    // left the operation. When the item already exists we can commit directly from
    // `committing` or `failed_after_graph_commit`, so no resume is needed there.
    match op.state {
        RestoreState::Pending => {
            tr(store, op_id, RestoreState::PreflightChecked, now, "recover")?;
            tr(store, op_id, RestoreState::Committing, now, "recover")?;
        }
        RestoreState::PreflightChecked => {
            tr(store, op_id, RestoreState::Committing, now, "recover")?;
        }
        RestoreState::Committing => {}
        RestoreState::FailedAfterGraphCommit => {
            if existing.is_none() {
                // Not in the cloud — resume for a fresh create.
                tr(store, op_id, RestoreState::Committing, now, "resume")?;
            }
        }
        RestoreState::Committed | RestoreState::Cancelled => {
            unreachable!("terminal states handled above")
        }
    }

    if let Some(id) = existing {
        commit(store, op_id, now, "reconciled: found by marker", &id)?;
        Ok(RestoreOutcome::Reconciled)
    } else {
        let id = sink.create(&marker, payload)?;
        commit(store, op_id, now, "created on recovery", &id)?;
        Ok(RestoreOutcome::Created)
    }
}

fn tr(store: &Store, op_id: &str, to: RestoreState, now: i64, detail: &str) -> RestoreResult<()> {
    store
        .transition_restore(op_id, to, now, Some(detail), None, None)
        .map_err(|e| RestoreError::internal(e.to_string()))
}

fn tr_marker(
    store: &Store,
    op_id: &str,
    to: RestoreState,
    now: i64,
    detail: &str,
    marker: Option<&str>,
) -> RestoreResult<()> {
    store
        .transition_restore(op_id, to, now, Some(detail), None, marker)
        .map_err(|e| RestoreError::internal(e.to_string()))
}

fn commit(store: &Store, op_id: &str, now: i64, detail: &str, new_id: &str) -> RestoreResult<()> {
    store
        .transition_restore(
            op_id,
            RestoreState::Committed,
            now,
            Some(detail),
            Some(new_id),
            None,
        )
        .map_err(|e| RestoreError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A non-idempotent fake cloud: `create` always appends a new item (modelling the
    /// real duplication danger). Safety must therefore come from the recovery logic,
    /// not from this sink.
    #[derive(Default)]
    struct FakeCloud {
        items: RefCell<Vec<(String, String)>>, // (marker, id)
        seq: RefCell<u32>,
        create_calls: RefCell<u32>,
    }
    impl FakeCloud {
        fn count(&self) -> usize {
            self.items.borrow().len()
        }
        fn create_calls(&self) -> u32 {
            *self.create_calls.borrow()
        }
    }
    impl RestoreSink for FakeCloud {
        fn create(&self, marker: &str, _payload: &[u8]) -> RestoreResult<String> {
            *self.create_calls.borrow_mut() += 1;
            let mut seq = self.seq.borrow_mut();
            *seq += 1;
            let id = format!("cloud-{}", *seq);
            self.items
                .borrow_mut()
                .push((marker.to_string(), id.clone()));
            Ok(id)
        }
        fn find_by_marker(&self, marker: &str) -> RestoreResult<Option<String>> {
            Ok(self
                .items
                .borrow()
                .iter()
                .find(|(m, _)| m == marker)
                .map(|(_, id)| id.clone()))
        }
    }

    const MARKER: &str = "msgid-deadbeef";
    const PAYLOAD: &[u8] = b"mime bytes";

    fn new_op(s: &Store, op_id: &str, key: &str) {
        s.create_restore_operation(op_id, "a", "mail", "src", key, 1)
            .unwrap();
    }

    #[test]
    fn happy_path_creates_exactly_one() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        new_op(&s, "op", "k");
        let (_id, out) = run_restore_op(&s, "op", MARKER, PAYLOAD, &cloud, 10).unwrap();
        assert_eq!(out, RestoreOutcome::Created);
        assert_eq!(cloud.count(), 1);
        assert_eq!(
            s.get_restore_operation("op").unwrap().unwrap().state,
            RestoreState::Committed
        );
    }

    #[test]
    fn c2_crash_after_preflight_before_post() {
        // preflight is the durable pre-POST checkpoint: intent + marker are recorded
        // before the Graph call. A crash here means nothing was sent.
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        new_op(&s, "op", "k");
        s.transition_restore(
            "op",
            RestoreState::PreflightChecked,
            2,
            None,
            None,
            Some(MARKER),
        )
        .unwrap();
        // [CRASH] after preflight, before the POST
        let out = recover_restore_op(&s, "op", PAYLOAD, &cloud, 20).unwrap();
        assert_eq!(out, RestoreOutcome::Created); // nothing in cloud -> safe to create
        assert_eq!(cloud.count(), 1);
    }

    #[test]
    fn c3_crash_during_post_landed_no_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        new_op(&s, "op", "k");
        s.transition_restore(
            "op",
            RestoreState::PreflightChecked,
            2,
            None,
            None,
            Some(MARKER),
        )
        .unwrap();
        s.transition_restore("op", RestoreState::Committing, 3, None, None, None)
            .unwrap();
        let _landed = cloud.create(MARKER, PAYLOAD).unwrap(); // the POST that landed
        assert_eq!(cloud.count(), 1);
        // [CRASH] before committing the ledger
        let out = recover_restore_op(&s, "op", PAYLOAD, &cloud, 20).unwrap();
        assert_eq!(out, RestoreOutcome::Reconciled);
        assert_eq!(cloud.count(), 1, "recovery must NOT create a duplicate");
        assert_eq!(cloud.create_calls(), 1);
    }

    #[test]
    fn c3_crash_during_post_not_landed_creates_one() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        new_op(&s, "op", "k");
        s.transition_restore(
            "op",
            RestoreState::PreflightChecked,
            2,
            None,
            None,
            Some(MARKER),
        )
        .unwrap();
        s.transition_restore("op", RestoreState::Committing, 3, None, None, None)
            .unwrap();
        // [CRASH] the POST never happened
        let out = recover_restore_op(&s, "op", PAYLOAD, &cloud, 20).unwrap();
        assert_eq!(out, RestoreOutcome::Created);
        assert_eq!(cloud.count(), 1);
    }

    #[test]
    fn c4_crash_after_post_before_record_no_duplicate() {
        // the textbook case: created, marked failed_after_graph_commit, then crashed
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        new_op(&s, "op", "k");
        s.transition_restore(
            "op",
            RestoreState::PreflightChecked,
            2,
            None,
            None,
            Some(MARKER),
        )
        .unwrap();
        s.transition_restore("op", RestoreState::Committing, 3, None, None, None)
            .unwrap();
        let _ = cloud.create(MARKER, PAYLOAD).unwrap(); // POST landed
        s.transition_restore(
            "op",
            RestoreState::FailedAfterGraphCommit,
            4,
            Some("post sent, outcome unknown"),
            None,
            None,
        )
        .unwrap();
        // [CRASH]
        let out = recover_restore_op(&s, "op", PAYLOAD, &cloud, 20).unwrap();
        assert_eq!(out, RestoreOutcome::Reconciled);
        assert_eq!(cloud.count(), 1);
        assert_eq!(cloud.create_calls(), 1);
    }

    #[test]
    fn c4_failed_not_landed_resumes_one() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        new_op(&s, "op", "k");
        s.transition_restore(
            "op",
            RestoreState::PreflightChecked,
            2,
            None,
            None,
            Some(MARKER),
        )
        .unwrap();
        s.transition_restore("op", RestoreState::Committing, 3, None, None, None)
            .unwrap();
        s.transition_restore(
            "op",
            RestoreState::FailedAfterGraphCommit,
            4,
            None,
            None,
            None,
        )
        .unwrap();
        // [CRASH] the POST did NOT land
        let out = recover_restore_op(&s, "op", PAYLOAD, &cloud, 20).unwrap();
        assert_eq!(out, RestoreOutcome::Created);
        assert_eq!(cloud.count(), 1);
    }

    #[test]
    fn c6_concurrent_identical_key_is_rejected_by_ledger() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        s.create_restore_operation("op1", "a", "mail", "src", "SAME", 1)
            .unwrap();
        let dup = s.create_restore_operation("op2", "a", "mail", "src", "SAME", 1);
        assert!(dup.is_err(), "duplicate idempotency key must be rejected");
        run_restore_op(&s, "op1", MARKER, PAYLOAD, &cloud, 10).unwrap();
        assert_eq!(cloud.count(), 1);
    }

    #[test]
    fn recovery_is_idempotent_when_already_committed() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        new_op(&s, "op", "k");
        run_restore_op(&s, "op", MARKER, PAYLOAD, &cloud, 10).unwrap();
        // running recovery again does nothing and creates nothing
        let out = recover_restore_op(&s, "op", PAYLOAD, &cloud, 30).unwrap();
        assert_eq!(out, RestoreOutcome::AlreadyDone);
        assert_eq!(cloud.count(), 1);
        assert_eq!(cloud.create_calls(), 1);
    }
}

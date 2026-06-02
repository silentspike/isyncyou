//! Periodic reconciler — the authoritative diff (plan §5.2).
//!
//! inotify/eBPF events are only an accelerator; the periodic reconciler is the
//! source of truth. It compares the **last-known** state (from the store) against
//! a **freshly scanned** state and emits the set of changes — independent of
//! whether any filesystem event was observed. The same symmetric diff drives both
//! the local scan (vs. store) and the remote delta (vs. store).
//!
//! Pure and deterministic: the caller supplies both snapshots as `(key,
//! fingerprint)` entries; how a fingerprint is computed (content hash, or
//! `mtime:size`) is opaque to the diff — equal fingerprint means unchanged.

use std::collections::BTreeMap;

/// One entry in a snapshot: a stable `key` (path or remote id) and an opaque
/// `fingerprint` that changes iff the item changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub fingerprint: String,
}

impl Entry {
    pub fn new(key: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        Entry {
            key: key.into(),
            fingerprint: fingerprint.into(),
        }
    }
}

/// A change detected by the reconciler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileChange {
    /// Present in the current scan, absent from the known state.
    Created(String),
    /// Present in both, fingerprint differs.
    Modified(String),
    /// Present in the known state, absent from the current scan.
    Deleted(String),
}

impl ReconcileChange {
    pub fn key(&self) -> &str {
        match self {
            ReconcileChange::Created(k)
            | ReconcileChange::Modified(k)
            | ReconcileChange::Deleted(k) => k,
        }
    }
}

/// Diff `known` (last synced) against `current` (fresh scan). Output is
/// deterministic, sorted by key. Duplicate keys within a side collapse (last
/// fingerprint wins) so a malformed snapshot can't produce duplicate changes.
pub fn reconcile(known: &[Entry], current: &[Entry]) -> Vec<ReconcileChange> {
    let known: BTreeMap<&str, &str> = known
        .iter()
        .map(|e| (e.key.as_str(), e.fingerprint.as_str()))
        .collect();
    let current: BTreeMap<&str, &str> = current
        .iter()
        .map(|e| (e.key.as_str(), e.fingerprint.as_str()))
        .collect();

    let mut out = Vec::new();
    for (key, fp) in &current {
        match known.get(key) {
            None => out.push(ReconcileChange::Created((*key).to_string())),
            Some(known_fp) if known_fp != fp => {
                out.push(ReconcileChange::Modified((*key).to_string()))
            }
            Some(_) => {} // unchanged
        }
    }
    for key in known.keys() {
        if !current.contains_key(key) {
            out.push(ReconcileChange::Deleted((*key).to_string()));
        }
    }
    out.sort_by(|a, b| a.key().cmp(b.key()));
    out
}

#[cfg(test)]
mod tests {
    use super::ReconcileChange::*;
    use super::*;

    fn e(k: &str, f: &str) -> Entry {
        Entry::new(k, f)
    }

    #[test]
    fn first_scan_is_all_created() {
        let out = reconcile(&[], &[e("a", "1"), e("b", "1")]);
        assert_eq!(out, vec![Created("a".into()), Created("b".into())]);
    }

    #[test]
    fn everything_gone_is_all_deleted() {
        let out = reconcile(&[e("a", "1"), e("b", "1")], &[]);
        assert_eq!(out, vec![Deleted("a".into()), Deleted("b".into())]);
    }

    #[test]
    fn unchanged_produces_nothing() {
        let snap = vec![e("a", "h1"), e("b", "h2")];
        assert!(reconcile(&snap, &snap).is_empty());
    }

    #[test]
    fn detects_modification_via_fingerprint() {
        let out = reconcile(&[e("a", "h1")], &[e("a", "h2")]);
        assert_eq!(out, vec![Modified("a".into())]);
    }

    #[test]
    fn mixed_changes_sorted_by_key() {
        let known = vec![e("keep", "1"), e("change", "1"), e("gone", "1")];
        let current = vec![e("keep", "1"), e("change", "2"), e("new", "1")];
        let out = reconcile(&known, &current);
        assert_eq!(
            out,
            vec![
                Modified("change".into()),
                Deleted("gone".into()),
                Created("new".into())
            ]
        );
    }

    #[test]
    fn duplicate_keys_collapse_last_wins() {
        // a malformed scan listing the same key twice must not yield two changes
        let out = reconcile(&[e("a", "h1")], &[e("a", "h1"), e("a", "h2")]);
        assert_eq!(out, vec![Modified("a".into())]);
    }
}

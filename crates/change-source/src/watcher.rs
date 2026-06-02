//! Filesystem-event coalescer (plan §5.2, §23 abraunegg `monitor.d`).
//!
//! Raw inotify events are noisy: editors emit create+modify bursts, atomic saves
//! show up as rename pairs, and the kernel queue can overflow. This module folds a
//! burst of [`RawEvent`]s into a minimal set of [`FsChange`]s:
//!
//! - same-path create/modify/delete bursts collapse to one net effect,
//! - `MovedFrom`/`MovedTo` with the same cookie pair into a `Renamed`,
//! - an unpaired move in/out becomes a `Created`/`Deleted`,
//! - a queue overflow invalidates the buffer and signals a full rescan.
//!
//! Pure and deterministic; the raw inotify fd reader feeds [`Coalescer::push`].

use std::collections::BTreeMap;

/// A raw filesystem event (as delivered by inotify/eBPF).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawEvent {
    Created(String),
    Modified(String),
    Deleted(String),
    MovedFrom {
        cookie: u32,
        path: String,
    },
    MovedTo {
        cookie: u32,
        path: String,
    },
    /// Kernel event queue overflowed (`IN_Q_OVERFLOW`) — events were lost.
    QueueOverflow,
}

/// A coalesced change ready for the sync engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsChange {
    Created(String),
    Modified(String),
    Deleted(String),
    Renamed { from: String, to: String },
}

impl FsChange {
    fn sort_key(&self) -> &str {
        match self {
            FsChange::Created(p) | FsChange::Modified(p) | FsChange::Deleted(p) => p,
            FsChange::Renamed { from, .. } => from,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Created,
    Modified,
    Deleted,
}

#[derive(Default)]
struct MovePair {
    from: Option<String>,
    to: Option<String>,
}

/// Buffers raw events and coalesces them on [`Coalescer::drain`].
#[derive(Default)]
pub struct Coalescer {
    effects: BTreeMap<String, Effect>,
    moves: BTreeMap<u32, MovePair>,
    overflow: bool,
}

impl Coalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a queue overflow was seen — the caller must perform a full rescan
    /// instead of trusting the (now incomplete) event buffer.
    pub fn overflow(&self) -> bool {
        self.overflow
    }

    /// Buffer one raw event.
    pub fn push(&mut self, ev: RawEvent) {
        match ev {
            RawEvent::QueueOverflow => {
                self.overflow = true;
                self.effects.clear();
                self.moves.clear();
            }
            RawEvent::Created(p) => self.apply(p, Effect::Created),
            RawEvent::Modified(p) => self.apply(p, Effect::Modified),
            RawEvent::Deleted(p) => self.apply(p, Effect::Deleted),
            RawEvent::MovedFrom { cookie, path } => {
                self.moves.entry(cookie).or_default().from = Some(path);
            }
            RawEvent::MovedTo { cookie, path } => {
                self.moves.entry(cookie).or_default().to = Some(path);
            }
        }
    }

    fn apply(&mut self, path: String, new: Effect) {
        use Effect::*;
        let merged = match (self.effects.get(&path).copied(), new) {
            (None, e) => Some(e),
            (Some(Created), Modified) => Some(Created), // still a net-new file
            (Some(Created), Deleted) => None,           // created then removed -> nothing
            (Some(Modified), Modified) => Some(Modified),
            (Some(Modified), Deleted) => Some(Deleted),
            (Some(Deleted), Created) => Some(Modified), // replaced in place
            (Some(Deleted), Modified) => Some(Modified),
            (Some(Created), Created) => Some(Created),
            (Some(Deleted), Deleted) => Some(Deleted),
            (Some(Modified), Created) => Some(Modified),
        };
        match merged {
            Some(e) => {
                self.effects.insert(path, e);
            }
            None => {
                self.effects.remove(&path);
            }
        }
    }

    /// Produce the coalesced changes and reset the buffer. Returns empty after an
    /// overflow (the caller is expected to full-rescan; call again afterwards).
    pub fn drain(&mut self) -> Vec<FsChange> {
        if self.overflow {
            self.overflow = false;
            self.effects.clear();
            self.moves.clear();
            return Vec::new();
        }

        let mut out: Vec<FsChange> = Vec::new();
        for (path, effect) in std::mem::take(&mut self.effects) {
            out.push(match effect {
                Effect::Created => FsChange::Created(path),
                Effect::Modified => FsChange::Modified(path),
                Effect::Deleted => FsChange::Deleted(path),
            });
        }
        for (_cookie, mv) in std::mem::take(&mut self.moves) {
            match (mv.from, mv.to) {
                (Some(from), Some(to)) => out.push(FsChange::Renamed { from, to }),
                (Some(from), None) => out.push(FsChange::Deleted(from)), // moved out of tree
                (None, Some(to)) => out.push(FsChange::Created(to)),     // moved into tree
                (None, None) => {}
            }
        }
        out.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::FsChange::*;
    use super::*;

    fn drain(events: Vec<RawEvent>) -> Vec<FsChange> {
        let mut c = Coalescer::new();
        for e in events {
            c.push(e);
        }
        c.drain()
    }

    #[test]
    fn create_then_modify_is_one_create() {
        let out = drain(vec![
            RawEvent::Created("a".into()),
            RawEvent::Modified("a".into()),
        ]);
        assert_eq!(out, vec![Created("a".into())]);
    }

    #[test]
    fn create_then_delete_cancels() {
        let out = drain(vec![
            RawEvent::Created("tmp".into()),
            RawEvent::Deleted("tmp".into()),
        ]);
        assert!(out.is_empty());
    }

    #[test]
    fn modify_burst_collapses() {
        let out = drain(vec![
            RawEvent::Modified("f".into()),
            RawEvent::Modified("f".into()),
            RawEvent::Modified("f".into()),
        ]);
        assert_eq!(out, vec![Modified("f".into())]);
    }

    #[test]
    fn delete_then_recreate_is_modify() {
        let out = drain(vec![
            RawEvent::Deleted("f".into()),
            RawEvent::Created("f".into()),
        ]);
        assert_eq!(out, vec![Modified("f".into())]);
    }

    #[test]
    fn move_pair_becomes_rename() {
        let out = drain(vec![
            RawEvent::MovedFrom {
                cookie: 7,
                path: "old".into(),
            },
            RawEvent::MovedTo {
                cookie: 7,
                path: "new".into(),
            },
        ]);
        assert_eq!(
            out,
            vec![Renamed {
                from: "old".into(),
                to: "new".into()
            }]
        );
    }

    #[test]
    fn unpaired_moves_become_delete_or_create() {
        assert_eq!(
            drain(vec![RawEvent::MovedFrom {
                cookie: 1,
                path: "gone".into()
            }]),
            vec![Deleted("gone".into())]
        );
        assert_eq!(
            drain(vec![RawEvent::MovedTo {
                cookie: 2,
                path: "arrived".into()
            }]),
            vec![Created("arrived".into())]
        );
    }

    #[test]
    fn overflow_clears_and_signals_rescan() {
        let mut c = Coalescer::new();
        c.push(RawEvent::Modified("a".into()));
        c.push(RawEvent::QueueOverflow);
        c.push(RawEvent::Created("b".into())); // buffered after overflow, but invalid
        assert!(c.overflow());
        assert!(c.drain().is_empty()); // caller must full-rescan
        assert!(!c.overflow()); // flag cleared for the next window
    }

    #[test]
    fn output_is_sorted_and_independent_paths_kept() {
        let out = drain(vec![
            RawEvent::Modified("z".into()),
            RawEvent::Created("a".into()),
            RawEvent::Deleted("m".into()),
        ]);
        assert_eq!(
            out,
            vec![
                Created("a".into()),
                Deleted("m".into()),
                Modified("z".into())
            ]
        );
    }
}

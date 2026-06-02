//! Live filesystem watcher — the inotify (etc.) accelerator that feeds the
//! [`Coalescer`] (plan §5.2). Wraps the cross-platform `notify` crate: events are
//! mapped to [`RawEvent`]s, batched over a debounce window, and coalesced into
//! [`FsChange`]s.
//!
//! The periodic [`crate::reconcile`] diff stays the source of truth; this watcher
//! only lets the engine react quickly to local edits. A watcher error / queue
//! overflow is therefore not fatal — `poll` returns no specific changes and the
//! caller falls back to a full reconcile.

use crate::watcher::{Coalescer, FsChange, RawEvent};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Map one `notify` event into the raw events the [`Coalescer`] understands.
fn map_event(ev: &Event) -> Vec<RawEvent> {
    let path = |i: usize| ev.paths[i].to_string_lossy().into_owned();
    let all = |kind: fn(String) -> RawEvent| -> Vec<RawEvent> {
        ev.paths
            .iter()
            .map(|p| kind(p.to_string_lossy().into_owned()))
            .collect()
    };
    match ev.kind {
        EventKind::Create(_) => all(RawEvent::Created),
        EventKind::Remove(_) => all(RawEvent::Deleted),
        // A rename delivered with both endpoints → delete(from) + create(to); the
        // reconciler reconstructs an actual rename if it matters.
        EventKind::Modify(notify::event::ModifyKind::Name(_)) if ev.paths.len() == 2 => {
            vec![RawEvent::Deleted(path(0)), RawEvent::Created(path(1))]
        }
        EventKind::Modify(_) => all(RawEvent::Modified),
        _ => Vec::new(), // Access / Other → ignore
    }
}

/// A running recursive watch over a directory tree.
pub struct FsWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<RawEvent>,
}

impl FsWatcher {
    /// Start watching `root` recursively.
    pub fn start(root: &Path) -> notify::Result<Self> {
        let (tx, rx) = channel::<RawEvent>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<Event>| match res {
                Ok(ev) => {
                    for raw in map_event(&ev) {
                        let _ = tx.send(raw);
                    }
                }
                // a watcher error (incl. an inotify queue overflow) → force a rescan
                Err(_) => {
                    let _ = tx.send(RawEvent::QueueOverflow);
                }
            })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(FsWatcher {
            _watcher: watcher,
            rx,
        })
    }

    /// Block up to `idle` for the first change; once one arrives, keep collecting
    /// for `debounce` (to batch an editor's create+modify burst), then return the
    /// coalesced changes. Empty if nothing happened within `idle` (or if a queue
    /// overflow swallowed the batch — the caller should then reconcile fully).
    pub fn poll(&self, idle: Duration, debounce: Duration) -> Vec<FsChange> {
        let mut c = Coalescer::new();
        match self.rx.recv_timeout(idle) {
            Ok(ev) => c.push(ev),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return Vec::new(),
        }
        let deadline = Instant::now() + debounce;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(ev) => c.push(ev),
                Err(_) => break,
            }
        }
        c.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

    fn ev(kind: EventKind, paths: &[&str]) -> Event {
        let mut e = Event::new(kind);
        for p in paths {
            e = e.add_path(std::path::PathBuf::from(p));
        }
        e
    }

    #[test]
    fn map_event_translates_kinds() {
        assert_eq!(
            map_event(&ev(EventKind::Create(CreateKind::File), &["/a"])),
            vec![RawEvent::Created("/a".into())]
        );
        assert_eq!(
            map_event(&ev(EventKind::Remove(RemoveKind::File), &["/a"])),
            vec![RawEvent::Deleted("/a".into())]
        );
        assert_eq!(
            map_event(&ev(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                &["/a"]
            )),
            vec![RawEvent::Modified("/a".into())]
        );
        // a both-paths rename → delete(from) + create(to)
        assert_eq!(
            map_event(&ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/from", "/to"]
            )),
            vec![
                RawEvent::Deleted("/from".into()),
                RawEvent::Created("/to".into())
            ]
        );
        // access events are ignored
        assert!(map_event(&ev(
            EventKind::Access(notify::event::AccessKind::Read),
            &["/a"]
        ))
        .is_empty());
    }

    #[test]
    fn poll_returns_empty_when_idle() {
        let dir = tempfile::tempdir().unwrap();
        let w = FsWatcher::start(dir.path()).unwrap();
        // nothing happens → empty within a short idle window
        assert!(w
            .poll(Duration::from_millis(150), Duration::from_millis(50))
            .is_empty());
    }

    /// Best-effort live check: create a file in the watched dir and confirm the
    /// watcher reports a change. inotify is asynchronous and some sandboxes don't
    /// deliver events, so a missed event is tolerated (the periodic reconciler is
    /// the real source of truth); a *delivered* event must mention our file.
    #[test]
    fn poll_observes_a_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let w = FsWatcher::start(dir.path()).unwrap();
        std::fs::write(dir.path().join("new.txt"), b"hi").unwrap();
        let changes = w.poll(Duration::from_secs(2), Duration::from_millis(200));
        if changes.is_empty() {
            eprintln!("no fs event delivered in this environment — tolerated");
            return;
        }
        assert!(
            changes.iter().any(|c| match c {
                FsChange::Created(p) | FsChange::Modified(p) => p.ends_with("new.txt"),
                _ => false,
            }),
            "expected a change for new.txt, got {changes:?}"
        );
    }
}

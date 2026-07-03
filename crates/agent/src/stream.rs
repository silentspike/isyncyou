//! Token-by-token streaming for a running turn (REQ-AGENT-007).
//!
//! The existing app SSE only emits a `change` ping; the agent needs a real typed stream.
//! [`AgentStreamHub`] keeps one **bounded** channel per `turn_id` carrying the
//! [`StreamEvent`] set (token / tool_call / tool_result / confirmation_required / error /
//! done). The bound provides backpressure; a turn can be cancelled, and closing a turn
//! disconnects its receiver. The HTTP layer subscribes a receiver and forwards events as
//! SSE; the agent loop (on a background thread) emits into the hub.

use crate::provider::StreamEvent;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

struct TurnSink {
    tx: SyncSender<StreamEvent>,
    cancelled: Arc<AtomicBool>,
}

/// A registry of per-turn event streams.
#[derive(Default)]
pub struct AgentStreamHub {
    turns: Mutex<HashMap<String, TurnSink>>,
}

impl AgentStreamHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a turn and return its receiver. `capacity` bounds the buffer (backpressure).
    /// A second open of the same id replaces the first (its receiver disconnects).
    pub fn open(&self, turn_id: &str, capacity: usize) -> Receiver<StreamEvent> {
        let (tx, rx) = sync_channel(capacity.max(1));
        let sink = TurnSink {
            tx,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.turns.lock().unwrap().insert(turn_id.to_string(), sink);
        rx
    }

    /// Emit an event to a turn. Returns `false` if the turn is unknown, cancelled, or its
    /// receiver has gone away (so the loop can stop). Blocks while the bounded buffer is
    /// full (backpressure) until the consumer drains or disconnects.
    pub fn emit(&self, turn_id: &str, event: StreamEvent) -> bool {
        // Clone the sender + flag out of the lock so a blocking send doesn't hold it.
        let (tx, cancelled) = {
            let turns = self.turns.lock().unwrap();
            match turns.get(turn_id) {
                Some(s) if !s.cancelled.load(Ordering::SeqCst) => {
                    (s.tx.clone(), s.cancelled.clone())
                }
                _ => return false,
            }
        };
        if cancelled.load(Ordering::SeqCst) {
            return false;
        }
        tx.send(event).is_ok()
    }

    /// Mark a turn cancelled; the loop observes [`is_cancelled`] and stops.
    pub fn cancel(&self, turn_id: &str) {
        if let Some(sink) = self.turns.lock().unwrap().get(turn_id) {
            sink.cancelled.store(true, Ordering::SeqCst);
        }
    }

    /// Whether a turn has been cancelled (or is unknown).
    pub fn is_cancelled(&self, turn_id: &str) -> bool {
        self.turns
            .lock()
            .unwrap()
            .get(turn_id)
            .map(|s| s.cancelled.load(Ordering::SeqCst))
            .unwrap_or(true)
    }

    /// Close a turn: drop its sender so the receiver sees a clean disconnect.
    pub fn close(&self, turn_id: &str) {
        self.turns.lock().unwrap().remove(turn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_events_in_order_then_disconnects_on_close() {
        let hub = AgentStreamHub::new();
        let rx = hub.open("t1", 16);
        assert!(hub.emit("t1", StreamEvent::Token("he".into())));
        assert!(hub.emit("t1", StreamEvent::Token("llo".into())));
        assert!(hub.emit("t1", StreamEvent::Done));
        match rx.recv().unwrap() {
            StreamEvent::Token(t) => assert_eq!(t, "he"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(rx.recv().unwrap(), StreamEvent::Token(t) if t == "llo"));
        assert!(matches!(rx.recv().unwrap(), StreamEvent::Done));
        hub.close("t1");
        assert!(rx.recv().is_err()); // sender dropped → disconnected
    }

    #[test]
    fn emit_to_unknown_turn_returns_false() {
        let hub = AgentStreamHub::new();
        assert!(!hub.emit("nope", StreamEvent::Done));
    }

    #[test]
    fn cancel_stops_emission() {
        let hub = AgentStreamHub::new();
        let _rx = hub.open("t1", 16);
        assert!(!hub.is_cancelled("t1"));
        hub.cancel("t1");
        assert!(hub.is_cancelled("t1"));
        assert!(
            !hub.emit("t1", StreamEvent::Token("x".into())),
            "no emit after cancel"
        );
    }

    #[test]
    fn emit_returns_false_when_receiver_dropped() {
        let hub = AgentStreamHub::new();
        let rx = hub.open("t1", 16);
        drop(rx);
        assert!(!hub.emit("t1", StreamEvent::Done));
    }
}

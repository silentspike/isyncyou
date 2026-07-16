//! Token-by-token streaming for a running turn (REQ-AGENT-007).
//!
//! The existing app SSE only emits a `change` ping; the agent needs a real typed stream.
//! [`AgentStreamHub`] keeps one **bounded** channel per `turn_id` carrying the
//! [`StreamEvent`] set (token / tool_call / tool_result / confirmation_required / error /
//! done). The bound provides backpressure; a turn can be cancelled, and closing a turn
//! disconnects its receiver. The HTTP layer subscribes a receiver and forwards events as
//! SSE; the agent loop (on a background thread) emits into the hub.

#[cfg(test)]
use crate::provider::DoneReason;
use crate::provider::StreamEvent;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_EMIT_TIMEOUT: Duration = Duration::from_secs(5);
const EMIT_RETRY_SLEEP: Duration = Duration::from_millis(2);

struct TurnSink {
    tx: SyncSender<StreamEvent>,
    cancelled: Arc<AtomicBool>,
}

/// Shared cancellation state transferred with a running turn.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
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

    /// Emit an event to a turn. Returns `false` if the turn is unknown, cancelled, full
    /// beyond the default timeout, or its receiver has gone away (so the loop can stop).
    pub fn emit(&self, turn_id: &str, event: StreamEvent) -> bool {
        self.emit_timeout(turn_id, event, DEFAULT_EMIT_TIMEOUT)
    }

    /// Emit an event without allowing a slow consumer to stall the turn forever.
    ///
    /// The queue remains bounded; a full queue is retried until `timeout` elapses. On
    /// timeout, the turn is marked cancelled so later emits stop quickly.
    pub fn emit_timeout(&self, turn_id: &str, event: StreamEvent, timeout: Duration) -> bool {
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
        let deadline = Instant::now() + timeout;
        let mut event = event;
        loop {
            if cancelled.load(Ordering::SeqCst) {
                return false;
            }
            match tx.try_send(event) {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_event)) => return false,
                Err(TrySendError::Full(returned)) => {
                    event = returned;
                    if Instant::now() >= deadline {
                        cancelled.store(true, Ordering::SeqCst);
                        return false;
                    }
                    std::thread::sleep(EMIT_RETRY_SLEEP.min(timeout));
                }
            }
        }
    }

    /// Mark a turn cancelled. The host persists the cancelled terminal state before
    /// emitting `done(cancelled)`; this method deliberately emits no event itself.
    pub fn cancel(&self, turn_id: &str) -> bool {
        let token = self.cancellation_token(turn_id);
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub fn cancellation_token(&self, turn_id: &str) -> Option<CancellationToken> {
        self.turns
            .lock()
            .unwrap()
            .get(turn_id)
            .map(|sink| CancellationToken(Arc::clone(&sink.cancelled)))
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
        assert!(hub.emit("t1", StreamEvent::done(DoneReason::Complete)));
        match rx.recv().unwrap() {
            StreamEvent::Token(t) => assert_eq!(t, "he"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(rx.recv().unwrap(), StreamEvent::Token(t) if t == "llo"));
        assert!(matches!(
            rx.recv().unwrap(),
            StreamEvent::Done {
                reason: DoneReason::Complete
            }
        ));
        hub.close("t1");
        assert!(rx.recv().is_err()); // sender dropped → disconnected
    }

    #[test]
    fn cancellation_only_marks_state_and_never_emits_terminal_event() {
        let hub = AgentStreamHub::new();
        let rx = hub.open("cancel-order", 4);
        assert!(hub.cancel("cancel-order"));
        assert!(hub.is_cancelled("cancel-order"));
        assert!(rx.try_recv().is_err(), "the host owns terminal emission");
        assert!(!hub.cancel("missing"));
    }

    #[test]
    fn emit_to_unknown_turn_returns_false() {
        let hub = AgentStreamHub::new();
        assert!(!hub.emit("nope", StreamEvent::done(DoneReason::Complete)));
    }

    #[test]
    fn cancel_stops_emission() {
        let hub = AgentStreamHub::new();
        let rx = hub.open("t1", 16);
        assert!(!hub.is_cancelled("t1"));
        hub.cancel("t1");
        assert!(hub.is_cancelled("t1"));
        assert!(rx.try_recv().is_err(), "the host owns terminal emission");
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
        assert!(!hub.emit("t1", StreamEvent::done(DoneReason::Complete)));
    }

    #[test]
    fn agent_stream_hub_slow_consumer_times_out_without_unbounded_buffer() {
        let hub = AgentStreamHub::new();
        let _rx = hub.open("t1", 1);
        assert!(hub.emit_timeout(
            "t1",
            StreamEvent::Token("first".into()),
            Duration::from_millis(50)
        ));
        let start = Instant::now();
        assert!(!hub.emit_timeout(
            "t1",
            StreamEvent::Token("second".into()),
            Duration::from_millis(25)
        ));
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "bounded queue timeout should return promptly, elapsed={:?}",
            start.elapsed()
        );
        assert!(hub.is_cancelled("t1"));
    }

    #[test]
    fn agent_stream_hub_emits_typed_events_and_cancels() {
        let hub = AgentStreamHub::new();
        let rx = hub.open("t1", 4);
        assert!(hub.emit(
            "t1",
            StreamEvent::ToolCall {
                id: "tool-1".into(),
                name: "isyncyou".into(),
                input: serde_json::json!({"op": "search"}),
            },
        ));
        hub.cancel("t1");
        assert!(matches!(
            rx.recv().unwrap(),
            StreamEvent::ToolCall { ref id, .. } if id == "tool-1"
        ));
        assert!(rx.try_recv().is_err(), "cancel adds no terminal event");
        assert!(!hub.emit("t1", StreamEvent::Token("late".into())));
    }
}

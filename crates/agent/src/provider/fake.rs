//! A deterministic, scripted provider for tests and CI. It carries **no** network code
//! and uses **no** real LLM token (REQ-AGENT-008), so the whole turn/tool/stream
//! machinery can be exercised in CI.

use super::{AssistantBlock, LlmProvider, StreamEvent};

/// Replays a fixed script: each call to [`LlmProvider::next`] pops the next response
/// (a list of [`AssistantBlock`]s). Text blocks are streamed token-by-token so the
/// loop's streaming path is exercised.
pub struct FakeProvider {
    script: std::collections::VecDeque<Vec<AssistantBlock>>,
}

impl FakeProvider {
    /// Build a fake provider from a list of scripted responses (one per `next` call).
    pub fn new(script: Vec<Vec<AssistantBlock>>) -> Self {
        Self {
            script: script.into_iter().collect(),
        }
    }
}

impl LlmProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn next(
        &mut self,
        _history: &[crate::turn::Message],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<Vec<AssistantBlock>, crate::AgentError> {
        let blocks = self.script.pop_front().unwrap_or_default();
        for b in &blocks {
            if let AssistantBlock::Text(t) = b {
                for tok in t.split_inclusive(' ') {
                    emit(StreamEvent::Token(tok.to_string()));
                }
            }
        }
        Ok(blocks)
    }
}

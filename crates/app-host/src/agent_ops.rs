use std::sync::Arc;

use isyncyou_core::Config;

use crate::{AgentConfirmedActionExecutor, ConfirmedActionResult};

/// Explicitly separates desktop Agent operation execution from the shared mobile router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOperationPolicy {
    DesktopEnabled,
    MobileDisabled,
}

pub(crate) fn confirmed_executor_for_policy(
    policy: AgentOperationPolicy,
    cfg: Config,
) -> Arc<dyn AgentConfirmedActionExecutor> {
    match policy {
        AgentOperationPolicy::DesktopEnabled => Arc::new(DesktopAgentOperations::new(cfg)),
        AgentOperationPolicy::MobileDisabled => Arc::new(MobileDisabledAgentOperations),
    }
}

/// Desktop operation executor. Later #624 tasks fill in the individual operation
/// dispatches; Task 1 only makes the desktop/mobile policy explicit.
pub(crate) struct DesktopAgentOperations {
    _cfg: Config,
}

impl DesktopAgentOperations {
    pub(crate) fn new(cfg: Config) -> Self {
        Self { _cfg: cfg }
    }
}

impl AgentConfirmedActionExecutor for DesktopAgentOperations {
    fn execute_confirmed(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        Err(format!(
            "not_implemented: confirmed agent action '{}' lands in S-AG.9/#624",
            action.op()
        ))
    }
}

pub(crate) struct MobileDisabledAgentOperations;

impl AgentConfirmedActionExecutor for MobileDisabledAgentOperations {
    fn execute_confirmed(
        &self,
        _action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        Err("not_available_on_mobile: mobile_agent_operations_land_in_625_626".to_string())
    }
}

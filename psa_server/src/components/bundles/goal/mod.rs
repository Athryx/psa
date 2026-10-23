//! Per-agent goal mode. State is persisted in the agent's inherited event trace.
mod state;

pub(crate) use state::GoalOperation;
pub use state::{GoalHandle, GoalState, GoalStatus, load_goal_state};

use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::{
    agents::Agent,
    components::{
        hooks::{HookKind, goal::GoalHook},
        tools::{
            ToolKind,
            goal::{SetGoal, SetGoalStatus},
        },
    },
    sessions::Session,
};

/// A retained owner, not cloned when workers retire or agents fork.
pub struct GoalBundle {
    runtime: state::Runtime,
}

impl Default for GoalBundle {
    fn default() -> Self {
        Self::new()
    }
}

impl GoalBundle {
    pub fn new() -> Self {
        Self {
            runtime: state::Runtime::new(),
        }
    }
}

impl Bundle for GoalBundle {
    fn install(
        &mut self,
        agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        if !agent.interactive {
            return Ok(());
        }
        let handle = self.runtime.handle();
        installer.add_tool(ToolKind::SetGoal(SetGoal::with_handle(handle.clone())))?;
        installer.add_tool(ToolKind::SetGoalStatus(SetGoalStatus::with_handle(
            handle.clone(),
        )))?;
        installer.add_hook(
            HookStage::AfterRequest,
            HookKind::Goal(GoalHook::with_handle(handle)),
        );
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.runtime.shutdown().await;
        Ok(())
    }
}

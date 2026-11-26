//! Per-agent goal mode. State is persisted in the agent's inherited event trace.
mod state;

pub(crate) use state::GoalOperation;
pub use state::{GoalHandle, GoalState, GoalStatus, load_goal_state};

use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::{
    hooks::{StopWithoutTools, goal::GoalHook},
    tools::goal::{SetGoal, SetGoalStatus},
};
use psa_agents::Agent;
use psa_store::Session;

/// A retained owner, not cloned when workers retire or agents fork.
pub struct GoalBundle {
    interactive_only: bool,
    runtime: state::Runtime,
}

impl Default for GoalBundle {
    fn default() -> Self {
        Self::new(true)
    }
}

impl GoalBundle {
    pub fn new(interactive_only: bool) -> Self {
        Self {
            interactive_only,
            runtime: state::Runtime::new(),
        }
    }
}

impl Bundle for GoalBundle {
    fn label(&self) -> String {
        "Goal".into()
    }

    fn install(
        &mut self,
        agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        if self.interactive_only && !agent.interactive {
            installer.add_hook(HookStage::AfterRequest, StopWithoutTools);
            return Ok(());
        }
        let handle = self.runtime.handle();
        installer.add_tool(SetGoal::with_handle(handle.clone()))?;
        installer.add_tool(SetGoalStatus::with_handle(handle.clone()))?;
        let hook = GoalHook::with_handle(handle);
        installer.add_command_hook(hook.clone());
        installer.add_hook(HookStage::AfterRequest, hook);
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.runtime.shutdown().await;
        Ok(())
    }
}

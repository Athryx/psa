use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::hooks::ExtraSystemPromptHook;
use psa_agents::Agent;
use psa_store::Session;

/// Adds a configured system message when an agent is created. Owns no resources.
pub struct ExtraSystemPromptBundle {
    prompt: String,
}

impl ExtraSystemPromptBundle {
    pub fn new(prompt: String) -> Self {
        Self { prompt }
    }
}

impl Bundle for ExtraSystemPromptBundle {
    fn label(&self) -> String {
        "ExtraSystemPrompt".into()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_hook(
            HookStage::AgentCreate,
            ExtraSystemPromptHook::new(self.prompt.clone()),
        );
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

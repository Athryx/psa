use super::{Bundle, CleanupError, InstallError, Installer};
use crate::codemode_functions::ToolFunction;
use crate::tools::AskQuestion;
use psa_agents::Agent;
use psa_store::Session;

/// Installation policy only; querying clients does not require interactivity.
pub struct AskQuestionBundle {
    interactive_only: bool,
}

impl AskQuestionBundle {
    pub fn new(interactive_only: bool) -> Self {
        Self { interactive_only }
    }
}

impl Bundle for AskQuestionBundle {
    fn label(&self) -> String {
        "AskQuestion".into()
    }

    fn install(
        &mut self,
        agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        if self.interactive_only && !agent.interactive {
            return Ok(());
        }
        installer.add_tool(AskQuestion)?;
        installer.add_codemode_function(ToolFunction::new(AskQuestion))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

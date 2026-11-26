use super::{Bundle, CleanupError, InstallError, Installer};
use crate::codemode_functions::ToolFunction;
use crate::tools::{ExportTrace, ListSessions};
use psa_agents::Agent;
use psa_store::Session;

/// Installs session listing and trace export as CodeMode-only functions,
/// documented through `help` rather than the system prompt. Owns no resources.
#[derive(Default)]
pub struct TracesBundle;

impl Bundle for TracesBundle {
    fn label(&self) -> String {
        "Traces".into()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_help_only_codemode_function(ToolFunction::new(ListSessions))?;
        installer.add_help_only_codemode_function(ToolFunction::new(ExportTrace))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

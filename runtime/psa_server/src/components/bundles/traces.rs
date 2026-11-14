use super::{Bundle, CleanupError, InstallError, Installer};
use crate::{
    agents::Agent,
    components::{
        codemode_functions::CodeModeFunctionKind,
        tools::{ExportTrace, ListSessions, ToolKind},
    },
    sessions::Session,
};

/// Installs session listing and trace export as CodeMode-only functions,
/// documented through `help` rather than the system prompt. Owns no resources.
#[derive(Default)]
pub struct TracesBundle;

impl Bundle for TracesBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_help_only_codemode_function(CodeModeFunctionKind::Tool(
            ToolKind::ListSessions(ListSessions),
        ))?;
        installer.add_help_only_codemode_function(CodeModeFunctionKind::Tool(
            ToolKind::ExportTrace(ExportTrace),
        ))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

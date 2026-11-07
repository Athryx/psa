use super::{Bundle, CleanupError, InstallError, Installer};
use crate::{
    agents::Agent,
    components::{
        codemode_functions::CodeModeFunctionKind,
        tools::{Noop, ToolKind},
    },
    sessions::Session,
};

/// Installs the no-operation tool without owning any runtime resources.
#[derive(Default)]
pub struct NoopBundle;

impl Bundle for NoopBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_tool(ToolKind::Noop(Noop))?;
        installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

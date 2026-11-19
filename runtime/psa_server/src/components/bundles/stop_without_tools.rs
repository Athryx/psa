use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::{
    agents::Agent,
    components::hooks::{HookKind, StopWithoutTools},
    sessions::Session,
};

/// Installs the ordinary completion policy: stop after a response without tool
/// calls. Owns no resources.
pub struct StopWithoutToolsBundle;

impl Bundle for StopWithoutToolsBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_hook(
            HookStage::AfterRequest,
            HookKind::StopWithoutTools(StopWithoutTools),
        );
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

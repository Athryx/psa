use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::hooks::StopWithoutTools;
use psa_agents::Agent;
use psa_store::Session;

/// Installs the ordinary completion policy: stop after a response without tool
/// calls. Owns no resources.
pub struct StopWithoutToolsBundle;

impl Bundle for StopWithoutToolsBundle {
    fn label(&self) -> String {
        "StopWithoutTools".into()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_hook(HookStage::AfterRequest, StopWithoutTools);
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

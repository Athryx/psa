use super::{Bundle, CleanupError, InstallError, Installer};
use crate::{
    agents::Agent,
    components::tools::{Edit, Read, Tool, ToolKind, Write},
    sessions::Session,
};

/// File mutation synchronization remains in the existing shared file operations
/// implementation; it must not become a per-worker lock.
#[derive(Default)]
pub struct FileToolsBundle;

impl FileToolsBundle {
    pub fn new() -> Self {
        Self
    }
}

impl Bundle for FileToolsBundle {
    fn install(
        &mut self,
        agent: &Agent,
        session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_tool(ToolKind::Read(Read::new(agent, session)))?;
        installer.add_tool(ToolKind::Edit(Edit::new(agent, session)))?;
        installer.add_tool(ToolKind::Write(Write::new(agent, session)))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

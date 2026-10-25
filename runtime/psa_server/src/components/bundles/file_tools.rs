use super::{Bundle, CleanupError, InstallError, Installer};
use crate::{
    agents::Agent,
    components::tools::{Edit, Read, ToolKind, Write},
    sessions::Session,
};

/// File mutation synchronization remains in the existing shared file operations
/// implementation; it must not become a per-worker lock.
#[derive(Default)]
pub struct FileToolsBundle {
    read_only: bool,
}

impl FileToolsBundle {
    pub fn new(read_only: bool) -> Self {
        Self { read_only }
    }
}

impl Bundle for FileToolsBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_tool(ToolKind::Read(Read))?;
        if self.read_only {
            return Ok(());
        }

        installer.add_tool(ToolKind::Edit(Edit))?;
        installer.add_tool(ToolKind::Write(Write))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

use super::{Bundle, CleanupError, InstallError, Installer};
use crate::codemode_functions::ToolFunction;
use crate::tools::{Edit, Read, Write};
use psa_agents::Agent;
use psa_store::Session;

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
    fn label(&self) -> String {
        "FileTools".into()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_tool(Read)?;
        installer.add_codemode_function(ToolFunction::new(Read))?;
        if self.read_only {
            return Ok(());
        }

        installer.add_tool(Edit)?;
        installer.add_tool(Write)?;
        installer.add_codemode_function(ToolFunction::new(Edit))?;
        installer.add_codemode_function(ToolFunction::new(Write))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

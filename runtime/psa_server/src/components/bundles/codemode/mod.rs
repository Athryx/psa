//! Per-agent owner for the isolated code execution tool.

pub(crate) mod engine;

use std::sync::Arc;

use super::{Bundle, CleanupError, InstallError, Installer};
use crate::{
    agents::Agent,
    components::tools::{CodeMode, ToolKind},
    sessions::Session,
};
pub(crate) use engine::MontyEngine;

pub struct CodeModeBundle {
    engine: Arc<MontyEngine>,
}

impl Default for CodeModeBundle {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeModeBundle {
    pub fn new() -> Self {
        let binary = std::env::var_os("PSA_MONTY_WORKER").unwrap_or_else(|| "monty".into());
        Self {
            engine: Arc::new(MontyEngine::new(binary)),
        }
    }
}

impl Drop for CodeModeBundle {
    fn drop(&mut self) {
        self.engine.shutdown();
    }
}

impl Bundle for CodeModeBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_tool(ToolKind::CodeMode(CodeMode::new(self.engine.clone())))
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.engine.close().await;
        Ok(())
    }
}

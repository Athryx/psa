//! Probe-backed persistent bash sessions. No server environment or filesystem is used.
//!
//! The probe supervises process groups and kills remaining descendants when the
//! leader exits. Unlike the Pi extension's 100ms post-exit idle timer, it cannot
//! preserve actively writing descendants after leader exit with the current API.
mod log;
mod session;
mod state;

use std::{path::PathBuf, time::Duration};

use super::{Bundle, CleanupError, InstallError, Installer};
use crate::{
    agents::Agent,
    components::{
        cancellation::Cancellation,
        codemode_functions::CodeModeFunctionKind,
        tools::{self, ToolKind},
    },
    probes::ProbeHandle,
    sessions::Session,
};

pub use state::BashHandle;

#[derive(Debug, thiserror::Error)]
pub enum BashError {
    #[error("Unknown bash session: {0}")]
    UnknownSession(String),
    #[error("Bash session {0} is already being polled")]
    Busy(String),
    #[error("Bash session {0} is not accepting stdin")]
    StdinClosed(String),
    #[error("Session name cannot be empty")]
    InvalidName,
    #[error("Bash session {0} already exists")]
    DuplicateName(String),
    #[error("Command interrupted")]
    Interrupted,
    #[error("Probe error: {0}")]
    Probe(String),
    #[error("{0}")]
    Other(String),
    #[error("Bash runtime is unavailable")]
    Unavailable,
}

impl From<psa_shared::probe_transport::ProbeError> for BashError {
    fn from(error: psa_shared::probe_transport::ProbeError) -> Self {
        Self::Probe(error.to_string())
    }
}

#[derive(Debug)]
pub struct CollectedOutput {
    pub name: String,
    /// None once the log was deleted after a clean foreground run.
    pub log_path: Option<PathBuf>,
    pub(crate) output: tools::bash::output::OutputSnapshot,
    pub elapsed: Duration,
    pub waited: Duration,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// This call (close_bash) killed the process.
    pub stopped: bool,
    /// The command had ended before this call; any output is what was left unread.
    pub already_finished: bool,
}

impl CollectedOutput {
    pub fn output_text(&self) -> &str {
        &self.output.content
    }
    pub fn was_truncated(&self) -> bool {
        self.output.truncation.truncated
    }
}

/// The owner is deliberately not Clone; installed tools share only its mailbox.
pub struct BashBundle {
    runtime: state::Runtime,
}

impl Default for BashBundle {
    fn default() -> Self {
        Self::new()
    }
}

impl BashBundle {
    pub fn new() -> Self {
        Self {
            runtime: state::Runtime::new(),
        }
    }

    pub fn handle(&self) -> BashHandle {
        self.runtime.handle()
    }
}

impl Bundle for BashBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        let handle = self.runtime.handle();
        installer.add_tool(ToolKind::Bash(tools::Bash::with_handle(handle.clone())))?;
        installer.add_tool(ToolKind::InteractBash(tools::InteractBash::with_handle(
            handle.clone(),
        )))?;
        installer.add_tool(ToolKind::CloseBash(tools::CloseBash::with_handle(
            handle.clone(),
        )))?;
        installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::Bash(
            tools::Bash::with_handle(handle.clone()),
        )))?;
        installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::InteractBash(
            tools::InteractBash::with_handle(handle.clone()),
        )))?;
        installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::CloseBash(
            tools::CloseBash::with_handle(handle),
        )))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.runtime.shutdown().await
    }

    fn cleanup_timeout(&self) -> Duration {
        Duration::from_secs(10)
    }
}

// Imported here so the backend's interface remains visible to sibling modules.
use self::session::Collect;

#[cfg(test)]
mod tests;

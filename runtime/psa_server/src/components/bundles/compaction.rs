//! Context compaction: automatic before requests and on the `Compact` command.
use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::{
    agents::Agent,
    components::hooks::{CommandHookKind, HookKind, compaction::CompactionHook},
};
use psa_store::Session;

/// Owns one compaction hook, shared by both installations so they reuse the
/// same summarizer bookkeeping.
pub struct CompactionBundle {
    hook: CompactionHook,
}

impl CompactionBundle {
    pub fn new(reserve_tokens: u64, keep_recent_tokens: u64, context_window: Option<u64>) -> Self {
        Self {
            hook: CompactionHook::new(reserve_tokens, keep_recent_tokens, context_window),
        }
    }
}

impl Bundle for CompactionBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        // Context replacement must happen between completed tool batches and inference.
        installer.add_hook(
            HookStage::BeforeRequest,
            HookKind::Compaction(self.hook.clone()),
        );
        installer.add_command_hook(CommandHookKind::Compaction(self.hook.clone()));
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

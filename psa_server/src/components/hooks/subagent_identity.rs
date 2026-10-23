//! Invocation identity is coordinator metadata, not Agent.parent_id fork lineage.
use super::{Hook, HookError};
use crate::{
    agents::{Agent, agent_context::AgentContext, session_manager::RuntimeError},
    components::{bundles::subagents::SubagentCoordinator, cancellation::Cancellation},
    sessions::Session,
};
use psa_api::events::{EventType, Message, MessageType, NewEvent};
use std::time::Duration;

#[derive(Default)]
pub struct SubagentIdentity {
    handle: Option<SubagentCoordinator>,
    emitted_identity: Option<String>,
}
impl SubagentIdentity {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self {
            handle: Some(handle),
            emitted_identity: None,
        }
    }
}
impl Hook for SubagentIdentity {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self::default()
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let handle = self.handle.as_ref().ok_or_else(|| {
            RuntimeError::Invalid("Subagent identity requires session bundle resources".into())
        })?;

        let identity = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = handle.identity(context) => result.map_err(RuntimeError::Invalid)?,
        };

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = handle.checkpoint(context) => result.map_err(RuntimeError::Invalid)?,
        }

        if self.emitted_identity.as_ref() == Some(&identity) {
            return Ok(());
        }

        let events = [NewEvent::new(EventType::Message(Message::text(
            MessageType::System,
            identity.clone(),
        )))];
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = context.save_events(&events) => { result?; },
        }

        self.emitted_identity = Some(identity);
        Ok(())
    }
}

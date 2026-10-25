//! Identity uses core agent ancestry, independently of fork lineage.
use super::{Hook, HookError};
use crate::{
    agents::{agent_context::AgentContext, session_manager::RuntimeError},
    components::cancellation::Cancellation,
};
use psa_api::events::{EventType, Message, MessageType, NewEvent};
use std::time::Duration;

#[derive(Default)]
pub struct SubagentIdentity {
    emitted_identity: Option<String>,
}
impl SubagentIdentity {
    async fn build_identity(context: &AgentContext<'_>) -> Result<String, String> {
        let agent = context.agent();
        let mut permitted = Vec::new();
        for id in &context.definition().permitted_subagents {
            let definition = context
                .sessions()
                .get_agent_definition(*id)
                .await
                .map_err(|error| error.to_string())?;
            let snapshot = context
                .sessions()
                .get_resolved_agent_config(*id)
                .await
                .map_err(|error| error.to_string())?;
            let (models, default) = match snapshot {
                Some(snapshot) => (snapshot.config.models, snapshot.config.default),
                None => (Vec::new(), true),
            };
            permitted.push(
                serde_json::json!({"name": definition.name, "models": models, "default": default}),
            );
        }
        Ok(format!(
            "<agent_identity>\nName: {}. Agent ID: {}. Session ID: {}. Subagent parent ID: {:?}. Template: {}.\nPermitted child templates (explicit agent_template required): {}\nAgents communicate within this session. Children run asynchronously; their final answers/errors are sent to their parent. send_message steers an existing agent. wait_message waits for steering, which the runtime injects into context.\n</agent_identity>",
            agent.name,
            agent.id.0,
            context.session().id.0,
            agent.subagent_parent_id.map(|id| id.0),
            context.definition().name,
            serde_json::to_string(&permitted).unwrap()
        ))
    }
}
impl Hook for SubagentIdentity {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let identity = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = Self::build_identity(context) => result.map_err(RuntimeError::Invalid)?,
        };

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

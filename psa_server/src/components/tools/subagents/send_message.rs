use super::{format_tool_output, require_coordinator};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    components::{
        bundles::subagents::SubagentCoordinator,
        cancellation::Cancellation,
        tools::{Tool, ToolOutput, typed_tool_execute},
    },
    prompts,
    sessions::Session,
};
use llm_api::api::ToolDefinition;
use psa_api::AgentMessageSource;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendMessageArgs {
    agent_name: String,
    message: String,
}
#[derive(Default)]
pub struct SendMessage {
    handle: Option<SubagentCoordinator>,
}
impl SendMessage {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self {
            handle: Some(handle),
        }
    }
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<SendMessageArgs>(
            "send_message",
            "Send attributed steering context to another agent in this session. Resumes idle agents.",
            Some(false),
        )
    }
    async fn execute_typed(
        &self,
        args: SendMessageArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = match require_coordinator(&self.handle, context) {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let SendMessageArgs {
            agent_name: name,
            message,
        } = args;
        if message.trim().is_empty() {
            return ToolOutput::error(prompts::text("message must be nonempty"));
        }

        let caller = context.agent().id;
        let operation = async {
            let _guard = handle.begin_operation()?;
            let agents = context
                .sessions()
                .list_agents(context.session().id)
                .await
                .map_err(|error| error.to_string())?;
            let mut matches = agents.iter().filter(|agent| agent.name == name);
            let target = matches
                .next()
                .ok_or_else(|| format!("Unknown agent in session: {name}"))?;
            if matches.next().is_some() {
                return Err(format!("Ambiguous agent name in session: {name}"));
            }
            if target.id == caller {
                return Err("Self-messaging is not supported".into());
            }

            let accepted = context
                .sessions()
                .send_subagent_message(
                    context.session().id,
                    target.id,
                    Some(AgentMessageSource {
                        agent_id: Some(caller),
                        name: None,
                    }),
                    crate::prompts::text(message),
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(json!({"status": "queued", "request_id": accepted.request_id}))
        };
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err("Message send cancelled".into()),
            result = operation => result,
        };
        format_tool_output(result)
    }
}
impl Tool for SendMessage {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self::default()
    }
    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    typed_tool_execute!(SendMessageArgs, execute_typed);
}
#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;
    #[test]
    fn rejects_missing_extra_and_wrong_type() {
        assert!(
            parse_tool_arguments::<SendMessageArgs>(json!({"agent_name":"peer","message":"hi"}))
                .is_ok()
        );
        for value in [
            json!({"agent_name":"peer"}),
            json!({"agent_name":1,"message":"hi"}),
            json!({"agent_name":"peer","message":"hi","extra":0}),
        ] {
            assert!(parse_tool_arguments::<SendMessageArgs>(value).is_err());
        }
    }
}

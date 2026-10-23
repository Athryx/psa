use super::{finish, prepare};
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
use schemars::JsonSchema;
use serde::Deserialize;
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
            "Send attributed steering context to an agent in your own group. Resumes idle agents in their existing session.",
            Some(false),
        )
    }
    async fn execute_typed(
        &self,
        args: SendMessageArgs,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = match prepare(&self.handle, context).await {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let result = handle
            .send(
                context.agent().id,
                args.agent_name,
                args.message,
                cancellation,
            )
            .await;
        finish(&handle, context, result).await
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

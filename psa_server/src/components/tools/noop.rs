use super::{Tool, ToolOutput, typed_tool_execute};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    components::cancellation::Cancellation,
    prompts,
    sessions::Session,
};
use llm_api::api::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;

pub struct Noop;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoopArgs {}

impl Noop {
    pub(super) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<NoopArgs>("noop", prompts::noop_description(), Some(true))
    }

    async fn execute_typed(
        &self,
        _arguments: NoopArgs,
        _context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> ToolOutput {
        ToolOutput::success(Vec::new())
    }
}

impl Tool for Noop {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_tool_execute!(NoopArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    #[test]
    fn definition_uses_a_closed_empty_object_schema() {
        let definition = Noop::definition();
        let parameters = serde_json::to_value(&definition.parameters).unwrap();

        assert_eq!(definition.name, "noop");
        assert_eq!(definition.description, prompts::noop_description());
        assert_eq!(definition.strict, Some(true));
        assert_eq!(parameters["type"], "object");
        assert!(
            parameters
                .get("properties")
                .is_none_or(|properties| properties == &json!({}))
        );
        assert_eq!(parameters["additionalProperties"], false);
        assert!(
            definition
                .parameters
                .to_openai_parameters(definition.strict)
                .is_ok()
        );
    }

    #[test]
    fn arguments_require_an_empty_object() {
        assert!(parse_tool_arguments::<NoopArgs>(json!({})).is_ok());
        for arguments in [
            json!({"unexpected": true}),
            json!(null),
            json!(1),
            json!([]),
        ] {
            assert!(parse_tool_arguments::<NoopArgs>(arguments).is_err());
        }
    }
}

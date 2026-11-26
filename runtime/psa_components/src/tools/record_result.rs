use super::{ExecutionMode, Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute};
use crate::{bundles::typed_result::TypedResultState, prompts};
use llm_api::api::ToolDefinition;
use psa::api::{AgentResult, events::EventType};
use psa_agents::{
    AgentState, agent_context::AgentContext, cancellation::Cancellation, publication::status_event,
};
use psa_store::NewEvent;
use serde::Deserialize;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

const MAX_ERROR_BYTES: usize = 50 * 1024;

// Keep the original invocation value intact. Its schema belongs to the bundle,
// not this transparent decoding bridge, and is validated before persistence.
#[derive(Deserialize)]
#[serde(transparent)]
struct RecordResultArgs(Value);

pub struct RecordResult {
    state: Arc<TypedResultState>,
}

impl RecordResult {
    pub(crate) fn with_state(state: Arc<TypedResultState>) -> Self {
        Self { state }
    }

    /// Reused when recovery repairs the acknowledgement after an accepted commit.
    pub(crate) fn recorded_output() -> ToolOutput {
        ToolOutput::success(prompts::result_recorded())
    }

    async fn execute_typed(
        &self,
        arguments: RecordResultArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        if cancellation.is_cancelled() {
            return ToolOutput::error(prompts::tool_interrupted());
        }
        if let Err(error) = self.state.compiled.validate(&arguments.0) {
            return result_error(format!(
                "Result does not match the configured schema: {error}"
            ));
        }
        if cancellation.is_cancelled() {
            return ToolOutput::error(prompts::tool_interrupted());
        }

        // Submit result and completion as one worker-owned persistence operation.
        // Once enqueued, cancellation cannot retract an accepted commit.
        let events = [
            NewEvent::new(EventType::Result(AgentResult::Json(arguments.0))),
            status_event(AgentState::Idle),
        ];
        if let Err(error) = context.save_events(&events).await {
            return result_error(format!("Could not record result: {error}"));
        }
        Self::recorded_output()
    }
}

impl Tool for RecordResult {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::RecordResult
    }

    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "record_result".into(),
            description: "Record the final structured result using arguments that match this tool's configured schema. Submit the result object itself, without an extra wrapper. A valid result ends the current run; validation errors leave it running so you can correct the result.".into(),
            parameters: self.state.schema.clone(),
            strict: Some(false),
        }
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Finish the task by calling record_result with the final object matching its schema. Pass the object directly, not inside a result/value wrapper. Correct validation errors and retry; ordinary assistant prose does not record a result.".into()),
            rules: vec!["Use record_result to finish with a structured result; do not end the run with only an assistant message.".into()],
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    typed_tool_execute!(RecordResultArgs, execute_typed);
}

fn result_error(message: String) -> ToolOutput {
    ToolOutput::error(prompts::text(
        prompts::truncate_middle(&message, MAX_ERROR_BYTES).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundles::typed_result::ResultSchema;
    use llm_api::api::{ToolSchema, parse_tool_arguments};
    use serde_json::json;

    fn tool(schema: Value) -> RecordResult {
        let schema = ToolSchema::from(schema);
        let compiled = ResultSchema::compile(&schema).unwrap();
        RecordResult::with_state(Arc::new(TypedResultState {
            schema,
            compiled,
            max_retries: 3,
        }))
    }

    #[test]
    fn instance_metadata_uses_its_exact_configured_schema() {
        let schema = json!({
            "type":"object",
            "properties":{"answer":{"type":"integer"}},
            "required":["answer"],
            "additionalProperties":false
        });
        let tool = tool(schema.clone());
        let definition = tool.tool_definition();
        assert_eq!(definition.name, "record_result");
        assert_eq!(definition.parameters.as_value(), &schema);
        assert_eq!(definition.strict, Some(false));
        assert_eq!(tool.execution_mode(), ExecutionMode::Sequential);
    }

    #[test]
    fn transparent_arguments_preserve_the_original_object() {
        let original = json!({"nested":{"value":[1,"two",null]},"extra":true});
        let arguments = parse_tool_arguments::<RecordResultArgs>(original.clone()).unwrap();
        assert_eq!(arguments.0, original);
    }

    #[test]
    fn configured_validator_rejects_wrong_types_and_extra_fields() {
        let tool = tool(json!({
            "type":"object",
            "properties":{"answer":{"type":"integer"}},
            "required":["answer"],
            "additionalProperties":false
        }));
        for value in [json!(null), json!([])] {
            assert!(parse_tool_arguments::<RecordResultArgs>(value).is_err());
        }
        for value in [
            json!({}),
            json!({"answer":"private-value"}),
            json!({"answer":1,"extra":true}),
            json!({"result":{"answer":1}}),
        ] {
            let arguments = parse_tool_arguments::<RecordResultArgs>(value).unwrap();
            assert!(tool.state.compiled.validate(&arguments.0).is_err());
        }
        assert!(tool.state.compiled.validate(&json!({"answer":1})).is_ok());
    }
}

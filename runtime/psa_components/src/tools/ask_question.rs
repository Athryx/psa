use super::{Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute};
use crate::prompts;
use llm_api::api::ToolDefinition;
use psa::api::{AskQuestionRequest, ClientQuery, ClientQueryResponse};
use psa_agents::{
    agent_context::AgentContext, cancellation::Cancellation, client_queries::ClientQueryError,
};
use std::time::Duration;

const MAX_OUTPUT_BYTES: usize = 50 * 1024;

pub struct AskQuestion;

impl AskQuestion {
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<AskQuestionRequest>(
            "ask_question",
            "Ask the user one or more related questions together. Each question may include suggested choices; free-form answers are always accepted. Waits for client answers or cancellation. Returns the ordered answers as JSON, middle-truncated to 50 KiB when necessary.",
            Some(false),
        )
    }

    async fn execute_typed(
        &self,
        arguments: AskQuestionRequest,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let result = context
            .query_client(ClientQuery::AskQuestion(arguments), &mut cancellation)
            .await;
        format_query_result(result)
    }
}

impl Tool for AskQuestion {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::AskQuestion
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Group related questions in one request. Supply choices when useful, but allow the user to answer freely. Answers correspond to questions in order; long answer output is middle-truncated to 50 KiB.".into()),
            rules: Vec::new(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    typed_tool_execute!(AskQuestionRequest, execute_typed);
}

fn format_query_result(result: Result<ClientQueryResponse, ClientQueryError>) -> ToolOutput {
    let message = match result {
        Ok(ClientQueryResponse::AskQuestion(response)) => match serde_json::to_string(&response) {
            Ok(json) => {
                return ToolOutput::success(prompts::text(
                    prompts::truncate_middle(&json, MAX_OUTPUT_BYTES).into_owned(),
                ));
            }
            Err(error) => format!("Could not encode client answers: {error}"),
        },
        Err(ClientQueryError::Cancelled) => {
            return ToolOutput::error(prompts::tool_interrupted());
        }
        Err(ClientQueryError::InvalidRequest(reason)) => {
            format!("Invalid question request: {reason}")
        }
        Err(ClientQueryError::InvalidResponse(reason)) => {
            format!("Invalid client answer: {reason}")
        }
        Err(error) => format!("Could not ask the client: {error}"),
    };
    ToolOutput::error(prompts::text(
        prompts::truncate_middle(&message, MAX_OUTPUT_BYTES).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::{ContentBlock, parse_tool_arguments};
    use psa::api::{AskQuestionResponse, ClientAnswer};
    use serde_json::json;

    fn output_text(output: &ToolOutput) -> &str {
        let [ContentBlock::Text(block)] = output.content.as_slice() else {
            panic!("expected one text block");
        };
        &block.text
    }

    #[test]
    fn schema_uses_the_shared_question_request() {
        let definition = AskQuestion::definition();
        let schema = definition.parameters.as_value();
        assert_eq!(definition.name, "ask_question");
        assert_eq!(definition.strict, Some(false));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["questions"]));
        assert_eq!(schema["properties"]["questions"]["type"], "array");
        let question = &schema["$defs"]["ClientQuestion"];
        assert_eq!(question["additionalProperties"], false);
        assert_eq!(question["required"], json!(["question"]));
        assert_eq!(question["properties"]["choices"]["type"], "array");
        assert!(
            definition
                .parameters
                .validate_openai_parameters(Some(false))
                .is_ok()
        );
    }

    #[test]
    fn typed_arguments_accept_grouped_questions_and_optional_choices() {
        let request = parse_tool_arguments::<AskQuestionRequest>(json!({
            "questions": [
                {"question": "Which direction?", "choices": ["Left", "Right"]},
                {"question": "Any constraints?"}
            ]
        }))
        .unwrap();
        assert_eq!(request.questions.len(), 2);
        assert_eq!(request.questions[0].choices, ["Left", "Right"]);
        assert!(request.questions[1].choices.is_empty());
        assert!(ClientQuery::AskQuestion(request).validate().is_ok());
    }

    #[test]
    fn typed_arguments_reject_missing_extra_wrong_type_and_nested_fields() {
        for arguments in [
            json!({}),
            json!(null),
            json!({"questions": "not an array"}),
            json!({"questions": [], "extra": true}),
            json!({"questions": [{}]}),
            json!({"questions": [{"question": 1}]}),
            json!({"questions": [{"question": "Q", "extra": true}]}),
            json!({"questions": [{"question": "Q", "choices": null}]}),
            json!({"questions": [{"question": "Q", "choices": [1]}]}),
        ] {
            assert!(parse_tool_arguments::<AskQuestionRequest>(arguments).is_err());
        }
        let error = parse_tool_arguments::<AskQuestionRequest>(json!({
            "questions": [{"question": "Q", "choices": ["yes", 1]}]
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("questions[0].choices[1]"), "{error}");
    }

    #[test]
    fn query_validation_rejects_empty_or_blank_questions_and_choices() {
        for arguments in [
            json!({"questions": []}),
            json!({"questions": [{"question": "  "}]}),
            json!({"questions": [{"question": "Q", "choices": [" "]}]}),
        ] {
            let request = parse_tool_arguments::<AskQuestionRequest>(arguments).unwrap();
            assert!(ClientQuery::AskQuestion(request).validate().is_err());
        }
    }

    #[test]
    fn output_is_the_answer_payload_including_freeform_text_and_notes() {
        let response = AskQuestionResponse {
            answers: vec![ClientAnswer {
                answer: "A different option".into(),
                notes: Some("Why this works".into()),
            }],
        };
        let expected = serde_json::to_value(&response).unwrap();
        let output = format_query_result(Ok(ClientQueryResponse::AskQuestion(response)));
        assert!(!output.error);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(output_text(&output)).unwrap(),
            expected
        );
    }

    #[test]
    fn answer_and_error_output_are_middle_bounded_on_unicode_boundaries() {
        let answer = format!("begin{}end", "界".repeat(MAX_OUTPUT_BYTES));
        let output =
            format_query_result(Ok(ClientQueryResponse::AskQuestion(AskQuestionResponse {
                answers: vec![ClientAnswer {
                    answer: answer.clone(),
                    notes: None,
                }],
            })));
        assert!(!output.error);
        assert!(output_text(&output).len() <= MAX_OUTPUT_BYTES);
        assert!(output_text(&output).contains("begin"));
        assert!(output_text(&output).contains("end"));

        let error = format_query_result(Err(ClientQueryError::InvalidRequest(answer)));
        assert!(error.error);
        assert!(output_text(&error).len() <= MAX_OUTPUT_BYTES);
        assert!(output_text(&error).contains("begin"));
        assert!(output_text(&error).ends_with("end"));
    }

    #[test]
    fn cancellation_and_service_errors_are_model_facing() {
        let cancelled = format_query_result(Err(ClientQueryError::Cancelled));
        assert!(cancelled.error);
        assert_eq!(
            serde_json::to_value(cancelled.content).unwrap(),
            serde_json::to_value(prompts::tool_interrupted()).unwrap()
        );
        for error in [ClientQueryError::Unavailable, ClientQueryError::Overloaded] {
            let output = format_query_result(Err(error));
            assert!(output.error);
            assert!(output_text(&output).contains("Could not ask the client"));
        }
    }
}

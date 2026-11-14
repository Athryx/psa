use std::{collections::HashSet, time::Duration};

use llm_api::api::{ContentBlock, ToolSchema};
use psa_api::events::{Event, EventId, EventRange, EventType, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{
    CodeModeError, CodeModeFunction, CodeModeFunctionDefinition, CodeModeFunctionResult,
    typed_codemode_execute,
};
use crate::{
    agents::agent_context::AgentContext,
    components::cancellation::Cancellation,
    db::{PrivateEvent, PrivateEventKind},
};

const EVENT_PAGE_SIZE: u32 = 128;

pub struct PriorResults;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PriorResultsArgs {
    /// Zero-based result index, oldest first. Negative indices count from the newest result.
    #[serde(default = "default_index")]
    pub(super) index: i64,
}

fn default_index() -> i64 {
    -1
}

impl PriorResults {
    async fn execute_typed(
        &self,
        arguments: PriorResultsArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> CodeModeFunctionResult {
        if cancellation.is_cancelled() {
            return Err(CodeModeError::runtime("Execution cancelled"));
        }
        let through = context.projection().last_event_id;
        let index = arguments.index;

        let event_id = if index >= 0 {
            scan_result_events(context, through, Some(index as u64), &mut cancellation)
                .await?
                .1
        } else {
            let (count, latest) =
                scan_result_events(context, through, None, &mut cancellation).await?;
            if index == -1 {
                latest
            } else {
                let target = count
                    .checked_sub(index.unsigned_abs())
                    .ok_or_else(|| index_error(index))?;
                scan_result_events(context, through, Some(target), &mut cancellation)
                    .await?
                    .1
            }
        }
        .ok_or_else(|| index_error(index))?;

        let events = load_event_page(
            context,
            EventRange {
                after: EventId(event_id.0.saturating_sub(1)),
                through: Some(event_id),
                limit: 1,
            },
            &mut cancellation,
        )
        .await?;
        let event = events
            .into_iter()
            .find(|event| event.id == event_id)
            .ok_or_else(|| {
                CodeModeError::runtime("Selected prior CodeMode result is unavailable")
            })?;
        let EventType::ToolResult(result) = event.kind else {
            return Err(CodeModeError::runtime(
                "Selected prior CodeMode event is not a tool result",
            ));
        };

        // Truncated results keep their full output privately; prefer it.
        let saved = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CodeModeError::runtime("Execution cancelled")),
            saved = context.load_private_event_at(event_id, PrivateEventKind::CodeModeOutput) => saved,
        }
        .map_err(|error| {
            CodeModeError::runtime(format!("Could not load prior codemode output: {error}"))
        })?;
        match saved {
            Some(PrivateEvent::CodeModeOutput(text)) => Ok(Value::String(text)),
            _ => result_text(result).map(Value::String),
        }
    }
}

impl CodeModeFunction for PriorResults {
    fn definition(&self) -> CodeModeFunctionDefinition {
        CodeModeFunctionDefinition {
            name: "prior_results".into(),
            description: "Return the full text output of an earlier codemode call, including errors and any part truncated from what you were shown (saved up to 2M characters). -1 is the latest, 0 the oldest. Not async; do not await.".into(),
            parameters: ToolSchema::for_type::<PriorResultsArgs>(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_codemode_execute!(PriorResultsArgs, execute_typed);
}

/// Count matching results and keep only the latest id, or stop at a requested index.
async fn scan_result_events(
    context: &AgentContext<'_>,
    through: EventId,
    target: Option<u64>,
    cancellation: &mut Cancellation,
) -> Result<(u64, Option<EventId>), CodeModeError> {
    let mut after = EventId(0);
    let mut pending_calls = HashSet::new();
    let mut count = 0;
    let mut latest = None;

    while after < through {
        let events = load_event_page(
            context,
            EventRange {
                after,
                through: Some(through),
                limit: EVENT_PAGE_SIZE,
            },
            cancellation,
        )
        .await?;
        if events.is_empty() {
            break;
        }

        for event in events {
            after = event.id;
            match event.kind {
                EventType::ToolCall(call) => {
                    if call.tool_name == "codemode" {
                        pending_calls.insert(call.call_id);
                    } else {
                        pending_calls.remove(&call.call_id);
                    }
                }
                EventType::ToolResult(result) if pending_calls.remove(&result.call_id) => {
                    let result_index = count;
                    count += 1;
                    if target == Some(result_index) {
                        return Ok((count, Some(event.id)));
                    }
                    if target.is_none() {
                        latest = Some(event.id);
                    }
                }
                _ => {}
            }
        }
    }

    Ok((count, latest))
}

async fn load_event_page(
    context: &AgentContext<'_>,
    range: EventRange,
    cancellation: &mut Cancellation,
) -> Result<Vec<Event>, CodeModeError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(CodeModeError::runtime("Execution cancelled")),
        events = context.load_events(range) => events.map_err(|error| {
            CodeModeError::runtime(format!("Could not load prior CodeMode results: {error}"))
        }),
    }
}

fn index_error(index: i64) -> CodeModeError {
    CodeModeError {
        exception_type: "IndexError".into(),
        message: format!("prior_results index {index} is out of range"),
    }
}

fn result_text(result: ToolResult) -> Result<String, CodeModeError> {
    let mut parts = Vec::with_capacity(result.content.len());
    for block in result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text),
            _ => {
                return Err(CodeModeError::runtime(
                    "Prior CodeMode result contains non-text content",
                ));
            }
        }
    }
    Ok(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::{HistoryItemMetadata, ImageBlock, MediaSource};
    use serde_json::json;

    #[test]
    fn definition_uses_closed_defaulted_integer_index() {
        let definition = PriorResults.definition();
        let schema = serde_json::to_value(definition.parameters).unwrap();
        assert_eq!(definition.name, "prior_results");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["index"]["type"], "integer");
        assert_eq!(schema["properties"]["index"]["default"], -1);
        assert!(
            schema
                .get("required")
                .is_none_or(|required| required == &json!([]))
        );
        assert_eq!(PriorResults.cancellation_timeout(), Duration::from_secs(1));
    }

    #[test]
    fn arguments_accept_default_and_full_signed_index_range() {
        let arguments: PriorResultsArgs =
            super::super::parse_codemode_arguments(json!({})).unwrap();
        assert_eq!(arguments.index, -1);
        for index in [i64::MIN, -2, -1, 0, 1, i64::MAX] {
            let arguments: PriorResultsArgs =
                super::super::parse_codemode_arguments(json!({"index": index})).unwrap();
            assert_eq!(arguments.index, index);
        }
    }

    #[test]
    fn arguments_reject_unknown_noninteger_and_nonobject_values() {
        for arguments in [
            json!({"extra": true}),
            json!({"index": null}),
            json!({"index": "-1"}),
            json!({"index": -1.5}),
            json!({"index": true}),
            json!({"index": u64::MAX}),
            json!(null),
            json!([]),
        ] {
            let error = super::super::parse_codemode_arguments::<PriorResultsArgs>(arguments)
                .err()
                .unwrap();
            assert_eq!(error.exception_type, "TypeError");
        }
    }

    fn result(content: Vec<ContentBlock>, error: bool) -> ToolResult {
        ToolResult {
            delivery: None,
            call_id: "codemode-call".into(),
            content,
            error,
            metadata: HistoryItemMetadata::default(),
        }
    }

    #[test]
    fn result_text_preserves_errors_and_block_order() {
        for error in [false, true] {
            let mut content = crate::prompts::text("first");
            content.extend(crate::prompts::text("second"));
            assert_eq!(
                result_text(result(content, error)).unwrap(),
                "first\nsecond"
            );
        }
        assert_eq!(result_text(result(vec![], false)).unwrap(), "");
    }

    #[test]
    fn result_text_rejects_nontext_content() {
        let mut content = crate::prompts::text("first");
        content.push(ContentBlock::Image(ImageBlock {
            source: MediaSource::Inline {
                data: vec![1],
                mime_type: "image/png".into(),
            },
            detail: None,
        }));
        let error = result_text(result(content, false)).unwrap_err();
        assert_eq!(error.exception_type, "RuntimeError");
        assert!(error.message.contains("non-text"));
    }
}

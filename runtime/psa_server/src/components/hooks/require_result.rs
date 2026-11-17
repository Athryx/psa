use super::{Hook, HookError};
use crate::db::NewEvent;
use crate::{
    agents::{AgentState, agent_context::AgentContext},
    components::{
        bundles::typed_result::{ResultSchema, TypedResultState},
        cancellation::Cancellation,
    },
};
use llm_api::api::{Outcome, StopReason};
use psa_api::events::{Event, EventId, EventType, Log, Message, MessageType, StatusChange};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc, time::Duration};

const RETRY_NAMESPACE: &str = "psa.typed_result.retry";
const RETRY_VERSION: u32 = 1;

pub struct RequireResult {
    state: Arc<TypedResultState>,
}

impl RequireResult {
    pub(crate) fn with_state(state: Arc<TypedResultState>) -> Self {
        Self { state }
    }

    async fn after_request(
        &self,
        context: &mut AgentContext<'_>,
        cancellation: &mut Cancellation,
    ) -> Result<(), HookError> {
        if cancellation.is_cancelled() || context.agent().state != AgentState::Running {
            return Ok(());
        }
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = context.load_last_inference_result() => result?,
        };
        let Some(response) = response else {
            return Ok(());
        };
        let EventType::InferenceResultEnd(end) = &response.end.kind else {
            return Ok(());
        };
        if !can_complete(&end.outcome) {
            return Ok(());
        }
        let Some(violation) = assess_response(&response.items, &self.state.compiled) else {
            return Ok(());
        };

        let events = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = context.load_all_events() => result?,
        };
        let history = RetryHistory::replay(&events);
        let Some(run_start) = history.run_start else {
            return Ok(());
        };
        if history.status != AgentState::Running
            || response.start.id <= run_start
            || history.attempts.contains(&response.start.id)
        {
            return Ok(());
        }

        let exhausted = history.attempts.len() as u64 >= u64::from(self.state.max_retries);
        let marker = RetryMarker {
            namespace: RETRY_NAMESPACE.into(),
            version: RETRY_VERSION,
            run_start,
            inference_start: response.start.id,
            attempt: history.attempts.len() as u64 + 1,
            violation,
        };
        let mut updates = vec![NewEvent::new(EventType::Log(Log {
            error: exhausted,
            message: serde_json::to_string(&marker).expect("retry marker is serializable"),
        }))];
        if exhausted {
            updates.push(NewEvent::new(EventType::StatusChange(StatusChange {
                new_status: AgentState::Interrupted,
                reason: Some(format!(
                    "Typed result completion failed: {}. Retry budget exhausted ({} retries).",
                    violation.description(),
                    self.state.max_retries
                )),
            })));
        } else if violation == Violation::MissingResult {
            // There are no pending calls in this branch. For invalid record_result
            // calls, their ordinary tool errors supply feedback instead: inserting
            // a message between tool calls and their results breaks replay ordering.
            updates.push(NewEvent::new(EventType::Message(Message::text(
                MessageType::Harness,
                "A structured result is required. Continue the task if needed, then call record_result with an object matching its schema. An assistant message alone does not finish this run.",
            ))));
        }

        if cancellation.is_cancelled() {
            return Ok(());
        }
        context.save_events(&updates).await?;
        Ok(())
    }
}

impl Hook for RequireResult {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        self.after_request(context, &mut cancellation).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Violation {
    MissingResult,
    InvalidResult,
}

impl Violation {
    fn description(self) -> &'static str {
        match self {
            Self::MissingResult => "the model completed without calling record_result",
            Self::InvalidResult => "the model called record_result without a valid result",
        }
    }
}

fn can_complete(outcome: &Outcome) -> bool {
    matches!(
        outcome,
        Outcome::Completed { .. }
            | Outcome::Incomplete {
                reason: StopReason::TokenLimit
            }
    )
}

fn assess_response(events: &[Event], schema: &ResultSchema) -> Option<Violation> {
    let mut has_tools = false;
    let mut has_result = false;
    for event in events {
        let EventType::ToolCall(call) = &event.kind else {
            continue;
        };
        has_tools = true;
        if call.tool_name != "record_result" {
            continue;
        }
        has_result = true;
        if call
            .metadata
            .item_status
            .as_ref()
            .is_some_and(llm_api::api::ItemStatus::is_unfinished)
        {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&call.arguments)
            && schema.validate(&value).is_ok()
        {
            // A valid sibling will record and finish the run; invalid siblings do
            // not consume retries or override that successful completion.
            return None;
        }
    }
    if has_result {
        Some(Violation::InvalidResult)
    } else if !has_tools {
        Some(Violation::MissingResult)
    } else {
        None
    }
}

#[derive(Serialize, Deserialize)]
struct RetryMarker {
    namespace: String,
    version: u32,
    run_start: EventId,
    inference_start: EventId,
    attempt: u64,
    violation: Violation,
}

struct RetryHistory {
    status: AgentState,
    run_start: Option<EventId>,
    attempts: HashSet<EventId>,
}

impl RetryHistory {
    fn replay(events: &[Event]) -> Self {
        let mut history = Self {
            status: AgentState::Idle,
            run_start: None,
            attempts: HashSet::new(),
        };
        for event in events {
            match &event.kind {
                EventType::StatusChange(change) => {
                    if change.new_status == AgentState::Running
                        && history.status != AgentState::Running
                    {
                        history.run_start = Some(event.id);
                        history.attempts.clear();
                    }
                    history.status = change.new_status;
                }
                // A result is a terminal checkpoint, even at a fork cut just
                // before its paired Idle event. A later Running starts a new run.
                EventType::Result(_) => history.status = AgentState::Idle,
                EventType::Log(log) => {
                    let Ok(marker) = serde_json::from_str::<RetryMarker>(&log.message) else {
                        continue;
                    };
                    if marker.namespace == RETRY_NAMESPACE
                        && marker.version == RETRY_VERSION
                        && history.run_start == Some(marker.run_start)
                        && marker.inference_start > marker.run_start
                        && marker.inference_start < event.id
                    {
                        history.attempts.insert(marker.inference_start);
                    }
                }
                _ => {}
            }
        }
        history
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::{HistoryItemMetadata, ToolSchema};
    use psa_api::{AgentId, AgentResult, events::ToolCall};
    use serde_json::json;
    use time::OffsetDateTime;

    fn event(id: u64, kind: EventType) -> Event {
        Event {
            id: EventId(id),
            source_agent_id: AgentId(1),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    fn status(id: u64, state: AgentState) -> Event {
        event(
            id,
            EventType::StatusChange(StatusChange {
                new_status: state,
                reason: None,
            }),
        )
    }

    fn marker(id: u64, run: u64, inference: u64) -> Event {
        event(
            id,
            EventType::Log(Log {
                error: false,
                message: serde_json::to_string(&RetryMarker {
                    namespace: RETRY_NAMESPACE.into(),
                    version: RETRY_VERSION,
                    run_start: EventId(run),
                    inference_start: EventId(inference),
                    attempt: 1,
                    violation: Violation::MissingResult,
                })
                .unwrap(),
            }),
        )
    }

    fn call(id: u64, name: &str, arguments: &str) -> Event {
        event(
            id,
            EventType::ToolCall(ToolCall {
                call_id: format!("call-{id}"),
                tool_name: name.into(),
                arguments: arguments.into(),
                metadata: HistoryItemMetadata::default(),
            }),
        )
    }

    fn schema() -> ResultSchema {
        ResultSchema::compile(&ToolSchema::from(json!({
            "type":"object",
            "required":["answer"],
            "properties":{"answer":{"type":"integer"}},
            "additionalProperties":false
        })))
        .unwrap()
    }

    #[test]
    fn completion_without_result_and_invalid_calls_are_violations() {
        let schema = schema();
        assert_eq!(
            assess_response(&[], &schema),
            Some(Violation::MissingResult)
        );
        for arguments in ["{", "null", "{}", r#"{"answer":"wrong"}"#] {
            assert_eq!(
                assess_response(&[call(2, "record_result", arguments)], &schema),
                Some(Violation::InvalidResult)
            );
        }
        assert_eq!(assess_response(&[call(2, "read", "{}")], &schema), None);
    }

    #[test]
    fn any_valid_record_call_wins_over_invalid_siblings_in_either_order() {
        let schema = schema();
        for calls in [
            vec![
                call(2, "record_result", "{}"),
                call(3, "record_result", r#"{"answer":1}"#),
            ],
            vec![
                call(2, "record_result", r#"{"answer":1}"#),
                call(3, "record_result", "{}"),
            ],
        ] {
            assert_eq!(assess_response(&calls, &schema), None);
        }
        let mut incomplete = call(2, "record_result", r#"{"answer":1}"#);
        let EventType::ToolCall(call) = &mut incomplete.kind else {
            unreachable!()
        };
        call.metadata.item_status = Some(llm_api::api::ItemStatus::Incomplete);
        assert_eq!(
            assess_response(&[incomplete], &schema),
            Some(Violation::InvalidResult)
        );
    }

    #[test]
    fn token_limited_responses_are_completion_attempts_not_other_failures() {
        assert!(can_complete(&Outcome::Incomplete {
            reason: StopReason::TokenLimit
        }));
        assert!(can_complete(&Outcome::Completed {
            reason: StopReason::EndTurn
        }));
        assert!(!can_complete(&Outcome::Incomplete {
            reason: StopReason::ContentFilter
        }));
        assert!(!can_complete(&Outcome::Cancelled));
        assert!(!can_complete(&Outcome::InProgress));
    }

    #[test]
    fn steering_and_worker_replay_keep_retry_count_and_deduplicate_inference() {
        let events = [
            status(1, AgentState::Running),
            marker(3, 1, 2),
            status(4, AgentState::Running),
            marker(6, 1, 5),
            marker(7, 1, 5),
        ];
        let history = RetryHistory::replay(&events);
        assert_eq!(history.run_start, Some(EventId(1)));
        assert_eq!(history.attempts.len(), 2);
        assert!(history.attempts.contains(&EventId(5)));
        assert_eq!(RetryHistory::replay(&events).attempts, history.attempts);
    }

    #[test]
    fn only_a_new_logical_run_resets_the_budget() {
        for terminal in [AgentState::Idle, AgentState::Interrupted] {
            let history = RetryHistory::replay(&[
                status(1, AgentState::Running),
                marker(3, 1, 2),
                status(4, terminal),
                status(5, AgentState::Running),
                marker(7, 1, 6),
                marker(9, 5, 8),
            ]);
            assert_eq!(history.run_start, Some(EventId(5)));
            assert_eq!(history.attempts, HashSet::from([EventId(8)]));
        }
    }

    #[test]
    fn result_checkpoint_starts_a_fresh_budget_on_future_running() {
        let history = RetryHistory::replay(&[
            status(1, AgentState::Running),
            marker(3, 1, 2),
            event(4, EventType::Result(AgentResult::Json(json!({"answer":1})))),
            status(5, AgentState::Running),
        ]);
        assert_eq!(history.run_start, Some(EventId(5)));
        assert!(history.attempts.is_empty());
    }
}

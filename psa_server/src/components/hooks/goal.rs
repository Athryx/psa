//! Goal-aware replacement for StopWithoutTools on interactive Goal agents.
use super::{Hook, HookError};
use crate::{
    agents::{Agent, AgentState, agent_context::AgentContext, publication::status_event},
    components::{
        bundles::goal::{GoalHandle, GoalState},
        cancellation::Cancellation,
    },
    sessions::Session,
};
use llm_api::api::{Outcome, StopReason};
use psa_api::events::{EventType, Message, MessageType, NewEvent};
use std::time::Duration;

#[derive(Default)]
pub struct GoalHook {
    handle: Option<GoalHandle>,
}

impl GoalHook {
    pub(crate) fn with_handle(handle: GoalHandle) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn after_request(&self, context: &mut AgentContext<'_>) -> Result<(), HookError> {
        if !context.agent().interactive || context.agent().state != AgentState::Running {
            return Ok(());
        }
        let Some(handle) = &self.handle else {
            return Ok(());
        };
        let Some(response) = context.load_last_inference_result().await? else {
            return Ok(());
        };
        let EventType::InferenceResultEnd(end) = &response.end.kind else {
            return Ok(());
        };
        if !can_settle(
            &end.outcome,
            response
                .items
                .iter()
                .any(|event| matches!(event.kind, EventType::ToolCall(_))),
        ) {
            return Ok(());
        }

        let Some(lease) = handle.lease(context).await? else {
            return Ok(());
        };
        if lease.state.is_running()
            && matches!(
                end.outcome,
                Outcome::Completed {
                    reason: StopReason::EndTurn
                }
            )
        {
            // Append only: reminders retain their original goal/generation in history.
            // Queued user policy intentionally waits until the goal is finished;
            // Steer and cancellation still intervene through the ordinary worker loop.
            context
                .save_events(&[NewEvent::new(EventType::Message(Message::text(
                    MessageType::Harness,
                    format_reminder(&lease.state),
                )))])
                .await?;
        } else {
            context
                .save_events(&[status_event(AgentState::Idle)])
                .await?;
        }
        Ok(())
    }
}

fn can_settle(outcome: &Outcome, has_tools: bool) -> bool {
    !has_tools && matches!(outcome, Outcome::Completed { .. })
}

fn format_reminder(state: &GoalState) -> String {
    let goal = serde_json::to_string(state.goal.as_deref().unwrap_or_default())
        .expect("goal string is serializable");
    format!(
        "Current goal: {goal}. Check whether it is completed or blocked. If there is strong evidence for either, call set_goal_status with that status and explain the evidence in reason. Otherwise keep working on the goal; do not just finish with a final answer.\nGoal generation: {}.",
        state.generation
    )
}

impl Hook for GoalHook {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self::default()
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }
    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Ok(()),
            result = self.after_request(context) => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::bundles::goal::GoalStatus;
    #[test]
    fn completed_without_tools_is_always_eligible_to_settle() {
        for reason in [
            StopReason::EndTurn,
            StopReason::StopSequence,
            StopReason::TokenLimit,
            StopReason::ToolCalls,
        ] {
            assert!(can_settle(
                &Outcome::Completed {
                    reason: reason.clone()
                },
                false
            ));
            assert!(!can_settle(&Outcome::Completed { reason }, true));
        }
        for outcome in [
            Outcome::Cancelled,
            Outcome::InProgress,
            Outcome::Incomplete {
                reason: StopReason::TokenLimit,
            },
        ] {
            assert!(!can_settle(&outcome, false));
        }
    }
    #[tokio::test]
    async fn context_settles_non_end_turn_but_only_normal_end_turn_continues_running_goal() {
        use crate::{
            agents::{
                NewAgent, agent_manager::AgentManagerState, publication::Publication,
                session_manager::Runtime,
            },
            components::{
                bundles::{InstalledComponents, goal::load_goal_state},
                tools::Tool,
            },
            db::Database,
            probes::ProbeRegistry,
            providers::ProviderRegistry,
            sessions::NewSession,
            test_support as support,
        };
        use psa_api::events::{InferenceResultEnd, InferenceResultStart};
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("goal-hook.sqlite3"))
            .await
            .unwrap();
        support::register_probe(&database).await;
        let publication = Publication::default();
        let probes = ProbeRegistry::start("goal-test-key".into());
        let runtime = Runtime::start(
            database.clone(),
            publication.clone(),
            ProviderRegistry::default(),
            probes.clone(),
        )
        .await
        .unwrap();

        // Exercise persisted agent status, not just a continuation predicate.
        for (reason, active, interrupted, cancelled, expected) in [
            (StopReason::EndTurn, true, false, false, AgentState::Running),
            (
                StopReason::StopSequence,
                true,
                false,
                false,
                AgentState::Idle,
            ),
            (StopReason::TokenLimit, true, false, false, AgentState::Idle),
            (StopReason::EndTurn, false, false, false, AgentState::Idle),
            (
                StopReason::EndTurn,
                true,
                true,
                false,
                AgentState::Interrupted,
            ),
            (StopReason::EndTurn, true, false, true, AgentState::Running),
        ] {
            let mut cursor = database.cursor().await.unwrap();
            let definition = cursor
                .create_agent_definition(psa_api::CreateAgentDefinition {
                    name: format!("goal-{reason:?}-{active}-{interrupted}-{cancelled}"),
                    bundles: vec![psa_api::BundleId::Goal],
                    ..Default::default()
                })
                .await
                .unwrap();
            let session = cursor
                .create_session(NewSession {
                    name: "goal hook".into(),
                    working_directory: "/work".into(),
                    probe_id: support::probe_metadata().id,
                    model_config: support::model_config(),
                })
                .await
                .unwrap();
            let agent = cursor
                .create_agent(
                    session.id,
                    NewAgent {
                        interactive: true,
                        definition_id: definition.id,
                        name: "goal agent".into(),
                    },
                )
                .await
                .unwrap();
            cursor.commit().await.unwrap();
            let mut state =
                AgentManagerState::load_persisted(database.clone(), publication.clone(), agent.id)
                    .await
                    .unwrap();
            let mut components = InstalledComponents::install(&definition, &agent, &session)
                .await
                .unwrap();
            let mut context = AgentContext::with_optional_probe(&mut state, None, &runtime.handle);
            context
                .save_events(&[status_event(AgentState::Running)])
                .await
                .unwrap();
            if active {
                let (_sender, cancellation) = Cancellation::channel();
                let output = components
                    .tools
                    .iter()
                    .find(|tool| tool.id() == psa_api::ToolId::SetGoal)
                    .unwrap()
                    .execute(
                        serde_json::json!({"goal":"prove the goal"}),
                        &mut context,
                        cancellation,
                    )
                    .await;
                assert!(!output.error);
                assert!(load_goal_state(&context).await.unwrap().is_running());
            }
            context
                .save_events(&[
                    NewEvent::new(EventType::Model(support::model_config())),
                    NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                        id: 1,
                        model_config: support::model_config(),
                        request_config: Default::default(),
                    })),
                    NewEvent::new(EventType::Message(Message::text(
                        MessageType::Agent,
                        "answer",
                    ))),
                    NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                        id: 1,
                        outcome: Outcome::Completed {
                            reason: reason.clone(),
                        },
                        usage: Default::default(),
                        request_time: Duration::ZERO,
                        response_id: None,
                        provider_metadata: Default::default(),
                    })),
                ])
                .await
                .unwrap();
            if interrupted {
                context
                    .save_events(&[status_event(AgentState::Interrupted)])
                    .await
                    .unwrap();
            }
            let (sender, cancellation) = Cancellation::channel();
            if cancelled {
                let _ = sender.send(());
            }
            components.hooks.after_request[0]
                .run(&mut context, cancellation)
                .await
                .unwrap();
            assert_eq!(
                context.agent().state,
                expected,
                "{reason:?}, active={active}, interrupted={interrupted}, cancelled={cancelled}"
            );
            let events = context.load_all_events().await.unwrap();
            let reminders = events.iter().filter(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Harness)).count();
            assert_eq!(
                reminders,
                usize::from(reason == StopReason::EndTurn && active && !interrupted && !cancelled)
            );
            let mut cursor = database.cursor().await.unwrap();
            assert_eq!(cursor.get_agent(agent.id).await.unwrap().state, expected);
            cursor.rollback().await.unwrap();
            drop(context);
            drop(state);
            components.cleanup().await.unwrap();
        }
        runtime.shutdown().await.unwrap();
        probes.shutdown().await;
        database.close().await;
    }

    #[test]
    fn reminder_preserves_goal_and_generation_with_json_escaping() {
        let state = GoalState {
            goal: Some("a \"quoted\"\n雪 goal".into()),
            status: GoalStatus::Running,
            reason: None,
            generation: 7,
        };
        let reminder = format_reminder(&state);
        assert!(reminder.starts_with("Current goal: \"a \\\"quoted\\\"\\n雪 goal\"."));
        assert!(reminder.contains("strong evidence"));
        assert!(reminder.ends_with("Goal generation: 7."));
    }
}

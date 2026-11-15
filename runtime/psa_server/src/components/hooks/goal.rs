//! Goal-aware replacement for StopWithoutTools on agents with Goal installed.
use super::{Hook, HookError, stop_without_tools::build_text_completion_events};
use crate::{
    agents::{AgentState, agent_context::AgentContext},
    components::{
        bundles::goal::{GoalHandle, GoalState},
        cancellation::Cancellation,
    },
};
use llm_api::api::{Outcome, StopReason};
use psa_api::events::{EventType, Message, MessageType, NewEvent};
use std::time::Duration;

pub struct GoalHook {
    handle: GoalHandle,
}

impl GoalHook {
    pub(crate) fn with_handle(handle: GoalHandle) -> Self {
        Self { handle }
    }

    async fn after_request(&self, context: &mut AgentContext<'_>) -> Result<(), HookError> {
        if context.agent().state != AgentState::Running {
            return Ok(());
        }
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

        let Some(lease) = self.handle.lease(context).await? else {
            return Ok(());
        };
        if lease.state.is_running() {
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
                .save_events(&build_text_completion_events(&response.items))
                .await?;
        }
        Ok(())
    }
}

fn can_settle(outcome: &Outcome, has_tools: bool) -> bool {
    !has_tools
        && matches!(
            outcome,
            Outcome::Completed { .. }
                | Outcome::Incomplete {
                    reason: StopReason::TokenLimit
                }
        )
}

fn format_reminder(state: &GoalState) -> String {
    let goal = serde_json::to_string(state.goal.as_deref().unwrap_or_default())
        .expect("goal string is serializable");
    format!(
        "Current goal: {goal}. Check whether it is completed or blocked. If there is strong evidence for either, call set_goal_status with that status and explain the evidence in reason. Otherwise keep working on the goal; do not just finish with a final answer."
    )
}

impl Hook for GoalHook {
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
    use crate::{agents::publication::status_event, components::bundles::goal::GoalStatus};
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
        assert!(can_settle(
            &Outcome::Incomplete {
                reason: StopReason::TokenLimit
            },
            false,
        ));
        assert!(!can_settle(
            &Outcome::Incomplete {
                reason: StopReason::TokenLimit
            },
            true,
        ));
        for outcome in [
            Outcome::Cancelled,
            Outcome::InProgress,
            Outcome::Incomplete {
                reason: StopReason::ContentFilter,
            },
        ] {
            assert!(!can_settle(&outcome, false));
        }
    }
    #[tokio::test]
    async fn installed_goal_policy_completes_only_without_an_active_goal() {
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

        // Exercise installed policy, persisted state, and conventional stopper
        // behavior for interactive, opted-in, and fallback noninteractive agents.
        for (interactive, interactive_only) in [(true, true), (false, false), (false, true)] {
            let installed = interactive || !interactive_only;
            for (reason, active, completed, interrupted, cancelled, expected) in [
                (
                    StopReason::EndTurn,
                    true,
                    false,
                    false,
                    false,
                    AgentState::Running,
                ),
                (
                    StopReason::StopSequence,
                    true,
                    false,
                    false,
                    false,
                    AgentState::Running,
                ),
                (
                    StopReason::TokenLimit,
                    true,
                    false,
                    false,
                    false,
                    AgentState::Running,
                ),
                (
                    StopReason::EndTurn,
                    false,
                    false,
                    false,
                    false,
                    AgentState::Idle,
                ),
                (
                    StopReason::TokenLimit,
                    false,
                    false,
                    false,
                    false,
                    AgentState::Idle,
                ),
                (
                    StopReason::EndTurn,
                    true,
                    true,
                    false,
                    false,
                    AgentState::Idle,
                ),
                (
                    StopReason::EndTurn,
                    true,
                    false,
                    true,
                    false,
                    AgentState::Interrupted,
                ),
                (
                    StopReason::EndTurn,
                    true,
                    false,
                    false,
                    true,
                    AgentState::Running,
                ),
            ] {
                let mut cursor = database.cursor().await.unwrap();
                let name = format!(
                    "goal-{interactive}-{interactive_only}-{reason:?}-{active}-{completed}-{interrupted}-{cancelled}"
                );
                let definition = cursor
                    .create_agent_definition(psa_api::CreateAgentDefinition {
                        name,
                        bundles: vec![psa_api::BundleConfig::Goal { interactive_only }],
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                let session = cursor
                    .create_session(NewSession {
                        decision_model: None,
                        name: "goal hook".into(),
                        working_directory: "/work".into(),
                        probe_id: support::probe_metadata().id,
                        project_id: None,
                        model_config: support::model_config(),
                    })
                    .await
                    .unwrap();
                let agent = cursor
                    .create_agent(
                        session.id,
                        NewAgent {
                            subagent_parent_id: None,
                            interactive,
                            hidden: false,
                            definition_id: definition.id,
                            name: "goal agent".into(),
                        },
                    )
                    .await
                    .unwrap();
                cursor.commit().await.unwrap();
                let mut state = AgentManagerState::load_persisted(
                    database.clone(),
                    publication.clone(),
                    agent.id,
                )
                .await
                .unwrap();
                let shared_components = std::sync::Arc::new(tokio::sync::Mutex::new(
                    InstalledComponents::install(&definition, &agent, &session)
                        .await
                        .unwrap(),
                ));
                state.attach_components(shared_components.clone()).await;
                let mut components = shared_components.lock().await;
                assert_eq!(components.hooks.after_request.len(), 1);
                let mut context =
                    AgentContext::with_optional_probe(&mut state, None, &runtime.handle);
                context
                    .save_events(&[status_event(AgentState::Running)])
                    .await
                    .unwrap();
                if active && installed {
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
                    if completed {
                        let (_sender, cancellation) = Cancellation::channel();
                        let output = components
                            .tools
                            .iter()
                            .find(|tool| tool.id() == psa_api::ToolId::SetGoalStatus)
                            .unwrap()
                            .execute(
                                serde_json::json!({"status":"completed", "reason":"verified"}),
                                &mut context,
                                cancellation,
                            )
                            .await;
                        assert!(!output.error);
                    }
                }
                assert_eq!(
                    load_goal_state(&context).await.unwrap().is_running(),
                    active && installed && !completed,
                );
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
                            outcome: if reason == StopReason::TokenLimit {
                                Outcome::Incomplete {
                                    reason: reason.clone(),
                                }
                            } else {
                                Outcome::Completed {
                                    reason: reason.clone(),
                                }
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
                let expected = if !installed && !interrupted && !cancelled {
                    AgentState::Idle
                } else {
                    expected
                };
                assert_eq!(
                    context.agent().state,
                    expected,
                    "{reason:?}, interactive={interactive}, interactive_only={interactive_only}, active={active}, completed={completed}, interrupted={interrupted}, cancelled={cancelled}"
                );
                let events = context.load_all_events().await.unwrap();
                let reminders = events.iter().filter(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Harness)).count();
                assert_eq!(
                    reminders,
                    usize::from(installed && active && !completed && !interrupted && !cancelled)
                );
                let results: Vec<_> = events
                    .iter()
                    .filter(|event| matches!(event.kind, EventType::Result(_)))
                    .collect();
                assert_eq!(results.len(), usize::from(expected == AgentState::Idle));
                if let Some(result) = results.first() {
                    assert!(
                        matches!(&result.kind, EventType::Result(psa_api::AgentResult::Text(text)) if text == "answer")
                    );
                    assert!(events.windows(2).any(|pair| {
                        pair[0].id == result.id
                            && matches!(&pair[1].kind, EventType::StatusChange(status) if status.new_status == AgentState::Idle)
                    }));
                }
                let mut cursor = database.cursor().await.unwrap();
                assert_eq!(cursor.get_agent(agent.id).await.unwrap().state, expected);
                cursor.rollback().await.unwrap();
                drop(context);
                drop(state);
                components.cleanup().await.unwrap();
            }
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
        assert!(!reminder.contains("generation"));
    }
}

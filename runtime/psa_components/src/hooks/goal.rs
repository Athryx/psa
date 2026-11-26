//! Goal-aware replacement for StopWithoutTools on agents with Goal installed,
//! and the handler for user `/goal` commands.
use super::{
    AgentCommand, CommandHook, CommandResult, Hook, HookError,
    stop_without_tools::build_text_completion_events,
};
use crate::bundles::goal::{GoalHandle, GoalOperation, GoalState, GoalStatus, load_goal_state};
use llm_api::api::{Outcome, StopReason};
use psa::api::{
    CommandOutcome, GoalCommand,
    events::{EventType, Message},
};
use psa_agents::{
    AgentState, agent_context::AgentContext, cancellation::Cancellation, publication::status_event,
};
use psa_store::NewEvent;
use std::time::Duration;

#[derive(Clone)]
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
                .save_events(&[NewEvent::new(EventType::Message(Message::harness(
                    format_reminder(&lease.state),
                    "Goal reminder",
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
    let goal = quote_goal(state.goal.as_deref().unwrap_or_default());
    format!(
        "Current goal: {goal}. Check whether it is completed or blocked. If there is strong evidence for either, call set_goal_status with that status and explain the evidence in reason. Otherwise keep working on the goal; do not just finish with a final answer."
    )
}

fn quote_goal(goal: &str) -> String {
    serde_json::to_string(goal).expect("goal string is serializable")
}

/// The goal operation a user command performs; `None` for read-only `Show`.
fn build_command_operation(command: &GoalCommand) -> Option<GoalOperation> {
    let operation = match command {
        GoalCommand::Show => return None,
        GoalCommand::Set { goal } => GoalOperation::Set(goal.clone()),
        GoalCommand::Pause { reason } => GoalOperation::Status {
            status: GoalStatus::Blocked,
            // Blocking requires a reason; users need not give one.
            reason: Some(
                reason
                    .clone()
                    .filter(|reason| !reason.trim().is_empty())
                    .unwrap_or_else(|| "Paused by the user".into()),
            ),
        },
        GoalCommand::Resume => GoalOperation::Status {
            status: GoalStatus::Running,
            reason: None,
        },
        GoalCommand::Clear { reason } => GoalOperation::Clear {
            reason: reason.clone(),
        },
    };
    Some(operation)
}

/// The model-facing note for a user goal change, so the model learns about the
/// change before its next request rather than at the next reminder.
fn build_command_note(command: &GoalCommand, previous: &GoalState, next: &GoalState) -> Message {
    let goal = quote_goal(next.goal.as_deref().unwrap_or_default());
    let previous_goal = quote_goal(previous.goal.as_deref().unwrap_or_default());
    let reason = next
        .reason
        .as_ref()
        .map(|reason| format!(" Reason: {}.", reason.trim_end_matches('.')))
        .unwrap_or_default();
    let (text, label) = match command {
        GoalCommand::Set { .. } if previous.goal.is_some() => (
            format!(
                "The user replaced the goal {previous_goal} with a new goal: {goal}. Work on the new goal until it is completed or blocked."
            ),
            "Goal replaced",
        ),
        GoalCommand::Set { .. } => (
            format!("The user set a goal: {goal}. Work on it until it is completed or blocked."),
            "Goal set",
        ),
        GoalCommand::Pause { .. } => (
            format!(
                "The user paused the goal: {goal}.{reason} Stop working on it and wait until the user resumes it."
            ),
            "Goal paused",
        ),
        GoalCommand::Resume => (
            format!(
                "The user resumed the goal: {goal}. Continue working on it until it is completed or blocked."
            ),
            "Goal resumed",
        ),
        GoalCommand::Clear { .. } => (
            format!(
                "The user cleared the goal: {previous_goal}.{reason} It is no longer active; stop working on it."
            ),
            "Goal cleared",
        ),
        GoalCommand::Show => unreachable!("showing the goal changes nothing"),
    };
    Message::harness(text, label)
}

fn describe_goal_state(state: &GoalState) -> String {
    let Some(goal) = &state.goal else {
        return "No current goal.".into();
    };
    let status = match state.status {
        GoalStatus::Running => "running",
        GoalStatus::Blocked => "blocked",
        GoalStatus::Completed => "completed",
    };
    match &state.reason {
        Some(reason) => format!("Goal {status}: {goal} ({reason})"),
        None => format!("Goal {status}: {goal}"),
    }
}

impl CommandHook for GoalHook {
    fn name(&self) -> &'static str {
        "Goal"
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn handle_command(
        &mut self,
        context: &mut AgentContext<'_>,
        command: &AgentCommand,
        mut cancellation: Cancellation,
    ) -> Result<Option<CommandResult>, HookError> {
        let AgentCommand::Goal(command) = command else {
            return Ok(None);
        };
        let Some(operation) = build_command_operation(command) else {
            let state = load_goal_state(context).await?;
            return Ok(Some(CommandResult {
                outcome: CommandOutcome::Succeeded,
                output: Some(describe_goal_state(&state)),
            }));
        };

        // Only running a goal starts a run; pause and clear leave stopping to
        // the model, which settles once it replies without tools.
        let start_run = matches!(command, GoalCommand::Set { .. } | GoalCommand::Resume)
            && context.agent().state != AgentState::Running;
        let result = self
            .handle
            .execute_with(operation, context, &mut cancellation, |previous, next| {
                let note = build_command_note(command, previous, next);
                let mut events = vec![NewEvent::new(EventType::Message(note))];
                if start_run {
                    events.push(status_event(AgentState::Running));
                }
                events
            })
            .await;

        let (outcome, output) = match result {
            Ok(execution) if execution.changed => (CommandOutcome::Succeeded, execution.message),
            Ok(execution) => (CommandOutcome::Failed, execution.message),
            Err(message) if cancellation.is_cancelled() => (CommandOutcome::Interrupted, message),
            Err(message) => (CommandOutcome::Failed, message),
        };
        Ok(Some(CommandResult {
            outcome,
            output: Some(output),
        }))
    }
}

impl Hook for GoalHook {
    fn name(&self) -> &'static str {
        "Goal"
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
    use psa::api::events::MessageType;
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
        use crate::{bundles::goal::load_goal_state, test_support as support};
        use psa::api::events::{InferenceResultEnd, InferenceResultStart};
        use psa_agents::{
            NewAgent, agent_manager::AgentManagerState, probes::ProbeRegistry,
            publication::Publication, supervisor::Runtime,
        };
        use psa_models::ProviderRegistry;
        use psa_store::{Database, NewSession};
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
            std::sync::Arc::new(crate::bundles::BuiltinComponents),
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
                let mut cursor = database.write().await.unwrap();
                let name = format!(
                    "goal-{interactive}-{interactive_only}-{reason:?}-{active}-{completed}-{interrupted}-{cancelled}"
                );
                let definition = cursor
                    .create_agent_definition(psa_store::CreateAgentDefinition {
                        name,
                        bundles: vec![psa::api::BundleConfig::Goal { interactive_only }],
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
                    crate::bundles::install_bundles(&definition, &agent, &session, None)
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
                        .find(|tool| tool.id() == psa::api::ToolId::SetGoal)
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
                            .find(|tool| tool.id() == psa::api::ToolId::SetGoalStatus)
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
                        matches!(&result.kind, EventType::Result(psa::api::AgentResult::Text(text)) if text == "answer")
                    );
                    assert!(events.windows(2).any(|pair| {
                        pair[0].id == result.id
                            && matches!(&pair[1].kind, EventType::StatusChange(status) if status.new_status == AgentState::Idle)
                    }));
                }
                let mut cursor = database.read().await.unwrap();
                assert_eq!(cursor.get_agent(agent.id).await.unwrap().state, expected);
                cursor.close().await.unwrap();
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

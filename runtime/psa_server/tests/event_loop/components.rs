use super::*;
use wire::{AgentConfig, BundleConfig, CreateAgent};

/// Startup and ordinary completion, followed by `extra` bundles.
fn bundles(extra: impl IntoIterator<Item = BundleConfig>) -> Vec<BundleConfig> {
    let mut bundles = vec![startup(), BundleConfig::StopWithoutTools];
    bundles.extend(extra);
    bundles
}

async fn create_session(harness: &Harness) -> wire::Session {
    harness
        .client
        .create_session(&wire::CreateSession {
            decision_model: None,
            model_config: Some(model()),
            probe_id: wire::ProbeId(wire::probes::Uuid::nil()),
            project_id: None,
            name: "component features".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap()
}

fn text(result: &[llm_api::api::ContentBlock]) -> String {
    result
        .iter()
        .filter_map(|block| match block {
            api::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_steering_wait(result: &wire::events::ToolResult) {
    assert!(!result.error, "{}", text(&result.content));
    assert_eq!(
        text(&result.content),
        "A message arrived; it follows this result."
    );
}

#[tokio::test]
async fn goal_continues_after_a_final_answer_and_stops_after_explicit_completion() {
    let mut harness = Harness::start(vec![
        Script::Calls(&[(
            "goal-set",
            "set_goal",
            r#"{"goal":"Verify the implementation"}"#,
        )]),
        Script::Text,
        Script::Calls(&[(
            "goal-done",
            "set_goal_status",
            r#"{"status":"completed","reason":"The implementation checks passed"}"#,
        )]),
        Script::Text,
    ])
    .await;
    let definition = harness
        .upsert_config(AgentConfig {
            bundles: vec![
                startup(),
                BundleConfig::Goal {
                    interactive_only: true,
                },
            ],
            ..agent_config("goal agent")
        })
        .await;
    let session = create_session(&harness).await;
    let agent = harness
        .client
        .create_agent(
            session.id,
            &CreateAgent {
                model: None,
                subagent_parent_id: None,
                interactive: true,
                hidden: false,
                name: "main".into(),
                config_id: definition.config_id,
            },
        )
        .await
        .unwrap();
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;

    let page = harness
        .client
        .list_events(session.id, Some(agent.id), &Default::default())
        .await
        .unwrap();
    let results: Vec<_> = page
        .events
        .iter()
        .filter_map(|event| match &event.event.kind {
            EventType::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| !result.error));
    assert_eq!(
        text(&results[0].content),
        "Goal running: Verify the implementation"
    );
    assert_eq!(
        text(&results[1].content),
        "Goal completed and cleared: Verify the implementation"
    );
    assert!(page.events.iter().any(|event| matches!(&event.event.kind,
        EventType::Message(message) if message.message_type == MessageType::Harness && text(&message.content).contains("Current goal:"))));
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );
    let results = harness
        .client
        .list_results(session.id, agent.id, 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].value, wire::AgentResult::Text("hello".into()));
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_secs(5), harness.requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(harness.requests.try_recv().is_err());
    harness.close().await;
}

async fn create_goal_agent(harness: &Harness, interactive: bool) -> (wire::Session, wire::Agent) {
    let definition = harness
        .upsert_config(AgentConfig {
            bundles: vec![
                startup(),
                BundleConfig::Goal {
                    interactive_only: true,
                },
            ],
            ..agent_config("goal agent")
        })
        .await;
    let session = create_session(harness).await;
    let agent = harness
        .client
        .create_agent(
            session.id,
            &CreateAgent {
                model: None,
                subagent_parent_id: None,
                interactive,
                hidden: false,
                name: "main".into(),
                config_id: definition.config_id,
            },
        )
        .await
        .unwrap();
    (session, agent)
}

async fn send_goal(
    harness: &Harness,
    agent: &wire::Agent,
    delivery: wire::CommandDelivery,
    command: wire::GoalCommand,
) -> wire::CommandResult {
    harness
        .client
        .send_command(
            agent.session_id,
            agent.id,
            delivery,
            wire::AgentCommand::Goal(command),
        )
        .await
        .unwrap()
}

fn harness_notes(events: &[wire::EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.event.kind {
            EventType::Message(message) if message.message_type == MessageType::Harness => {
                Some(text(&message.content))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn goal_commands_start_runs_and_leave_stopping_to_the_model() {
    // Unscripted requests answer with plain text, so a running goal keeps
    // continuing until a command stops it.
    let mut harness = Harness::start(Vec::new()).await;
    let (session, agent) = create_goal_agent(&harness, true).await;
    harness.wait_stopped(session.id).await;

    let set = send_goal(
        &harness,
        &agent,
        wire::CommandDelivery::Queue,
        wire::GoalCommand::Set {
            goal: "Verify the release".into(),
        },
    )
    .await;
    assert_eq!(set.outcome, wire::CommandOutcome::Succeeded);
    assert_eq!(
        set.output.as_deref(),
        Some("Goal running: Verify the release")
    );
    let first = harness.requests.recv().await.unwrap().to_string();
    assert!(first.contains("The user set a goal"), "{first}");

    // Pausing never stops the run; the model settles after the note.
    let pause = send_goal(
        &harness,
        &agent,
        wire::CommandDelivery::Steer,
        wire::GoalCommand::Pause { reason: None },
    )
    .await;
    assert_eq!(pause.outcome, wire::CommandOutcome::Succeeded);
    harness.wait_stopped(session.id).await;
    let show = send_goal(
        &harness,
        &agent,
        wire::CommandDelivery::Queue,
        wire::GoalCommand::Show,
    )
    .await;
    assert_eq!(
        show.output.as_deref(),
        Some("Goal blocked: Verify the release (Paused by the user)")
    );

    let resume = send_goal(
        &harness,
        &agent,
        wire::CommandDelivery::Queue,
        wire::GoalCommand::Resume,
    )
    .await;
    assert_eq!(resume.outcome, wire::CommandOutcome::Succeeded);
    let again = send_goal(
        &harness,
        &agent,
        wire::CommandDelivery::Steer,
        wire::GoalCommand::Resume,
    )
    .await;
    assert_eq!(again.outcome, wire::CommandOutcome::Failed);
    assert_eq!(
        again.output.as_deref(),
        Some("Only a blocked goal can be resumed.")
    );
    let clear = send_goal(
        &harness,
        &agent,
        wire::CommandDelivery::Steer,
        wire::GoalCommand::Clear { reason: None },
    )
    .await;
    assert_eq!(clear.outcome, wire::CommandOutcome::Succeeded);
    harness.wait_stopped(session.id).await;
    let show = send_goal(
        &harness,
        &agent,
        wire::CommandDelivery::Queue,
        wire::GoalCommand::Show,
    )
    .await;
    assert_eq!(show.output.as_deref(), Some("No current goal."));
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );

    let page = harness
        .client
        .list_events(session.id, Some(agent.id), &Default::default())
        .await
        .unwrap();
    let notes: Vec<_> = harness_notes(&page.events)
        .into_iter()
        .filter(|note| note.starts_with("The user"))
        .collect();
    assert_eq!(
        notes,
        [
            r#"The user set a goal: "Verify the release". Work on it until it is completed or blocked."#,
            r#"The user paused the goal: "Verify the release". Reason: Paused by the user. Stop working on it and wait until the user resumes it."#,
            r#"The user resumed the goal: "Verify the release". Continue working on it until it is completed or blocked."#,
            r#"The user cleared the goal: "Verify the release". It is no longer active; stop working on it."#,
        ]
    );
    // Only set and resume start runs; pause and clear settle with ordinary results.
    let starts = page
        .events
        .iter()
        .filter(|event| {
            matches!(&event.event.kind,
            EventType::StatusChange(change) if change.new_status == AgentState::Running)
        })
        .count();
    assert_eq!(starts, 2);
    let results = harness
        .client
        .list_results(session.id, agent.id, 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    harness.close().await;
}

#[tokio::test]
async fn goal_commands_require_goal_tools_installed_for_the_agent() {
    let harness = Harness::start(Vec::new()).await;
    let (_, agent) = create_goal_agent(&harness, false).await;
    let error = harness
        .client
        .send_command(
            agent.session_id,
            agent.id,
            wire::CommandDelivery::Queue,
            wire::AgentCommand::Goal(wire::GoalCommand::Show),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Goal bundle"), "{error}");
    harness.close().await;
}

#[tokio::test]
async fn parallel_wait_then_create_receives_child_completion_and_keeps_agents_addressable() {
    let mut harness = Harness::start(vec![
        Script::Calls(&[
            ("wait-child", "wait_message", r#"{"timeout":10}"#),
            (
                "create-child",
                "create_agent",
                r#"{"name":"worker-one","task":"Say hello","agent_template":"worker"}"#,
            ),
        ]),
        Script::Text,
        Script::Text,
    ])
    .await;
    let worker = harness
        .upsert_config(AgentConfig {
            bundles: bundles([BundleConfig::Subagents]),
            ..agent_config("worker")
        })
        .await;
    let definition = harness
        .upsert_config(AgentConfig {
            bundles: bundles([BundleConfig::Subagents]),
            subagents: wire::IncludedItems {
                items: vec![worker.config_id.clone()],
                ..Default::default()
            },
            ..agent_config("coordinator")
        })
        .await;
    let session = create_session(&harness).await;
    let parent = harness
        .client
        .create_agent(
            session.id,
            &CreateAgent {
                model: None,
                subagent_parent_id: None,
                interactive: true,
                hidden: false,
                name: "main".into(),
                config_id: definition.config_id,
            },
        )
        .await
        .unwrap();
    harness
        .client
        .submit_request(session.id, parent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;

    let agents = harness.client.list_agents(session.id).await.unwrap();
    assert_eq!(agents.len(), 2);
    let child = agents.iter().find(|agent| agent.id != parent.id).unwrap();
    assert_eq!(child.name, "worker-one");
    assert!(!child.interactive);
    assert_eq!(
        child.forked_from, None,
        "subagent parentage must not be stored as fork ancestry"
    );
    let child_snapshot = harness
        .client
        .get_resolved_agent_config(child.definition_id)
        .await
        .unwrap();
    assert_eq!(child_snapshot.config.config_id, worker.config_id);
    assert_eq!(child.subagent_parent_id, Some(parent.id));
    assert_eq!(child.state, AgentState::Idle);
    let page = harness
        .client
        .list_events(session.id, Some(parent.id), &Default::default())
        .await
        .unwrap();
    let results: Vec<_> = page
        .events
        .iter()
        .filter_map(|event| match &event.event.kind {
            EventType::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|result| !result.error),
        "{:?}",
        results
            .iter()
            .map(|result| text(&result.content))
            .collect::<Vec<_>>()
    );
    let wait = results
        .iter()
        .find(|result| result.call_id == "wait-child")
        .unwrap();
    assert_steering_wait(wait);
    assert!(page.events.iter().any(|event| matches!(&event.event.kind,
        EventType::Message(message) if message.message_type == MessageType::Subagent && message.source.as_ref().map(|source| source.sender) == Some(child.id) && text(&message.content).contains("hello"))));
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(5), harness.requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        harness.requests.try_recv().is_err(),
        "completion should not be delivered twice"
    );
    let child_page = harness
        .client
        .list_events(session.id, Some(child.id), &Default::default())
        .await
        .unwrap();
    assert!(
        !child_page
            .events
            .iter()
            .any(|event| matches!(&event.event.kind,
        EventType::ToolDefinition(tool) if tool.id == ToolId::CreateAgent))
    );
    assert!(
        child_page
            .events
            .iter()
            .any(|event| matches!(&event.event.kind,
        EventType::ToolDefinition(tool) if tool.id == ToolId::SendMessage))
    );

    // A later parent worker generation reuses the session coordinator and wakes
    // the existing child rather than creating a new agent/conversation. Only
    // expose this phase's script after checking phase one's exact request count.
    harness.scripts.lock().await.extend([
        Script::Calls(&[
            ("wait-again", "wait_message", r#"{"timeout":10}"#),
            (
                "send-again",
                "send_message",
                r#"{"agent_name":"worker-one","message":"Run a second task"}"#,
            ),
        ]),
        Script::Text,
        Script::Text,
    ]);
    harness
        .client
        .submit_request(session.id, parent.id, &message(MessageDelivery::Steer))
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    let resumed = harness.client.list_agents(session.id).await.unwrap();
    assert_eq!(resumed.len(), 2);
    assert!(
        resumed
            .iter()
            .any(|agent| agent.id == child.id && agent.state == AgentState::Idle)
    );
    let parent_page = harness
        .client
        .list_events(session.id, Some(parent.id), &Default::default())
        .await
        .unwrap();
    let resumed_wait = parent_page
        .events
        .iter()
        .find_map(|event| match &event.event.kind {
            EventType::ToolResult(result) if result.call_id == "wait-again" => Some(result),
            _ => None,
        })
        .unwrap();
    assert_steering_wait(resumed_wait);
    let child_page = harness
        .client
        .list_events(session.id, Some(child.id), &Default::default())
        .await
        .unwrap();
    assert!(child_page.events.iter().any(|event| matches!(&event.event.kind,
        EventType::Message(message) if message.message_type == MessageType::Subagent && message.source.as_ref().map(|source| source.sender) == Some(parent.id) && text(&message.content).contains("Run a second task"))));
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(5), harness.requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        harness.requests.try_recv().is_err(),
        "resumed completion should not be duplicated"
    );
    harness.close().await;
}

#[tokio::test]
async fn subagent_creation_checks_permitted_definitions_without_creating_a_child() {
    let harness = Harness::start(vec![
        Script::Calls(&[(
            "not-allowed",
            "create_agent",
            r#"{"name":"worker-one","task":"Run","agent_template":"unrelated"}"#,
        )]),
        Script::Text,
    ])
    .await;
    let worker = harness
        .upsert_config(AgentConfig {
            bundles: bundles([]),
            ..agent_config("worker")
        })
        .await;
    let definition = harness
        .upsert_config(AgentConfig {
            bundles: bundles([BundleConfig::Subagents]),
            subagents: wire::IncludedItems {
                items: vec![worker.config_id.clone()],
                ..Default::default()
            },
            ..agent_config("coordinator")
        })
        .await;
    let session = create_session(&harness).await;
    let parent = harness
        .client
        .create_agent(
            session.id,
            &CreateAgent {
                model: None,
                subagent_parent_id: None,
                interactive: true,
                hidden: false,
                name: "main".into(),
                config_id: definition.config_id,
            },
        )
        .await
        .unwrap();
    harness
        .client
        .submit_request(session.id, parent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness.client.list_agents(session.id).await.unwrap().len(),
        1
    );
    let page = harness
        .client
        .list_events(session.id, Some(parent.id), &Default::default())
        .await
        .unwrap();
    assert!(page.events.iter().any(|event| matches!(&event.event.kind,
        EventType::ToolResult(result) if result.call_id == "not-allowed" && result.error)));
    harness.close().await;
}

#[tokio::test]
async fn subagent_tools_list_and_message_unrelated_roots_in_the_same_session() {
    let harness = Harness::start(vec![
        Script::Calls(&[
            ("list-session", "list_agents", r#"{"all":true}"#),
            (
                "send-peer",
                "send_message",
                r#"{"agent_name":"peer","message":"hello peer"}"#,
            ),
        ]),
        Script::Text,
        Script::Text,
    ])
    .await;
    let definition = harness
        .upsert_config(AgentConfig {
            bundles: bundles([BundleConfig::Subagents]),
            ..agent_config("messenger")
        })
        .await;
    let session = create_session(&harness).await;
    let parent = harness
        .client
        .create_agent(
            session.id,
            &CreateAgent {
                model: None,
                subagent_parent_id: None,
                config_id: definition.config_id,
                name: "main".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    let peer = harness
        .client
        .create_agent(
            session.id,
            &CreateAgent {
                model: None,
                subagent_parent_id: None,
                config_id: config_id("default"),
                name: "peer".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    harness
        .client
        .submit_request(session.id, parent.id, &message(MessageDelivery::Steer))
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    let page = harness
        .client
        .list_events(session.id, Some(parent.id), &Default::default())
        .await
        .unwrap();
    let list = page
        .events
        .iter()
        .find_map(|event| match &event.event.kind {
            EventType::ToolResult(result) if result.call_id == "list-session" => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(!list.error, "{}", text(&list.content));
    assert_eq!(text(&list.content).lines().count(), 2);
    let page = harness
        .client
        .list_events(session.id, Some(peer.id), &Default::default())
        .await
        .unwrap();
    assert!(page.events.iter().any(|event| matches!(&event.event.kind,
        EventType::Message(message) if message.message_type == MessageType::Subagent
            && message.source.as_ref().map(|source| source.sender) == Some(parent.id)
            && text(&message.content) == "hello peer")));
    harness.close().await;
}

use super::*;
use wire::{AgentConfig, BundleConfig, CreateAgent, HookConfig, HookConfigs};

fn hooks() -> HookConfigs {
    HookConfigs {
        agent_create: vec![HookConfig::DefaultStartup, HookConfig::ToolDefinitions],
        after_request: vec![HookConfig::StopWithoutTools],
        ..Default::default()
    }
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
            bundles: vec![BundleConfig::Goal {
                interactive_only: true,
            }],
            hooks: HookConfigs {
                after_request: vec![],
                ..hooks()
            },
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
            bundles: vec![BundleConfig::Subagents],
            hooks: hooks(),
            ..agent_config("worker")
        })
        .await;
    let definition = harness
        .upsert_config(AgentConfig {
            bundles: vec![BundleConfig::Subagents],
            subagents: wire::IncludedItems {
                items: vec![worker.config_id.clone()],
                ..Default::default()
            },
            hooks: hooks(),
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
        child.parent_id, None,
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
            hooks: hooks(),
            ..agent_config("worker")
        })
        .await;
    let definition = harness
        .upsert_config(AgentConfig {
            bundles: vec![BundleConfig::Subagents],
            subagents: wire::IncludedItems {
                items: vec![worker.config_id.clone()],
                ..Default::default()
            },
            hooks: hooks(),
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
            bundles: vec![BundleConfig::Subagents],
            hooks: hooks(),
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

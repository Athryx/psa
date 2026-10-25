use super::*;

async fn create_compacting_agent(harness: &Harness) -> (wire::Session, wire::Agent) {
    let mut config = agent_config("compacting");
    config.bundles = vec![wire::BundleConfig::Noop];
    config.hooks = wire::HookConfigs {
        agent_create: vec![
            wire::HookConfig::DefaultStartup,
            wire::HookConfig::ToolDefinitions,
            wire::HookConfig::ExtraSystemPrompt {
                prompt: "Keep this system instruction.".into(),
            },
        ],
        before_request: vec![wire::HookConfig::Compaction {
            reserve_tokens: 0,
            keep_recent_tokens: 2,
            context_window: Some(8),
        }],
        after_request: vec![wire::HookConfig::StopWithoutTools],
        ..Default::default()
    };
    harness.upsert_config(config).await;
    let (session, _) = harness.create_agent().await;
    harness.wait_stopped(session.id).await;
    let agent = harness
        .client
        .create_agent(
            session.id,
            &wire::NewConfiguredAgent {
                config_id: config_id("compacting"),
                name: "worker".into(),
                model: None,
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    harness
        .client
        .submit_request(
            session.id,
            agent.id,
            &AgentRequest::Message {
                delivery: MessageDelivery::Steer,
                content: psa_server::prompts::text("old work ".repeat(100)),
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    (session, agent)
}

async fn submit_recent(harness: &Harness, session: wire::SessionId, agent: wire::AgentId) {
    harness
        .client
        .submit_request(
            session,
            agent,
            &AgentRequest::Message {
                delivery: MessageDelivery::Steer,
                content: psa_server::prompts::text("next task"),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn compaction_runs_hidden_summarizers_preserves_snapshots_and_updates_prior_summary() {
    let mut harness = Harness::start(vec![Script::Text; 5]).await;
    let (session, agent) = create_compacting_agent(&harness).await;
    harness.requests.recv().await.unwrap();
    for count in 1..=2 {
        submit_recent(&harness, session.id, agent.id).await;
        harness.wait_stopped(session.id).await;
        let summary_request = harness.requests.recv().await.unwrap();
        let rendered = summary_request.to_string();
        assert!(rendered.contains("context summarization assistant"));
        assert!(!rendered.contains("expert coding assistant"));
        assert!(rendered.contains("[User]:"));
        assert_eq!(rendered.contains("<previous-summary>"), count == 2);
        assert!(
            summary_request
                .get("tools")
                .is_none_or(|tools| tools.as_array().unwrap().is_empty())
        );
        let next_request = harness.requests.recv().await.unwrap();
        let rendered = next_request.to_string();
        assert!(rendered.contains("Keep this system instruction."));
        assert!(rendered.contains("Previous context summary:"));
        assert!(rendered.contains("next task"));
        assert!(!rendered.contains("old work"));
        assert_eq!(next_request["tools"].as_array().unwrap().len(), 1);
        let agents = harness.client.list_agents(session.id).await.unwrap();
        let internal = agents
            .iter()
            .find(|agent| agent.name == format!("_internal_worker_compaction{count}"))
            .unwrap();
        assert!(internal.hidden);
        assert!(!internal.interactive);
        assert_eq!(internal.state, AgentState::Idle);
    }
    let mut cursor = harness.db.cursor().await.unwrap();
    let context = cursor.load_context_window(agent.id).await.unwrap();
    assert_eq!(
        context
            .iter()
            .filter(|event| matches!(event.kind, EventType::Compaction(_)))
            .count(),
        1
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn cancelled_compaction_stops_its_summarizer_without_replacing_context() {
    let mut harness = Harness::start(vec![Script::Text, Script::Wait]).await;
    let (session, agent) = create_compacting_agent(&harness).await;
    harness.requests.recv().await.unwrap();
    submit_recent(&harness, session.id, agent.id).await;
    let summary_request = tokio::time::timeout(Duration::from_secs(10), harness.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        summary_request
            .to_string()
            .contains("context summarization assistant")
    );
    harness
        .client
        .submit_request(session.id, agent.id, &AgentRequest::Interrupt)
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    let agents = harness.client.list_agents(session.id).await.unwrap();
    assert_eq!(
        agents.iter().find(|agent| agent.hidden).unwrap().state,
        AgentState::Interrupted
    );
    let mut cursor = harness.db.cursor().await.unwrap();
    let context = cursor.load_context_window(agent.id).await.unwrap();
    assert!(
        !context
            .iter()
            .any(|event| matches!(event.kind, EventType::Compaction(_)))
    );
    assert!(context.iter().any(|event| matches!(&event.kind, EventType::Message(message) if serde_json::to_string(message).unwrap().contains("old work"))));
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn incomplete_summary_never_replaces_parent_context() {
    let mut harness = Harness::start(vec![Script::Text, Script::TokenLimit]).await;
    let (session, agent) = create_compacting_agent(&harness).await;
    harness.requests.recv().await.unwrap();
    submit_recent(&harness, session.id, agent.id).await;
    harness.wait_stopped(session.id).await;
    let parent = harness
        .client
        .get_agent(session.id, agent.id)
        .await
        .unwrap();
    assert_eq!(parent.state, AgentState::Interrupted);
    let mut cursor = harness.db.cursor().await.unwrap();
    let context = cursor.load_context_window(agent.id).await.unwrap();
    assert!(
        !context
            .iter()
            .any(|event| matches!(event.kind, EventType::Compaction(_)))
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

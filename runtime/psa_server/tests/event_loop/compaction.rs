use super::*;
use psa_store::EventRange;

/// A tiny `context_window` compacts before every request; a large one only on command.
async fn create_compacting_agent(
    harness: &Harness,
    context_window: u64,
) -> (wire::Session, wire::Agent) {
    let mut config = agent_config("compacting");
    config.bundles = vec![
        startup(),
        wire::BundleConfig::ExtraSystemPrompt {
            prompt: "Keep this system instruction.".into(),
        },
        wire::BundleConfig::StopWithoutTools,
        wire::BundleConfig::FileTools { read_only: true },
        wire::BundleConfig::Compaction {
            reserve_tokens: 0,
            keep_recent_tokens: 2,
            context_window: Some(context_window),
        },
    ];
    harness.upsert_config(config).await;
    let (session, _) = harness.create_agent().await;
    harness.wait_stopped(session.id).await;
    let agent = harness
        .client
        .create_agent(
            session.id,
            &wire::CreateAgent {
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
                source: None,
                delivery: MessageDelivery::Steer,
                content: psa_agents::prompts::text("old work ".repeat(100)),
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
                source: None,
                delivery: MessageDelivery::Steer,
                content: psa_agents::prompts::text("next task"),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn compaction_runs_hidden_summarizers_preserves_snapshots_and_updates_prior_summary() {
    let mut harness = Harness::start(vec![Script::Text; 5]).await;
    let (session, agent) = create_compacting_agent(&harness, 8).await;
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
    let mut cursor = harness.db.read().await.unwrap();
    let context = cursor.load_context_window(agent.id).await.unwrap();
    assert_eq!(
        context
            .iter()
            .filter(|event| matches!(event.kind, EventType::Compaction(_)))
            .count(),
        1
    );
    cursor.close().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn cancelled_compaction_stops_its_summarizer_without_replacing_context() {
    let mut harness = Harness::start(vec![Script::Text, Script::Wait]).await;
    let (session, agent) = create_compacting_agent(&harness, 8).await;
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
    let mut cursor = harness.db.read().await.unwrap();
    let context = cursor.load_context_window(agent.id).await.unwrap();
    assert!(
        !context
            .iter()
            .any(|event| matches!(event.kind, EventType::Compaction(_)))
    );
    assert!(context.iter().any(|event| matches!(&event.kind, EventType::Message(message) if serde_json::to_string(message).unwrap().contains("old work"))));
    cursor.close().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn incomplete_summary_never_replaces_parent_context() {
    let mut harness = Harness::start(vec![Script::Text, Script::TokenLimit]).await;
    let (session, agent) = create_compacting_agent(&harness, 8).await;
    harness.requests.recv().await.unwrap();
    submit_recent(&harness, session.id, agent.id).await;
    harness.wait_stopped(session.id).await;
    let parent = harness
        .client
        .get_agent(session.id, agent.id)
        .await
        .unwrap();
    assert_eq!(parent.state, AgentState::Interrupted);
    let mut cursor = harness.db.read().await.unwrap();
    let context = cursor.load_context_window(agent.id).await.unwrap();
    assert!(
        !context
            .iter()
            .any(|event| matches!(event.kind, EventType::Compaction(_)))
    );
    cursor.close().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn compact_command_summarizes_idle_agent_without_starting_a_run() {
    let mut harness = Harness::start(vec![Script::Text; 3]).await;
    let (session, agent) = create_compacting_agent(&harness, 1_000_000).await;
    harness.requests.recv().await.unwrap();

    let root = harness
        .client
        .list_agents(session.id)
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.id != agent.id && !candidate.hidden)
        .unwrap();
    let compact = wire::AgentCommand::Compact {
        instructions: Some("Focus on the old work.".into()),
    };
    assert!(
        harness
            .client
            .send_command(
                session.id,
                root.id,
                wire::CommandDelivery::Steer,
                compact.clone()
            )
            .await
            .is_err(),
        "agents without the Compaction bundle reject the command"
    );

    // The request stays open until compaction finishes.
    let client = harness.client.clone();
    let (session_id, agent_id) = (session.id, agent.id);
    let command = compact.clone();
    let result = tokio::spawn(async move {
        client
            .send_command(session_id, agent_id, wire::CommandDelivery::Steer, command)
            .await
    });
    let summary_request = harness.requests.recv().await.unwrap().to_string();
    assert!(summary_request.contains("context summarization assistant"));
    assert!(summary_request.contains("Additional instructions for this summary:"));
    assert!(summary_request.contains("Focus on the old work."));
    harness.wait_stopped(session.id).await;
    let result = result.await.unwrap().unwrap();
    assert_eq!(result.outcome, wire::CommandOutcome::Succeeded);
    let worker = harness
        .client
        .get_agent(session.id, agent.id)
        .await
        .unwrap();
    assert_eq!(worker.state, AgentState::Idle);

    let mut cursor = harness.db.read().await.unwrap();
    let events = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    let command = events
        .iter()
        .position(
            |event| matches!(&event.kind, EventType::Command(recorded) if *recorded == wire::events::CommandRecord::new(&compact, result.clone())),
        )
        .unwrap();
    let compaction = events
        .iter()
        .position(|event| matches!(event.kind, EventType::Compaction(_)))
        .unwrap();
    // Recorded once finished, after the compaction it produced.
    assert!(compaction < command);
    assert!(!events[command..].iter().any(|event| matches!(
        &event.kind,
        EventType::StatusChange(change) if change.new_status == AgentState::Running
    )));
    cursor.close().await.unwrap();

    submit_recent(&harness, session.id, agent.id).await;
    let next_request = harness.requests.recv().await.unwrap().to_string();
    assert!(next_request.contains("Previous context summary:"));
    assert!(next_request.contains("next task"));
    assert!(!next_request.contains("old work"));
    harness.wait_stopped(session.id).await;
    harness.close().await;
}

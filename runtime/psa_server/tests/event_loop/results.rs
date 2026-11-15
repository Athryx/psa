use super::*;
use wire::{AgentResult, BundleConfig, HookConfig, HookConfigs};

#[derive(Debug, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Answer {
    count: u32,
}

fn typed_config(name: &str, max_retries: u32) -> wire::AgentConfig {
    wire::AgentConfig {
        bundles: vec![BundleConfig::typed_result_with_retries::<Answer>(
            max_retries,
        )],
        hooks: HookConfigs {
            agent_create: vec![HookConfig::DefaultStartup, HookConfig::ToolDefinitions],
            after_request: vec![],
            ..Default::default()
        },
        ..agent_config(name)
    }
}

async fn create_configured(
    harness: &Harness,
    config: wire::AgentConfig,
) -> (wire::Session, wire::Agent) {
    let config_id = harness.upsert_config(config).await.config_id;
    let session = harness
        .client
        .create_session(&wire::CreateSession {
            decision_model: None,
            name: "results".into(),
            working_directory: "/work".into(),
            probe_id: support::probe_metadata().id,
            project_id: None,
            model_config: Some(model()),
        })
        .await
        .unwrap();
    let agent = harness
        .client
        .create_agent(
            session.id,
            &wire::NewConfiguredAgent {
                config_id,
                name: "result producer".into(),
                model: None,
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    (session, agent)
}

async fn run(harness: &Harness, session: wire::SessionId, agent: wire::AgentId) {
    harness
        .client
        .submit_request(session, agent, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.wait_stopped(session).await;
}

#[tokio::test]
async fn ordinary_runs_record_one_text_result_including_token_limited_answers() {
    let mut harness =
        Harness::start(vec![Script::TextTools, Script::Text, Script::TokenLimit]).await;
    let (session, agent) = harness.create_agent().await;
    harness.wait_stopped(session.id).await;
    assert!(
        harness
            .client
            .last_result_raw(session.id, agent.id)
            .await
            .unwrap()
            .is_none()
    );
    run(&harness, session.id, agent.id).await;
    let first = harness
        .client
        .last_result::<String>(session.id, agent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.value, "hello");
    let transcript = harness
        .client
        .events(session.id, Some(agent.id), &Default::default())
        .await
        .unwrap();
    assert!(transcript.events.iter().any(|event| matches!(&event.event.kind,
        EventType::Message(message) if message.message_type == MessageType::Agent
            && matches!(&message.content[..], [api::ContentBlock::Text(text)] if text.text == "intermediate message"))));
    assert_eq!(
        transcript
            .events
            .iter()
            .filter(|event| matches!(event.event.kind, EventType::Result(_)))
            .count(),
        1
    );
    run(&harness, session.id, agent.id).await;
    let latest = harness
        .client
        .last_results::<String>(session.id, agent.id, 2)
        .await
        .unwrap();
    assert_eq!(latest.len(), 2);
    assert!(latest[0].event_id > latest[1].event_id);
    assert_eq!(latest[1], first);
    assert_eq!(latest[0].value, "hello");
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );
    let all = harness
        .client
        .events(session.id, Some(agent.id), &Default::default())
        .await
        .unwrap();
    assert!(all.events.iter().any(|event| matches!(&event.event.kind,
        EventType::InferenceResultEnd(end) if matches!(end.outcome, api::Outcome::Incomplete { reason: api::StopReason::TokenLimit }))));
    for limit in [0, 10001] {
        assert!(matches!(
            harness.client.results(session.id, agent.id, limit).await,
            Err(wire::ClientError::Api { status: 400, .. })
        ));
    }
    assert!(matches!(
        harness
            .client
            .last_result::<Answer>(session.id, agent.id)
            .await,
        Err(wire::ClientError::Json(_))
    ));

    let raw: Vec<wire::ResultRecord> = reqwest::Client::new()
        .get(format!(
            "http://{}/sessions/{}/agents/{}/results",
            harness.address, session.id.0, agent.id.0
        ))
        .bearer_auth("test-client-key")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(raw.len(), 1, "omitted result limit defaults to one");
    assert_eq!(raw[0].value, AgentResult::Text("hello".into()));
    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::ThroughEvent {
                    agent_id: agent.id,
                    event_id: first.event_id,
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(fork.session.id).await;
    let child = &fork.agents[0];
    assert_eq!(child.state, AgentState::Idle);
    let inherited = harness
        .client
        .last_results::<String>(fork.session.id, child.id, 10)
        .await
        .unwrap();
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].source_agent_id, agent.id);
    assert_eq!(inherited[0].agent_id, child.id);
    assert_eq!(inherited[0].event_id, first.event_id);
    assert!(matches!(
        harness.client.results(session.id, child.id, 1).await,
        Err(wire::ClientError::Api { status: 404, .. })
    ));

    // Retain the database directory while shutting down the original HTTP runtime.
    let directory = std::mem::replace(&mut harness._dir, tempfile::tempdir().unwrap());
    harness.close().await;
    let db = Database::open(directory.path().join("test.sqlite3"))
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let providers = ProviderRegistry::default().with_catalog_source(
        psa_server::models::CatalogSource::new(&format!("http://{address}")).unwrap(),
    );
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(psa_server::server::serve_with_configuration(
        listener,
        db,
        providers,
        ProbeRegistry::start("restart".into()),
        psa_server::config_registry::ConfigRegistry::new(directory.path().join("config")),
        model(),
        "test-client-key".into(),
        async {
            let _ = stopped.await;
        },
    ));
    let client = wire::Client::new(&format!("http://{address}"), "test-client-key").unwrap();
    assert_eq!(
        client
            .last_results::<String>(session.id, agent.id, 2)
            .await
            .unwrap(),
        latest
    );
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn result_only_pages_and_sse_filter_before_pagination_without_changing_cursors() {
    let harness = Harness::start(vec![Script::Text, Script::Text, Script::Text]).await;
    let (session, agent) = harness.create_agent().await;
    harness.wait_stopped(session.id).await;
    for _ in 0..2 {
        run(&harness, session.id, agent.id).await;
    }
    let newest = harness
        .client
        .results(session.id, agent.id, 10)
        .await
        .unwrap();
    for scope in [None, Some(agent.id)] {
        let mut query = wire::EventsQuery {
            results_only: true,
            include_pending: true,
            limit: 1,
            ..Default::default()
        };
        let mut ids = Vec::new();
        loop {
            let page = harness
                .client
                .events(session.id, scope, &query)
                .await
                .unwrap();
            assert!(page.pending.is_empty());
            assert!(page.events.len() <= 1);
            for event in &page.events {
                assert!(matches!(event.event.kind, EventType::Result(_)));
                ids.push(event.event.id);
            }
            if !page.has_more {
                break;
            }
            assert!(page.next_after > query.after);
            query.after = page.next_after;
        }
        assert_eq!(
            ids,
            newest
                .iter()
                .rev()
                .map(|record| record.event_id)
                .collect::<Vec<_>>()
        );
    }
    let full = harness
        .client
        .events(session.id, Some(agent.id), &Default::default())
        .await
        .unwrap();
    let mut stream = harness
        .client
        .stream_events(
            session.id,
            Some(agent.id),
            &wire::EventsQuery {
                results_only: true,
                include_pending: true,
                limit: 1,
                through: Some(full.head),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let mut replayed = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
    {
        match event.unwrap().message {
            StreamMessage::Committed { event, .. } => {
                assert!(matches!(event.event.kind, EventType::Result(_)));
                replayed.push(event.event.id);
            }
            StreamMessage::Snapshot { pending, .. } => assert!(pending.is_empty()),
            other => panic!("unexpected finite result stream notification: {other:?}"),
        }
    }
    assert_eq!(
        replayed,
        newest
            .iter()
            .rev()
            .map(|record| record.event_id)
            .collect::<Vec<_>>()
    );
    let mut live = harness
        .client
        .stream_events(
            session.id,
            Some(agent.id),
            &wire::EventsQuery {
                after: full.head,
                results_only: true,
                include_pending: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        live.next().await.unwrap().unwrap().message,
        StreamMessage::Snapshot { .. }
    ));
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match live.next().await.unwrap().unwrap().message {
                StreamMessage::Committed { event, .. } => {
                    assert!(matches!(event.event.kind, EventType::Result(_)));
                    break;
                }
                StreamMessage::Upsert { .. } | StreamMessage::Removed { .. } => {
                    panic!("result streams must not expose provisional drafts")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .results(session.id, agent.id, 10)
            .await
            .unwrap()
            .len(),
        3
    );
    drop(live);
    drop(stream);
    harness.close().await;
}

#[tokio::test]
async fn typed_result_retries_invalid_and_missing_calls_then_persists_the_exact_object() {
    let harness = Harness::start(vec![
        Script::Calls(&[("invalid", "record_result", r#"{"count":"wrong"}"#)]),
        Script::TokenLimit,
        Script::Calls(&[("valid", "record_result", r#"{"count":4}"#)]),
    ])
    .await;
    let (session, agent) = create_configured(&harness, typed_config("typed", 3)).await;
    run(&harness, session.id, agent.id).await;
    let result = harness
        .client
        .last_result::<Answer>(session.id, agent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.value, Answer { count: 4 });
    assert_eq!(
        harness
            .client
            .results(session.id, agent.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let page = harness
        .client
        .events(session.id, Some(agent.id), &Default::default())
        .await
        .unwrap();
    assert_eq!(
        page.events
            .iter()
            .filter(|event| matches!(event.event.kind, EventType::InferenceResultStart(_)))
            .count(),
        3
    );
    assert!(page.events.iter().any(|event| matches!(&event.event.kind, EventType::ToolResult(result) if result.call_id == "invalid" && result.error)));
    let metadata = page
        .events
        .iter()
        .find_map(|event| match &event.event.kind {
            EventType::ToolDefinition(tool) if tool.id == ToolId::RecordResult => Some(tool),
            _ => None,
        })
        .unwrap();
    assert!(
        serde_json::to_string(&metadata.definition.parameters)
            .unwrap()
            .contains("count")
    );
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );
    harness.close().await;
}

#[tokio::test]
async fn typed_result_retry_budgets_stop_without_fabricating_results_and_reset_on_new_runs() {
    for max_retries in [0, 3] {
        let harness = Harness::start(vec![Script::Text; max_retries as usize + 1]).await;
        let (session, agent) =
            create_configured(&harness, typed_config("budget", max_retries)).await;
        run(&harness, session.id, agent.id).await;
        assert_eq!(
            harness
                .client
                .get_agent(session.id, agent.id)
                .await
                .unwrap()
                .state,
            AgentState::Interrupted
        );
        assert!(
            harness
                .client
                .last_result_raw(session.id, agent.id)
                .await
                .unwrap()
                .is_none()
        );
        let page = harness
            .client
            .events(session.id, Some(agent.id), &Default::default())
            .await
            .unwrap();
        assert_eq!(
            page.events
                .iter()
                .filter(|event| matches!(event.event.kind, EventType::InferenceResultStart(_)))
                .count(),
            max_retries as usize + 1
        );
        harness.scripts.lock().await.push_back(Script::Calls(&[(
            "fresh",
            "record_result",
            r#"{"count":8}"#,
        )]));
        run(&harness, session.id, agent.id).await;
        assert_eq!(
            harness
                .client
                .last_result::<Answer>(session.id, agent.id)
                .await
                .unwrap()
                .unwrap()
                .value,
            Answer { count: 8 }
        );
        harness.close().await;
    }
}

#[tokio::test]
async fn incompatible_completion_modes_and_nonobject_result_schemas_are_rejected_before_creation() {
    let harness = Harness::start(vec![]).await;
    let typed = BundleConfig::typed_result::<Answer>();
    for (bundles, after_request) in [
        (
            vec![BundleConfig::Goal {
                interactive_only: true,
            }],
            vec![HookConfig::StopWithoutTools],
        ),
        (vec![typed.clone()], vec![HookConfig::StopWithoutTools]),
        (
            vec![
                BundleConfig::Goal {
                    interactive_only: false,
                },
                typed,
            ],
            vec![],
        ),
        (
            vec![BundleConfig::TypedResult {
                schema: json!({"type":"array","items":{"type":"string"}}).into(),
                max_retries: 3,
            }],
            vec![],
        ),
    ] {
        let mut config = agent_config("invalid-completion");
        config.bundles = bundles;
        config.hooks.after_request = after_request;
        assert!(matches!(
            harness.client.upsert_agent_configs(&[config]).await,
            Err(wire::ClientError::Api { status: 400, .. })
        ));
    }
    assert!(matches!(
        harness
            .client
            .get_agent_config(&config_id("invalid-completion"))
            .await,
        Err(wire::ClientError::Api { status: 404, .. })
    ));
    harness.close().await;
}

#[tokio::test]
async fn interrupting_typed_completion_does_not_create_a_result() {
    let mut harness = Harness::start(vec![Script::Wait]).await;
    let (session, agent) = create_configured(&harness, typed_config("cancelled", 3)).await;
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), harness.requests.recv())
        .await
        .unwrap()
        .unwrap();
    harness
        .client
        .submit_request(session.id, agent.id, &wire::AgentRequest::Interrupt)
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Interrupted
    );
    assert!(
        harness
            .client
            .results(session.id, agent.id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    harness.close().await;
}

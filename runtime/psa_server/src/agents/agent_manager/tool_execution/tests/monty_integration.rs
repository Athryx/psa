//! Full worker/tool/checkpoint integration using an explicitly configured worker.

use super::*;
use crate::components::{
    bundles::codemode::MontyEngine,
    codemode_functions::CodeModeFunctionKind,
    tools::{CodeMode, Noop},
};
use llm_api::api::ContentBlock;
use serde_json::json;

async fn append_tool_call(
    state: &mut AgentManagerState,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) {
    let inference_id = state.agent.next_event_id.0 as u64;
    state
        .persist_events(&[
            NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                id: inference_id,
                model_config: support::model_config(),
                request_config: Default::default(),
            })),
            NewEvent::new(EventType::ToolCall(ToolCall {
                call_id: id.into(),
                tool_name: name.into(),
                arguments: arguments.to_string(),
                metadata: Default::default(),
            })),
            NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                id: inference_id,
                outcome: Outcome::Completed {
                    reason: api::StopReason::ToolCalls,
                },
                usage: Default::default(),
                request_time: Duration::ZERO,
                response_id: None,
                provider_metadata: Default::default(),
            })),
        ])
        .await
        .unwrap();
}

async fn invoke_codemode(
    fixture: &Fixture,
    mut state: AgentManagerState,
    id: &str,
    code: &str,
) -> AgentManagerState {
    append_tool_call(&mut state, id, "codemode", json!({"code": code})).await;
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    drop(controls);
    state
}

async fn load_tool_result(
    fixture: &Fixture,
    state: &AgentManagerState,
    id: &str,
) -> (bool, String) {
    let mut cursor = fixture.database.cursor().await.unwrap();
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    cursor.rollback().await.unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) if result.call_id == id => Some(result),
            _ => None,
        })
        .expect("tool result persisted");
    let text = result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("unexpected content: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (result.error, text)
}

#[tokio::test]
async fn real_worker_persists_snapshots_errors_output_and_registered_functions() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let mut state = fixture
        .state(vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))], &[])
        .await;
    state.codemode_functions = vec![CodeModeFunctionKind::Tool(ToolKind::Noop(Noop))].into();

    state = invoke_codemode(&fixture, state, "first", "x = 41\nprint('first')").await;
    let (error, text) = load_tool_result(&fixture, &state, "first").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "first");
    let mut cursor = fixture.database.cursor().await.unwrap();
    assert!(
        cursor
            .load_latest_private_event(
                state.agent.id,
                None,
                crate::db::PrivateEventKind::CodeModeSnapshot,
            )
            .await
            .unwrap()
            .is_some()
    );
    cursor.rollback().await.unwrap();

    state = invoke_codemode(&fixture, state, "second", "print(x + 1)").await;
    let (error, text) = load_tool_result(&fixture, &state, "second").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "42");

    state = invoke_codemode(&fixture, state, "final-expression", "x").await;
    let (error, text) = load_tool_result(&fixture, &state, "final-expression").await;
    assert!(!error, "{text}");
    assert!(
        text.is_empty(),
        "final expression must not be printed: {text}"
    );

    state = invoke_codemode(
        &fixture,
        state,
        "exception",
        "x = 99\nprint('before error')\nraise ValueError('broken')",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "exception").await;
    assert!(error, "{text}");
    assert!(text.contains("before error"), "{text}");
    assert!(
        text.contains("ValueError") && text.contains("broken"),
        "{text}"
    );

    state = invoke_codemode(&fixture, state, "after-error", "print(x)").await;
    let (error, text) = load_tool_result(&fixture, &state, "after-error").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "99");

    state = invoke_codemode(&fixture, state, "noop", "import asyncio\nasync def main():\n    result = await noop()\n    print('noop result:', repr(result))\nasyncio.run(main())").await;
    let (error, text) = load_tool_result(&fixture, &state, "noop").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "noop result: ''");

    state = invoke_codemode(
        &fixture,
        state,
        "bad-arguments",
        "asyncio.run(noop(unexpected=True))",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "bad-arguments").await;
    assert!(error, "{text}");
    assert!(text.contains("RuntimeError"), "{text}");

    state = invoke_codemode(
        &fixture,
        state,
        "caught-host-error",
        "async def handle_error():\n    try:\n        await noop(unexpected=True)\n    except RuntimeError:\n        print('caught host error')\nasyncio.run(handle_error())",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "caught-host-error").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "caught host error");

    state = invoke_codemode(
        &fixture,
        state,
        "gather",
        "async def both():\n    return await asyncio.gather(noop(), noop())\nprint(asyncio.run(both()))",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "gather").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "['', '']");

    state = invoke_codemode(&fixture, state, "large-output", "print('a' * 13000)").await;
    let (error, text) = load_tool_result(&fixture, &state, "large-output").await;
    assert!(!error, "{text}");
    assert!(text.contains("[output truncated]"));
    assert!(text.chars().count() <= 12_000);

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_shares_bash_sessions_in_both_directions() {
    if std::env::var_os("PSA_MONTY_WORKER").is_none() {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    }
    let fixture = Fixture::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = crate::http::router(AppState {
        database: fixture.database.clone(),
        publication: fixture.publication.clone(),
        runtime: fixture.runtime.handle.clone(),
        accepting: Arc::new(AtomicBool::new(true)),
        client_key: "test-client-key".into(),
        client_config: psa_api::ClientConfig {
            default_agent_name: "default".into(),
        },
        probes: fixture.probes.clone(),
    });
    let (stop_server, stopped_server) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped_server.await;
            })
            .await
            .unwrap();
    });
    let mut metadata = support::probe_metadata();
    metadata.id = psa_api::ProbeId(psa_api::probes::Uuid::new_v4());
    metadata.data_dir = fixture._directory.path().join("real-probe");
    let probe_id = metadata.id;
    let config = psa_probe::config::ResolvedConfig {
        metadata,
        server_url: format!("ws://{address}/probes/connect"),
        key: fake_probe::PROBE_KEY.into(),
    };
    let (stop_probe, stopped_probe) = oneshot::channel();
    let probe_task = tokio::spawn(psa_probe::run(config, async {
        let _ = stopped_probe.await;
    }));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.probes.is_active(probe_id).await {
            assert!(
                !probe_task.is_finished(),
                "real probe exited before connecting"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let mut state = fixture.state(vec![], &[]).await;
    state.session.probe_id = probe_id;
    state.session.working_directory = fixture._directory.path().to_owned();
    let mut definition = state.definition.clone();
    definition.bundles = vec![psa_api::BundleConfig::Bash, psa_api::BundleConfig::CodeMode];
    let installed = crate::components::bundles::InstalledComponents::install(
        &definition,
        &state.agent,
        &state.session,
    )
    .await
    .unwrap();
    let definitions = installed
        .tools
        .iter()
        .map(|tool| {
            NewEvent::new(EventType::ToolDefinition(psa_api::events::ToolDefinition {
                id: tool.id(),
                definition: tool.tool_definition(),
            }))
        })
        .collect::<Vec<_>>();
    state.persist_events(&definitions).await.unwrap();
    let components = Arc::new(tokio::sync::Mutex::new(installed));
    state.attach_components(components.clone()).await;

    append_tool_call(&mut state, "native-start", "bash", json!({"command": "read -r line; printf 'native-received:%s\\n' \"$line\"", "session_name":"native-session", "yield_time_ms":50})).await;
    let (controls, run) = fixture.run(state).await;
    state = finish(run).await;
    drop(controls);
    let (error, text) = load_tool_result(&fixture, &state, "native-start").await;
    assert!(!error && text.contains("native-session"), "{text}");

    state = invoke_codemode(&fixture, state, "python-interact", "import asyncio\nasync def run():\n    print(await interact_bash(session_name='native-session', stdin='hello from python\\n', yield_time_ms=2000))\nasyncio.run(run())").await;
    let (error, text) = load_tool_result(&fixture, &state, "python-interact").await;
    assert!(
        !error && text.contains("native-received:hello from python"),
        "{text}"
    );

    state = invoke_codemode(&fixture, state, "python-start", "async def start():\n    print(await bash(command=\"read -r line; printf 'python-received:%s\\\\n' \\\"$line\\\"\", session_name='python-session', yield_time_ms=50))\nasyncio.run(start())").await;
    let (error, text) = load_tool_result(&fixture, &state, "python-start").await;
    assert!(!error && text.contains("python-session"), "{text}");

    append_tool_call(&mut state, "native-interact", "interact_bash", json!({"session_name":"python-session", "stdin":"hello from native\n", "yield_time_ms":2000})).await;
    let (controls, run) = fixture.run(state).await;
    state = finish(run).await;
    drop(controls);
    let (error, text) = load_tool_result(&fixture, &state, "native-interact").await;
    assert!(
        !error && text.contains("python-received:hello from native"),
        "{text}"
    );

    drop(state);
    components.lock().await.cleanup().await.unwrap();
    let _ = stop_probe.send(());
    tokio::time::timeout(Duration::from_secs(5), probe_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _ = stop_server.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    fixture.close().await;
}

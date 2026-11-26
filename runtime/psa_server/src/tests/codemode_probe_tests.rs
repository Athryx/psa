//! CodeMode and native Bash sharing sessions on a real probe connected over
//! the server's WebSocket route.

use crate::http::AppState;
use psa::api::events::*;
use psa_agents::{
    agent_manager::{
        AgentManagerState,
        testing::{Fixture, append_tool_call, finish, load_tool_result},
    },
    probes::fake_probe,
    test_support as support,
};
use psa_store::NewEvent;
use serde_json::json;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot};

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
        client_config: psa::api::ClientConfig {
            default_agent_name: "root".into(),
            default_sandbox_image: None,
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
    metadata.id = psa::api::ProbeId(psa::api::probes::Uuid::new_v4());
    metadata.data_dir = fixture.directory.path().join("real-probe");
    let probe_id = metadata.id;
    let config = psa_probe::config::ResolvedConfig {
        metadata,
        server_url: format!("ws://{address}/probes/connect"),
        key: fake_probe::PROBE_KEY.into(),
        sandbox_probe_path: None,
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
    let mut session = state.session().clone();
    session.probe_id = probe_id;
    session.working_directory = fixture.directory.path().to_owned();
    state.update_session(session);
    let mut definition = state.definition().clone();
    definition.bundles = vec![
        psa::api::BundleConfig::Bash,
        psa::api::BundleConfig::FileTools { read_only: true },
        psa::api::BundleConfig::CodeMode,
    ];
    let installed =
        psa_components::bundles::install_bundles(&definition, state.agent(), state.session(), None)
            .await
            .unwrap();
    let definitions = installed
        .tools
        .iter()
        .map(|tool| {
            NewEvent::new(EventType::ToolDefinition(
                psa::api::events::ToolDefinition {
                    id: tool.id(),
                    definition: tool.tool_definition(),
                },
            ))
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

    // A failing command is a result, not an exception; str() is the tool text.
    state = invoke_codemode(
        &fixture,
        state,
        "python-bash-failure",
        "async def fail():\n    r = await bash(command='echo out; exit 3')\n    print(r.exit_code, r.running, r.output.strip())\n    print(repr(r))\n    print(r)\nasyncio.run(fail())",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "python-bash-failure").await;
    assert!(!error, "{text}");
    assert!(text.starts_with("3 False out\nBashResult("), "{text}");
    assert!(text.contains("exit_code=3"), "{text}");
    assert!(text.contains("Command exited with code 3"), "{text}");

    // Finished sessions stay addressable; only names never used are unknown.
    state = invoke_codemode(
        &fixture,
        state,
        "python-finished",
        "r = await bash(command='exit 4')\nc = await close_bash(session_name=r.session_name)\nprint(c.running, c.exit_code, repr(c.output))\nprint(c)\np = await interact_bash(session_name=r.session_name)\nprint(p.exit_code)\ntry:\n    await interact_bash(session_name=r.session_name, stdin='x')\nexcept RuntimeError as e:\n    print('stdin:', e)\ntry:\n    await close_bash(session_name='never-started')\nexcept RuntimeError as e:\n    print('unknown:', e)",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "python-finished").await;
    assert!(!error, "{text}");
    assert!(text.starts_with("False 4 ''\n"), "{text}");
    assert!(text.contains("had already finished"), "{text}");
    assert!(text.contains("Command exited with code 4\n4\n"), "{text}");
    assert!(text.contains("stdin: Bash session"), "{text}");
    assert!(text.contains("not accepting stdin"), "{text}");
    assert!(
        text.contains("unknown: Unknown bash session: never-started"),
        "{text}"
    );

    std::fs::write(
        fixture.directory.path().join("lines.txt"),
        "one\ntwo\nthree\n",
    )
    .unwrap();
    state = invoke_codemode(
        &fixture,
        state,
        "python-read",
        "r = await read(path='lines.txt', limit=2)\nprint(r.content.split())\nprint(r.start_line, r.end_line, r.total_lines, r.next_offset)\nprint(r)",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "python-read").await;
    assert!(!error, "{text}");
    assert!(
        text.starts_with("['one', 'two']\n1 2 4 3\none\ntwo"),
        "{text}"
    );
    assert!(text.contains("Use offset=3 to continue."), "{text}");

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

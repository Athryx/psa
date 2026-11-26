//! CodeMode tools run by the worker's tool executor.

use super::*;
use crate::tools::CodeMode;
use psa_codemode::MontyEngine;
use serde_json::json;

#[tokio::test]
async fn sequential_codemode_calls_observe_committed_private_events() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let state = fixture
        .state(
            vec![Box::new(CodeMode::new(engine.clone())) as Box<dyn crate::tools::DynTool>],
            &[
                ("first", "codemode", json!({"code": "x = 41"})),
                ("second", "codemode", json!({"code": "print(x + 1)"})),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    drop(controls);
    let mut cursor = fixture.database.read().await.unwrap();
    let events = cursor.load_all_events(state.agent().id).await.unwrap();
    cursor.close().await.unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) if result.call_id == "second" => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(!result.error, "{:?}", result.content);
    assert!(format!("{:?}", result.content).contains("42"));
    drop(state);
    engine.close().await;
    fixture.close().await;
}

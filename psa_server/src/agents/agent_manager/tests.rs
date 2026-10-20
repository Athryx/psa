use super::*;
use crate::sessions::NewSession;
use std::time::Duration;

#[tokio::test]
async fn committed_calls_get_not_run_results_when_interrupted_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("test.sqlite3"))
        .await
        .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            name: "test".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            super::super::NewAgent {
                name: "test".into(),
            },
        )
        .await
        .unwrap();
    let model = llm_api::providers::ModelConfig {
        provider: llm_api::providers::ProviderType::Openrouter,
        model: llm_api::providers::Model::new("test").unwrap(),
        effort: llm_api::providers::Effort::Medium,
    };
    cursor
        .add_events(
            agent.id,
            &[
                NewEvent::new(EventType::Model(model.clone())),
                status_event(AgentState::Running),
                NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                    id: 1,
                    model_config: model,
                    request_config: Default::default(),
                })),
                NewEvent::new(EventType::ToolCall(ToolCall {
                    call_id: "unstarted".into(),
                    tool_name: "noop".into(),
                    arguments: "{}".into(),
                    metadata: Default::default(),
                })),
                NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                    id: 1,
                    outcome: Outcome::Completed {
                        reason: api::StopReason::ToolCalls,
                    },
                    usage: Default::default(),
                    request_time: Duration::ZERO,
                    response_id: None,
                    provider_metadata: Default::default(),
                })),
            ],
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let mut state = AgentManagerState::load(db.clone(), Publication::default(), agent.id)
        .await
        .unwrap();
    state.interrupt().await.unwrap();
    assert!(state.outstanding_calls().is_empty());
    assert_eq!(state.agent.state, AgentState::Interrupted);
    let mut cursor = db.cursor().await.unwrap();
    let events = cursor
        .load_events_since_last_inference(agent.id)
        .await
        .unwrap()
        .unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(result.error);
    assert_eq!(
        serde_json::to_value(&result.content).unwrap(),
        serde_json::to_value(prompts::tool_not_run()).unwrap()
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

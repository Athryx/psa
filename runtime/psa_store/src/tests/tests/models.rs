use super::*;
use crate::NewEvent;
use llm_api::providers::Effort;

#[tokio::test]
async fn new_efforts_and_resolved_reasoning_snapshots_survive_restart() {
    let (directory, database, _session, agent) = fixture().await;
    let mut cursor = database.write().await.unwrap();
    for (index, effort) in [Effort::Off, Effort::Minimal, Effort::Max]
        .into_iter()
        .enumerate()
    {
        let config = llm_api::providers::ModelConfig { effort, ..model() };
        let reasoning = if effort == Effort::Off {
            api::ReasoningOverride::Omit
        } else {
            api::ReasoningOverride::Effort(Effort::Low)
        };
        cursor
            .add_events(
                agent.id,
                &[
                    NewEvent::new(EventType::Model(config.clone())),
                    NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                        id: index as u64,
                        model_config: config,
                        request_config: api::RequestConfig {
                            reasoning: Some(reasoning),
                            ..Default::default()
                        },
                    })),
                    NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                        id: index as u64,
                        outcome: Outcome::Completed {
                            reason: StopReason::EndTurn,
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
    }
    cursor.commit().await.unwrap();
    database.close().await;
    let database = Database::open(directory.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = database.read().await.unwrap();
    let events = cursor.load_all_events(agent.id).await.unwrap();
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventType::InferenceResultStart(start) => Some(start),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[0].model_config.effort, Effort::Off);
    assert_eq!(
        starts[0].request_config.reasoning,
        Some(api::ReasoningOverride::Omit)
    );
    assert_eq!(starts[1].model_config.effort, Effort::Minimal);
    assert_eq!(
        starts[1].request_config.reasoning,
        Some(api::ReasoningOverride::Effort(Effort::Low))
    );
    assert_eq!(starts[2].model_config.effort, Effort::Max);
    cursor.close().await.unwrap();
    database.close().await;
}

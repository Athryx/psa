use super::*;
use crate::sessions::NewSession;
use std::time::Duration;

#[tokio::test]
async fn persisted_model_updates_live_request_projection_and_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("model.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            model_config: crate::test_support::model_config(),
            probe_id: crate::test_support::probe_metadata().id,
            name: "model test".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            super::super::NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "agent".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();

    let publication = Publication::default();
    let mut state = AgentManagerState::load(db.clone(), publication.clone(), agent.id)
        .await
        .unwrap();
    assert!(state.context().model_config().is_none());
    let mut model = session.model_config.clone();
    model.model =
        llm_api::providers::Model::new(llm_api::providers::ProviderType::Openrouter, "saved-model")
            .unwrap();
    state
        .persist_events(&[
            NewEvent::new(EventType::NewContext),
            NewEvent::new(EventType::Model(model.clone())),
        ])
        .await
        .unwrap();
    assert_eq!(state.context().model_config(), Some(&model));
    assert_eq!(
        state
            .context()
            .prepare_request(Default::default())
            .unwrap()
            .history
            .config,
        model
    );

    model.model = llm_api::providers::Model::new(
        llm_api::providers::ProviderType::Openrouter,
        "updated-model",
    )
    .unwrap();
    state
        .persist_events(&[NewEvent::new(EventType::Model(model.clone()))])
        .await
        .unwrap();
    assert_eq!(state.context().model_config(), Some(&model));
    assert_eq!(
        state
            .context()
            .prepare_request(Default::default())
            .unwrap()
            .history
            .config,
        model
    );
    drop(state);

    let restored = AgentManagerState::load(db.clone(), publication, agent.id)
        .await
        .unwrap();
    assert_eq!(restored.context().model_config(), Some(&model));
    assert_eq!(
        restored
            .context()
            .prepare_request(Default::default())
            .unwrap()
            .history
            .config,
        model
    );
    db.close().await;
}

#[tokio::test]
async fn repeated_status_changes_are_omitted_using_prior_transitions_in_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("test.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            model_config: crate::test_support::model_config(),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
            name: "test".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            super::super::NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "test".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();

    let publication = Publication::default();
    let mut published = publication.lock().await.subscribe();
    let mut state = AgentManagerState::load(db.clone(), publication, agent.id)
        .await
        .unwrap();

    let repeated_interruption = NewEvent::new(EventType::StatusChange(StatusChange {
        new_status: AgentState::Interrupted,
        reason: Some("a second tool also lost its probe".into()),
    }));
    let events = state
        .persist_events(&[
            status_event(AgentState::Idle),
            NewEvent::new(EventType::Log(Log {
                error: false,
                message: "before transitions".into(),
            })),
            status_event(AgentState::Running),
            status_event(AgentState::Running),
            status_event(AgentState::Interrupted),
            repeated_interruption.clone(),
            status_event(AgentState::Running),
            status_event(AgentState::Running),
            status_event(AgentState::Interrupted),
            NewEvent::new(EventType::Log(Log {
                error: false,
                message: "after transitions".into(),
            })),
        ])
        .await
        .unwrap();
    assert_eq!(events.len(), 6);
    assert_eq!(
        events.iter().map(|event| event.id.0).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    let transitions: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventType::StatusChange(change) => Some(change.new_status),
            _ => None,
        })
        .collect();
    assert_eq!(
        transitions,
        vec![
            AgentState::Running,
            AgentState::Interrupted,
            AgentState::Running,
            AgentState::Interrupted,
        ]
    );
    assert_eq!(state.agent().state, AgentState::Interrupted);
    assert_eq!(state.context().status, AgentState::Interrupted);
    assert_eq!(state.context().last_event_id, EventId(6));
    assert_eq!(state.agent().next_event_id, EventId(7));
    assert!(
        state
            .persist_events(&[repeated_interruption])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(state.agent().next_event_id, EventId(7));

    let mut committed = Vec::new();
    while let Ok(message) = published.try_recv() {
        if let psa_api::StreamMessage::Committed { event, .. } = message {
            committed.push((event.position, event.event.id.0));
        }
    }
    assert_eq!(committed.len(), 6);
    assert!(committed.windows(2).all(|pair| pair[1].0 == pair[0].0 + 1));
    assert_eq!(
        committed.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(cursor.load_all_events(agent.id).await.unwrap().len(), 6);
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().next_event_id,
        EventId(7)
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn cancelled_persistence_finishes_before_the_next_append() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("test.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            model_config: crate::test_support::model_config(),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
            name: "test".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            super::super::NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "test".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();

    let publication = Publication::default();
    let mut state = AgentManagerState::load(db.clone(), publication.clone(), agent.id)
        .await
        .unwrap();

    let publication_guard = publication.lock().await;
    let mut published = publication_guard.subscribe();
    let initial_events = [
        status_event(AgentState::Running),
        NewEvent::new(EventType::Message(Message {
            source: None,
            description: None,
            message_type: MessageType::Harness,
            content: prompts::text("saved despite cancellation"),
            metadata: Default::default(),
        })),
    ];
    {
        let persistence = state.persist_events(&initial_events);
        tokio::pin!(persistence);
        assert!(futures_util::poll!(&mut persistence).is_pending());
    }

    // The publication guard prevents the spawned append from finishing, but
    // dropping its caller must not drop the worker's ownership of the commit.
    assert!(!state.pending_persistence.as_ref().unwrap().is_finished());
    assert_eq!(state.agent().state, AgentState::Idle);
    assert_eq!(state.context().last_event_id, EventId(0));
    let mut cursor = db.cursor().await.unwrap();
    assert!(cursor.load_all_events(agent.id).await.unwrap().is_empty());
    cursor.rollback().await.unwrap();
    drop(publication_guard);

    let appended = tokio::time::timeout(
        Duration::from_secs(5),
        state.persist_events(&[
            status_event(AgentState::Running),
            NewEvent::new(EventType::Log(Log {
                error: false,
                message: "saved after cancellation".into(),
            })),
        ]),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].id, EventId(3));
    assert!(state.pending_persistence.is_none());
    assert_eq!(state.agent().state, AgentState::Running);
    assert_eq!(state.agent().next_event_id, EventId(4));
    assert_eq!(state.context().status, AgentState::Running);
    assert_eq!(state.context().last_event_id, EventId(3));
    assert_eq!(state.context().items().len(), 1);

    let mut cursor = db.cursor().await.unwrap();
    let events = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(
        events.iter().map(|event| event.id.0).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        matches!(&events[0].kind, EventType::StatusChange(change) if change.new_status == AgentState::Running)
    );
    assert!(
        matches!(&events[1].kind, EventType::Message(message) if message.message_type == MessageType::Harness)
    );
    assert!(
        matches!(&events[2].kind, EventType::Log(log) if log.message == "saved after cancellation")
    );
    let persisted = cursor.get_agent(agent.id).await.unwrap();
    assert_eq!(persisted.state, state.agent().state);
    assert_eq!(persisted.next_event_id, state.agent().next_event_id);
    cursor.rollback().await.unwrap();

    let mut committed = Vec::new();
    while let Ok(message) = published.try_recv() {
        if let psa_api::StreamMessage::Committed { event, .. } = message {
            committed.push((event.position, event.event.id.0));
        }
    }
    assert_eq!(
        committed.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(committed.windows(2).all(|pair| pair[1].0 == pair[0].0 + 1));
    assert!(state.finish_persistence().await.unwrap().is_empty());
    db.close().await;
}

#[tokio::test]
async fn committed_calls_get_not_run_results_when_interrupted_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("test.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            model_config: crate::test_support::model_config(),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
            name: "test".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            super::super::NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "test".into(),
            },
        )
        .await
        .unwrap();
    let model = llm_api::providers::ModelConfig {
        model: llm_api::providers::Model::new(llm_api::providers::ProviderType::Openrouter, "test")
            .unwrap(),
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

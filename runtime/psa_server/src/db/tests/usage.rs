use super::*;
use crate::db::NewEvent;

fn reported(tokens: u64) -> Usage {
    Usage {
        input_tokens: Some(tokens),
        output_tokens: Some(0),
        total_tokens: Some(tokens),
        cached_input_tokens: Some(0),
        reasoning_output_tokens: Some(0),
        ..Default::default()
    }
}

fn result(usage: Usage) -> NewEvent {
    NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
        id: 1,
        outcome: Outcome::Cancelled,
        usage,
        request_time: Duration::ZERO,
        response_id: None,
        provider_metadata: Default::default(),
    }))
}

#[tokio::test]
async fn cache_writes_count_only_reported_values() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    assert_eq!(
        cursor
            .get_agent(agent.id)
            .await
            .unwrap()
            .usage
            .cache_write_tokens,
        Some(0)
    );

    let mut usage = reported(5);
    usage.cache_write_tokens = Some(4);
    let updated = cursor
        .add_events(agent.id, &[result(usage.clone()), result(usage)])
        .await
        .unwrap();
    assert_eq!(updated.new_usage.unwrap().cache_write_tokens, Some(8));
    assert_eq!(
        super::super::queries::usage_at(
            cursor.connection(),
            agent.id,
            psa::api::events::EventId(2)
        )
        .await
        .unwrap()
        .cache_write_tokens,
        Some(8)
    );

    let updated = cursor
        .add_events(agent.id, &[result(reported(1))])
        .await
        .unwrap();
    assert_eq!(updated.new_usage.unwrap().cache_write_tokens, None);
    cursor.rollback().await.unwrap();
    db.close().await;
}

fn cached(input: u64, cached: u64) -> NewEvent {
    let mut usage = reported(input);
    usage.cached_input_tokens = Some(cached);
    result(usage)
}

#[tokio::test]
async fn cacheable_input_counts_input_repeated_within_each_context() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    let initial = cursor.get_agent(agent.id).await.unwrap().cache;
    assert_eq!(initial.cacheable_input_tokens, Some(0));

    // First request: only its shared-cache hit is cacheable. Later requests
    // repeat the previous request's input, up to their own size.
    let updated = cursor
        .add_events(
            agent.id,
            &[cached(100, 40), cached(150, 90), cached(120, 120)],
        )
        .await
        .unwrap();
    assert_eq!(
        updated.new_cache.unwrap().cacheable_input_tokens,
        Some(40 + 100 + 120)
    );
    // A new context starts over: nothing is repeated, and nothing was cached.
    let updated = cursor
        .add_events(
            agent.id,
            &[NewEvent::new(EventType::NewContext), cached(80, 0)],
        )
        .await
        .unwrap();
    let tracked = updated.new_cache.unwrap();
    assert_eq!(tracked.cacheable_input_tokens, Some(260));
    assert_eq!(tracked.previous_input_tokens, Some(80));
    assert_eq!(cursor.get_agent(agent.id).await.unwrap().cache, tracked);

    // Forks and backfills recompute the same tracking from events.
    let through = psa::api::events::EventId(5);
    assert_eq!(
        super::super::cache_usage::cache_tracking_at(cursor.connection(), agent.id, through)
            .await
            .unwrap(),
        tracked
    );
    let usage = cursor.get_agent(agent.id).await.unwrap().usage;
    sqlx::query("UPDATE agents SET usage = ? WHERE id = ?")
        .bind(serde_json::to_string(&usage).unwrap())
        .bind(agent.id.0 as i64)
        .execute(cursor.connection())
        .await
        .unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().cache,
        CacheTracking::default()
    );
    assert_eq!(cursor.backfill_cache_tracking().await.unwrap(), 1);
    let agent_now = cursor.get_agent(agent.id).await.unwrap();
    assert_eq!(agent_now.cache, tracked);
    assert_eq!(agent_now.usage, usage);
    assert_eq!(cursor.backfill_cache_tracking().await.unwrap(), 0);

    // An unreported input leaves the count unknown.
    let mut unreported = reported(10);
    unreported.input_tokens = None;
    let updated = cursor
        .add_events(agent.id, &[result(unreported)])
        .await
        .unwrap();
    assert_eq!(updated.new_cache.unwrap().cacheable_input_tokens, None);
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn usage_batches_unknown_fields_and_rollback() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage,
        llm_api::api::Usage::zero()
    );
    let appended = cursor
        .add_events(agent.id, &[result(reported(3)), result(reported(5))])
        .await
        .unwrap();
    assert_eq!(appended.new_usage.unwrap().total_tokens, Some(8));
    assert!(
        cursor
            .add_events(agent.id, &[result(reported(0)), log("no usage")])
            .await
            .unwrap()
            .new_usage
            .is_none()
    );
    let mut partial = reported(2);
    partial.input_tokens = None;
    let usage = cursor
        .add_events(agent.id, &[result(partial)])
        .await
        .unwrap()
        .new_usage
        .unwrap();
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.total_tokens, Some(10));
    cursor.commit().await.unwrap();

    let mut cursor = db.write().await.unwrap();
    let before = cursor.get_agent(agent.id).await.unwrap();
    assert!(
        cursor
            .add_events(agent.id, &[result(reported(i64::MAX as u64))])
            .await
            .is_err()
    );
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().next_event_id,
        before.next_event_id
    );
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage,
        before.usage
    );
    cursor
        .add_events(agent.id, &[result(reported(1))])
        .await
        .unwrap();
    cursor.rollback().await.unwrap();
    let mut cursor = db.write().await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage,
        before.usage
    );
    cursor
        .add_events(agent.id, &[NewEvent::new(EventType::NewContext)])
        .await
        .unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage,
        before.usage
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn usage_updates_serialize_concurrent_writers_and_include_compaction() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut writers = Vec::new();
    for _ in 0..12 {
        let db = db.clone();
        writers.push(tokio::spawn(async move {
            let mut cursor = db.write().await.unwrap();
            cursor
                .add_events(
                    agent.id,
                    &[
                        result(reported(2)),
                        NewEvent::new(EventType::Compaction(Compaction {
                            summary: CompactionSummary::Text("summary".into()),
                            usage: reported(3),
                        })),
                    ],
                )
                .await
                .unwrap();
            cursor.commit().await.unwrap();
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    let mut cursor = db.write().await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage.total_tokens,
        Some(60)
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn migration_backfills_usage_through_each_ancestor_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite3");
    let pool = create_legacy_database(&path).await;
    sqlx::raw_sql(
        "INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (1,NULL,1,4,'root','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (2,1,3,4,'child','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (3,2,4,4,'nested','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (1,1,NULL,1,3,1,'root',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (2,2,1,2,3,1,'child',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (3,3,2,3,3,1,'nested',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (4,1,NULL,1,1,1,'empty',0,0,1);
         INSERT INTO events VALUES (1,1,1,2,0,'inference_end');
         INSERT INTO events VALUES (2,1,2,3,0,'inference_end');
         INSERT INTO events VALUES (3,2,2,3,0,'inference_end');
         INSERT INTO event_inference_end (event_pk,inference_id,outcome,request_time_us,provider_metadata,input_tokens,output_tokens,total_tokens,cached_tokens,reasoning_tokens,usage_details)
         VALUES (1,1,'cancelled',0,'{}',10,0,10,0,0,'{\"prompt_tokens_details\":{\"cache_write_tokens\":5}}'),
                (2,2,'cancelled',0,'{}',NULL,0,20,0,0,'{}'),
                (3,2,'cancelled',0,'{}',4,0,4,0,0,'{\"prompt_tokens_details\":{\"cache_write_tokens\":2}}');"
    ).execute(&pool).await.unwrap();
    pool.close().await;
    let db = Database::open(path).await.unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.write().await.unwrap();
    let root = cursor.get_agent(crate::agents::AgentId(1)).await.unwrap();
    assert_eq!(root.usage.input_tokens, None);
    assert_eq!(root.usage.total_tokens, Some(30));
    assert_eq!(root.usage.cache_write_tokens, None);
    for id in [2, 3] {
        let child = cursor.get_agent(crate::agents::AgentId(id)).await.unwrap();
        assert_eq!(child.usage.input_tokens, Some(14));
        assert_eq!(child.usage.total_tokens, Some(14));
        assert_eq!(child.usage.cache_write_tokens, Some(7));
    }
    assert_eq!(
        cursor
            .get_agent(crate::agents::AgentId(4))
            .await
            .unwrap()
            .usage,
        llm_api::api::Usage::zero()
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn status_projects_fork_bounded_markers_and_context_resets() {
    let (_dir, db, session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    let completed = |tokens| {
        NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
            id: 1,
            outcome: Outcome::Completed {
                reason: StopReason::EndTurn,
            },
            usage: reported(tokens),
            request_time: Duration::ZERO,
            response_id: None,
            provider_metadata: Default::default(),
        }))
    };
    cursor
        .add_events(
            agent.id,
            &[
                NewEvent::new(EventType::NewContext),
                NewEvent::new(EventType::Model(model())),
                completed(42),
            ],
        )
        .await
        .unwrap();
    cursor
        .add_events(
            agent.id,
            &(0..300).map(|_| log("later")).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let child = fork.agents[0].id;
    cursor
        .add_events(agent.id, &[NewEvent::new(EventType::NewContext)])
        .await
        .unwrap();
    let status = cursor
        .load_agent_context_state(fork.session.id, child)
        .await
        .unwrap();
    assert_eq!(status.model_config, Some(model()));
    assert_eq!(status.context_tokens, Some(42));
    assert_eq!(status.head, 303);
    assert!(
        cursor
            .load_agent_context_state(session.id, child)
            .await
            .is_err()
    );
    cursor
        .add_events(child, &[NewEvent::new(EventType::NewContext)])
        .await
        .unwrap();
    let status = cursor
        .load_agent_context_state(fork.session.id, child)
        .await
        .unwrap();
    assert!(status.model_config.is_none());
    assert_eq!(status.context_tokens, None);
    cursor.commit().await.unwrap();
    db.close().await;
}

fn unreported_compaction() -> NewEvent {
    NewEvent::new(EventType::Compaction(Compaction {
        summary: CompactionSummary::Text("summary".into()),
        usage: Default::default(),
    }))
}

#[tokio::test]
async fn unreported_compaction_usage_keeps_totals_known_and_poisoned_totals_are_repaired() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    cursor
        .add_events(
            agent.id,
            &[
                result(reported(3)),
                NewEvent::new(EventType::NewContext),
                unreported_compaction(),
                result(reported(5)),
            ],
        )
        .await
        .unwrap();
    let usage = cursor.get_agent(agent.id).await.unwrap().usage;
    assert_eq!(usage.input_tokens, Some(8));
    assert_eq!(usage.total_tokens, Some(8));
    assert_eq!(
        super::super::queries::usage_at(
            cursor.connection(),
            agent.id,
            psa::api::events::EventId(4)
        )
        .await
        .unwrap(),
        usage
    );

    // Totals stored before the fix lost every count after a compaction.
    let poisoned = llm_api::api::Usage {
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        ..usage.clone()
    };
    sqlx::query("UPDATE agents SET usage = ? WHERE id = ?")
        .bind(serde_json::to_string(&poisoned).unwrap())
        .bind(agent.id.0 as i64)
        .execute(cursor.connection())
        .await
        .unwrap();
    assert_eq!(cursor.repair_unknown_usage().await.unwrap(), 1);
    assert_eq!(cursor.get_agent(agent.id).await.unwrap().usage, usage);
    assert_eq!(cursor.repair_unknown_usage().await.unwrap(), 0);
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn model_usage_groups_inference_and_decision_calls_by_model() {
    use llm_api::api::decisions::{DecisionRequest, DecisionResponse};
    use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
    use psa::api::DecisionCall;

    let (_dir, db, session, agent) = fixture().await;
    let model = |name: &str| Model::new(ProviderType::Openrouter, name).unwrap();
    let start = |id: u64, name: &str| {
        NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
            id,
            model_config: ModelConfig {
                model: model(name),
                effort: Effort::Low,
            },
            request_config: api::RequestConfig::default(),
        }))
    };
    let end = |id: u64, tokens: u64, cost: Option<f64>| {
        NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
            id,
            outcome: Outcome::Cancelled,
            usage: Usage {
                cost,
                ..reported(tokens)
            },
            request_time: Duration::ZERO,
            response_id: None,
            provider_metadata: Default::default(),
        }))
    };
    let decision = |response: Result<f64, &str>| {
        let decider = model("vendor/decider");
        NewEvent::new(EventType::DecisionCall(DecisionCall {
            tool_call_id: None,
            request: DecisionRequest {
                model: decider.clone(),
                state: "state".into(),
                questions: Default::default(),
            },
            response: response
                .map(|cost| DecisionResponse {
                    model: decider,
                    response_id: None,
                    answers: Default::default(),
                    usage: Usage {
                        input_tokens: Some(4),
                        cost: Some(cost),
                        ..Usage::default()
                    },
                    provider_metadata: Default::default(),
                })
                .map_err(String::from),
            duration: Duration::ZERO,
        }))
    };

    let mut cursor = db.write().await.unwrap();
    let first = cursor.get_agent(agent.id).await.unwrap().next_event_id.0;
    let updated = cursor
        .add_events(
            agent.id,
            &[
                start(first, "vendor/b"),
                end(first, 10, Some(0.5)),
                start(first + 2, "vendor/a"),
                end(first + 2, 7, None),
                start(first + 4, "vendor/b"),
                end(first + 4, 3, Some(0.25)),
                decision(Ok(0.125)),
                decision(Err("timeout")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(updated.new_usage.unwrap().cost, None);

    let usage = cursor
        .load_agent_model_usage(session.id, agent.id)
        .await
        .unwrap();
    let names: Vec<_> = usage.iter().map(|entry| entry.model.slug()).collect();
    assert_eq!(
        names,
        [
            "openrouter/vendor/a",
            "openrouter/vendor/b",
            "openrouter/vendor/decider"
        ]
    );
    assert_eq!(usage[0].usage.input_tokens, Some(7));
    assert_eq!(usage[0].usage.cost, None);
    assert_eq!(usage[1].usage.input_tokens, Some(13));
    assert_eq!(usage[1].usage.cost, Some(0.75));
    assert_eq!(usage[2].usage.input_tokens, Some(4));
    assert_eq!(usage[2].usage.output_tokens, None);
    assert_eq!(usage[2].usage.cost, Some(0.125));
    cursor.rollback().await.unwrap();
    db.close().await;
}

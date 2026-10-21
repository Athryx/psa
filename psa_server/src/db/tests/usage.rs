use super::*;

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
async fn usage_batches_unknown_fields_and_rollback() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage,
        crate::agents::AgentUsage::default()
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

    let mut cursor = db.cursor().await.unwrap();
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
    let mut cursor = db.cursor().await.unwrap();
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
            let mut cursor = db.cursor().await.unwrap();
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
    let mut cursor = db.cursor().await.unwrap();
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
        "INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (1,NULL,1,4,'root','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"api_name\":\"test-model\"},\"provider\":\"Openrouter\",\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (2,1,3,4,'child','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"api_name\":\"test-model\"},\"provider\":\"Openrouter\",\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (3,2,4,4,'nested','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"api_name\":\"test-model\"},\"provider\":\"Openrouter\",\"effort\":\"High\"}');
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (1,1,NULL,1,3,1,'root',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (2,2,1,2,3,1,'child',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (3,3,2,3,3,1,'nested',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (4,1,NULL,1,1,1,'empty',0,0,1);
         INSERT INTO events VALUES (1,1,1,2,0,'inference_end');
         INSERT INTO events VALUES (2,1,2,3,0,'inference_end');
         INSERT INTO events VALUES (3,2,2,3,0,'inference_end');
         INSERT INTO event_inference_end (event_pk,inference_id,outcome,request_time_us,provider_metadata,input_tokens,output_tokens,total_tokens,cached_tokens,reasoning_tokens,usage_details)
         VALUES (1,1,'cancelled',0,'{}',10,0,10,0,0,'{}'),
                (2,2,'cancelled',0,'{}',NULL,0,20,0,0,'{}'),
                (3,2,'cancelled',0,'{}',4,0,4,0,0,'{}');"
    ).execute(&pool).await.unwrap();
    pool.close().await;
    let db = Database::open(path).await.unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let root = cursor.get_agent(crate::agents::AgentId(1)).await.unwrap();
    assert_eq!(root.usage.input_tokens, None);
    assert_eq!(root.usage.total_tokens, Some(30));
    for id in [2, 3] {
        let child = cursor.get_agent(crate::agents::AgentId(id)).await.unwrap();
        assert_eq!(child.usage.input_tokens, Some(14));
        assert_eq!(child.usage.total_tokens, Some(14));
    }
    assert_eq!(
        cursor
            .get_agent(crate::agents::AgentId(4))
            .await
            .unwrap()
            .usage,
        crate::agents::AgentUsage::default()
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

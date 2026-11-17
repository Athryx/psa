use super::*;

#[tokio::test]
async fn hidden_round_trips_reloads_and_is_inherited_by_forks() {
    let (dir, db, session, default_agent) = fixture().await;
    assert!(!default_agent.hidden);
    let mut cursor = db.cursor().await.unwrap();
    let mut originals = Vec::new();
    for hidden in [false, true] {
        let agent = cursor
            .create_agent(
                session.id,
                NewAgent {
                    subagent_parent_id: None,
                    definition_id: psa::api::AgentDefinitionId(1),
                    name: format!("hidden-{hidden}"),
                    interactive: false,
                    hidden,
                },
            )
            .await
            .unwrap();
        assert_eq!(agent.hidden, hidden);
        assert_eq!(cursor.get_agent(agent.id).await.unwrap().hidden, hidden);
        originals.push(agent);
    }
    let listed = cursor.list_agents(session.id).await.unwrap();
    for agent in &originals {
        assert_eq!(
            listed
                .iter()
                .find(|listed| listed.id == agent.id)
                .unwrap()
                .hidden,
            agent.hidden
        );
    }
    cursor.commit().await.unwrap();
    db.close().await;

    let db = Database::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    for agent in &originals {
        assert_eq!(
            cursor.get_agent(agent.id).await.unwrap().hidden,
            agent.hidden
        );
    }
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
    for agent in &originals {
        let child = fork
            .agents
            .iter()
            .find(|copy| copy.parent_id == Some(agent.id))
            .unwrap();
        assert_eq!(child.hidden, agent.hidden);
        assert_eq!(
            cursor.get_agent(child.id).await.unwrap().hidden,
            agent.hidden
        );
    }
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn migration_defaults_legacy_agents_to_visible_and_checks_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite3");
    let pool = create_legacy_database(&path).await;
    sqlx::raw_sql(
        "INSERT INTO sessions (id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (1,1,2,'legacy','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO agents (id,session_id,definition_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us) VALUES (1,1,1,1,1,1,'legacy',0,0);",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let db = Database::open(&path).await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert!(
        !cursor
            .get_agent(crate::agents::AgentId(1))
            .await
            .unwrap()
            .hidden
    );
    for value in [Some(2_i64), Some(-1), None] {
        assert!(
            sqlx::query("UPDATE agents SET hidden = ? WHERE id = 1")
                .bind(value)
                .execute(cursor.connection())
                .await
                .is_err()
        );
    }
    cursor.commit().await.unwrap();
    db.close().await;
}

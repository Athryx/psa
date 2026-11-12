use super::*;

#[tokio::test]
async fn recent_pages_include_fork_inheritance_and_merge_multiple_agent_positions() {
    let (_dir, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    cursor
        .add_events(
            agent.id,
            &(0..5).map(|i| log(&i.to_string())).collect::<Vec<_>>(),
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
        .add_events(child, &[log("child 1"), log("child 2")])
        .await
        .unwrap();
    let sibling = cursor
        .create_agent(
            fork.session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "sibling".into(),
            },
        )
        .await
        .unwrap();
    cursor
        .add_events(sibling.id, &[log("sibling")])
        .await
        .unwrap();
    let head = cursor
        .get_session(fork.session.id)
        .await
        .unwrap()
        .next_position
        - 1;
    let recent = cursor
        .load_recent_session_events(fork.session.id, head, 4, false)
        .await
        .unwrap();
    assert_eq!(recent.len(), 4);
    assert_eq!(
        recent.iter().map(|e| e.agent_id).collect::<Vec<_>>(),
        vec![child, child, child, sibling.id]
    );
    assert_eq!(recent[0].event.source_agent_id, agent.id);
    let older = cursor
        .load_recent_session_events(fork.session.id, recent[0].position - 1, 4, false)
        .await
        .unwrap();
    assert_eq!(older.len(), 4);
    assert!(
        older
            .iter()
            .all(|e| e.agent_id == child && e.event.source_agent_id == agent.id)
    );
    let agent_recent = cursor
        .load_recent_agent_events(fork.session.id, child, 7, 4, false)
        .await
        .unwrap();
    assert_eq!(
        agent_recent
            .iter()
            .map(|e| e.event.id.0)
            .collect::<Vec<_>>(),
        vec![4, 5, 6, 7]
    );
    let agent_older = cursor
        .load_recent_agent_events(fork.session.id, child, 3, 4, false)
        .await
        .unwrap();
    assert_eq!(
        agent_older.iter().map(|e| e.event.id.0).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        cursor
            .load_recent_agent_events(fork.session.id, child, 0, 4, false)
            .await
            .unwrap()
            .is_empty()
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn summary_equal_timestamps_use_descending_id_keysets_and_direct_children() {
    let (_dir, db, session, _agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
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
    cursor.commit().await.unwrap();
    sqlx::query("UPDATE sessions SET updated_at_us=1000000")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let query = psa_api::SessionsQuery {
        flat: true,
        limit: 1,
        ..Default::default()
    };
    let first = cursor.query_sessions(&query, None).await.unwrap();
    assert_eq!(
        first.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![fork.session.id, session.id]
    );
    let second = cursor
        .query_sessions(&query, Some((1000000, fork.session.id.0 as i64)))
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, session.id);
    let children = cursor
        .query_sessions(
            &psa_api::SessionsQuery {
                parent: Some(session.id),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, fork.session.id);
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn summary_filters_match_names_probes_and_projects_like_the_shared_filter() {
    let (_dir, db, global, _agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let project = psa_api::ProjectId("Pipeline".into());
    let in_project = cursor
        .create_session(NewSession {
            name: "Nightly Build".into(),
            working_directory: "/work".into(),
            probe_id: global.probe_id,
            project_id: Some(project.clone()),
            model_config: crate::test_support::model_config(),
        })
        .await
        .unwrap();
    let fork = cursor
        .fork_session(
            in_project.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(fork.session.project_id, Some(project.clone()));
    for invalid in ["", " padded", &"x".repeat(psa_api::ProjectId::MAX_LEN + 1)] {
        let error = cursor
            .create_session(NewSession {
                name: "invalid".into(),
                working_directory: "/work".into(),
                probe_id: global.probe_id,
                project_id: Some(psa_api::ProjectId(invalid.into())),
                model_config: crate::test_support::model_config(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::InvalidInput(_)), "{invalid:?}");
    }
    assert_eq!(cursor.list_projects().await.unwrap(), vec![project.clone()]);

    let filters = [
        psa_api::SessionFilter::default(),
        psa_api::SessionFilter {
            name_contains: "NIGHTLY".into(),
            ..Default::default()
        },
        psa_api::SessionFilter {
            project_ids: vec![None],
            ..Default::default()
        },
        psa_api::SessionFilter {
            project_ids: vec![Some(project.clone())],
            ..Default::default()
        },
        psa_api::SessionFilter {
            project_ids: vec![None, Some(project)],
            ..Default::default()
        },
        psa_api::SessionFilter {
            probe_ids: vec![psa_api::ProbeId(psa_api::probes::Uuid::new_v4())],
            ..Default::default()
        },
    ];
    let all = cursor
        .query_sessions(
            &psa_api::SessionsQuery {
                flat: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    for filter in filters {
        let query = psa_api::SessionsQuery {
            flat: true,
            filter: filter.clone(),
            ..Default::default()
        };
        let mut queried: Vec<_> = cursor
            .query_sessions(&query, None)
            .await
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect();
        let mut matched: Vec<_> = all
            .iter()
            .filter(|session| filter.matches(&(*session).clone().into_api(false)))
            .map(|session| session.id)
            .collect();
        queried.sort_by_key(|id| id.0);
        matched.sort_by_key(|id| id.0);
        assert_eq!(queried, matched, "{filter:?}");
    }
    cursor.rollback().await.unwrap();
    db.close().await;
}

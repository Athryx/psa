use super::*;
use crate::agents::{NewAgent, supervisor::Runtime};
use futures_util::StreamExt;
use psa::api::events::Log;
use psa_models::ProviderRegistry;
use psa_store::NewEvent;

pub(super) async fn fixture() -> (
    tempfile::TempDir,
    AppState,
    Runtime,
    psa_store::Session,
    crate::agents::Agent,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("desktop.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.write().await.unwrap();
    let session = cursor
        .create_session(psa_store::NewSession {
            decision_model: None,
            probe_id: api::ProbeId(api::probes::Uuid::nil()),
            project_id: None,
            name: "test".into(),
            working_directory: "/work".into(),
            model_config: crate::test_support::model_config(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                name: "test".into(),
                definition_id: api::AgentDefinitionId(1),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let publication = Publication::default();
    let probes = crate::probes::ProbeRegistry::start("test".into());
    let runtime = Runtime::start(
        db.clone(),
        publication.clone(),
        ProviderRegistry::default(),
        probes.clone(),
    )
    .await
    .unwrap();
    let state = AppState {
        database: db,
        publication,
        runtime: runtime.handle.clone(),
        accepting: Arc::new(AtomicBool::new(true)),
        client_key: "test-client-key".into(),
        client_config: psa::api::ClientConfig {
            default_agent_name: "root".into(),
            default_sandbox_image: None,
        },
        probes,
    };
    (dir, state, runtime, session, agent)
}

fn log(index: usize) -> NewEvent {
    NewEvent::new(EventType::Log(Log {
        error: false,
        message: index.to_string(),
    }))
}

#[tokio::test]
async fn context_pages_start_at_the_marker_opening_the_requested_context() {
    let (_dir, state, _runtime, session, agent) = fixture().await;
    let new_context = || NewEvent::new(EventType::NewContext);
    let mut events: Vec<_> = (0..3).map(log).collect();
    events.push(new_context());
    events.extend((3..6).map(log));
    events.push(new_context());
    events.push(log(6));
    let mut cursor = state.database.write().await.unwrap();
    cursor.add_events(agent.id, &events).await.unwrap();
    cursor.commit().await.unwrap();

    let all = load_page(&state, session.id, Some(agent.id), &Default::default())
        .await
        .unwrap();
    let ids: Vec<_> = all.events.iter().map(|event| event.event.id.0).collect();
    let markers: Vec<_> = all
        .events
        .iter()
        .filter(|event| matches!(event.event.kind, EventType::NewContext))
        .map(|event| event.event.id.0)
        .collect();
    let [first_marker, second_marker] = markers[..] else {
        panic!("expected two context markers");
    };
    let context = |through, limit| {
        let state = &state;
        async move {
            load_page(
                state,
                session.id,
                Some(agent.id),
                &api::EventsQuery {
                    span: api::EventSpan::ContextOf { through },
                    limit,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        }
    };
    let page_ids =
        |page: &api::EventsPage| -> Vec<_> { page.events.iter().map(|e| e.event.id.0).collect() };

    // Before any marker the context begins with the agent's first event.
    let page = context(first_marker - 1, 1000).await;
    assert_eq!(page_ids(&page), ids[..ids.len() - 6]);
    assert_eq!(page.events.first().unwrap().event.id.0, ids[0]);

    // A later context leads with its own marker and stops at the bound.
    let page = context(second_marker - 1, 1000).await;
    assert_eq!(
        page_ids(&page),
        (first_marker..second_marker).collect::<Vec<_>>()
    );
    assert!(!page.has_more);
    let page = context(second_marker, 1000).await;
    assert_eq!(page_ids(&page), [second_marker]);

    // A partial page continues with ordinary forward pagination.
    let page = context(second_marker - 1, 2).await;
    assert_eq!(page_ids(&page), [first_marker, first_marker + 1]);
    assert!(page.has_more);
    let rest = load_page(
        &state,
        session.id,
        Some(agent.id),
        &api::EventsQuery {
            span: api::EventSpan::Forward {
                after: page.next_after,
                through: Some(second_marker - 1),
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        page_ids(&rest),
        (first_marker + 2..second_marker).collect::<Vec<_>>()
    );

    // Context pages need an agent route and every event.
    let context_query = || api::EventsQuery {
        span: api::EventSpan::ContextOf {
            through: second_marker,
        },
        ..Default::default()
    };
    for (scope, query) in [
        (None, context_query()),
        (
            Some(agent.id),
            api::EventsQuery {
                results_only: true,
                ..context_query()
            },
        ),
    ] {
        assert!(load_page(&state, session.id, scope, &query).await.is_err());
    }
}

#[tokio::test]
async fn recent_backward_pages_and_forward_replay_cover_both_cursor_scopes() {
    let (_dir, state, runtime, session, agent) = fixture().await;
    let mut cursor = state.database.write().await.unwrap();
    cursor
        .add_events(agent.id, &(0..7).map(log).collect::<Vec<_>>())
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let mut heads = Vec::new();
    for scope in [None, Some(agent.id)] {
        let recent = load_page(
            &state,
            session.id,
            scope,
            &api::EventsQuery {
                span: api::EventSpan::Latest,
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(recent.events.len(), 3);
        assert!(recent.has_older);
        assert!(
            recent
                .events
                .windows(2)
                .all(|v| event_cursor(&v[0], scope) < event_cursor(&v[1], scope))
        );
        let older = load_page(
            &state,
            session.id,
            scope,
            &api::EventsQuery {
                span: api::EventSpan::Before(recent.next_before.unwrap()),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(older.events.len(), 3);
        let oldest = load_page(
            &state,
            session.id,
            scope,
            &api::EventsQuery {
                span: api::EventSpan::Before(older.next_before.unwrap()),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(oldest.events.len(), 1);
        assert!(!oldest.has_older);
        let mut identities = oldest
            .events
            .iter()
            .chain(&older.events)
            .chain(&recent.events)
            .map(|e| event_cursor(e, scope))
            .collect::<Vec<_>>();
        identities.dedup();
        assert_eq!(identities.len(), 7);
        heads.push((scope, recent.head));
    }
    // A commit after history capture is replayed from either scoped head.
    let mut cursor = state.database.write().await.unwrap();
    cursor.add_events(agent.id, &[log(99)]).await.unwrap();
    cursor.commit().await.unwrap();
    for (scope, head) in heads {
        let replay = load_page(
            &state,
            session.id,
            scope,
            &api::EventsQuery {
                span: api::EventSpan::Forward {
                    after: head,
                    through: None,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(replay.events.len(), 1);
    }
    // Agent IDs and session positions differ because creation consumes a position.
    let agent_page = load_page(
        &state,
        session.id,
        Some(agent.id),
        &api::EventsQuery {
            span: api::EventSpan::Latest,
            limit: 10_000,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(agent_page.events.len(), 8);
    assert_ne!(
        agent_page.events[0].position,
        agent_page.events[0].event.id.0
    );
    assert!(!agent_page.has_older);
    let mut headers = HeaderMap::new();
    headers.insert("accept", "text/event-stream".parse().unwrap());
    assert!(
        events_response(
            state.clone(),
            session.id,
            None,
            api::EventsQuery {
                span: api::EventSpan::Latest,
                ..Default::default()
            },
            headers
        )
        .await
        .is_err()
    );
    runtime.shutdown().await.unwrap();
    state.database.close().await;
}

#[tokio::test]
async fn summary_keysets_preserve_roots_include_forks_and_reject_cross_scope_cursors() {
    let (_dir, state, runtime, session, _agent) = fixture().await;
    let mut cursor = state.database.write().await.unwrap();
    let fork = cursor
        .fork_session(
            session.id,
            psa_store::SessionFork {
                point: psa_store::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let list = |query: api::SessionsQuery| list_sessions(State(state.clone()), ApiJson(query));
    let Json(roots) = list(Default::default()).await.unwrap();
    assert_eq!(roots.sessions.len(), 1);
    let query = api::SessionsQuery {
        flat: true,
        limit: 1,
        ..Default::default()
    };
    let Json(first) = list(query.clone()).await.unwrap();
    assert_eq!(first.sessions[0].id, fork.session.id);
    let Json(second) = list(api::SessionsQuery {
        before: first.next_before.clone(),
        ..query.clone()
    })
    .await
    .unwrap();
    assert_eq!(second.sessions[0].id, session.id);
    assert!(second.next_before.is_none());
    assert!(
        list(api::SessionsQuery {
            flat: false,
            before: first.next_before.clone(),
            ..query.clone()
        })
        .await
        .is_err()
    );
    assert!(
        list(api::SessionsQuery {
            filter: api::SessionFilter {
                name_contains: "x".into(),
                ..Default::default()
            },
            before: first.next_before,
            ..query.clone()
        })
        .await
        .is_err(),
        "a cursor must not be reused with another filter"
    );
    let Json(filtered) = list(api::SessionsQuery {
        filter: api::SessionFilter {
            probe_ids: vec![api::ProbeId(api::probes::Uuid::new_v4())],
            ..Default::default()
        },
        ..query
    })
    .await
    .unwrap();
    assert!(filtered.sessions.is_empty());
    runtime.shutdown().await.unwrap();
    state.database.close().await;
}

#[tokio::test]
async fn global_ready_page_revision_busy_race_filter_reconnect_and_overflow() {
    let (_dir, state, runtime, session, _agent) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = api::Client::new(&base, "test-client-key").unwrap();
    let mut stream = client
        .stream_global_events(&Default::default(), None)
        .await
        .unwrap();
    let api::GlobalStreamMessage::Ready { revision: ready } =
        stream.next().await.unwrap().unwrap().message
    else {
        panic!("missing ready")
    };
    state
        .publication
        .update_session_busy(&state.database, session.id, true)
        .await
        .unwrap();
    let page = client
        .list_sessions(&api::SessionsQuery {
            flat: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(page.sessions[0].busy);
    assert!(page.revision > ready);
    let api::GlobalStreamMessage::SessionUpdated {
        revision,
        session: busy,
    } = stream.next().await.unwrap().unwrap().message
    else {
        panic!("missing upsert")
    };
    assert!(revision <= page.revision);
    state
        .publication
        .update_session_busy(&state.database, session.id, false)
        .await
        .unwrap();
    let api::GlobalStreamMessage::SessionUpdated {
        revision,
        session: idle,
    } = stream.next().await.unwrap().unwrap().message
    else {
        panic!("missing idle")
    };
    assert!(revision > page.revision);
    assert_eq!(busy.updated_at, idle.updated_at);
    let mut reconnect = client
        .stream_global_events(&Default::default(), Some("0"))
        .await
        .unwrap();
    assert!(matches!(
        reconnect.next().await.unwrap().unwrap().message,
        api::GlobalStreamMessage::ResyncRequired
    ));
    assert!(reconnect.next().await.is_none());
    // Capture the subscription without polling its body; overflow is explicit.
    let response = global_events(
        State(state.clone()),
        ApiQuery(Default::default()),
        HeaderMap::new(),
    )
    .await
    .unwrap();
    for _ in 0..1100 {
        state
            .publication
            .lock()
            .await
            .publish(api::StreamMessage::SessionUpdated {
                session: session.clone().into_api(false),
            });
    }
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("ready"));
    assert!(text.contains("resync_required"));
    server.abort();
    runtime.shutdown().await.unwrap();
    state.database.close().await;
}

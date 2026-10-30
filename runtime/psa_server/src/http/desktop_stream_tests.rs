use super::desktop_tests::fixture;
use super::*;
use crate::agents::agent_projection::AgentProjection;
use futures_util::StreamExt;
use std::time::Duration;

async fn next(stream: &mut api::EventStream) -> api::StreamMessage {
    tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .message
}

#[tokio::test]
async fn recent_to_sse_gap_replays_then_live_promotions_keep_identity() {
    let (_dir, state, runtime, session, agent) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = api::Client::new(&base, "test-client-key").unwrap();
    let page = client
        .events(
            session.id,
            Some(agent.id),
            &api::EventsQuery {
                recent: true,
                include_pending: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(page.events.is_empty());
    let event = NewEvent::new(EventType::Log(Log {
        error: false,
        message: "gap".into(),
    }));
    let mut global = state.publication.lock().await.subscribe_global();
    let (_, context) = state
        .publication
        .append(
            &state.database,
            agent.id,
            &[event.clone()],
            &[],
            &AgentProjection::default(),
        )
        .await
        .unwrap();
    let api::GlobalStreamMessage::SessionUpdated {
        session: committed, ..
    } = global.recv().await.unwrap()
    else {
        panic!("commit did not publish summary")
    };
    let persisted = state.runtime.get_session(session.id).await.unwrap();
    assert_eq!(committed.updated_at, persisted.updated_at);
    let mut stream = client
        .stream_events(
            session.id,
            Some(agent.id),
            &api::EventsQuery {
                after: page.head,
                include_pending: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let api::StreamMessage::Committed { event: replay, .. } = next(&mut stream).await else {
        panic!("missing gap replay")
    };
    assert_eq!(replay.event.id.0, 1);
    assert!(matches!(
        next(&mut stream).await,
        api::StreamMessage::Snapshot { cursor: 1, .. }
    ));
    state
        .publication
        .upsert(session.id, agent.id, "draft".into(), event.kind.clone())
        .await;
    assert!(
        matches!(next(&mut stream).await, api::StreamMessage::Upsert { pending } if pending.id == "draft")
    );
    state
        .publication
        .append(
            &state.database,
            agent.id,
            &[event],
            &[(0, "draft".into())],
            &context,
        )
        .await
        .unwrap();
    assert!(
        matches!(next(&mut stream).await, api::StreamMessage::Committed { pending_id: Some(id), .. } if id == "draft")
    );
    let older = client
        .events(
            session.id,
            Some(agent.id),
            &api::EventsQuery {
                before: Some(2),
                include_pending: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(older.pending.is_empty() && older.queries.is_empty());
    assert_eq!(older.events.len(), 1);
    server.abort();
    runtime.shutdown().await.unwrap();
    state.database.close().await;
}

#[tokio::test]
async fn global_probe_filter_suppresses_other_probes_and_agent_creation_updates_summary() {
    let (_dir, state, runtime, session, _agent) = fixture().await;
    let mut summaries = state.publication.lock().await.subscribe_global();
    let created = state
        .runtime
        .create_agent(
            session.id,
            api::CreateAgent {
                subagent_parent_id: None,
                definition_id: api::AgentDefinitionId(1),
                name: "new".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    let api::GlobalStreamMessage::SessionUpdated {
        session: summary, ..
    } = tokio::time::timeout(Duration::from_secs(3), summaries.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("missing agent creation summary")
    };
    assert_eq!(summary.updated_at, created.updated_at);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = api::Client::new(&base, "test-client-key").unwrap();
    let mut filtered = client
        .stream_global_events(
            &api::GlobalEventsQuery {
                probe_id: Some(api::ProbeId(api::probes::Uuid::new_v4())),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        filtered.next().await.unwrap().unwrap().message,
        api::GlobalStreamMessage::Ready { .. }
    ));
    state
        .publication
        .update_session_busy(&state.database, session.id, true)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), filtered.next())
            .await
            .is_err()
    );
    server.abort();
    runtime.shutdown().await.unwrap();
    state.database.close().await;
}

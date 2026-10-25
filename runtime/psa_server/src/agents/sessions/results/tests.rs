use super::*;
use crate::agents::{
    agent_manager::AgentManagerState,
    publication::status_event,
    sessions::tests::{Fixture, create_agent_request},
};
use psa_api::{AgentResult, ForkPoint, ForkSession};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

async fn create_fixture() -> (Fixture, SessionId, AgentId) {
    let fixture = Fixture::new().await;
    let session = fixture.create_session().await;
    let agent = fixture
        .sessions
        .create_agent(session.id, create_agent_request())
        .await
        .unwrap();
    (fixture, session.id, agent.id)
}

async fn append(sessions: &Sessions, agent: AgentId, events: &[NewEvent]) -> Vec<Event> {
    let mut state = AgentManagerState::load_persisted(
        sessions.database.clone(),
        sessions.publication.clone(),
        agent,
    )
    .await
    .unwrap();
    // Results require a running agent and an atomic paired Idle completion.
    // Supply that fixture protocol even in tests focused only on the artifact.
    let mut batch = Vec::new();
    for event in events {
        if matches!(event.kind, EventType::Result(_)) {
            batch.push(status_event(AgentState::Running));
            batch.push(event.clone());
            batch.push(status_event(AgentState::Idle));
        } else {
            batch.push(event.clone());
        }
    }
    state.persist_events(&batch).await.unwrap()
}

fn result(value: &str) -> NewEvent {
    NewEvent::new(EventType::Result(AgentResult::Text(value.into())))
}

struct ResultWait {
    task: tokio::task::JoinHandle<Result<ResultRecord, RuntimeError>>,
}

impl Future for ResultWait {
    type Output = Result<ResultRecord, RuntimeError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().task)
            .poll(context)
            .map(|result| result.expect("result waiting task must not panic"))
    }
}

impl Drop for ResultWait {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn spawn_wait(
    sessions: &Sessions,
    session: SessionId,
    agent: AgentId,
    after: Option<EventId>,
    mut cancellation: Cancellation,
) -> ResultWait {
    let sessions = sessions.clone();
    ResultWait {
        task: tokio::spawn(async move {
            sessions
                .await_result(session, agent, after, &mut cancellation)
                .await
        }),
    }
}

async fn assert_pending<F: Future>(wait: Pin<&mut F>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(20), wait)
            .await
            .is_err(),
        "result wait must remain pending"
    );
}

async fn finish<F: Future>(wait: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .expect("result wait must not hang")
}

#[tokio::test]
async fn existing_results_are_newest_first_and_retain_fork_provenance() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    append(sessions, agent, &[result("first"), result("newest")]).await;
    let expected = sessions
        .load_last_result(session, agent)
        .await
        .unwrap()
        .unwrap();
    let (_cancel, mut cancellation) = Cancellation::channel();
    assert_eq!(
        finish(sessions.await_result(session, agent, None, &mut cancellation))
            .await
            .unwrap(),
        expected
    );
    let fork = sessions
        .fork_session(
            session,
            ForkSession {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let inherited =
        finish(sessions.await_result(fork.session.id, fork.agents[0].id, None, &mut cancellation))
            .await
            .unwrap();
    assert_eq!(inherited.session_id, fork.session.id);
    assert_eq!(inherited.agent_id, fork.agents[0].id);
    assert_eq!(inherited.source_agent_id, agent);
    assert_eq!(inherited.event_id, expected.event_id);
    assert_eq!(inherited.value, expected.value);
}

#[tokio::test]
async fn boundary_excludes_stale_results_and_unstarted_idle_agents_can_wait() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let (_cancel, cancellation) = Cancellation::channel();
    let initial_wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(initial_wait);
    assert_pending(initial_wait.as_mut()).await;
    append(
        sessions,
        agent,
        &[result("old"), status_event(AgentState::Idle)],
    )
    .await;
    let old = finish(initial_wait).await.unwrap();
    let baseline = EventId(
        sessions
            .get_agent(session, agent)
            .await
            .unwrap()
            .next_event_id
            .0
            - 1,
    );
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, Some(baseline), cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    append(sessions, agent, &[status_event(AgentState::Running)]).await;
    assert_pending(wait.as_mut()).await;
    append(
        sessions,
        agent,
        &[result("new"), status_event(AgentState::Idle)],
    )
    .await;
    let new = finish(wait).await.unwrap();
    assert!(new.event_id > baseline);
    assert!(new.event_id > old.event_id);
    assert_eq!(new.value, AgentResult::Text("new".into()));
}

#[tokio::test]
async fn a_result_committed_before_first_poll_is_not_missed() {
    let (fixture, session, agent) = create_fixture().await;
    let (_cancel, mut cancellation) = Cancellation::channel();
    let wait = fixture
        .sessions
        .await_result(session, agent, Some(EventId(0)), &mut cancellation);
    append(&fixture.sessions, agent, &[result("ready")]).await;
    assert_eq!(
        finish(wait).await.unwrap().value,
        AgentResult::Text("ready".into())
    );
}

#[tokio::test]
async fn interrupted_and_completed_agents_without_matching_results_do_not_hang() {
    for state in [AgentState::Interrupted, AgentState::Idle] {
        let (fixture, session, agent) = create_fixture().await;
        let sessions = &fixture.sessions;
        append(sessions, agent, &[status_event(AgentState::Running)]).await;
        let (_cancel, cancellation) = Cancellation::channel();
        let wait = spawn_wait(sessions, session, agent, Some(EventId(0)), cancellation);
        tokio::pin!(wait);
        assert_pending(wait.as_mut()).await;
        append(sessions, agent, &[status_event(state)]).await;
        assert!(matches!(
            finish(wait).await,
            Err(RuntimeError::ResultUnavailable { .. })
        ));
        let (_cancel, mut cancellation) = Cancellation::channel();
        assert!(matches!(
            finish(sessions.await_result(session, agent, None, &mut cancellation)).await,
            Err(RuntimeError::ResultUnavailable { .. })
        ));
    }
}

#[tokio::test]
async fn failed_requests_wake_waiters_and_result_commit_wins_over_terminal_state() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    sessions
        .publication
        .lock()
        .await
        .publish(StreamMessage::RequestFailed {
            session_id: session,
            agent_id: agent,
            request_id: "failed".into(),
            message: "worker failed".into(),
        });
    assert!(matches!(
        finish(wait).await,
        Err(RuntimeError::ResultUnavailable { .. })
    ));
    append(
        sessions,
        agent,
        &[result("durable"), status_event(AgentState::Interrupted)],
    )
    .await;
    let (_cancel, mut cancellation) = Cancellation::channel();
    assert_eq!(
        finish(sessions.await_result(session, agent, None, &mut cancellation))
            .await
            .unwrap()
            .value,
        AgentResult::Text("durable".into())
    );
}

#[tokio::test]
async fn idle_startup_failure_is_terminal_but_historical_errors_are_excluded() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    let events = append(
        sessions,
        agent,
        &[NewEvent::new(EventType::Log(Log {
            error: true,
            message: EXECUTION_FAILURE_MESSAGE.into(),
        }))],
    )
    .await;
    assert!(matches!(
        finish(wait).await,
        Err(RuntimeError::ResultUnavailable { .. })
    ));
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(
        sessions,
        session,
        agent,
        Some(events.last().unwrap().id),
        cancellation,
    );
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    append(sessions, agent, &[result("retry succeeds")]).await;
    assert!(finish(wait).await.is_ok());
}

#[tokio::test]
async fn nonfatal_idle_error_logs_do_not_end_waits() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    append(
        sessions,
        agent,
        &[NewEvent::new(EventType::Log(Log {
            error: true,
            message: "Could not load optional project context".into(),
        }))],
    )
    .await;
    assert_pending(wait.as_mut()).await;
    append(sessions, agent, &[result("completed despite warning")]).await;
    assert_eq!(
        finish(wait).await.unwrap().value,
        AgentResult::Text("completed despite warning".into())
    );
}

#[tokio::test]
async fn lagged_subscription_resyncs_from_durable_state() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    // Commit without waking the receiver, then overflow its channel without
    // yielding. The waiter stays runnable during DB access and can release any
    // in-progress read transaction, but cannot consume these notifications until
    // the bounded channel has lagged.
    let publication = sessions.publication.lock().await;
    let mut cursor = sessions.database.cursor().await.unwrap();
    cursor
        .add_events(
            agent,
            &[
                status_event(AgentState::Running),
                result("survives lag"),
                status_event(AgentState::Idle),
            ],
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    for _ in 0..2048 {
        publication.publish(StreamMessage::ResyncRequired);
    }
    drop(publication);
    assert_eq!(
        finish(wait).await.unwrap().value,
        AgentResult::Text("survives lag".into())
    );
}

#[tokio::test]
async fn cancellation_takes_precedence_and_does_not_mutate_target_work() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let (cancel, mut cancellation) = Cancellation::channel();
    cancel.send(()).unwrap();
    assert!(matches!(
        finish(sessions.await_result(SessionId(0), AgentId(0), None, &mut cancellation)).await,
        Err(RuntimeError::Cancelled)
    ));
    let (cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    cancel.send(()).unwrap();
    assert!(matches!(finish(wait).await, Err(RuntimeError::Cancelled)));
    assert_eq!(
        sessions.get_agent(session, agent).await.unwrap().state,
        AgentState::Idle
    );
    assert!(
        sessions
            .load_last_result(session, agent)
            .await
            .unwrap()
            .is_none()
    );
    append(sessions, agent, &[result("still running independently")]).await;
    let (_cancel, mut cancellation) = Cancellation::channel();
    assert!(
        finish(sessions.await_result(session, agent, None, &mut cancellation))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn cancellation_and_shutdown_interrupt_gate_and_database_waits() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let publication = sessions.publication.lock().await;
    let (cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    cancel.send(()).unwrap();
    assert!(matches!(finish(wait).await, Err(RuntimeError::Cancelled)));
    drop(publication);
    let cursor = sessions.database.cursor().await.unwrap();
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    sessions.publication.close();
    assert!(matches!(finish(wait).await, Err(RuntimeError::Unavailable)));
    cursor.rollback().await.unwrap();
    let (_cancel, mut cancellation) = Cancellation::channel();
    assert!(matches!(
        finish(sessions.await_result(session, agent, None, &mut cancellation)).await,
        Err(RuntimeError::Unavailable)
    ));
}

#[tokio::test]
async fn dropping_a_wait_has_no_side_effects_and_scope_is_validated() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let before = sessions
        .get_agent(session, agent)
        .await
        .unwrap()
        .next_event_id;
    let (_cancel, mut cancellation) = Cancellation::channel();
    {
        let wait = sessions.await_result(session, agent, None, &mut cancellation);
        tokio::pin!(wait);
        assert_pending(wait.as_mut()).await;
    }
    assert_eq!(
        sessions
            .get_agent(session, agent)
            .await
            .unwrap()
            .next_event_id,
        before
    );
    let other_session = fixture.create_session().await;
    assert!(matches!(
        finish(sessions.await_result(other_session.id, agent, None, &mut cancellation)).await,
        Err(RuntimeError::Database(crate::db::DbError::NotFound { .. }))
    ));
    append(sessions, agent, &[result("not consumed")]).await;
    assert!(
        finish(sessions.await_result(session, agent, None, &mut cancellation))
            .await
            .is_ok()
    );
    assert!(
        finish(sessions.await_result(session, agent, None, &mut cancellation))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn unrelated_agent_publications_do_not_resolve_the_wait() {
    let (fixture, session, agent) = create_fixture().await;
    let sessions = &fixture.sessions;
    let other = sessions
        .create_agent(session, create_agent_request())
        .await
        .unwrap();
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    append(sessions, other.id, &[result("other")]).await;
    sessions
        .publication
        .lock()
        .await
        .publish(StreamMessage::RequestFailed {
            session_id: session,
            agent_id: other.id,
            request_id: "other".into(),
            message: "failed".into(),
        });
    assert_pending(wait.as_mut()).await;
    append(sessions, agent, &[result("target")]).await;
    assert_eq!(
        finish(wait).await.unwrap().value,
        AgentResult::Text("target".into())
    );
}

#[tokio::test]
async fn standalone_runtime_shutdown_releases_result_waits() {
    let (fixture, session, agent) = create_fixture().await;
    let runtime = crate::agents::session_manager::Runtime::start(
        fixture.sessions.database.clone(),
        fixture.sessions.publication.clone(),
        crate::providers::ProviderRegistry::default(),
        crate::probes::ProbeRegistry::start("test-key".into()),
    )
    .await
    .unwrap();
    let sessions = runtime.handle.clone();
    let (_cancel, cancellation) = Cancellation::channel();
    let wait = spawn_wait(&sessions, session, agent, None, cancellation);
    tokio::pin!(wait);
    assert_pending(wait.as_mut()).await;
    finish(runtime.shutdown()).await.unwrap();
    assert!(matches!(finish(wait).await, Err(RuntimeError::Unavailable)));
}

use super::*;
use crate::agents::{
    client_queries::ClientQueries,
    publication::Publication,
    supervisor::{SupervisorClient, SupervisorCommand},
};

async fn fixture() -> (
    tempfile::TempDir,
    ClientQueries,
    actors::Mailbox<SupervisorCommand>,
    State,
) {
    let directory = tempfile::tempdir().unwrap();
    let database = crate::db::Database::open(directory.path().join("subagents.sqlite3"))
        .await
        .unwrap();
    let publication = Publication::default();
    let queries = ClientQueries::start(publication.clone());
    let (sender, manager) = actors::channel(32);
    let sessions = RuntimeHandle::new(
        database,
        publication,
        SupervisorClient::new(sender),
        queries.handle(),
    );
    (
        directory,
        queries,
        manager,
        State::new(SessionId(1), sessions),
    )
}

fn start_run(state: &mut State, agent: u64) {
    state.handle_command(Command::Lifecycle(AgentLifecycleEvent::Started {
        session_id: SessionId(1),
        agent_id: AgentId(agent),
        run_id: format!("run-{agent}"),
    }));
}

fn register_wait(state: &mut State, agent: u64) -> tokio::sync::oneshot::Receiver<ResultValue> {
    let (reply, received) = tokio::sync::oneshot::channel();
    state.handle_command(Command::Wait {
        agent: AgentId(agent),
        token: Uuid::new_v4(),
        reply,
    });
    received
}

#[tokio::test]
async fn session_progress_and_pending_operations_defer_deadlock() {
    let (_directory, _queries, _manager, mut state) = fixture().await;
    start_run(&mut state, 1);
    start_run(&mut state, 2); // Any session run, including agents without the bundle.
    let mut first = register_wait(&mut state, 1);
    state.deadlock_after = tokio::time::Instant::now();
    state.check_deadlock();
    assert!(first.try_recv().is_err());
    let second = register_wait(&mut state, 2);
    let token = Uuid::new_v4();
    state.handle_command(Command::BeginOperation(token));
    state.deadlock_after = tokio::time::Instant::now();
    state.check_deadlock();
    assert!(first.try_recv().is_err());
    state.handle_command(Command::EndOperation(token));
    state.check_deadlock();
    assert!(first.await.unwrap().unwrap_err().contains("session"));
    assert!(second.await.unwrap().is_err());
    assert!(state.waiters.is_empty());
}

#[tokio::test]
async fn dropped_wait_and_stale_end_wait_do_not_remove_new_registration() {
    let (_directory, _queries, _manager, mut state) = fixture().await;
    start_run(&mut state, 1);
    let received = register_wait(&mut state, 1);
    let old = state.waiters[&AgentId(1)].token;
    drop(received);
    state.check_deadlock();
    assert!(state.waiters.is_empty());
    let _new = register_wait(&mut state, 1);
    state.handle_command(Command::EndWait {
        agent: AgentId(1),
        token: old,
    });
    assert_eq!(state.waiters.len(), 1);
}

#[tokio::test]
async fn lifecycle_callbacks_are_once_per_logical_run_and_ignore_stale_finishes() {
    let (_directory, _queries, _manager, mut state) = fixture().await;
    start_run(&mut state, 1);
    let finished = AgentLifecycleEvent::Finished {
        session_id: SessionId(1),
        agent_id: AgentId(1),
        run_id: "run-1".into(),
        outcome: crate::agents::lifecycle::AgentRunOutcome::Completed,
        final_text: Some("answer".into()),
    };
    state.handle_command(Command::Lifecycle(finished.clone()));
    state.handle_command(Command::Lifecycle(finished.clone()));
    assert_eq!(state.jobs.len(), 1);
    state.handle_command(Command::Lifecycle(AgentLifecycleEvent::Started {
        session_id: SessionId(1),
        agent_id: AgentId(1),
        run_id: "new-run".into(),
    }));
    state.handle_command(Command::Lifecycle(finished));
    assert_eq!(state.active[&AgentId(1)], "new-run");
    assert_eq!(state.jobs.len(), 1);
}

#[tokio::test]
async fn shutdown_cancels_blocked_callbacks_and_waiters() {
    let (_directory, _queries, _manager, state) = fixture().await;
    state.jobs.push(Box::pin(std::future::pending()));
    let (sender, mailbox) = actors::unbounded_channel();
    let task = tokio::spawn(state.run(mailbox));
    let (reply, received) = tokio::sync::oneshot::channel();
    sender
        .send(Command::Wait {
            agent: AgentId(1),
            token: Uuid::new_v4(),
            reply,
        })
        .unwrap();
    sender.send(Command::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(received.await.unwrap().is_err());
}

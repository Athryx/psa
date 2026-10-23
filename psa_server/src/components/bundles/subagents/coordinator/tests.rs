use super::*;
mod concurrent_wait_tests;
mod lifecycle_tests;
mod validation_tests;
use crate::{
    agents::{
        client_queries::ClientQueries,
        publication::Publication,
        session_manager::{ManagerClient, ManagerCommand},
    },
    db::Database,
};

struct Fixture {
    _directory: tempfile::TempDir,
    _queries: ClientQueries,
    manager: actors::Mailbox<ManagerCommand>,
    state: State,
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("subagents.sqlite3"))
            .await
            .unwrap();
        let publication = Publication::default();
        let queries = ClientQueries::start(publication.clone());
        let (sender, manager) = actors::channel(32);
        let sessions = Sessions::new(
            database,
            publication,
            ManagerClient::new(sender),
            queries.handle(),
        );
        let mut state = State::new(SessionId(1), sessions);
        state.ready = true;
        for (id, name, parent, group) in [
            (1, "main", None, 1),
            (2, "child", Some(1), 1),
            (3, "other", None, 3),
        ] {
            state.names.insert(name.into());
            state.members.insert(
                AgentId(id),
                Member {
                    id: AgentId(id),
                    name: name.into(),
                    parent: parent.map(AgentId),
                    group: AgentId(group),
                    template: "test".into(),
                    status: "running".into(),
                    error: None,
                },
            );
        }
        Self {
            _directory: directory,
            _queries: queries,
            manager,
            state,
        }
    }
}

async fn accept_steering(state: &mut State, manager: &mut actors::Mailbox<ManagerCommand>) {
    let serve = async {
        let ManagerCommand::Submit { reply, .. } = manager.recv().await.unwrap() else {
            panic!("expected steering request");
        };
        let _ = reply.send(Ok(psa_api::RequestAccepted {
            request_id: "accepted".into(),
        }));
    };
    let (job, ()) = tokio::join!(state.jobs.next(), serve);
    state.handle_job_result(job.unwrap());
}

#[tokio::test]
async fn waiter_receives_acknowledgement_only_after_steering_is_accepted() {
    let mut fixture = Fixture::new().await;
    let (waiter, received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: WaitToken::new_v4(),
        reply: waiter,
    });
    let (reply, result) = tokio::sync::oneshot::channel();
    fixture
        .state
        .deliver(AgentId(2), AgentId(1), "hello".into(), Some(reply), None);
    assert_eq!(fixture.state.jobs.len(), 1);
    accept_steering(&mut fixture.state, &mut fixture.manager).await;
    let message = received.await.unwrap().unwrap();
    assert_eq!(message["messages"][0]["from"], "child");
    assert!(message["messages"][0].get("message").is_none());
    assert!(message["messages"][0].get("from_id").is_none());
    assert_eq!(result.await.unwrap().unwrap()["status"], "queued");
    assert!(fixture.state.jobs.is_empty());
    assert!(fixture.manager.try_recv().is_err());
}

#[tokio::test]
async fn group_authorization_and_logical_run_report_deduplication() {
    let mut fixture = Fixture::new().await;
    assert_eq!(
        fixture.state.resolve_target(AgentId(1), "child").unwrap(),
        AgentId(2)
    );
    assert!(fixture.state.resolve_target(AgentId(1), "other").is_err());
    let (waiter, received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: WaitToken::new_v4(),
        reply: waiter,
    });
    let event = AgentLifecycleEvent::Finished {
        session_id: SessionId(1),
        agent_id: AgentId(2),
        run_id: "logical-run".into(),
        outcome: AgentRunOutcome::Completed,
        final_text: Some("answer".into()),
    };
    fixture.state.apply_lifecycle_event(event.clone());
    accept_steering(&mut fixture.state, &mut fixture.manager).await;
    assert_eq!(
        received.await.unwrap().unwrap()["messages"][0]["from"],
        "child"
    );
    fixture.state.apply_lifecycle_event(event);
    assert!(fixture.state.jobs.is_empty());
    assert_eq!(fixture.state.members[&AgentId(2)].status, "idle");
}

#[tokio::test]
async fn deferred_deadlock_counts_pending_parallel_jobs_as_progress() {
    let mut fixture = Fixture::new().await;
    fixture.state.members.get_mut(&AgentId(2)).unwrap().status = "idle".into();
    let (waiter, mut received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: WaitToken::new_v4(),
        reply: waiter,
    });
    fixture.state.check_deadlock();
    assert!(received.try_recv().is_err());
    fixture.state.deadlock_after = tokio::time::Instant::now();
    fixture.state.jobs.push(std::future::pending().boxed());
    fixture.state.check_deadlock();
    assert!(received.try_recv().is_err());
    fixture.state.jobs.clear();
    fixture.state.check_deadlock();
    assert!(received.await.unwrap().unwrap_err().contains("nobody else"));
}

#[tokio::test]
async fn shutdown_remains_pollable_while_session_manager_rpc_is_blocked() {
    let fixture = Fixture::new().await;
    let (sender, mailbox) = actors::unbounded_channel();
    let task = AbortTask(tokio::spawn(fixture.state.run(mailbox)));
    let (reply, _received) = tokio::sync::oneshot::channel();
    let (_cancel, cancellation) = Cancellation::channel();
    sender
        .send(Command::Send {
            caller: AgentId(1),
            name: "child".into(),
            message: "hi".into(),
            cancellation,
            reply,
        })
        .unwrap();
    let mut manager = fixture.manager;
    let pending = tokio::time::timeout(Duration::from_secs(1), manager.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(pending, ManagerCommand::Submit { .. }));
    // Retain the unserved manager reply: the coordinator must still shut down.
    sender.send(Command::Shutdown).unwrap();
    let mut task = task;
    tokio::time::timeout(Duration::from_secs(1), &mut task.0)
        .await
        .unwrap()
        .unwrap();
    drop(pending);
}

#[tokio::test]
async fn completed_parent_does_not_interrupt_background_child() {
    let mut fixture = Fixture::new().await;
    fixture
        .state
        .apply_lifecycle_event(AgentLifecycleEvent::Finished {
            session_id: SessionId(1),
            agent_id: AgentId(1),
            run_id: "parent-run".into(),
            outcome: AgentRunOutcome::Completed,
            final_text: None,
        });
    assert_eq!(fixture.state.members[&AgentId(2)].status, "running");
    assert!(fixture.state.jobs.is_empty());
}

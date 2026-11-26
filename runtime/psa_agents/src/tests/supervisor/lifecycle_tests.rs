use super::{testing::lifecycle::*, *};

#[tokio::test]
async fn creation_runs_once_after_commit_and_not_on_worker_reload_or_restart() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start().await;
    let session = fixture.create_session(&runtime.handle).await;
    let agent = runtime
        .handle
        .create_agent(session.id, agent_request())
        .await
        .unwrap();
    let created = fixture.next("create", agent.id).await;
    assert_eq!(created.session.name, "original");
    assert_eq!(created.state, AgentState::Idle);
    // Reading while the hook is suspended also proves commit/publication locks
    // are released before dispatching lifecycle work.
    assert_eq!(
        runtime
            .handle
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .id,
        agent.id
    );
    created.release.send(()).unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;

    rename(&runtime.handle, session.id, "reloaded").await;
    fixture
        .next("update", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    runtime.shutdown().await.unwrap();

    let runtime = fixture.start().await;
    rename(&runtime.handle, session.id, "restarted").await;
    let updated = fixture.next("update", agent.id).await;
    assert_eq!(updated.session.name, "restarted");
    updated.release.send(()).unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert!(fixture.recorded.try_recv().is_err());
    runtime.shutdown().await.unwrap();
    fixture.probes.shutdown().await;
}

#[tokio::test]
async fn fork_runs_creation_only_for_the_new_child_agent() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start().await;
    let session = fixture.create_session(&runtime.handle).await;
    let parent = runtime
        .handle
        .create_agent(session.id, agent_request())
        .await
        .unwrap();
    fixture
        .next("create", parent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;

    let fork = runtime
        .handle
        .fork_session(
            session.id,
            psa::api::ForkSession {
                point: psa::api::ForkPoint::Head,
                metadata: psa::api::UpdateSession {
                    name: Some("child".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(fork.agents.len(), 1);
    assert_eq!(fork.agents[0].forked_from, Some(parent.id));
    let child = &fork.agents[0];
    assert_ne!(child.id, parent.id);
    let created = fixture.next("create", child.id).await;
    assert_eq!(created.session.id, fork.session.id);
    assert_eq!(created.session.forked_from, Some(session.id));
    assert_eq!(created.session.name, "child");
    assert_eq!(created.state, AgentState::Idle);
    assert_eq!(
        runtime
            .handle
            .get_agent(fork.session.id, child.id)
            .await
            .unwrap()
            .id,
        child.id
    );
    created.release.send(()).unwrap();
    fixture.wait_idle(&runtime.handle, fork.session.id).await;
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert!(
        fixture.recorded.try_recv().is_err(),
        "fork must not recreate the parent"
    );
    runtime.shutdown().await.unwrap();
    fixture.probes.shutdown().await;
}

#[tokio::test]
async fn idle_session_updates_spawn_all_agents_and_preserve_each_snapshot() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start().await;
    let session = fixture.create_session(&runtime.handle).await;
    let mut agents = Vec::new();
    for _ in 0..2 {
        let agent = runtime
            .handle
            .create_agent(session.id, agent_request())
            .await
            .unwrap();
        fixture
            .next("create", agent.id)
            .await
            .release
            .send(())
            .unwrap();
        agents.push(agent.id);
    }
    fixture.wait_idle(&runtime.handle, session.id).await;
    rename(&runtime.handle, session.id, "first").await;
    rename(&runtime.handle, session.id, "second").await;

    let mut seen = HashMap::<AgentId, Vec<String>>::new();
    for _ in 0..4 {
        let event = tokio::time::timeout(WAIT, fixture.recorded.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.name, "update");
        assert!(agents.contains(&event.agent));
        assert_eq!(
            event.session.working_directory,
            std::path::PathBuf::from(format!("/{}", event.session.name))
        );
        seen.entry(event.agent)
            .or_default()
            .push(event.session.name);
        event.release.send(()).unwrap();
    }
    for agent in agents {
        assert_eq!(seen[&agent], ["first", "second"]);
    }
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert!(fixture.recorded.try_recv().is_err());
    runtime.shutdown().await.unwrap();
    fixture.probes.shutdown().await;
}

#[tokio::test]
async fn update_arriving_after_stop_boundary_survives_worker_retirement() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start().await;
    let session = fixture.create_session(&runtime.handle).await;
    let agent = runtime
        .handle
        .create_agent(session.id, agent_request())
        .await
        .unwrap();
    fixture
        .next("create", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    runtime.shutdown().await.unwrap();

    // Drive the supervisor manually to pin the race between returning Stop and
    // processing the worker's join. No scheduler sleeps are needed.
    let (commands, received) = actors::channel(256);
    let commands = SupervisorClient::new(commands);
    let client_queries = ClientQueries::start(fixture.publication.clone());
    let sessions = RuntimeHandle::new(
        fixture.db.clone(),
        fixture.publication.clone(),
        commands.clone(),
        client_queries.handle(),
        std::sync::Arc::new(RecordingComponents(crate::bundles::testing::TestComponents)),
    );
    let mut manager = Supervisor {
        db: fixture.db.clone(),
        publication: fixture.publication.clone(),
        providers: ProviderRegistry::default(),
        probes: fixture.probes.clone(),
        sessions: sessions.clone(),
        client_queries,
        commands,
        received,
        workers: HashMap::new(),
        components: HashMap::new(),
        session_resources: HashMap::new(),
        steering: HashMap::new(),
        runs: HashMap::new(),
        lifecycle_events: Vec::new(),
        tasks: JoinSet::new(),
        next_generation: 1,
        shutting_down: false,
        shutdown_reply: None,
        cleanup_failed: false,
    };
    manager
        .spawn_worker(agent.id, session.id, VecDeque::new(), VecDeque::new())
        .await
        .unwrap();
    let command = tokio::time::timeout(WAIT, manager.received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(&command, SupervisorCommand::Boundary { .. }));
    manager.handle_command(command).await;
    assert!(manager.workers[&agent.id].stopping);
    let joined = tokio::time::timeout(WAIT, manager.tasks.join_next_with_id())
        .await
        .unwrap()
        .unwrap();

    let mut updated = session.clone();
    updated.name = "during-retirement".into();
    updated.working_directory = "/during-retirement".into();
    manager.schedule_session_update(&updated).await.unwrap();
    manager.handle_join(joined).await;
    assert!(
        manager.workers.contains_key(&agent.id),
        "queued hook must restart a retiring worker"
    );
    let task = tokio::spawn(manager.run());
    let event = fixture.next("update", agent.id).await;
    assert_eq!(event.session.name, "during-retirement");
    assert_eq!(
        event.session.working_directory,
        std::path::PathBuf::from("/during-retirement")
    );
    event.release.send(()).unwrap();
    fixture.wait_idle(&sessions, session.id).await;
    assert!(fixture.recorded.try_recv().is_err());
    sessions.commands.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    fixture.probes.shutdown().await;
}

#[tokio::test]
async fn running_worker_executes_queued_updates_before_resuming_inference() {
    let mut fixture = Fixture::new(true).await;
    let runtime = fixture.start().await;
    let session = fixture.create_session(&runtime.handle).await;
    let agent = runtime
        .handle
        .create_agent(session.id, agent_request())
        .await
        .unwrap();
    let created = fixture.next("create", agent.id).await;
    assert_eq!(created.state, AgentState::Running);

    // Keep the running worker inside its current hook while updates arrive.
    // Neither the update facade nor the supervisor may await hook completion.
    rename(&runtime.handle, session.id, "first").await;
    rename(&runtime.handle, session.id, "second").await;
    assert!(runtime.handle.is_busy(session.id).await.unwrap());
    created.release.send(()).unwrap();
    for name in ["first", "second"] {
        let event = fixture.next("update", agent.id).await;
        assert_eq!(event.state, AgentState::Running);
        assert_eq!(event.session.name, name);
        event.release.send(()).unwrap();
    }
    // The disconnected probe is consulted only after both hooks, and then
    // interrupts execution without ever needing a real provider.
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert_eq!(
        runtime
            .handle
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Interrupted
    );
    assert!(fixture.recorded.try_recv().is_err());
    runtime.shutdown().await.unwrap();
    fixture.probes.shutdown().await;
}

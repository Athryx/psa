use super::*;

#[tokio::test]
async fn invalid_names_blank_inputs_and_self_send_do_not_start_jobs() {
    let mut fixture = Fixture::new().await;
    for (name, task) in [("has space", "task"), ("", "task"), ("valid", " ")] {
        let (reply, received) = tokio::sync::oneshot::channel();
        let (_sender, cancellation) = Cancellation::channel();
        fixture.state.handle_command(Command::Create {
            caller: AgentId(1),
            allowed: vec![AgentDefinitionId(5)],
            name: name.into(),
            task: task.into(),
            template: Some("test".into()),
            cancellation,
            reply,
        });
        assert!(received.await.unwrap().is_err());
    }
    for (name, message) in [("main", "self"), ("child", " ")] {
        let (reply, received) = tokio::sync::oneshot::channel();
        let (_sender, cancellation) = Cancellation::channel();
        fixture.state.handle_command(Command::Send {
            caller: AgentId(1),
            name: name.into(),
            message: message.into(),
            cancellation,
            reply,
        });
        assert!(received.await.unwrap().is_err());
    }
    assert!(fixture.state.jobs.is_empty());
    assert!(fixture.state.reserved.is_empty());
}

#[tokio::test]
async fn missing_default_does_not_select_first_permitted_template_and_releases_reservation() {
    let mut fixture = Fixture::new().await;
    let (reply, received) = tokio::sync::oneshot::channel();
    let (_sender, cancellation) = Cancellation::channel();
    fixture.state.handle_command(Command::Create {
        caller: AgentId(1),
        allowed: vec![AgentDefinitionId(5)],
        name: "new-child".into(),
        task: "work".into(),
        template: None,
        cancellation,
        reply,
    });
    assert!(fixture.state.reserved.contains("new-child"));
    let job = fixture.state.jobs.next().await.unwrap();
    fixture.state.handle_job_result(job);
    assert!(
        received
            .await
            .unwrap()
            .unwrap_err()
            .contains("No default agent template")
    );
    assert!(fixture.state.reserved.is_empty());
    assert!(!fixture.state.names.contains("new-child"));
    assert!(fixture.manager.try_recv().is_err());
}

#[tokio::test]
async fn fixed_worker_limit_counts_parallel_creation_reservations() {
    let mut fixture = Fixture::new().await;
    // One existing worker plus seven in-flight creates consumes all eight slots.
    for index in 0..7 {
        let (reply, _received) = tokio::sync::oneshot::channel();
        let (_sender, cancellation) = Cancellation::channel();
        fixture.state.handle_command(Command::Create {
            caller: AgentId(1),
            allowed: vec![AgentDefinitionId(5)],
            name: format!("pending-{index}"),
            task: "work".into(),
            template: None,
            cancellation,
            reply,
        });
    }
    assert_eq!(fixture.state.reserved.len(), 7);
    assert_eq!(fixture.state.jobs.len(), 7);
    let (reply, received) = tokio::sync::oneshot::channel();
    let (_sender, cancellation) = Cancellation::channel();
    fixture.state.handle_command(Command::Create {
        caller: AgentId(1),
        allowed: vec![AgentDefinitionId(5)],
        name: "over-limit".into(),
        task: "work".into(),
        template: None,
        cancellation,
        reply,
    });
    assert!(
        received
            .await
            .unwrap()
            .unwrap_err()
            .contains("Worker capacity reached (8)")
    );
    assert_eq!(fixture.state.reserved.len(), 7);
    assert_eq!(fixture.state.jobs.len(), 7);
}

#[tokio::test]
async fn idle_resume_reserves_last_slot_before_parallel_send() {
    let mut fixture = Fixture::new().await;
    for id in 10..18 {
        fixture.state.members.insert(
            AgentId(id),
            Member {
                id: AgentId(id),
                name: format!("worker-{id}"),
                parent: Some(AgentId(1)),
                group: AgentId(1),
                template: "test".into(),
                status: if id < 16 {
                    "running".into()
                } else {
                    "idle".into()
                },
                error: None,
            },
        );
    }
    // child plus workers 10..16 holds seven slots, leaving exactly one.
    let (reply, _received) = tokio::sync::oneshot::channel();
    fixture
        .state
        .deliver(AgentId(1), AgentId(16), "first".into(), Some(reply), None);
    assert_eq!(fixture.state.members[&AgentId(16)].status, "starting");
    let (reply, received) = tokio::sync::oneshot::channel();
    fixture
        .state
        .deliver(AgentId(1), AgentId(17), "second".into(), Some(reply), None);
    assert!(
        received
            .await
            .unwrap()
            .unwrap_err()
            .contains("Worker capacity reached (8)")
    );
    assert_eq!(fixture.state.jobs.len(), 1);
    assert_eq!(fixture.state.members[&AgentId(17)].status, "idle");
}

#[tokio::test]
async fn fixed_depth_limit_allows_four_levels_and_refuses_fifth() {
    let mut fixture = Fixture::new().await;
    for (id, parent) in [(4, 2), (5, 4), (6, 5)] {
        fixture.state.members.insert(
            AgentId(id),
            Member {
                id: AgentId(id),
                name: format!("level-{id}"),
                parent: Some(AgentId(parent)),
                group: AgentId(1),
                template: "test".into(),
                status: "idle".into(),
                error: None,
            },
        );
    }
    assert!(fixture.state.validate_creation_capacity(AgentId(5)).is_ok());
    assert!(
        fixture
            .state
            .validate_creation_capacity(AgentId(6))
            .unwrap_err()
            .contains("Maximum agent nesting depth")
    );
}

#[tokio::test]
async fn list_uses_source_roles_and_omits_absent_error() {
    let mut fixture = Fixture::new().await;
    let (reply, received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::List {
        caller: AgentId(1),
        all: true,
        reply,
    });
    let list = received.await.unwrap().unwrap();
    let members = list.as_array().unwrap();
    let root = members
        .iter()
        .find(|member| member["name"] == "main")
        .unwrap();
    let child = members
        .iter()
        .find(|member| member["name"] == "child")
        .unwrap();
    assert_eq!(root["role"], "interactive");
    assert_eq!(child["role"], "worker");
    assert!(root.get("error").is_none());
    assert!(child.get("error").is_none());
}

#[tokio::test]
async fn identity_is_static_and_does_not_include_group_status_snapshots() {
    let mut fixture = Fixture::new().await;
    let (reply, received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Identity {
        caller: AgentId(2),
        reply,
    });
    let first = received.await.unwrap().unwrap();
    fixture.state.members.get_mut(&AgentId(1)).unwrap().status = "idle".into();
    fixture.state.members.get_mut(&AgentId(2)).unwrap().status = "waiting".into();
    let (reply, received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Identity {
        caller: AgentId(2),
        reply,
    });
    let second = received.await.unwrap().unwrap();
    assert_eq!(first, second);
    assert_eq!(first["parent"], "main");
    assert_eq!(first["group_id"], 1);
    assert!(first.get("status").is_none());
}

#[tokio::test]
async fn cancelled_ensure_during_restore_does_not_create_phantom_running_member() {
    let mut fixture = Fixture::new().await;
    let agent = Agent {
        id: AgentId(99),
        session_id: SessionId(1),
        definition_id: AgentDefinitionId(1),
        parent_id: None,
        first_event_id: psa_api::events::EventId(0),
        next_event_id: psa_api::events::EventId(0),
        created_position: 0,
        name: "cancelled-root".into(),
        interactive: true,
        state: psa_api::AgentState::Running,
        usage: psa_api::AgentUsage::default(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    };
    fixture.state.ready = false;
    fixture.state.restoring = true;
    let (reply, received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Ensure {
        agent: agent.clone(),
        template: "test".into(),
        reply,
    });
    drop(received);
    fixture
        .state
        .apply_lifecycle_event(AgentLifecycleEvent::Finished {
            session_id: SessionId(1),
            agent_id: agent.id,
            run_id: "cancelled-run".into(),
            outcome: AgentRunOutcome::Interrupted,
            final_text: None,
        });
    fixture.state.handle_job_result(Job::Restored {
        members: Vec::new(),
        names: HashSet::new(),
        result: Ok(()),
    });
    assert!(!fixture.state.members.contains_key(&agent.id));
    assert!(!fixture.state.names.contains(&agent.name));

    let (reply, received) = tokio::sync::oneshot::channel();
    fixture
        .state
        .ensure_member(agent.clone(), "test".into(), reply);
    assert!(received.await.unwrap().is_ok());
    assert_eq!(fixture.state.members[&agent.id].status, "running");

    // A stale but still-live ensure cannot undo authoritative terminal events.
    for (run_id, outcome, status) in [
        (
            "interrupted-run",
            AgentRunOutcome::Interrupted,
            "interrupted",
        ),
        (
            "failed-run",
            AgentRunOutcome::Failed {
                message: "failure".into(),
            },
            "error",
        ),
    ] {
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Started {
                session_id: SessionId(1),
                agent_id: agent.id,
                run_id: run_id.into(),
            });
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Finished {
                session_id: SessionId(1),
                agent_id: agent.id,
                run_id: run_id.into(),
                outcome,
                final_text: None,
            });
        let (reply, received) = tokio::sync::oneshot::channel();
        fixture
            .state
            .ensure_member(agent.clone(), "test".into(), reply);
        assert!(received.await.unwrap().is_ok());
        assert_eq!(fixture.state.members[&agent.id].status, status);
    }
    assert_eq!(
        fixture.state.members[&agent.id].error.as_deref(),
        Some("failure")
    );
    fixture
        .state
        .apply_lifecycle_event(AgentLifecycleEvent::Started {
            session_id: SessionId(1),
            agent_id: agent.id,
            run_id: "new-live-run".into(),
        });
    let (reply, received) = tokio::sync::oneshot::channel();
    fixture
        .state
        .ensure_member(agent.clone(), "test".into(), reply);
    assert!(received.await.unwrap().is_ok());
    assert_eq!(fixture.state.members[&agent.id].status, "running");
    assert!(fixture.state.members[&agent.id].error.is_none());
}

#[tokio::test]
async fn duplicate_permitted_ids_are_not_ambiguous_but_distinct_same_name_versions_are() {
    let mut fixture = Fixture::new().await;
    let first = fixture
        .state
        .sessions
        .create_agent_definition(psa_api::CreateAgentDefinition {
            name: "same-alias".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let second = fixture
        .state
        .sessions
        .update_agent_definition(
            first.id,
            psa_api::UpdateAgentDefinition {
                name: "same-alias".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for (allowed, expected) in [
        (vec![first.id, first.id], "Agent creation cancelled"),
        (
            vec![first.id, second.id],
            "Ambiguous permitted agent template: same-alias",
        ),
    ] {
        let (reply, received) = tokio::sync::oneshot::channel();
        let (sender, cancellation) = Cancellation::channel();
        drop(sender);
        fixture.state.handle_command(Command::Create {
            caller: AgentId(1),
            allowed,
            name: "candidate".into(),
            task: "work".into(),
            template: Some("same-alias".into()),
            cancellation,
            reply,
        });
        let job = fixture.state.jobs.next().await.unwrap();
        fixture.state.handle_job_result(job);
        assert_eq!(received.await.unwrap().unwrap_err(), expected);
    }
    assert!(fixture.state.reserved.is_empty());
    assert!(fixture.manager.try_recv().is_err());
}

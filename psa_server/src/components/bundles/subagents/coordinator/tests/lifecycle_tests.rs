use super::*;

#[tokio::test]
async fn child_interruption_does_not_automatically_resume_stopped_parent() {
    for outcome in [
        AgentRunOutcome::Interrupted,
        AgentRunOutcome::Failed {
            message: "parent failure".into(),
        },
    ] {
        let mut fixture = Fixture::new().await;
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Finished {
                session_id: SessionId(1),
                agent_id: AgentId(1),
                run_id: "parent-run".into(),
                outcome,
                final_text: None,
            });
        let serve_interrupt = async {
            let ManagerCommand::Submit { request, reply, .. } =
                fixture.manager.recv().await.unwrap()
            else {
                panic!("expected child interrupt");
            };
            assert!(matches!(request, psa_api::AgentRequest::Interrupt));
            let _ = reply.send(Ok(psa_api::RequestAccepted {
                request_id: "interrupt".into(),
            }));
        };
        let (job, ()) = tokio::join!(fixture.state.jobs.next(), serve_interrupt);
        fixture.state.handle_job_result(job.unwrap());
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Finished {
                session_id: SessionId(1),
                agent_id: AgentId(2),
                run_id: "child-run".into(),
                outcome: AgentRunOutcome::Interrupted,
                final_text: None,
            });
        assert_eq!(fixture.state.members[&AgentId(2)].status, "interrupted");
        assert!(fixture.state.jobs.is_empty());
        assert!(fixture.manager.try_recv().is_err());
        assert!(
            ["interrupted", "error"].contains(&fixture.state.members[&AgentId(1)].status.as_str())
        );

        // The automatic-report policy must not disable an explicit user send.
        let (_cancel, cancellation) = Cancellation::channel();
        let (reply, received) = tokio::sync::oneshot::channel();
        fixture.state.handle_command(Command::Send {
            caller: AgentId(2),
            name: "main".into(),
            message: "explicit resume".into(),
            cancellation,
            reply,
        });
        accept_steering(&mut fixture.state, &mut fixture.manager).await;
        assert_eq!(received.await.unwrap().unwrap()["status"], "queued");
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Started {
                session_id: SessionId(1),
                agent_id: AgentId(1),
                run_id: "explicit-new-run".into(),
            });
        assert_eq!(fixture.state.members[&AgentId(1)].status, "running");
    }
}

#[tokio::test]
async fn idle_parent_still_receives_background_child_completion() {
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
    assert_eq!(fixture.state.members[&AgentId(1)].status, "idle");
    fixture
        .state
        .apply_lifecycle_event(AgentLifecycleEvent::Finished {
            session_id: SessionId(1),
            agent_id: AgentId(2),
            run_id: "child-run".into(),
            outcome: AgentRunOutcome::Completed,
            final_text: Some("background answer".into()),
        });
    assert_eq!(fixture.state.jobs.len(), 1);
    accept_steering(&mut fixture.state, &mut fixture.manager).await;
    assert!(fixture.state.jobs.is_empty());
}

#[tokio::test]
async fn recovery_uses_persisted_agent_states_and_live_started_authority() {
    let mut fixture = Fixture::new().await;
    let mut snapshots: Vec<_> = fixture.state.members.values().cloned().collect();
    for member in &mut snapshots {
        member.status = "error".into();
        member.error = Some("old failure".into());
    }
    snapshots
        .iter_mut()
        .find(|member| member.id == AgentId(1))
        .unwrap()
        .status = "running".into();
    let mut failed = snapshots[0].clone();
    failed.id = AgentId(4);
    failed.name = "failed-parent".into();
    failed.status = "error".into();
    snapshots.push(failed);
    let mut nonexistent = snapshots[0].clone();
    nonexistent.id = AgentId(99);
    snapshots.push(nonexistent);
    let rows: Vec<_> = [
        (1, psa_api::AgentState::Interrupted),
        (2, psa_api::AgentState::Idle),
        (3, psa_api::AgentState::Running),
        (4, psa_api::AgentState::Interrupted),
    ]
    .into_iter()
    .map(|(id, state)| Agent {
        id: AgentId(id),
        session_id: SessionId(1),
        definition_id: AgentDefinitionId(1),
        parent_id: None,
        first_event_id: psa_api::events::EventId(0),
        next_event_id: psa_api::events::EventId(0),
        created_position: 0,
        name: format!("agent-{id}"),
        interactive: id == 1,
        state,
        usage: psa_api::AgentUsage::default(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    })
    .collect();
    let members = reconcile_checkpoint_members(snapshots, &rows);
    assert_eq!(members.len(), 4);
    assert!(!members.iter().any(|member| member.id == AgentId(99)));
    for member in &members {
        match member.id.0 {
            1 => {
                assert_eq!(member.status, "interrupted");
                assert!(member.error.is_none());
            }
            2 => {
                assert_eq!(member.status, "idle");
                assert!(member.error.is_none());
            }
            3 => {
                assert_eq!(member.status, "running");
                assert!(member.error.is_none());
            }
            4 => {
                assert_eq!(member.status, "error");
                assert_eq!(member.error.as_deref(), Some("old failure"));
            }
            _ => unreachable!(),
        }
    }

    fixture.state.members.clear();
    fixture.state.handle_job_result(Job::Restored {
        members: members.clone(),
        names: HashSet::new(),
        result: Ok(()),
    });
    assert_eq!(fixture.state.members[&AgentId(1)].status, "interrupted");
    fixture.state.members.clear();
    fixture
        .state
        .apply_lifecycle_event(AgentLifecycleEvent::Started {
            session_id: SessionId(1),
            agent_id: AgentId(1),
            run_id: "live-recovered-run".into(),
        });
    fixture.state.handle_job_result(Job::Restored {
        members,
        names: HashSet::new(),
        result: Ok(()),
    });
    assert_eq!(fixture.state.members[&AgentId(1)].status, "running");
    assert!(fixture.state.members[&AgentId(1)].error.is_none());
}

#[tokio::test]
async fn finished_during_restore_overrides_older_running_snapshot() {
    for (outcome, expected_status, expected_error) in [
        (AgentRunOutcome::Completed, "idle", None),
        (AgentRunOutcome::Interrupted, "interrupted", None),
        (
            AgentRunOutcome::Failed {
                message: "failed during restore".into(),
            },
            "error",
            Some("failed during restore"),
        ),
    ] {
        let mut fixture = Fixture::new().await;
        // Represents a checkpoint reconciled with rows read while still Running.
        let snapshot = fixture.state.members[&AgentId(1)].clone();
        fixture.state.members.clear();
        fixture.state.ready = false;
        fixture.state.restoring = true;
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Started {
                session_id: SessionId(1),
                agent_id: AgentId(1),
                run_id: "restoring-run".into(),
            });
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Finished {
                session_id: SessionId(1),
                agent_id: AgentId(1),
                run_id: "restoring-run".into(),
                outcome,
                final_text: None,
            });
        fixture.state.handle_job_result(Job::Restored {
            members: vec![snapshot.clone()],
            names: HashSet::new(),
            result: Ok(()),
        });
        assert_eq!(fixture.state.members[&AgentId(1)].status, expected_status);
        assert_eq!(
            fixture.state.members[&AgentId(1)].error.as_deref(),
            expected_error
        );

        // A newer Started event wins over both old rows and terminal history.
        fixture.state.members.clear();
        fixture
            .state
            .apply_lifecycle_event(AgentLifecycleEvent::Started {
                session_id: SessionId(1),
                agent_id: AgentId(1),
                run_id: "newer-run".into(),
            });
        fixture.state.handle_job_result(Job::Restored {
            members: vec![snapshot],
            names: HashSet::new(),
            result: Ok(()),
        });
        assert_eq!(fixture.state.members[&AgentId(1)].status, "running");
        assert!(fixture.state.members[&AgentId(1)].error.is_none());
    }
}

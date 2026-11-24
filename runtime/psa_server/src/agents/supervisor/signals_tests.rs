use super::*;
use crate::{agents::NewAgent, components::cancellation::Cancellation};
use psa::api::events::Message;
use psa_store::NewSession;
use std::time::Duration;

struct Fixture {
    _directory: tempfile::TempDir,
    manager: Supervisor,
    session: SessionId,
    agent: AgentId,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_bundles(Vec::new()).await
    }

    async fn with_bundles(bundles: Vec<psa::api::BundleConfig>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open(directory.path().join("signals.sqlite3"))
            .await
            .unwrap();
        crate::test_support::register_probe(&db).await;
        let mut cursor = db.write().await.unwrap();
        let definition = cursor
            .create_agent_definition(psa_store::CreateAgentDefinition {
                name: "signals".into(),
                bundles,
                ..Default::default()
            })
            .await
            .unwrap();
        let model = crate::test_support::model_config();
        let session = cursor
            .create_session(NewSession {
                decision_model: None,
                name: "signals".into(),
                working_directory: "/work".into(),
                probe_id: crate::test_support::probe_metadata().id,
                project_id: None,
                model_config: model.clone(),
            })
            .await
            .unwrap();
        let agent = cursor
            .create_agent(
                session.id,
                NewAgent {
                    subagent_parent_id: None,
                    name: "signals".into(),
                    definition_id: definition.id,
                    interactive: false,
                    hidden: false,
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
        let publication = Publication::default();
        let (commands, received) = actors::channel(256);
        let commands = SupervisorClient::new(commands);
        let client_queries = ClientQueries::start(publication.clone());
        let sessions = RuntimeHandle::new(
            db.clone(),
            publication.clone(),
            commands.clone(),
            client_queries.handle(),
        );
        let manager = Supervisor {
            db,
            publication,
            providers: ProviderRegistry::new(vec![
                llm_api::api::Provider::new(model.model.provider, "test-key").unwrap(),
            ]),
            probes: ProbeRegistry::start("signals".into()),
            sessions,
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
        Self {
            _directory: directory,
            manager,
            session: session.id,
            agent: agent.id,
        }
    }

    // A parked worker lets tests drive actor boundaries without racing inference.
    fn add_worker(&mut self) {
        let generation = self.manager.next_generation;
        self.manager.next_generation += 1;
        let task = self
            .manager
            .tasks
            .spawn(std::future::pending::<WorkerResult>());
        let (controls, _received) = actors::unbounded_channel();
        self.manager.workers.insert(
            self.agent,
            Worker {
                session: self.session,
                generation,
                task_id: task.id(),
                controls: AgentClient::new(controls),
                queue: VecDeque::new(),
                lifecycle: VecDeque::new(),
                in_flight: None,
                stopping: false,
            },
        );
    }

    async fn submit(&mut self, delivery: MessageDelivery) {
        self.manager
            .submit(
                self.session,
                self.agent,
                AgentRequest::Message {
                    source: None,
                    delivery,
                    content: crate::prompts::text("work"),
                },
            )
            .await
            .unwrap();
    }

    async fn boundary(&mut self, state: AgentState) -> BoundaryReply {
        let generation = self.manager.workers[&self.agent].generation;
        let (reply, response) = oneshot::channel();
        self.manager
            .handle_boundary(self.agent, generation, state, reply)
            .await;
        response.await.unwrap()
    }

    async fn save_message(&self, text: &str) {
        let mut cursor = self.manager.db.write().await.unwrap();
        cursor
            .add_events(
                self.agent,
                &[NewEvent::new(EventType::Message(Message::text(
                    MessageType::Agent,
                    text,
                )))],
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
    }

    async fn save_inference(&self, outcome: llm_api::api::Outcome) {
        use psa::api::events::{InferenceResultEnd, InferenceResultStart};
        let mut cursor = self.manager.db.write().await.unwrap();
        let id = cursor.get_agent(self.agent).await.unwrap().next_event_id.0;
        cursor
            .add_events(
                self.agent,
                &[
                    NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                        id,
                        model_config: crate::test_support::model_config(),
                        request_config: Default::default(),
                    })),
                    NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                        id,
                        outcome,
                        usage: Default::default(),
                        request_time: Duration::ZERO,
                        response_id: None,
                        provider_metadata: Default::default(),
                    })),
                ],
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
    }

    async fn close(mut self) {
        self.manager.begin_shutdown().await;
        self.manager.tasks.abort_all();
        while self.manager.tasks.join_next().await.is_some() {}
        self.manager.probes.shutdown().await;
    }
}

#[tokio::test]
async fn steering_is_level_triggered_notify_only_and_excludes_queue() {
    let mut fixture = Fixture::new().await;
    fixture.add_worker();
    let mut signal = fixture
        .manager
        .subscribe_steering(fixture.session, fixture.agent)
        .await
        .unwrap();
    assert_eq!(*signal.borrow_and_update(), Some(false));
    fixture.submit(MessageDelivery::Queue).await;
    assert!(!signal.has_changed().unwrap());
    fixture.submit(MessageDelivery::Steer).await;
    signal.changed().await.unwrap();
    assert_eq!(*signal.borrow_and_update(), Some(true));
    let second = fixture
        .manager
        .subscribe_steering(fixture.session, fixture.agent)
        .await
        .unwrap();
    assert_eq!(*second.borrow(), Some(true));
    assert_eq!(fixture.manager.workers[&fixture.agent].queue.len(), 2);
    assert!(fixture.manager.runs.is_empty());

    let sessions = fixture.manager.sessions.clone();
    let (_cancel, mut cancellation) = Cancellation::channel();
    let wait = sessions.wait_for_steering(fixture.session, fixture.agent, &mut cancellation);
    let manager = &mut fixture.manager;
    let serve = async {
        let command = manager.received.recv().await.unwrap();
        manager.handle_command(command).await;
    };
    let (result, ()) = tokio::join!(wait, serve);
    result.unwrap();
    assert_eq!(fixture.manager.workers[&fixture.agent].queue.len(), 2);
    assert!(matches!(
        fixture.boundary(AgentState::Running).await,
        BoundaryReply::Work(_)
    ));
    assert_eq!(*signal.borrow(), Some(false));
    assert_eq!(fixture.manager.workers[&fixture.agent].queue.len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn commands_are_not_steering_and_run_without_starting_runs() {
    let mut fixture = Fixture::with_bundles(vec![psa::api::BundleConfig::Compaction {
        reserve_tokens: 0,
        keep_recent_tokens: 0,
        context_window: None,
    }])
    .await;
    fixture.add_worker();
    let mut signal = fixture
        .manager
        .subscribe_steering(fixture.session, fixture.agent)
        .await
        .unwrap();
    assert_eq!(*signal.borrow_and_update(), Some(false));
    for delivery in [CommandDelivery::Queue, CommandDelivery::Steer] {
        fixture
            .manager
            .submit_command(
                fixture.session,
                fixture.agent,
                psa::api::RunCommand {
                    delivery,
                    command: AgentCommand::Compact { instructions: None },
                },
            )
            .await
            .unwrap();
    }
    assert!(!signal.has_changed().unwrap());

    // A running agent only takes the steered command; the queued one waits.
    let BoundaryReply::Command(command) = fixture.boundary(AgentState::Running).await else {
        panic!("expected the steered command")
    };
    assert_eq!(command.delivery, CommandDelivery::Steer);
    assert!(fixture.manager.runs.is_empty());
    let BoundaryReply::Command(command) = fixture.boundary(AgentState::Idle).await else {
        panic!("expected the queued command")
    };
    assert_eq!(command.delivery, CommandDelivery::Queue);
    assert!(fixture.manager.runs.is_empty());
    assert!(matches!(
        fixture.boundary(AgentState::Idle).await,
        BoundaryReply::Stop
    ));
    fixture.close().await;
}

#[tokio::test]
async fn commands_require_their_bundle() {
    let mut fixture = Fixture::new().await;
    fixture.add_worker();
    let result = fixture
        .manager
        .submit_command(
            fixture.session,
            fixture.agent,
            psa::api::RunCommand {
                delivery: CommandDelivery::Steer,
                command: AgentCommand::Compact { instructions: None },
            },
        )
        .await;
    assert!(
        matches!(result, Err(RuntimeError::Invalid(message)) if message.contains("Compaction"))
    );
    assert!(fixture.manager.workers[&fixture.agent].queue.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn steering_subscriptions_survive_worker_retirement_and_shutdown() {
    let mut fixture = Fixture::new().await;
    let signal = fixture
        .manager
        .subscribe_steering(fixture.session, fixture.agent)
        .await
        .unwrap();
    fixture.add_worker();
    assert!(matches!(
        fixture.boundary(AgentState::Idle).await,
        BoundaryReply::Stop
    ));
    let generation = fixture.manager.workers[&fixture.agent].generation;
    fixture
        .manager
        .handle_exit(fixture.agent, generation, Ok(()))
        .await;
    assert!(fixture.manager.lifecycle_events.is_empty());
    fixture.add_worker();
    fixture.submit(MessageDelivery::Steer).await;
    assert_eq!(*signal.borrow(), Some(true));
    drop(signal);
    assert_eq!(fixture.manager.steering[&fixture.agent].receiver_count(), 0);
    let signal = fixture
        .manager
        .subscribe_steering(fixture.session, fixture.agent)
        .await
        .unwrap();
    fixture.manager.begin_shutdown().await;
    assert_eq!(*signal.borrow(), None);
    assert!(matches!(
        fixture
            .manager
            .subscribe_steering(fixture.session, fixture.agent)
            .await,
        Err(RuntimeError::Unavailable)
    ));
    fixture.close().await;
}

#[tokio::test]
async fn steering_wait_cancellation_drops_receiver_and_registration() {
    let mut fixture = Fixture::new().await;
    let sessions = fixture.manager.sessions.clone();
    let (cancel, mut cancellation) = Cancellation::channel();
    let wait = sessions.wait_for_steering(fixture.session, fixture.agent, &mut cancellation);
    let manager = &mut fixture.manager;
    let serve = async {
        let command = manager.received.recv().await.unwrap();
        manager.handle_command(command).await;
        cancel.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(wait, serve);
    assert!(matches!(result, Err(RuntimeError::Cancelled)));
    assert_eq!(fixture.manager.steering[&fixture.agent].receiver_count(), 0);

    let (_cancel, mut cancellation) = Cancellation::channel();
    let wait = sessions.wait_for_steering(fixture.session, fixture.agent, &mut cancellation);
    tokio::pin!(wait);
    tokio::select! {
        result = &mut wait => panic!("unexpected result: {result:?}"),
        command = fixture.manager.received.recv() => {
            // Drop an unregistered RPC reply: no receiver is retained.
            drop(command.unwrap());
        }
    }
    assert!(matches!(wait.await, Err(RuntimeError::Unavailable)));
    fixture.close().await;
}

#[tokio::test]
async fn steering_and_event_reads_validate_agent_session() {
    let mut fixture = Fixture::new().await;
    let wrong = SessionId(fixture.session.0 + 1);
    assert!(matches!(
        fixture
            .manager
            .subscribe_steering(wrong, fixture.agent)
            .await,
        Err(RuntimeError::Database(DbError::NotFound { .. }))
    ));
    assert!(fixture.manager.steering.is_empty());
    assert!(matches!(
        fixture
            .manager
            .sessions
            .load_agent_events(wrong, fixture.agent, EventRange::default())
            .await,
        Err(RuntimeError::Database(DbError::NotFound { .. }))
    ));
    fixture.save_message("persisted").await;
    let events = fixture
        .manager
        .sessions
        .load_agent_events(fixture.session, fixture.agent, EventRange::default())
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn logical_runs_keep_running_steering_but_restart_at_idle_or_interrupted_boundaries() {
    let mut fixture = Fixture::new().await;
    fixture.add_worker();
    fixture.save_message("old output").await;
    fixture.submit(MessageDelivery::Queue).await;
    assert!(matches!(
        fixture.boundary(AgentState::Idle).await,
        BoundaryReply::Work(_)
    ));
    let first = fixture.manager.runs[&fixture.agent].id.clone();
    assert!(
        matches!(&fixture.manager.lifecycle_events[..], [AgentLifecycleEvent::Started { run_id, .. }] if run_id == &first)
    );
    fixture.submit(MessageDelivery::Steer).await;
    assert!(matches!(
        fixture.boundary(AgentState::Running).await,
        BoundaryReply::Work(_)
    ));
    assert_eq!(fixture.manager.runs[&fixture.agent].id, first);
    assert_eq!(fixture.manager.lifecycle_events.len(), 1);
    fixture.save_message("current output").await;
    fixture.submit(MessageDelivery::Queue).await;
    assert!(matches!(
        fixture.boundary(AgentState::Idle).await,
        BoundaryReply::Work(_)
    ));
    let second = fixture.manager.runs[&fixture.agent].id.clone();
    assert_ne!(first, second);
    assert!(
        matches!(&fixture.manager.lifecycle_events[1], AgentLifecycleEvent::Finished { run_id, outcome: AgentRunOutcome::Completed, final_text: Some(text), .. } if run_id == &first && text == "current output")
    );
    let mut cursor = fixture.manager.db.write().await.unwrap();
    cursor
        .add_events(
            fixture.agent,
            &[NewEvent::new(EventType::StatusChange(
                psa::api::events::StatusChange {
                    new_status: AgentState::Interrupted,
                    reason: None,
                },
            ))],
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    fixture.submit(MessageDelivery::Resume).await;
    assert!(matches!(
        fixture.boundary(AgentState::Interrupted).await,
        BoundaryReply::Work(_)
    ));
    assert!(
        matches!(&fixture.manager.lifecycle_events[3], AgentLifecycleEvent::Finished { run_id, outcome: AgentRunOutcome::Interrupted, final_text: None, .. } if run_id == &second)
    );
    assert_ne!(fixture.manager.runs[&fixture.agent].id, second);
    fixture.close().await;
}

#[tokio::test]
async fn latest_current_run_provider_failure_is_safe_and_distinct_from_interruption() {
    use llm_api::api::{Outcome, StopReason};
    let failure = || {
        Outcome::Failed(llm_api::Error::Provider {
            code: None,
            message: "secret provider credential must not escape".into(),
            metadata: Default::default(),
        })
    };
    for (outcome, expected) in [
        (
            failure(),
            AgentRunOutcome::Failed {
                message: "Agent execution failed; see server logs for details.".into(),
            },
        ),
        (Outcome::Cancelled, AgentRunOutcome::Interrupted),
        (
            Outcome::Incomplete {
                reason: StopReason::TokenLimit,
            },
            AgentRunOutcome::Interrupted,
        ),
        (
            Outcome::Completed {
                reason: StopReason::EndTurn,
            },
            AgentRunOutcome::Interrupted,
        ),
    ] {
        let mut fixture = Fixture::new().await;
        fixture.add_worker();
        fixture.save_inference(failure()).await;
        fixture.submit(MessageDelivery::Steer).await;
        assert!(matches!(
            fixture.boundary(AgentState::Idle).await,
            BoundaryReply::Work(_)
        ));
        // The newest terminal outcome, not an earlier failure in this run or
        // inherited from before the baseline, determines provider failure.
        fixture.save_inference(failure()).await;
        fixture.save_inference(outcome).await;
        assert!(matches!(
            fixture.boundary(AgentState::Interrupted).await,
            BoundaryReply::Stop
        ));
        assert!(
            matches!(&fixture.manager.lifecycle_events[1], AgentLifecycleEvent::Finished {
            outcome, ..
        } if outcome == &expected)
        );
        assert_eq!(fixture.manager.lifecycle_events.len(), 2);
        fixture.close().await;
    }
}

#[tokio::test]
async fn empty_latest_agent_message_clears_earlier_run_text() {
    let mut fixture = Fixture::new().await;
    fixture.add_worker();
    fixture
        .save_inference(llm_api::api::Outcome::Failed(llm_api::Error::Transport {
            message: "failure from a prior run must not affect this run".into(),
        }))
        .await;
    fixture.submit(MessageDelivery::Steer).await;
    assert!(matches!(
        fixture.boundary(AgentState::Idle).await,
        BoundaryReply::Work(_)
    ));
    fixture.save_message("earlier intermediate output").await;
    fixture.save_message("").await;
    assert!(matches!(
        fixture.boundary(AgentState::Idle).await,
        BoundaryReply::Stop
    ));
    assert!(matches!(
        &fixture.manager.lifecycle_events[1],
        AgentLifecycleEvent::Finished {
            outcome: AgentRunOutcome::Completed,
            final_text: None,
            ..
        }
    ));
    fixture.close().await;
}

#[tokio::test]
async fn worker_error_or_panic_reports_one_safe_failure_per_active_run() {
    for panic in [false, true] {
        let mut fixture = Fixture::new().await;
        fixture.add_worker();
        fixture.submit(MessageDelivery::Steer).await;
        assert!(matches!(
            fixture.boundary(AgentState::Idle).await,
            BoundaryReply::Work(_)
        ));
        fixture
            .save_inference(llm_api::api::Outcome::Cancelled)
            .await;
        let generation = fixture.manager.workers[&fixture.agent].generation;
        if panic {
            let task = fixture.manager.tasks.spawn(async {
                panic!("test credential must not appear in lifecycle notifications");
            });
            fixture
                .manager
                .workers
                .get_mut(&fixture.agent)
                .unwrap()
                .task_id = task.id();
            let joined = tokio::time::timeout(
                Duration::from_secs(2),
                fixture.manager.tasks.join_next_with_id(),
            )
            .await
            .unwrap()
            .unwrap();
            fixture.manager.handle_join(joined).await;
        } else {
            fixture
                .manager
                .handle_exit(
                    fixture.agent,
                    generation,
                    Err(AgentError::SupervisorUnavailable),
                )
                .await;
        }
        fixture
            .manager
            .handle_exit(
                fixture.agent,
                generation,
                Err(AgentError::SupervisorUnavailable),
            )
            .await;
        assert!(fixture.manager.runs.is_empty());
        assert_eq!(fixture.manager.lifecycle_events.len(), 2);
        assert!(
            matches!(&fixture.manager.lifecycle_events[1], AgentLifecycleEvent::Finished { outcome: AgentRunOutcome::Failed { message }, .. } if message == "Agent execution failed; see server logs for details.")
        );
        fixture.close().await;
    }
}

#[tokio::test]
async fn subagent_steering_resolves_sender_and_shares_level_triggered_waits() {
    let mut fixture = Fixture::new().await;
    fixture.add_worker();
    let mut signal = fixture
        .manager
        .subscribe_steering(fixture.session, fixture.agent)
        .await
        .unwrap();
    fixture
        .manager
        .submit(
            fixture.session,
            fixture.agent,
            AgentRequest::Message {
                delivery: psa::api::MessageDelivery::Steer,
                source: Some(fixture.agent),
                content: crate::prompts::text("hello"),
            },
        )
        .await
        .unwrap();
    assert_eq!(*signal.borrow_and_update(), Some(true));
    let BoundaryReply::Work(request) = fixture.boundary(AgentState::Idle).await else {
        panic!("expected work")
    };
    assert_eq!(request.delivery, MessageDelivery::Steer);
    assert_eq!(request.message_type, MessageType::Subagent);
    assert_eq!(request.source.unwrap().sender_name, "signals");
    assert_eq!(*signal.borrow_and_update(), Some(false));
    assert!(fixture.manager.workers[&fixture.agent].queue.is_empty());
}

#[tokio::test]
async fn subagent_steering_rejects_missing_and_cross_session_senders() {
    let mut fixture = Fixture::new().await;
    fixture.add_worker();
    let mut cursor = fixture.manager.db.write().await.unwrap();
    let fork = cursor
        .fork_session(
            fixture.session,
            psa_store::SessionFork {
                point: psa_store::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    for source in [AgentId(u32::MAX.into()), fork.agents[0].id] {
        let result = fixture
            .manager
            .submit(
                fixture.session,
                fixture.agent,
                AgentRequest::Message {
                    delivery: psa::api::MessageDelivery::Steer,
                    source: Some(source),
                    content: crate::prompts::text("hello"),
                },
            )
            .await;
        assert!(result.is_err());
        assert!(fixture.manager.workers[&fixture.agent].queue.is_empty());
    }
}

#[tokio::test]
async fn background_reports_cannot_restart_parent_even_if_interrupted_after_acceptance() {
    let mut fixture = Fixture::new().await;
    fixture.add_worker();
    let request = AgentRequest::Message {
        delivery: psa::api::MessageDelivery::Steer,
        source: None,
        content: crate::prompts::text("report"),
    };
    fixture
        .manager
        .submit_request(fixture.session, fixture.agent, request.clone(), false)
        .await
        .unwrap();
    assert!(matches!(
        fixture.boundary(AgentState::Interrupted).await,
        BoundaryReply::Stop
    ));
    assert!(fixture.manager.workers[&fixture.agent].queue.is_empty());

    let mut cursor = fixture.manager.db.write().await.unwrap();
    cursor
        .add_events(
            fixture.agent,
            &[NewEvent::new(EventType::StatusChange(
                psa::api::events::StatusChange {
                    new_status: AgentState::Interrupted,
                    reason: None,
                },
            ))],
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    assert!(
        fixture
            .manager
            .submit_request(fixture.session, fixture.agent, request.clone(), false)
            .await
            .is_err()
    );
    // Explicit messages retain normal steering's ability to resume the parent.
    fixture
        .manager
        .workers
        .get_mut(&fixture.agent)
        .unwrap()
        .stopping = false;
    fixture
        .manager
        .submit(fixture.session, fixture.agent, request)
        .await
        .unwrap();
    assert!(matches!(
        fixture.boundary(AgentState::Interrupted).await,
        BoundaryReply::Work(_)
    ));
}

#[tokio::test]
async fn completion_hook_reports_children_without_bundle_and_preserves_outcomes() {
    use crate::components::hooks::subagent_completion::SubagentCompletion;
    let mut fixture = Fixture::with_bundles(vec![psa::api::BundleConfig::Subagents]).await;
    let mut cursor = fixture.manager.db.write().await.unwrap();
    let definition = cursor
        .create_agent_definition(psa_store::CreateAgentDefinition {
            name: "plain child".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let child = cursor
        .create_agent(
            fixture.session,
            NewAgent {
                subagent_parent_id: Some(fixture.agent),
                definition_id: definition.id,
                name: "child".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    for (outcome, expected) in [
        (AgentRunOutcome::Completed, "answer"),
        (
            AgentRunOutcome::Interrupted,
            "Interrupted before finishing.",
        ),
        (
            AgentRunOutcome::Failed {
                message: "failure".into(),
            },
            "Failed: failure",
        ),
    ] {
        let sessions = fixture.manager.sessions.clone();
        let report = SubagentCompletion.run(
            &sessions,
            AgentLifecycleEvent::Finished {
                session_id: fixture.session,
                agent_id: child.id,
                run_id: "run".into(),
                outcome,
                final_text: Some("answer".into()),
            },
        );
        let receive = async {
            let SupervisorCommand::SubmitIfNotInterrupted {
                session,
                agent,
                request,
                reply,
            } = fixture.manager.received.recv().await.unwrap()
            else {
                panic!("expected report")
            };
            assert_eq!(session, fixture.session);
            assert_eq!(agent, fixture.agent);
            let AgentRequest::Message {
                source, content, ..
            } = request
            else {
                panic!("expected attributed message")
            };
            assert_eq!(source, Some(child.id));
            assert!(
                matches!(&content[0], llm_api::api::ContentBlock::Text(text) if text.text == expected)
            );
            reply
                .send(Ok(RequestAccepted {
                    request_id: "report".into(),
                }))
                .unwrap();
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(report, receive);
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn starting_plain_child_restores_parent_bundle_lifecycle_callbacks() {
    let mut fixture = Fixture::with_bundles(vec![psa::api::BundleConfig::Subagents]).await;
    let mut cursor = fixture.manager.db.write().await.unwrap();
    let definition = cursor
        .create_agent_definition(psa_store::CreateAgentDefinition {
            name: "plain child after restart".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let child = cursor
        .create_agent(
            fixture.session,
            NewAgent {
                subagent_parent_id: Some(fixture.agent),
                definition_id: definition.id,
                name: "child".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    assert!(fixture.manager.session_resources.is_empty());
    fixture
        .manager
        .spawn_worker(child.id, fixture.session, VecDeque::new(), VecDeque::new())
        .await
        .unwrap();
    assert!(
        fixture
            .manager
            .session_resources
            .contains_key(&fixture.session)
    );
    fixture.manager.cleanup_session_resources().await;
    assert!(!fixture.manager.cleanup_failed);
}

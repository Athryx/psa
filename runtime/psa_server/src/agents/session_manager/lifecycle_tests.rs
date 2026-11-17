use super::*;
use crate::{
    agents::agent_context::AgentContext,
    components::{
        cancellation::Cancellation,
        hooks::{Hook, HookError},
    },
};
use psa::api::{ProbeId, events::StatusChange, probes::Uuid};
use std::{
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);

struct Registration {
    sender: mpsc::UnboundedSender<Recorded>,
    mark_running: bool,
}

static RECORDERS: LazyLock<Mutex<HashMap<ProbeId, Registration>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Recorded {
    name: &'static str,
    agent: AgentId,
    session: Session,
    state: AgentState,
    release: oneshot::Sender<()>,
}

/// Installed only in test builds. Each test owns a unique probe ID, rather than
/// an agent ID (which starts at one in every temporary database).
pub struct RecordLifecycle {
    pub name: &'static str,
}

impl Hook for RecordLifecycle {
    fn cancellation_timeout(&self) -> Duration {
        WAIT
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let registration = RECORDERS
            .lock()
            .unwrap()
            .get(&context.session().probe_id)
            .map(|entry| (entry.sender.clone(), entry.mark_running));
        let Some((sender, mark_running)) = registration else {
            return Ok(());
        };
        assert!(matches!(
            context.probe(),
            Err(psa_shared::probe_transport::ProbeError::Disconnected)
        ));

        if self.name == "create" && mark_running {
            context
                .save_events(&[NewEvent::new(EventType::StatusChange(StatusChange {
                    new_status: AgentState::Running,
                    reason: None,
                }))])
                .await?;
        }
        let (release, released) = oneshot::channel();
        if sender
            .send(Recorded {
                name: self.name,
                agent: context.agent().id,
                session: context.session().clone(),
                state: context.agent().state,
                release,
            })
            .is_ok()
        {
            let _ = released.await;
        }
        Ok(())
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    db: Database,
    publication: Publication,
    probes: ProbeRegistry,
    probe: ProbeId,
    recorded: mpsc::UnboundedReceiver<Recorded>,
}

impl Fixture {
    async fn new(mark_running: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open(directory.path().join("lifecycle.sqlite3"))
            .await
            .unwrap();
        let probe = ProbeId(Uuid::new_v4());
        let mut metadata = crate::test_support::probe_metadata();
        metadata.id = probe;
        let mut cursor = db.cursor().await.unwrap();
        cursor.upsert_probe(&metadata).await.unwrap();
        crate::test_support::create_agent_definition(&mut cursor).await;
        cursor.commit().await.unwrap();
        let (sender, recorded) = mpsc::unbounded_channel();
        RECORDERS.lock().unwrap().insert(
            probe,
            Registration {
                sender,
                mark_running,
            },
        );
        Self {
            _directory: directory,
            db,
            publication: Publication::default(),
            probes: ProbeRegistry::start("test-key".into()),
            probe,
            recorded,
        }
    }

    async fn start(&self) -> Runtime {
        Runtime::start(
            self.db.clone(),
            self.publication.clone(),
            ProviderRegistry::default(),
            self.probes.clone(),
        )
        .await
        .unwrap()
    }

    async fn create_session(&self, sessions: &Sessions) -> Session {
        sessions
            .create_session(psa::api::CreateSession {
                decision_model: None,
                model_config: Some(crate::test_support::model_config()),
                name: "original".into(),
                working_directory: "/work".into(),
                probe_id: self.probe,
                project_id: None,
            })
            .await
            .unwrap()
    }

    async fn next(&mut self, name: &'static str, agent: AgentId) -> Recorded {
        let event = tokio::time::timeout(WAIT, self.recorded.recv())
            .await
            .expect("lifecycle hook timed out")
            .expect("recorder closed");
        assert_eq!(event.name, name);
        assert_eq!(event.agent, agent);
        assert_eq!(event.session.probe_id, self.probe);
        event
    }

    async fn identity_prompt(&self, agent: AgentId) -> String {
        let mut cursor = self.db.cursor().await.unwrap();
        let events = cursor.load_all_events(agent).await.unwrap();
        cursor.rollback().await.unwrap();
        assert_eq!(
            serde_json::to_string(&events)
                .unwrap()
                .matches("<agent_identity>")
                .count(),
            1
        );

        let projection = crate::agents::agent_projection::AgentProjection::replay(&events).unwrap();
        let prompts = projection
            .items()
            .iter()
            .filter_map(|item| match &item.kind {
                llm_api::api::HistoryItemKind::Message(message) => Some(&message.content),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                llm_api::api::ContentBlock::Text(text)
                    if text.text.starts_with("<agent_identity>") =>
                {
                    Some(text.text.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prompts.len(),
            1,
            "identity must survive startup initialization"
        );
        prompts.into_iter().next().unwrap()
    }

    async fn wait_idle(&self, sessions: &Sessions, session: SessionId) {
        tokio::time::timeout(WAIT, async {
            while sessions.is_busy(session).await.unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker did not retire");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        RECORDERS.lock().unwrap().remove(&self.probe);
    }
}

fn agent_request() -> crate::agents::NewAgent {
    crate::agents::NewAgent {
        subagent_parent_id: None,
        interactive: false,
        hidden: false,
        name: "agent".into(),
        definition_id: psa::api::AgentDefinitionId(1),
    }
}

async fn rename(sessions: &Sessions, session: SessionId, name: &str) {
    tokio::time::timeout(
        WAIT,
        sessions.update_session(
            session,
            psa::api::UpdateSession {
                name: Some(name.into()),
                working_directory: Some(format!("/{name}").into()),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("session update waited for its lifecycle hook")
    .unwrap();
}

#[tokio::test]
async fn startup_inherits_session_model_and_updates_leave_explicit_definition_model_pinned() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start().await;
    let session = fixture.create_session(&runtime.handle).await;
    let inherited = runtime
        .handle
        .create_agent(session.id, agent_request())
        .await
        .unwrap();
    fixture
        .next("create", inherited.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;

    let mut explicit_model = session.model_config.clone();
    explicit_model.model = llm_api::providers::Model::new(
        llm_api::providers::ProviderType::Openrouter,
        "explicit-model",
    )
    .unwrap();
    let definition = runtime
        .handle
        .update_agent_definition(
            psa::api::AgentDefinitionId(1),
            crate::db::CreateAgentDefinition {
                name: "explicit".into(),
                model: explicit_model.clone().into(),
                bundles: vec![psa::api::BundleConfig::Noop],
                hooks: psa::api::HookConfigs {
                    agent_create: vec![
                        psa::api::HookConfig::DefaultStartup,
                        psa::api::HookConfig::ToolDefinitions,
                    ],
                    session_update: vec![psa::api::HookConfig::SessionModelUpdate],
                    after_request: vec![psa::api::HookConfig::StopWithoutTools],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        definition.previous_version_id,
        Some(inherited.definition_id)
    );
    let explicit = runtime
        .handle
        .create_agent(
            session.id,
            crate::agents::NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                name: "explicit".into(),
                definition_id: definition.id,
            },
        )
        .await
        .unwrap();
    fixture
        .next("create", explicit.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;

    let mut cursor = fixture.db.cursor().await.unwrap();
    for (agent, expected) in [
        (&inherited, &session.model_config),
        (&explicit, &explicit_model),
    ] {
        let events = cursor.load_all_events(agent.id).await.unwrap();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0].kind, EventType::NewContext));
        assert!(matches!(&events[1].kind, EventType::Model(model) if model == expected));
        assert!(
            matches!(&events[2].kind, EventType::Message(message) if message.message_type == psa::api::events::MessageType::System)
        );
        assert!(matches!(events[3].kind, EventType::ToolDefinition(_)));
    }
    cursor.rollback().await.unwrap();

    let mut updated_model = session.model_config.clone();
    updated_model.model = llm_api::providers::Model::new(
        llm_api::providers::ProviderType::Openrouter,
        "updated-model",
    )
    .unwrap();
    runtime
        .handle
        .update_session(
            session.id,
            psa::api::UpdateSession {
                model_config: Some(updated_model.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut updated_agents = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(WAIT, fixture.recorded.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.name, "update");
        assert_eq!(event.session.model_config, updated_model);
        updated_agents.push(event.agent);
        event.release.send(()).unwrap();
    }
    assert!(updated_agents.contains(&inherited.id));
    assert!(updated_agents.contains(&explicit.id));
    fixture.wait_idle(&runtime.handle, session.id).await;

    runtime
        .handle
        .update_session(
            session.id,
            psa::api::UpdateSession {
                name: Some("renamed without changing model".into()),
                model_config: Some(updated_model.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut unchanged_agents = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(WAIT, fixture.recorded.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.name, "update");
        assert_eq!(event.session.model_config, updated_model);
        unchanged_agents.push(event.agent);
        event.release.send(()).unwrap();
    }
    assert!(unchanged_agents.contains(&inherited.id));
    assert!(unchanged_agents.contains(&explicit.id));
    fixture.wait_idle(&runtime.handle, session.id).await;

    let mut cursor = fixture.db.cursor().await.unwrap();
    for (agent, expected, model_count) in [
        (&inherited, &updated_model, 2),
        (&explicit, &explicit_model, 1),
    ] {
        let events = cursor.load_all_events(agent.id).await.unwrap();
        let models: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventType::Model(model) => Some(model),
                _ => None,
            })
            .collect();
        assert_eq!(models.len(), model_count);
        assert_eq!(*models.last().unwrap(), expected);
    }
    cursor.rollback().await.unwrap();
    runtime.shutdown().await.unwrap();
    fixture.probes.shutdown().await;
}

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
async fn subagent_identity_is_created_once_and_preserved_on_reload_restart_and_fork() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start().await;
    let definition = runtime
        .handle
        .update_agent_definition(
            psa::api::AgentDefinitionId(1),
            crate::db::CreateAgentDefinition {
                name: "subagents".into(),
                bundles: vec![psa::api::BundleConfig::Subagents],
                hooks: psa::api::HookConfigs {
                    agent_create: vec![
                        psa::api::HookConfig::DefaultStartup,
                        psa::api::HookConfig::ToolDefinitions,
                    ],
                    after_request: vec![psa::api::HookConfig::StopWithoutTools],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let session = fixture.create_session(&runtime.handle).await;
    let agent = runtime
        .handle
        .create_agent(
            session.id,
            crate::agents::NewAgent {
                definition_id: definition.id,
                ..agent_request()
            },
        )
        .await
        .unwrap();
    fixture
        .next("create", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    let identity = fixture.identity_prompt(agent.id).await;
    assert!(identity.contains(&format!("You are agent \"{}\"", agent.name)));
    assert!(!identity.contains("Agent ID"));

    // Configured startup initializes context before the bundle's create hook.
    let mut resources = crate::components::bundles::session::SessionBundleResources::start(
        session.id,
        runtime.handle.clone(),
        None,
    );
    let mut installed = crate::components::bundles::InstalledComponents::install_with_session(
        &definition,
        &agent,
        &session,
        Some(&resources.handle()),
    )
    .await
    .unwrap();
    assert!(matches!(
        installed.hooks.agent_create[0],
        crate::components::hooks::HookKind::DefaultStartup(_)
    ));
    assert!(matches!(
        installed.hooks.agent_create[2],
        crate::components::hooks::HookKind::SubagentIdentity(_)
    ));
    assert!(installed.hooks.before_request.is_empty());
    installed.cleanup().await.unwrap();
    resources.cleanup().await.unwrap();

    rename(&runtime.handle, session.id, "reloaded").await;
    fixture
        .next("update", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert_eq!(fixture.identity_prompt(agent.id).await, identity);
    runtime.shutdown().await.unwrap();

    let runtime = fixture.start().await;
    rename(&runtime.handle, session.id, "restarted").await;
    fixture
        .next("update", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert_eq!(fixture.identity_prompt(agent.id).await, identity);

    let fork = runtime
        .handle
        .fork_session(
            session.id,
            psa::api::ForkSession {
                point: psa::api::ForkPoint::Head,
                metadata: psa::api::UpdateSession::default(),
            },
        )
        .await
        .unwrap();
    let child = &fork.agents[0];
    fixture
        .next("create", child.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, fork.session.id).await;
    assert_ne!(child.id, agent.id);
    assert_eq!(fixture.identity_prompt(child.id).await, identity);
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
    assert_eq!(fork.agents[0].parent_id, Some(parent.id));
    let child = &fork.agents[0];
    assert_ne!(child.id, parent.id);
    let created = fixture.next("create", child.id).await;
    assert_eq!(created.session.id, fork.session.id);
    assert_eq!(created.session.parent_id, Some(session.id));
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
    let commands = ManagerClient::new(commands);
    let client_queries = ClientQueries::start(fixture.publication.clone());
    let sessions = Sessions::new(
        fixture.db.clone(),
        fixture.publication.clone(),
        commands.clone(),
        client_queries.handle(),
    );
    let mut manager = SessionManager {
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
        next_request_id: 1,
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
    assert!(matches!(&command, ManagerCommand::Boundary { .. }));
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

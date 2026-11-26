//! A lifecycle-recording runtime fixture, for testing when the runtime runs
//! agent-create and session-update hooks.

use crate::supervisor::Runtime;
use crate::{
    AgentId, AgentState,
    agent_context::AgentContext,
    bundles::InstalledComponents,
    cancellation::Cancellation,
    factory::ComponentFactory,
    hooks::{Hook, HookError},
    probes::ProbeRegistry,
    publication::Publication,
    runtime_handle::RuntimeHandle,
};
use futures_util::future::BoxFuture;
use psa::api::{
    ProbeId, SessionId,
    events::{EventType, StatusChange},
    probes::Uuid,
};
use psa_models::ProviderRegistry;
use psa_store::{Database, NewEvent, Session};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

pub const WAIT: Duration = Duration::from_secs(5);

struct Registration {
    sender: mpsc::UnboundedSender<Recorded>,
    mark_running: bool,
}

static RECORDERS: LazyLock<Mutex<HashMap<ProbeId, Registration>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub struct Recorded {
    pub name: &'static str,
    pub agent: AgentId,
    pub session: Session,
    pub state: AgentState,
    pub release: oneshot::Sender<()>,
}

/// Installed by [`RecordingComponents`]. Each test owns a unique probe ID,
/// rather than an agent ID (which starts at one in every temporary database).
pub struct RecordLifecycle {
    pub name: &'static str,
}

impl Hook for RecordLifecycle {
    fn name(&self) -> &'static str {
        "RecordLifecycle"
    }

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

/// A runtime over a fresh database whose agents report their creation and
/// session-update hooks to the test, which releases each in turn.
pub struct Fixture {
    _directory: tempfile::TempDir,
    pub db: Database,
    pub publication: Publication,
    pub probes: ProbeRegistry,
    pub probe: ProbeId,
    pub recorded: mpsc::UnboundedReceiver<Recorded>,
}

impl Fixture {
    pub async fn new(mark_running: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open(directory.path().join("lifecycle.sqlite3"))
            .await
            .unwrap();
        let probe = ProbeId(Uuid::new_v4());
        let mut metadata = crate::test_support::probe_metadata();
        metadata.id = probe;
        let mut cursor = db.write().await.unwrap();
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

    /// Start a runtime installing no concrete components.
    pub async fn start(&self) -> Runtime {
        self.start_with(crate::bundles::testing::TestComponents)
            .await
    }

    /// Start a runtime installing `components` plus the recording hooks.
    pub async fn start_with(&self, components: impl ComponentFactory) -> Runtime {
        Runtime::start(
            self.db.clone(),
            self.publication.clone(),
            ProviderRegistry::default(),
            self.probes.clone(),
            Arc::new(RecordingComponents(components)),
        )
        .await
        .unwrap()
    }

    pub async fn create_session(&self, sessions: &RuntimeHandle) -> Session {
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

    pub async fn next(&mut self, name: &'static str, agent: AgentId) -> Recorded {
        let event = tokio::time::timeout(WAIT, self.recorded.recv())
            .await
            .expect("lifecycle hook timed out")
            .expect("recorder closed");
        assert_eq!(event.name, name);
        assert_eq!(event.agent, agent);
        assert_eq!(event.session.probe_id, self.probe);
        event
    }

    pub async fn wait_idle(&self, sessions: &RuntimeHandle, session: SessionId) {
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

pub fn agent_request() -> crate::NewAgent {
    crate::NewAgent {
        subagent_parent_id: None,
        interactive: false,
        hidden: false,
        name: "agent".into(),
        definition_id: psa::api::AgentDefinitionId(1),
    }
}

pub async fn rename(sessions: &RuntimeHandle, session: SessionId, name: &str) {
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

/// Wraps a factory, appending [`RecordLifecycle`] hooks to every agent it installs.
pub struct RecordingComponents<F>(pub F);

impl<F: ComponentFactory> ComponentFactory for RecordingComponents<F> {
    fn validate_bundles(&self, bundles: &[psa::api::BundleConfig]) -> Result<(), String> {
        self.0.validate_bundles(bundles)
    }

    fn require_command_support(
        &self,
        definition: &crate::AgentDefinition,
        interactive: bool,
        command: &psa::api::AgentCommand,
    ) -> Result<(), String> {
        self.0
            .require_command_support(definition, interactive, command)
    }

    fn needs_session_components(
        &self,
        definition: &crate::AgentDefinition,
        parent: Option<&crate::AgentDefinition>,
    ) -> bool {
        self.0.needs_session_components(definition, parent)
    }

    fn start_session(
        &self,
        session: SessionId,
        runtime: RuntimeHandle,
        providers: ProviderRegistry,
    ) -> Box<dyn crate::factory::SessionComponents> {
        self.0.start_session(session, runtime, providers)
    }

    fn install<'a>(
        &'a self,
        definition: &'a crate::AgentDefinition,
        agent: &'a crate::Agent,
        session: &'a Session,
        resources: Option<&'a crate::factory::SessionComponentsHandle>,
    ) -> BoxFuture<'a, Result<InstalledComponents, crate::bundles::InstallError>> {
        Box::pin(async move {
            let mut components = self
                .0
                .install(definition, agent, session, resources)
                .await?;
            let hooks = &mut components.hooks;
            hooks
                .agent_create
                .push(Box::new(RecordLifecycle { name: "create" }));
            hooks
                .session_update
                .push(Box::new(RecordLifecycle { name: "update" }));
            Ok(components)
        })
    }
}

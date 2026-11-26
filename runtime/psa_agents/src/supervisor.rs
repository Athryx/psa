//! A single global supervisor. Only active agents have worker tasks.

use super::{
    AgentError, AgentId, AgentState,
    agent_manager::{AgentClient, AgentManagerState, recover_agent, run_worker},
    client_queries::ClientQueries,
    lifecycle::{AgentLifecycleEvent, AgentRunOutcome},
    publication::Publication,
    runtime_handle::RuntimeHandle,
};
use crate::{
    bundles::InstalledComponents,
    factory::{ComponentFactory, SessionComponents, SessionComponentsHandle},
    probes::ProbeRegistry,
};
use actors::Mailbox;
use psa::api::{
    AgentCommand, AgentRequest, CommandDelivery, CommandResult, MessageDelivery, RequestAccepted,
    RequestId, StreamMessage,
    events::{AgentMessageLink, CommandRecord, EventId, EventType, Log, MessageType},
};
use psa_models::ProviderRegistry;
use psa_store::{Database, DbError, Session, SessionId};
use psa_store::{EventRange, NewEvent};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use tokio::{
    sync::{Mutex, oneshot, watch},
    task::{Id, JoinError, JoinHandle, JoinSet},
};

#[cfg(test)]
#[path = "tests/supervisor/bundle_tests.rs"]
mod bundle_tests;
#[cfg(test)]
#[path = "tests/supervisor/lifecycle_tests.rs"]
mod lifecycle_tests;
#[cfg(test)]
#[path = "tests/supervisor/signals_tests.rs"]
mod signals_tests;
#[cfg(any(test, feature = "testing"))]
#[path = "tests/supervisor/testing.rs"]
pub mod testing;
#[cfg(test)]
#[path = "tests/supervisor/tests.rs"]
mod tests;

const MAX_PENDING: usize = 128;

fn new_request_id() -> RequestId {
    RequestId(psa::api::probes::Uuid::new_v4().to_string())
}
pub(crate) const EXECUTION_FAILURE_MESSAGE: &str =
    "Agent execution failed; see server logs for details.";

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("agent runtime failed: {0}")]
    Agent(#[from] AgentError),
    #[error("agent runtime is shutting down or unavailable")]
    Unavailable,
    #[error("operation cancelled")]
    Cancelled,
    #[error("agent {agent:?} in session {session:?} stopped without a matching durable result")]
    ResultUnavailable { session: SessionId, agent: AgentId },
    #[error("provider is not configured")]
    MissingProvider,
    #[error("agent request queue is full")]
    Overloaded,
    #[error("resume requires an interrupted agent")]
    InvalidResume,
    #[error("worker cleanup or persistence failed")]
    CleanupFailed,
    #[error("{0}")]
    Invalid(String),
}

pub struct Runtime {
    pub handle: RuntimeHandle,
    task: JoinHandle<Result<(), RuntimeError>>,
}

impl Runtime {
    pub async fn start(
        db: Database,
        publication: Publication,
        providers: ProviderRegistry,
        probes: ProbeRegistry,
        components: Arc<dyn ComponentFactory>,
    ) -> Result<Self, RuntimeError> {
        Self::start_with_configuration(
            db,
            publication,
            providers,
            probes,
            components,
            None,
            psa_config::server::default_model_config(),
        )
        .await
    }

    pub async fn start_with_configuration(
        db: Database,
        publication: Publication,
        providers: ProviderRegistry,
        probes: ProbeRegistry,
        components: Arc<dyn ComponentFactory>,
        configuration: Option<std::sync::Arc<dyn crate::config_discovery::ConfigDiscovery>>,
        default_model_config: llm_api::providers::ModelConfig,
    ) -> Result<Self, RuntimeError> {
        let mut cursor = db.read().await?;
        let agents = cursor.list_all_agents().await?;
        cursor.close().await?;

        // Repair incomplete event streams left by a server crash.
        for agent in agents {
            publication.drain_appends(agent.id).await;
            recover_agent(&db, &publication, agent.id).await?;
        }
        let mut cursor = db.write().await?;
        // Backfill first: repairs rewrite totals and keep stored tracking.
        let backfilled = cursor.backfill_cache_tracking().await?;
        let repaired = cursor.repair_unknown_usage().await?;
        cursor.commit().await?;
        if repaired > 0 {
            tracing::info!(repaired, "recomputed agent usage totals");
        }
        if backfilled > 0 {
            tracing::info!(backfilled, "computed agent cache tracking");
        }

        let (commands, received) = actors::channel(256);
        let commands = SupervisorClient::new(commands);
        let client_queries = ClientQueries::start(publication.clone());
        let handle = RuntimeHandle::new(
            db.clone(),
            publication.clone(),
            commands.clone(),
            client_queries.handle(),
            components,
        )
        .with_catalog(providers.catalog().cloned())
        .with_default_model_config(default_model_config)
        .with_configuration(configuration, probes.clone());
        let manager = Supervisor {
            db,
            publication,
            providers,
            probes,
            sessions: handle.clone(),
            client_queries,
            commands,
            received,
            workers: HashMap::new(),
            components: HashMap::new(),
            session_resources: HashMap::new(),
            steering: HashMap::new(),
            runs: HashMap::new(),
            #[cfg(test)]
            lifecycle_events: Vec::new(),
            tasks: JoinSet::new(),
            next_generation: 1,
            shutting_down: false,
            shutdown_reply: None,
            cleanup_failed: false,
        };
        Ok(Self {
            handle,
            task: tokio::spawn(manager.run()),
        })
    }

    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        let _ = self.handle.commands.shutdown().await;
        self.task.await.map_err(|_| RuntimeError::CleanupFailed)?
    }
}

#[derive(Clone)]
pub(crate) struct QueuedRequest {
    pub id: RequestId,
    pub delivery: MessageDelivery,
    pub content: Vec<llm_api::api::ContentBlock>,
    pub message_type: MessageType,
    pub source: Option<psa::api::events::AgentMessageLink>,
    pub allow_restart: bool,
}

pub(crate) struct QueuedCommand {
    pub id: RequestId,
    pub delivery: CommandDelivery,
    pub work: CommandWork,
}

/// What a queued command asks the worker to do. Either way, the worker records
/// the finished command in the transcript.
pub(crate) enum CommandWork {
    /// Run a typed command with the command hooks and report its result.
    Run {
        command: AgentCommand,
        /// Dropped unanswered when the command is discarded before it finishes.
        reply: oneshot::Sender<CommandResult>,
    },
    /// Record a client-side command that has already finished.
    Record(CommandRecord),
}

/// Messages and commands share one FIFO so their relative order is preserved.
enum QueuedItem {
    Message(QueuedRequest),
    Command(QueuedCommand),
}

impl QueuedItem {
    fn id(&self) -> &RequestId {
        match self {
            Self::Message(request) => &request.id,
            Self::Command(command) => &command.id,
        }
    }

    /// Only messages are steering; a steered command must not wake `wait_message`.
    fn is_steering_message(&self) -> bool {
        matches!(self, Self::Message(request) if request.delivery == MessageDelivery::Steer)
    }

    fn is_eligible(&self, state: AgentState) -> bool {
        match self {
            Self::Message(request) => match request.delivery {
                MessageDelivery::Queue => state != AgentState::Running,
                MessageDelivery::Steer => true,
                MessageDelivery::Resume => state == AgentState::Interrupted,
            },
            Self::Command(command) => match command.delivery {
                CommandDelivery::Queue => state != AgentState::Running,
                CommandDelivery::Steer => true,
            },
        }
    }

    /// Commands run in any agent state, so only messages can become stale.
    fn rejection(&self, state: AgentState) -> Option<&'static str> {
        let Self::Message(request) = self else {
            return None;
        };
        if !request.allow_restart && state == AgentState::Interrupted {
            return Some("background message cannot restart an interrupted agent");
        }
        if request.delivery == MessageDelivery::Resume && state != AgentState::Interrupted {
            return Some("resume requires an interrupted agent");
        }
        None
    }
}

pub(crate) enum LifecycleHook {
    AgentCreate,
    SessionUpdate(Session),
}

pub(crate) enum BoundaryReply {
    Hook(LifecycleHook),
    Work(QueuedRequest),
    Command(QueuedCommand),
    Continue,
    Stop,
}

/// Test seam: the supervisor's mailbox protocol. Worker-only variants carry
/// runtime-internal types.
#[doc(hidden)]
#[allow(private_interfaces)]
#[actors::mailbox(client = SupervisorClient)]
pub enum SupervisorCommand {
    /// Check whether a session has any active workers.
    IsBusy {
        session: SessionId,
        #[reply]
        reply: oneshot::Sender<bool>,
    },
    /// Atomically validate and subscribe to level-triggered queued steering.
    SubscribeSteering {
        session: SessionId,
        agent: AgentId,
        #[reply]
        reply: oneshot::Sender<Result<watch::Receiver<Option<bool>>, RuntimeError>>,
    },
    /// Validate and enqueue an agent request, starting its worker if needed.
    Submit {
        session: SessionId,
        agent: AgentId,
        request: AgentRequest,
        #[reply]
        reply: oneshot::Sender<Result<RequestAccepted, RuntimeError>>,
    },
    /// Validate and enqueue a command; the inner receiver yields its result.
    RunCommand {
        session: SessionId,
        agent: AgentId,
        request: psa::api::RunCommand,
        #[reply]
        reply: oneshot::Sender<Result<oneshot::Receiver<CommandResult>, RuntimeError>>,
    },
    /// Conditional steering for background hooks; never restarts a stopped agent.
    SubmitIfNotInterrupted {
        session: SessionId,
        agent: AgentId,
        request: AgentRequest,
        #[reply]
        reply: oneshot::Sender<Result<RequestAccepted, RuntimeError>>,
    },
    /// Report worker state and select queued work, continuation, or stop.
    Boundary {
        agent: AgentId,
        generation: u64,
        state: AgentState,
        #[reply]
        reply: oneshot::Sender<BoundaryReply>,
    },
    /// Clear the in-flight request after its worker consumes it.
    Consumed {
        agent: AgentId,
        generation: u64,
        request_id: RequestId,
    },
    /// Describe an agent's installed components, installing them if needed.
    DescribeComponents {
        session: SessionId,
        agent: AgentId,
        #[reply]
        reply: oneshot::Sender<Result<psa::api::AgentComponents, RuntimeError>>,
    },
    /// Schedule the creation hook only for a newly committed agent.
    AgentsCreated { agents: Vec<AgentId> },
    /// Notify every agent in the session, starting idle workers as needed.
    SessionUpdated { session: Session },
    /// Discard queued work, interrupt workers, and wait for their exit.
    Shutdown {
        #[reply]
        reply: oneshot::Sender<()>,
    },
}

impl SupervisorClient {
    pub(crate) async fn reserve_notification(
        &self,
    ) -> Result<actors::MailboxPermit<'_, SupervisorCommand>, RuntimeError> {
        self.handle
            .reserve()
            .await
            .map_err(|_| RuntimeError::Unavailable)
    }
}

struct Worker {
    session: SessionId,
    generation: u64,
    task_id: Id,
    controls: AgentClient,
    queue: VecDeque<QueuedItem>,
    lifecycle: VecDeque<LifecycleHook>,
    in_flight: Option<RequestId>,
    stopping: bool,
}

type WorkerResult = (AgentId, u64, Result<(), AgentError>);

struct ActiveRun {
    session: SessionId,
    id: String,
    baseline: EventId,
}

#[derive(Default)]
struct RunSummary {
    final_text: Option<String>,
    inference_outcome: Option<AgentRunOutcome>,
}

struct Supervisor {
    db: Database,
    publication: Publication,
    providers: ProviderRegistry,
    probes: ProbeRegistry,
    sessions: RuntimeHandle,
    client_queries: ClientQueries,
    commands: SupervisorClient,
    received: Mailbox<SupervisorCommand>,
    workers: HashMap<AgentId, Worker>,
    components: HashMap<AgentId, Arc<Mutex<InstalledComponents>>>,
    session_resources: HashMap<SessionId, Box<dyn SessionComponents>>,
    steering: HashMap<AgentId, watch::Sender<Option<bool>>>,
    runs: HashMap<AgentId, ActiveRun>,
    #[cfg(test)]
    lifecycle_events: Vec<AgentLifecycleEvent>,
    tasks: JoinSet<WorkerResult>,
    next_generation: u64,
    shutting_down: bool,
    shutdown_reply: Option<oneshot::Sender<()>>,
    cleanup_failed: bool,
}

impl Supervisor {
    fn is_busy(&self, session: SessionId) -> bool {
        self.workers
            .values()
            .any(|worker| worker.session == session)
    }

    async fn publish_session_activity(&self, session: SessionId, busy: bool) {
        if let Err(error) = self
            .publication
            .update_session_busy(&self.db, session, busy)
            .await
        {
            tracing::error!(session = session.0, %error, "could not publish session activity");
        }
    }

    async fn run(mut self) -> Result<(), RuntimeError> {
        loop {
            if self.shutting_down && self.workers.is_empty() {
                break;
            }

            tokio::select! {
                command = self.received.recv() => {
                    if let Some(command) = command {
                        self.handle_command(command).await;
                    } else {
                        self.begin_shutdown().await;
                    }
                }
                joined = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    if let Some(joined) = joined {
                        self.handle_join(joined).await;
                    }
                }
            }
        }

        // Also close on any normal run exit without an explicit Shutdown.
        self.close_client_queries().await;
        self.cleanup_components().await;

        if let Some(reply) = self.shutdown_reply {
            let _ = reply.send(());
        }
        if self.cleanup_failed {
            Err(RuntimeError::CleanupFailed)
        } else {
            Ok(())
        }
    }

    async fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        self.publication.close();
        for worker in self.workers.values_mut() {
            worker.queue.clear();
            worker.lifecycle.clear();
            let _ = worker.controls.interrupt();
        }

        for signal in self.steering.values() {
            signal.send_replace(None);
        }
        // Interrupt every worker before awaiting the registry. Closing it now
        // releases outstanding queries before waiting for worker cleanup, and
        // rejects queries from any cleanup hooks that run afterward.
        self.close_client_queries().await;
        self.cleanup_session_resources().await;
    }

    async fn close_client_queries(&mut self) {
        if let Err(error) = self.client_queries.shutdown().await {
            self.cleanup_failed = true;
            tracing::error!(%error, "client query registry shutdown failed");
        }
    }

    async fn cleanup_session_resources(&mut self) {
        let mut cleanup = JoinSet::new();
        for (session, mut resources) in self.session_resources.drain() {
            cleanup.spawn(async move {
                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(3), resources.cleanup())
                        .await
                        .map_err(|_| "session bundle cleanup timed out".to_owned())
                        .and_then(|result| result.map_err(|error| error.to_string()));
                (session, result)
            });
        }
        while let Some(result) = cleanup.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((session, Err(error))) => {
                    self.cleanup_failed = true;
                    tracing::error!(session = session.0, %error, "session bundle cleanup failed");
                }
                Err(error) => {
                    self.cleanup_failed = true;
                    tracing::error!(%error, "session bundle cleanup task failed");
                }
            }
        }
    }

    async fn cleanup_components(&mut self) {
        // Workers have all exited, releasing tool references and hook borrows.
        // Clean agents concurrently so shutdown is not serial in agent count.
        let mut cleanup = JoinSet::new();
        for (agent, components) in self.components.drain() {
            cleanup.spawn(async move {
                let result = components.lock().await.cleanup().await;
                (agent, result)
            });
        }
        while let Some(result) = cleanup.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((agent, Err(error))) => {
                    self.cleanup_failed = true;
                    tracing::error!(agent = agent.0, %error, "agent bundle cleanup failed");
                }
                Err(error) => {
                    self.cleanup_failed = true;
                    tracing::error!(%error, "agent bundle cleanup task failed");
                }
            }
        }
    }

    async fn handle_join(&mut self, joined: Result<(Id, WorkerResult), JoinError>) {
        match joined {
            Ok((_, (agent, generation, result))) => {
                self.handle_exit(agent, generation, result).await;
            }
            Err(error) => {
                let owner = self
                    .workers
                    .iter()
                    .find(|(_, worker)| worker.task_id == error.id())
                    .map(|(id, worker)| (*id, worker.generation));
                if let Some((agent, generation)) = owner {
                    self.handle_exit(agent, generation, Err(AgentError::SupervisorUnavailable))
                        .await;
                }
            }
        }
    }

    async fn subscribe_steering(
        &mut self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<watch::Receiver<Option<bool>>, RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::Unavailable);
        }
        let mut cursor = self.db.read().await?;
        cursor.require_agent_session(session, agent).await?;
        cursor.close().await?;

        // Queue inspection and subscription happen in one actor turn. Channels
        // outlive worker generations; dropping a waiter drops only its receiver.
        let queued = self
            .workers
            .get(&agent)
            .is_some_and(|worker| worker.queue.iter().any(QueuedItem::is_steering_message));
        let signal = self
            .steering
            .entry(agent)
            .or_insert_with(|| watch::channel(Some(queued)).0);
        signal.send_if_modified(|value| {
            if *value == Some(queued) {
                return false;
            }
            *value = Some(queued);
            true
        });
        Ok(signal.subscribe())
    }

    fn refresh_steering(&self, agent: AgentId) {
        if let Some(signal) = self.steering.get(&agent) {
            let queued = self
                .workers
                .get(&agent)
                .is_some_and(|worker| worker.queue.iter().any(QueuedItem::is_steering_message));
            let next = if self.shutting_down {
                None
            } else {
                Some(queued)
            };
            signal.send_if_modified(|value| {
                if *value == next {
                    return false;
                }
                *value = next;
                true
            });
        }
    }

    fn notify_lifecycle(&mut self, session: SessionId, event: AgentLifecycleEvent) {
        #[cfg(test)]
        self.lifecycle_events.push(event.clone());
        if let Some(resources) = self.session_resources.get(&session) {
            resources.notify(event);
        }
    }

    async fn start_run(&mut self, agent: AgentId, session: SessionId) -> Result<(), RuntimeError> {
        if self.runs.contains_key(&agent) {
            return Ok(());
        }
        let mut cursor = self.db.read().await?;
        let snapshot = cursor.require_agent_session(session, agent).await?;
        cursor.close().await?;
        let run_id = psa::api::probes::Uuid::new_v4().to_string();
        self.runs.insert(
            agent,
            ActiveRun {
                session,
                id: run_id.clone(),
                baseline: EventId(snapshot.next_event_id.0.saturating_sub(1)),
            },
        );
        // Publish before replying Work/Continue: no child tool can run first.
        self.notify_lifecycle(
            session,
            AgentLifecycleEvent::Started {
                session_id: session,
                agent_id: agent,
                run_id,
            },
        );
        Ok(())
    }

    async fn finish_run(&mut self, agent: AgentId, outcome: AgentRunOutcome) {
        let Some(run) = self.runs.remove(&agent) else {
            return;
        };
        let summary = match self.load_run_summary(agent, &run).await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::error!(agent = agent.0, %error, "failed to read agent run summary");
                RunSummary::default()
            }
        };
        // Provider failures are persisted as an Interrupted agent state rather
        // than worker errors. Never expose their error bodies to subscribers,
        // and never let transcript outcomes mask an explicit worker failure.
        let outcome = if matches!(outcome, AgentRunOutcome::Failed { .. }) {
            outcome
        } else {
            summary.inference_outcome.unwrap_or(outcome)
        };
        self.notify_lifecycle(
            run.session,
            AgentLifecycleEvent::Finished {
                session_id: run.session,
                agent_id: agent,
                run_id: run.id,
                outcome,
                final_text: summary.final_text,
            },
        );
    }

    async fn load_run_summary(
        &self,
        agent: AgentId,
        run: &ActiveRun,
    ) -> Result<RunSummary, RuntimeError> {
        let mut cursor = self.db.read().await?;
        let mut after = run.baseline;
        let mut summary = RunSummary::default();
        loop {
            let events = cursor
                .load_events(
                    agent,
                    EventRange {
                        after,
                        through: None,
                        limit: 1000,
                    },
                )
                .await?;
            if events.is_empty() {
                break;
            }
            for event in events {
                after = event.id;
                match event.kind {
                    EventType::Message(message) if message.message_type == MessageType::Agent => {
                        let text: String = message
                            .content
                            .into_iter()
                            .filter_map(|block| match block {
                                llm_api::api::ContentBlock::Text(text) => Some(text.text),
                                _ => None,
                            })
                            .collect();
                        summary.final_text = (!text.is_empty()).then_some(text);
                    }
                    EventType::Result(result) => {
                        summary.final_text = Some(match result {
                            psa::api::AgentResult::Text(text) => text,
                            psa::api::AgentResult::Json(value) => value.to_string(),
                        });
                        summary.inference_outcome = None;
                    }
                    EventType::InferenceResultEnd(result) => {
                        summary.inference_outcome = match result.outcome {
                            llm_api::api::Outcome::Failed(_) => Some(AgentRunOutcome::Failed {
                                message: EXECUTION_FAILURE_MESSAGE.into(),
                            }),
                            llm_api::api::Outcome::Incomplete {
                                reason: llm_api::api::StopReason::TokenLimit,
                            } => None,
                            llm_api::api::Outcome::Cancelled
                            | llm_api::api::Outcome::Incomplete { .. } => {
                                Some(AgentRunOutcome::Interrupted)
                            }
                            _ => None,
                        };
                    }
                    _ => {}
                }
            }
        }
        cursor.close().await?;
        Ok(summary)
    }

    async fn handle_command(&mut self, command: SupervisorCommand) {
        match command {
            SupervisorCommand::IsBusy { session, reply } => {
                let _ = reply.send(self.is_busy(session));
            }
            SupervisorCommand::SubmitIfNotInterrupted {
                session,
                agent,
                request,
                reply,
            } => {
                let result = self.submit_request(session, agent, request, false).await;
                let _ = reply.send(result);
            }
            SupervisorCommand::SubscribeSteering {
                session,
                agent,
                reply,
            } => {
                let result = self.subscribe_steering(session, agent).await;
                let _ = reply.send(result);
            }
            SupervisorCommand::Submit {
                session,
                agent,
                request,
                reply,
            } => {
                let result = self.submit(session, agent, request).await;
                let _ = reply.send(result);
            }
            SupervisorCommand::RunCommand {
                session,
                agent,
                request,
                reply,
            } => {
                let result = self.submit_command(session, agent, request).await;
                let _ = reply.send(result);
            }
            SupervisorCommand::Boundary {
                agent,
                generation,
                state,
                reply,
            } => {
                self.handle_boundary(agent, generation, state, reply).await;
            }
            SupervisorCommand::Consumed {
                agent,
                generation,
                request_id,
            } => {
                if let Some(worker) = self
                    .workers
                    .get_mut(&agent)
                    .filter(|worker| worker.generation == generation)
                    && worker.in_flight.as_ref() == Some(&request_id)
                {
                    worker.in_flight = None;
                }
            }
            SupervisorCommand::DescribeComponents {
                session,
                agent,
                reply,
            } => {
                let result = self.describe_components(session, agent).await;
                let _ = reply.send(result);
            }
            SupervisorCommand::AgentsCreated { agents } => {
                for agent in agents {
                    if let Err(error) = self.schedule_agent_create(agent).await {
                        tracing::error!(agent = agent.0, %error, "failed to schedule agent creation hook");
                    }
                }
            }
            SupervisorCommand::SessionUpdated { session } => {
                if let Err(error) = self.schedule_session_update(&session).await {
                    tracing::error!(session = session.id.0, %error, "failed to schedule session update hooks");
                }
            }
            SupervisorCommand::Shutdown { reply } => {
                self.shutdown_reply = Some(reply);
                self.begin_shutdown().await;
            }
        }
    }

    async fn handle_boundary(
        &mut self,
        agent: AgentId,
        generation: u64,
        state: AgentState,
        reply: oneshot::Sender<BoundaryReply>,
    ) {
        if !self
            .workers
            .get(&agent)
            .is_some_and(|worker| worker.generation == generation)
        {
            return;
        }
        if state != AgentState::Running {
            let outcome = if state == AgentState::Interrupted {
                AgentRunOutcome::Interrupted
            } else {
                AgentRunOutcome::Completed
            };
            self.finish_run(agent, outcome).await;
        }
        let worker = self.workers.get_mut(&agent).expect("worker exists");

        let mut rejected = Vec::new();
        worker.queue.retain(|item| {
            let Some(reason) = item.rejection(state) else {
                return true;
            };
            rejected.push((item.id().to_owned(), reason));
            false
        });

        let session = worker.session;
        let eligible = if self.shutting_down {
            None
        } else {
            worker.queue.iter().position(|item| item.is_eligible(state))
        };
        let lifecycle = if self.shutting_down {
            None
        } else {
            worker.lifecycle.pop_front()
        };
        let response = if let Some(hook) = lifecycle {
            BoundaryReply::Hook(hook)
        } else if let Some(index) = eligible {
            let item = worker.queue.remove(index).expect("queue index exists");
            worker.in_flight = Some(item.id().to_owned());
            match item {
                QueuedItem::Message(request) => BoundaryReply::Work(request),
                QueuedItem::Command(command) => BoundaryReply::Command(command),
            }
        } else if state == AgentState::Running {
            BoundaryReply::Continue
        } else {
            worker.stopping = true;
            BoundaryReply::Stop
        };
        self.refresh_steering(agent);
        if matches!(&response, BoundaryReply::Work(_))
            || matches!(&response, BoundaryReply::Continue)
        {
            if let Err(error) = self.start_run(agent, session).await {
                tracing::error!(agent = agent.0, %error, "failed to start logical agent run");
                self.cleanup_failed = true;
                return;
            }
        }
        let _ = reply.send(response);

        for (request_id, reason) in rejected {
            self.publication
                .lock_state()
                .await
                .publish(StreamMessage::RequestFailed {
                    session_id: session,
                    agent_id: agent,
                    request_id,
                    message: reason.into(),
                });
        }
    }

    async fn submit(
        &mut self,
        session: SessionId,
        agent: AgentId,
        request: AgentRequest,
    ) -> Result<RequestAccepted, RuntimeError> {
        self.submit_request(session, agent, request, true).await
    }

    async fn submit_request(
        &mut self,
        session: SessionId,
        agent: AgentId,
        request: AgentRequest,
        allow_restart: bool,
    ) -> Result<RequestAccepted, RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::Unavailable);
        }

        let mut cursor = self.db.read().await?;
        let snapshot = cursor.require_agent_session(session, agent).await?;
        if !allow_restart && snapshot.state == AgentState::Interrupted {
            return Err(RuntimeError::Invalid(
                "background message cannot restart an interrupted agent".into(),
            ));
        }
        let context = cursor.load_context_window(agent).await?;
        let definition = cursor.get_agent_definition(snapshot.definition_id).await?;
        let session_snapshot = cursor.get_session(session).await?;

        let (delivery, content, message_type, source) = match request {
            AgentRequest::Message {
                delivery,
                content,
                source: None,
            } => (delivery, content, MessageType::Human, None),
            AgentRequest::Message {
                delivery,
                content,
                source: Some(sender),
            } => {
                let name = cursor.require_agent_session(session, sender).await?.name;
                (
                    delivery,
                    content,
                    MessageType::Subagent,
                    Some((sender, name)),
                )
            }
            AgentRequest::RecordCommand(record) => {
                cursor.close().await?;
                if record.name.trim().is_empty() || record.name.contains(char::is_whitespace) {
                    return Err(RuntimeError::Invalid(
                        "command name must be one nonempty word".into(),
                    ));
                }
                return self
                    .enqueue(agent, session, |id| {
                        QueuedItem::Command(QueuedCommand {
                            id,
                            delivery: CommandDelivery::Steer,
                            work: CommandWork::Record(record),
                        })
                    })
                    .await;
            }
            AgentRequest::Interrupt => {
                cursor.close().await?;
                if let Some(worker) = self.workers.get(&agent) {
                    let _ = worker.controls.interrupt();
                }
                return Ok(RequestAccepted {
                    request_id: new_request_id(),
                });
            }
        };
        if content.is_empty() {
            return Err(RuntimeError::Invalid(
                "message content must not be empty".into(),
            ));
        }
        cursor.close().await?;

        if delivery == MessageDelivery::Resume && snapshot.state != AgentState::Interrupted {
            return Err(RuntimeError::InvalidResume);
        }

        // Creation hooks may still be queued when the first request arrives.
        // Validate its provider without making startup initialization implicit.
        let model = context
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                EventType::Model(model) => Some(model.clone()),
                _ => None,
            })
            .unwrap_or_else(|| definition.model.resolve(&session_snapshot.model_config));
        if self.providers.provider(model.model.provider).is_none() {
            return Err(RuntimeError::MissingProvider);
        }

        let recipient_name = snapshot.name;
        self.enqueue(agent, session, |id| {
            // Every subagent message records the request that delivered it.
            let source = source.map(|(sender, sender_name)| AgentMessageLink {
                request_id: id.clone(),
                sender,
                sender_name,
                recipient: agent,
                recipient_name,
            });
            QueuedItem::Message(QueuedRequest {
                id,
                delivery,
                content,
                message_type,
                source,
                allow_restart,
            })
        })
        .await
    }

    async fn submit_command(
        &mut self,
        session: SessionId,
        agent: AgentId,
        request: psa::api::RunCommand,
    ) -> Result<oneshot::Receiver<CommandResult>, RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::Unavailable);
        }

        let mut cursor = self.db.read().await?;
        let snapshot = cursor.require_agent_session(session, agent).await?;
        let definition = cursor.get_agent_definition(snapshot.definition_id).await?;
        cursor.close().await?;
        self.sessions
            .components()
            .require_command_support(&definition, snapshot.interactive, &request.command)
            .map_err(RuntimeError::Invalid)?;

        let (reply, result) = oneshot::channel();
        self.enqueue(agent, session, |id| {
            QueuedItem::Command(QueuedCommand {
                id,
                delivery: request.delivery,
                work: CommandWork::Run {
                    command: request.command,
                    reply,
                },
            })
        })
        .await?;
        Ok(result)
    }

    async fn enqueue(
        &mut self,
        agent: AgentId,
        session: SessionId,
        build: impl FnOnce(RequestId) -> QueuedItem,
    ) -> Result<RequestAccepted, RuntimeError> {
        if self
            .workers
            .get(&agent)
            .is_some_and(|worker| worker.queue.len() >= MAX_PENDING)
        {
            return Err(RuntimeError::Overloaded);
        }
        let id = new_request_id();

        self.ensure_worker(agent, session).await?;
        self.workers
            .get_mut(&agent)
            .expect("worker exists")
            .queue
            .push_back(build(id.clone()));

        self.refresh_steering(agent);
        Ok(RequestAccepted { request_id: id })
    }

    async fn schedule_agent_create(&mut self, agent: AgentId) -> Result<(), RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::Unavailable);
        }

        let mut cursor = self.db.read().await?;
        let snapshot = cursor.get_agent(agent).await?;
        cursor.close().await?;

        self.ensure_worker(agent, snapshot.session_id).await?;
        self.workers
            .get_mut(&agent)
            .expect("worker exists")
            .lifecycle
            .push_back(LifecycleHook::AgentCreate);
        Ok(())
    }

    async fn schedule_session_update(&mut self, session: &Session) -> Result<(), RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::Unavailable);
        }

        let mut cursor = self.db.read().await?;
        let agents = cursor.list_agents(session.id).await?;
        cursor.close().await?;

        for agent in agents {
            self.ensure_worker(agent.id, session.id).await?;
            let worker = self.workers.get_mut(&agent.id).expect("worker exists");
            let _ = worker.controls.session_updated(session.clone());
            worker
                .lifecycle
                .push_back(LifecycleHook::SessionUpdate(session.clone()));
        }
        Ok(())
    }

    async fn ensure_worker(
        &mut self,
        agent: AgentId,
        session: SessionId,
    ) -> Result<(), RuntimeError> {
        if self.workers.contains_key(&agent) {
            return Ok(());
        }

        let became_busy = !self.is_busy(session);
        self.spawn_worker(agent, session, VecDeque::new(), VecDeque::new())
            .await?;
        if became_busy {
            self.publish_session_activity(session, true).await;
        }
        Ok(())
    }

    /// Installed components are cached per agent, so describing an agent that
    /// has not run yet installs what its first worker would use.
    async fn describe_components(
        &mut self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<psa::api::AgentComponents, RuntimeError> {
        self.sessions.get_agent(session, agent).await?;
        let components = match self.components.get(&agent) {
            Some(components) => Arc::clone(components),
            None => {
                let state = AgentManagerState::load_persisted(
                    self.db.clone(),
                    self.publication.clone(),
                    agent,
                )
                .await?;
                self.ensure_components(&state).await?
            }
        };
        Ok(components.lock().await.describe())
    }

    async fn ensure_components(
        &mut self,
        state: &AgentManagerState,
    ) -> Result<Arc<Mutex<InstalledComponents>>, RuntimeError> {
        let agent = state.agent();
        if let Some(components) = self.components.get(&agent.id) {
            return Ok(Arc::clone(components));
        }

        let parent = match agent.subagent_parent_id {
            Some(parent) => {
                let parent = self.sessions.get_agent(agent.session_id, parent).await?;
                Some(
                    self.sessions
                        .get_agent_definition(parent.definition_id)
                        .await?,
                )
            }
            None => None,
        };
        let needs_resources = self
            .sessions
            .components()
            .needs_session_components(state.definition(), parent.as_ref());
        let resources = needs_resources.then(|| self.ensure_session_resources(agent.session_id));

        let components = Arc::new(Mutex::new(
            state
                .install_components_with_session(
                    self.sessions.components().as_ref(),
                    resources.as_ref(),
                )
                .await?,
        ));
        self.components.insert(agent.id, Arc::clone(&components));
        Ok(components)
    }

    fn ensure_session_resources(&mut self, session: SessionId) -> SessionComponentsHandle {
        let resources = self.session_resources.entry(session).or_insert_with(|| {
            let resources = self.sessions.components().start_session(
                session,
                self.sessions.clone(),
                self.providers.clone(),
            );
            // A bundle may be installed after other session agents started.
            for (agent_id, run) in self.runs.iter().filter(|(_, run)| run.session == session) {
                resources.notify(AgentLifecycleEvent::Started {
                    session_id: session,
                    agent_id: *agent_id,
                    run_id: run.id.clone(),
                });
            }
            resources
        });
        resources.handle()
    }

    async fn spawn_worker(
        &mut self,
        agent: AgentId,
        session: SessionId,
        queue: VecDeque<QueuedItem>,
        lifecycle: VecDeque<LifecycleHook>,
    ) -> Result<(), RuntimeError> {
        let mut state =
            AgentManagerState::load_persisted(self.db.clone(), self.publication.clone(), agent)
                .await?;
        let components = self.ensure_components(&state).await?;
        state.attach_components(components).await;

        let generation = self.next_generation;
        self.next_generation += 1;

        let (controls, received) = actors::unbounded_channel();
        let controls = AgentClient::new(controls);
        let manager = self.commands.clone();
        let providers = self.providers.clone();
        let probes = self.probes.clone();
        let sessions = self.sessions.clone();

        let task = self.tasks.spawn(async move {
            let result = run_worker(
                state, generation, manager, received, providers, probes, sessions,
            )
            .await;
            (agent, generation, result)
        });

        self.workers.insert(
            agent,
            Worker {
                session,
                generation,
                task_id: task.id(),
                controls,
                queue,
                lifecycle,
                in_flight: None,
                stopping: false,
            },
        );

        Ok(())
    }

    async fn handle_exit(
        &mut self,
        agent: AgentId,
        generation: u64,
        result: Result<(), AgentError>,
    ) {
        if !self
            .workers
            .get(&agent)
            .is_some_and(|worker| worker.generation == generation)
        {
            return;
        }

        let worker = self.workers.remove(&agent).expect("worker exists");
        // A hard-aborted worker can leave an append task detached. Reconcile it
        // before any recovery reads or a replacement worker loads its state.
        self.publication.drain_appends(agent).await;
        if result.is_err() {
            self.finish_run(
                agent,
                AgentRunOutcome::Failed {
                    message: EXECUTION_FAILURE_MESSAGE.into(),
                },
            )
            .await;
        }
        if let Err(error) = result {
            if self.shutting_down {
                self.cleanup_failed = true;
            }
            if self
                .recover_failed_worker(agent, &worker, error)
                .await
                .is_err()
            {
                self.cleanup_failed = true;
            }
        }

        self.publication.discard_pending(agent).await;
        // Do not clear the signal during retirement: queued requests move to
        // the next generation without changing their steering availability.
        if !self.shutting_down && (!worker.queue.is_empty() || !worker.lifecycle.is_empty()) {
            if self
                .spawn_worker(agent, worker.session, worker.queue, worker.lifecycle)
                .await
                .is_ok()
            {
                return;
            }
            self.cleanup_failed = true;
        }

        self.refresh_steering(agent);
        if !self.is_busy(worker.session) {
            self.publish_session_activity(worker.session, false).await;
        }
    }

    async fn recover_failed_worker(
        &self,
        agent: AgentId,
        worker: &Worker,
        error: AgentError,
    ) -> Result<(), AgentError> {
        tracing::error!(agent = agent.0, %error, "agent worker failed");
        if let Some(request_id) = &worker.in_flight {
            self.publication
                .lock_state()
                .await
                .publish(StreamMessage::RequestFailed {
                    session_id: worker.session,
                    agent_id: agent,
                    request_id: request_id.clone(),
                    message: "worker exited; request disposition is unknown".into(),
                });
        }

        recover_agent(&self.db, &self.publication, agent).await?;
        let mut state =
            AgentManagerState::load_persisted(self.db.clone(), self.publication.clone(), agent)
                .await?;
        state
            .persist_events(&[NewEvent::new(EventType::Log(Log {
                error: true,
                message: EXECUTION_FAILURE_MESSAGE.into(),
            }))])
            .await?;

        Ok(())
    }
}

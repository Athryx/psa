//! A single global supervisor. Only active agents have worker tasks.

use super::{
    AgentError, AgentId, AgentState,
    agent_manager::{AgentClient, AgentManagerState, recover_agent, run_worker},
    publication::Publication,
    sessions::Sessions,
};
use crate::{
    db::{Database, DbError},
    probes::ProbeRegistry,
    providers::ProviderRegistry,
    sessions::{Session, SessionId},
};
use actors::Mailbox;
use psa_api::{
    AgentRequest, MessageDelivery, RequestAccepted, StreamMessage,
    events::{EventType, Log, NewEvent},
};
use std::collections::{HashMap, VecDeque};
use tokio::{
    sync::oneshot,
    task::{Id, JoinError, JoinHandle, JoinSet},
};

#[cfg(test)]
mod tests;

const MAX_PENDING: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("agent runtime failed: {0}")]
    Agent(#[from] AgentError),
    #[error("agent runtime is shutting down or unavailable")]
    Unavailable,
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
    pub handle: Sessions,
    task: JoinHandle<Result<(), RuntimeError>>,
}

impl Runtime {
    pub async fn start(
        db: Database,
        publication: Publication,
        providers: ProviderRegistry,
        probes: ProbeRegistry,
    ) -> Result<Self, RuntimeError> {
        let mut cursor = db.cursor().await?;
        let agents = cursor.list_all_agents().await?;
        cursor.rollback().await?;

        // Repair incomplete event streams left by a server crash.
        for agent in agents {
            recover_agent(&db, &publication, agent.id).await?;
        }

        let (commands, received) = actors::channel(256);
        let commands = ManagerClient::new(commands);
        let handle = Sessions::new(db.clone(), publication.clone(), commands.clone());
        let manager = SessionManager {
            db,
            publication,
            providers,
            probes,
            sessions: handle.clone(),
            commands,
            received,
            workers: HashMap::new(),
            tasks: JoinSet::new(),
            next_request_id: 1,
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
    pub id: String,
    pub delivery: MessageDelivery,
    pub content: Vec<llm_api::api::ContentBlock>,
}

pub(crate) enum BoundaryReply {
    Work(QueuedRequest),
    Continue,
    Stop,
}

#[actors::mailbox(client = ManagerClient)]
pub(crate) enum ManagerCommand {
    /// Check whether a session has any active workers.
    IsBusy {
        session: SessionId,
        #[reply]
        reply: oneshot::Sender<bool>,
    },
    /// Validate and enqueue an agent request, starting its worker if needed.
    Submit {
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
        request_id: String,
    },
    /// Forward updated session settings to its active workers.
    SessionUpdated { session: Session },
    /// Discard queued work, interrupt workers, and wait for their exit.
    Shutdown {
        #[reply]
        reply: oneshot::Sender<()>,
    },
}

struct Worker {
    session: SessionId,
    generation: u64,
    task_id: Id,
    controls: AgentClient,
    queue: VecDeque<QueuedRequest>,
    in_flight: Option<QueuedRequest>,
    stopping: bool,
}

type WorkerResult = (AgentId, u64, Result<(), AgentError>);

struct SessionManager {
    db: Database,
    publication: Publication,
    providers: ProviderRegistry,
    probes: ProbeRegistry,
    sessions: Sessions,
    commands: ManagerClient,
    received: Mailbox<ManagerCommand>,
    workers: HashMap<AgentId, Worker>,
    tasks: JoinSet<WorkerResult>,
    next_request_id: u64,
    next_generation: u64,
    shutting_down: bool,
    shutdown_reply: Option<oneshot::Sender<()>>,
    cleanup_failed: bool,
}

impl SessionManager {
    fn is_busy(&self, session: SessionId) -> bool {
        self.workers
            .values()
            .any(|worker| worker.session == session)
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
                    }
                }
                joined = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    if let Some(joined) = joined {
                        self.handle_join(joined).await;
                    }
                }
            }
        }

        if let Some(reply) = self.shutdown_reply {
            let _ = reply.send(());
        }
        if self.cleanup_failed {
            Err(RuntimeError::CleanupFailed)
        } else {
            Ok(())
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

    async fn handle_command(&mut self, command: ManagerCommand) {
        match command {
            ManagerCommand::IsBusy { session, reply } => {
                let _ = reply.send(self.is_busy(session));
            }
            ManagerCommand::Submit {
                session,
                agent,
                request,
                reply,
            } => {
                let result = self.submit(session, agent, request).await;
                let _ = reply.send(result);
            }
            ManagerCommand::Boundary {
                agent,
                generation,
                state,
                reply,
            } => {
                self.handle_boundary(agent, generation, state, reply).await;
            }
            ManagerCommand::Consumed {
                agent,
                generation,
                request_id,
            } => {
                if let Some(worker) = self
                    .workers
                    .get_mut(&agent)
                    .filter(|worker| worker.generation == generation)
                    && worker
                        .in_flight
                        .as_ref()
                        .is_some_and(|request| request.id == request_id)
                {
                    worker.in_flight = None;
                }
            }
            ManagerCommand::SessionUpdated { session } => {
                for worker in self
                    .workers
                    .values()
                    .filter(|worker| worker.session == session.id)
                {
                    let _ = worker.controls.session_updated(session.clone());
                }
            }
            ManagerCommand::Shutdown { reply } => {
                self.shutting_down = true;
                self.shutdown_reply = Some(reply);
                for worker in self.workers.values_mut() {
                    worker.queue.clear();
                    let _ = worker.controls.interrupt();
                }
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
        let Some(worker) = self
            .workers
            .get_mut(&agent)
            .filter(|worker| worker.generation == generation)
        else {
            return;
        };

        let mut rejected = Vec::new();
        worker.queue.retain(|request| {
            let is_valid =
                request.delivery != MessageDelivery::Resume || state == AgentState::Interrupted;
            if !is_valid {
                rejected.push(request.id.clone());
            }
            is_valid
        });

        let session = worker.session;
        let eligible = if self.shutting_down {
            None
        } else {
            worker
                .queue
                .iter()
                .position(|request| match request.delivery {
                    MessageDelivery::Queue => state != AgentState::Running,
                    MessageDelivery::Steer => true,
                    MessageDelivery::Resume => state == AgentState::Interrupted,
                })
        };
        let response = if let Some(index) = eligible {
            let request = worker.queue.remove(index).expect("queue index exists");
            worker.in_flight = Some(request.clone());
            BoundaryReply::Work(request)
        } else if state == AgentState::Running {
            BoundaryReply::Continue
        } else {
            worker.stopping = true;
            BoundaryReply::Stop
        };
        let _ = reply.send(response);

        for request_id in rejected {
            self.publication
                .lock()
                .await
                .publish(StreamMessage::RequestFailed {
                    session_id: session,
                    agent_id: agent,
                    request_id,
                    message: "resume requires an interrupted agent".into(),
                });
        }
    }

    async fn submit(
        &mut self,
        session: SessionId,
        agent: AgentId,
        request: AgentRequest,
    ) -> Result<RequestAccepted, RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::Unavailable);
        }

        let mut cursor = self.db.cursor().await?;
        let snapshot = cursor.require_agent_session(session, agent).await?;
        let context = cursor.load_context_window(agent).await?;
        cursor.rollback().await?;

        let id = format!(
            "{}-{}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
            self.next_request_id
        );
        self.next_request_id += 1;
        let AgentRequest::Message { delivery, content } = request else {
            if let Some(worker) = self.workers.get(&agent) {
                let _ = worker.controls.interrupt();
            }
            return Ok(RequestAccepted { request_id: id });
        };

        if delivery == MessageDelivery::Resume && snapshot.state != AgentState::Interrupted {
            return Err(RuntimeError::InvalidResume);
        }
        let model = context.iter().rev().find_map(|event| match &event.kind {
            EventType::Model(model) => Some(model),
            _ => None,
        });
        if !model.is_some_and(|model| self.providers.provider(model.provider).is_some()) {
            return Err(RuntimeError::MissingProvider);
        }
        if self
            .workers
            .get(&agent)
            .is_some_and(|worker| worker.queue.len() >= MAX_PENDING)
        {
            return Err(RuntimeError::Overloaded);
        }

        if !self.workers.contains_key(&agent) {
            let became_busy = !self.is_busy(session);
            self.spawn_worker(agent, session, VecDeque::new()).await?;
            if became_busy {
                self.publication
                    .lock()
                    .await
                    .publish(StreamMessage::SessionBusy {
                        session_id: session,
                        busy: true,
                    });
            }
        }
        self.workers
            .get_mut(&agent)
            .expect("worker exists")
            .queue
            .push_back(QueuedRequest {
                id: id.clone(),
                delivery,
                content,
            });

        Ok(RequestAccepted { request_id: id })
    }

    async fn spawn_worker(
        &mut self,
        agent: AgentId,
        session: SessionId,
        queue: VecDeque<QueuedRequest>,
    ) -> Result<(), RuntimeError> {
        let state =
            AgentManagerState::load(self.db.clone(), self.publication.clone(), agent).await?;

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
        if !self.shutting_down && !worker.queue.is_empty() {
            if self
                .spawn_worker(agent, worker.session, worker.queue)
                .await
                .is_ok()
            {
                return;
            }
            self.cleanup_failed = true;
        }

        if !self.is_busy(worker.session) {
            self.publication
                .lock()
                .await
                .publish(StreamMessage::SessionBusy {
                    session_id: worker.session,
                    busy: false,
                });
        }
    }

    async fn recover_failed_worker(
        &self,
        agent: AgentId,
        worker: &Worker,
        error: AgentError,
    ) -> Result<(), AgentError> {
        tracing::error!(agent = agent.0, %error, "agent worker failed");
        if let Some(request) = &worker.in_flight {
            self.publication
                .lock()
                .await
                .publish(StreamMessage::RequestFailed {
                    session_id: worker.session,
                    agent_id: agent,
                    request_id: request.id.clone(),
                    message: "worker exited; request disposition is unknown".into(),
                });
        }

        recover_agent(&self.db, &self.publication, agent).await?;
        let mut state =
            AgentManagerState::load(self.db.clone(), self.publication.clone(), agent).await?;
        state
            .persist_events(&[NewEvent::new(EventType::Log(Log {
                error: true,
                message: "Agent execution failed; see server logs for details.".into(),
            }))])
            .await?;

        Ok(())
    }
}

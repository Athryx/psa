//! Session-scoped bundle progress tracking. Agent metadata and steering belong to core.
use crate::{
    agents::{
        agent_context::AgentContext,
        lifecycle::{AgentLifecycleEvent, SessionLifecycleHook},
        runtime_handle::RuntimeHandle,
    },
    components::{
        bundles::CleanupError, cancellation::Cancellation,
        hooks::subagent_completion::SubagentCompletion,
    },
};
use actors::{Reply, UnboundedMailbox, UnboundedMailboxHandle};
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use psa::api::{AgentId, SessionId, probes::Uuid};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::task::JoinHandle;

type ResultValue = Result<Value, String>;

/// How long a message recipient counts as about to run before it starts.
const STARTING_GRACE: Duration = Duration::from_secs(2);

#[cfg(test)]
#[path = "../../../tests/components/bundles/subagents/coordinator/tests.rs"]
mod tests;

#[derive(Clone)]
pub struct SubagentCoordinator {
    session: SessionId,
    sessions: RuntimeHandle,
    sender: UnboundedMailboxHandle<Command>,
}

impl SessionLifecycleHook for SubagentCoordinator {
    fn notify(&self, event: AgentLifecycleEvent) {
        let _ = self.sender.send(Command::Lifecycle(event));
    }
}

struct AbortTask(JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct CoordinatorRuntime {
    handle: SubagentCoordinator,
    task: Option<AbortTask>,
}

impl CoordinatorRuntime {
    pub(crate) fn start(session: SessionId, sessions: RuntimeHandle) -> Self {
        let (sender, mailbox) = actors::unbounded_channel();
        let handle = SubagentCoordinator {
            session,
            sessions: sessions.clone(),
            sender,
        };
        let task = tokio::spawn(State::new(session, sessions).run(mailbox));
        Self {
            handle,
            task: Some(AbortTask(task)),
        }
    }

    pub(crate) fn handle(&self) -> SubagentCoordinator {
        self.handle.clone()
    }

    pub(crate) async fn cleanup(&mut self) -> Result<(), CleanupError> {
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        let _ = self.handle.sender.send(Command::Shutdown);
        match tokio::time::timeout(Duration::from_secs(2), &mut task.0).await {
            Ok(result) => result.map_err(|error| CleanupError::new(error.to_string())),
            Err(_) => {
                task.0.abort();
                Err(CleanupError::new("Subagents shutdown timed out"))
            }
        }
    }
}

impl Drop for CoordinatorRuntime {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.0.abort();
        }
    }
}

enum Command {
    Lifecycle(AgentLifecycleEvent),
    BeginOperation(Uuid),
    EndOperation(Uuid),
    /// A message was submitted to this agent; it may not have started yet.
    Delivered(AgentId),
    Wait {
        agent: AgentId,
        token: Uuid,
        reply: Reply<ResultValue>,
    },
    EndWait {
        agent: AgentId,
        token: Uuid,
    },
    Shutdown,
}

struct Waiter {
    token: Uuid,
    reply: Reply<ResultValue>,
}

struct State {
    session: SessionId,
    sessions: RuntimeHandle,
    active: HashMap<AgentId, String>,
    /// Recipients of delivered messages that have not reported a run start.
    /// A recipient already running never starts again, so entries expire.
    starting: HashMap<AgentId, tokio::time::Instant>,
    pending: HashSet<Uuid>,
    waiters: HashMap<AgentId, Waiter>,
    jobs: FuturesUnordered<BoxFuture<'static, ()>>,
    deadlock_after: tokio::time::Instant,
}

impl State {
    fn new(session: SessionId, sessions: RuntimeHandle) -> Self {
        Self {
            session,
            sessions,
            active: HashMap::new(),
            starting: HashMap::new(),
            pending: HashSet::new(),
            waiters: HashMap::new(),
            jobs: FuturesUnordered::new(),
            deadlock_after: tokio::time::Instant::now(),
        }
    }

    async fn run(mut self, mut mailbox: UnboundedMailbox<Command>) {
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                command = mailbox.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(command) => self.handle_command(command),
                },
                Some(()) = self.jobs.next(), if !self.jobs.is_empty() => {},
                _ = tick.tick() => self.check_deadlock(),
            }
        }

        // Dropping jobs cancels RPCs; shutdown must never await a request back to
        // the manager that is waiting for this actor to exit.
        for (_, waiter) in self.waiters.drain() {
            let _ = waiter.reply.send(Err("Subagents session closed".into()));
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Lifecycle(event) => self.handle_lifecycle_event(event),
            Command::BeginOperation(token) => {
                self.pending.insert(token);
            }
            Command::EndOperation(token) => {
                self.pending.remove(&token);
            }
            Command::Delivered(agent) => {
                let expires = tokio::time::Instant::now() + STARTING_GRACE;
                self.starting.insert(agent, expires);
            }
            Command::Wait {
                agent,
                token,
                reply,
            } => {
                if self.waiters.contains_key(&agent) {
                    let _ = reply.send(Err("This agent is already waiting".into()));
                    return;
                }
                self.waiters.insert(agent, Waiter { token, reply });
                self.deadlock_after = tokio::time::Instant::now() + Duration::from_millis(100);
            }
            Command::EndWait { agent, token }
                if self
                    .waiters
                    .get(&agent)
                    .is_some_and(|waiter| waiter.token == token) =>
            {
                self.waiters.remove(&agent);
            }
            _ => {}
        }
    }

    fn handle_lifecycle_event(&mut self, event: AgentLifecycleEvent) {
        match &event {
            AgentLifecycleEvent::Started {
                session_id,
                agent_id,
                run_id,
            } if *session_id == self.session => {
                self.starting.remove(agent_id);
                self.active.insert(*agent_id, run_id.clone());
            }
            AgentLifecycleEvent::Finished {
                session_id,
                agent_id,
                run_id,
                ..
            } if *session_id == self.session && self.active.get(agent_id) == Some(run_id) => {
                self.active.remove(agent_id);

                let sessions = self.sessions.clone();
                self.jobs.push(Box::pin(async move {
                    SubagentCompletion.run(&sessions, event).await;
                }));
            }
            _ => {}
        }
    }

    fn check_deadlock(&mut self) {
        self.waiters.retain(|_, waiter| !waiter.reply.is_closed());
        let now = tokio::time::Instant::now();
        self.starting.retain(|_, expires| now < *expires);
        if !self.pending.is_empty()
            || !self.starting.is_empty()
            || !self.jobs.is_empty()
            || tokio::time::Instant::now() < self.deadlock_after
            || self
                .active
                .keys()
                .any(|agent| !self.waiters.contains_key(agent))
        {
            return;
        }
        for (_, waiter) in self.waiters.drain() {
            let _ = waiter.reply.send(Err(
                "Cannot wait: no other agent in this session is running, so no message can arrive"
                    .into(),
            ));
        }
    }
}

/// Registered before awaiting anything, and released even if a future is dropped.
pub(crate) struct OperationGuard {
    sender: UnboundedMailboxHandle<Command>,
    token: Uuid,
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::EndOperation(self.token));
    }
}
struct WaitGuard {
    sender: UnboundedMailboxHandle<Command>,
    agent: AgentId,
    token: Uuid,
}
impl Drop for WaitGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::EndWait {
            agent: self.agent,
            token: self.token,
        });
    }
}

impl SubagentCoordinator {
    pub(crate) fn begin_operation(&self) -> Result<OperationGuard, String> {
        let token = Uuid::new_v4();
        self.sender
            .send(Command::BeginOperation(token))
            .map_err(|error| error.to_string())?;
        Ok(OperationGuard {
            sender: self.sender.clone(),
            token,
        })
    }

    pub(crate) async fn load_child_templates(
        &self,
        ids: &[psa::api::AgentDefinitionId],
    ) -> Result<Vec<super::ChildTemplate>, String> {
        super::templates::load_child_templates(&self.sessions, ids).await
    }

    /// Record that `agent` was sent a message, so waits do not report a
    /// deadlock before it starts running. Call while the operation guard is held.
    pub(crate) fn note_delivery(&self, agent: AgentId) {
        let _ = self.sender.send(Command::Delivered(agent));
    }

    pub(crate) fn require_session(&self, session: SessionId) -> Result<(), String> {
        if session != self.session {
            return Err("Subagents session mismatch".into());
        }
        Ok(())
    }

    pub async fn wait(
        &self,
        context: &AgentContext<'_>,
        seconds: f64,
        mut cancellation: Cancellation,
    ) -> Result<String, String> {
        if !seconds.is_finite() || seconds <= 0.0 || seconds > 600.0 {
            return Err("timeout must be >0 and <=600 seconds".into());
        }
        let agent = context.agent().id;
        let token = Uuid::new_v4();
        let _guard = WaitGuard {
            sender: self.sender.clone(),
            agent,
            token,
        };
        let wait = self.sender.request(|reply| Command::Wait {
            agent,
            token,
            reply,
        });
        let (_signal, mut steering_cancel) = Cancellation::channel();
        let steering = self
            .sessions
            .wait_for_steering(self.session, agent, &mut steering_cancel);
        tokio::pin!(steering);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err("Message wait cancelled".into()),
            result = &mut steering => {
                result.map_err(|error| error.to_string())?;
                Ok("A message arrived; it follows this result.".to_owned())
            }
            result = wait => {
                // Steering queued concurrently with a deadlock check wins.
                tokio::select! {
                    biased;
                    result = &mut steering => {
                        result.map_err(|error| error.to_string())?;
                        Ok("A message arrived; it follows this result.".to_owned())
                    }
                    // Waiters are only ever released with an error.
                    _ = std::future::ready(()) => result
                        .map_err(|error| error.to_string())?
                        .map(|value| value.to_string()),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs_f64(seconds)) => {
                Ok(format!("No message arrived within {seconds}s."))
            }
        }
    }
}

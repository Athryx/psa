use crate::{
    agents::{
        Agent, AgentDefinition,
        agent_context::AgentContext,
        lifecycle::{AgentLifecycleEvent, AgentRunOutcome},
        sessions::Sessions,
    },
    components::{bundles::CleanupError, cancellation::Cancellation},
    prompts,
};
use actors::{Reply, UnboundedMailbox, UnboundedMailboxHandle};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use psa_api::{
    AgentDefinitionId, AgentId, SessionId,
    events::{Event, EventRange, EventType, Log, NewEvent},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::task::JoinHandle;

#[cfg(test)]
mod tests;

const CHECKPOINT: &str = "psa.subagents/v1:";
const MAX_RUNNING: usize = 8;
const MAX_DEPTH: usize = 4;
type ResultValue = Result<Value, String>;
type Response = Reply<ResultValue>;

#[derive(Clone, Serialize, Deserialize)]
struct Member {
    id: AgentId,
    name: String,
    parent: Option<AgentId>,
    group: AgentId,
    template: String,
    #[serde(default = "idle")]
    status: String,
    #[serde(default)]
    error: Option<String>,
}
fn idle() -> String {
    "idle".into()
}
#[derive(Clone, Serialize)]
struct Envelope {
    id: String,
    from: String,
    from_id: AgentId,
    message: String,
}

/// Cloneable mailbox only: ownership and shutdown belong to the session resource.
#[derive(Clone)]
pub struct SubagentCoordinator {
    sender: UnboundedMailboxHandle<Command>,
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
    pub(crate) fn start(session: SessionId, sessions: Sessions) -> Self {
        let (sender, mailbox) = actors::unbounded_channel();
        let handle = SubagentCoordinator { sender };
        let task = tokio::spawn(State::new(session, sessions).run(mailbox));
        Self {
            handle,
            task: Some(AbortTask(task)),
        }
    }
    pub(crate) fn handle(&self) -> SubagentCoordinator {
        self.handle.clone()
    }
    pub(crate) fn notify(&self, event: AgentLifecycleEvent) {
        let _ = self.handle.sender.send(Command::Lifecycle(event));
    }
    pub(crate) async fn cleanup(&mut self) -> Result<(), CleanupError> {
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = self.handle.sender.send(Command::Shutdown);
            (&mut task.0)
                .await
                .map_err(|error| CleanupError::new(error.to_string()))
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                task.0.abort();
                Err(CleanupError::new(
                    "Subagents shutdown timed out; coordinator aborted",
                ))
            }
        }
    }
}
impl Drop for CoordinatorRuntime {
    fn drop(&mut self) {
        self.task.take();
    }
}

enum Command {
    Ensure {
        agent: Agent,
        template: String,
        reply: Response,
    },
    Create {
        caller: AgentId,
        allowed: Vec<AgentDefinitionId>,
        name: String,
        task: String,
        template: Option<String>,
        cancellation: Cancellation,
        reply: Response,
    },
    Send {
        caller: AgentId,
        name: String,
        message: String,
        cancellation: Cancellation,
        reply: Response,
    },
    List {
        caller: AgentId,
        all: bool,
        reply: Response,
    },
    Identity {
        caller: AgentId,
        reply: Response,
    },
    Wait {
        caller: AgentId,
        token: WaitToken,
        reply: Response,
    },
    EndWait {
        caller: AgentId,
        token: WaitToken,
    },
    Checkpoint {
        reply: Reply<String>,
    },
    Lifecycle(AgentLifecycleEvent),
    Shutdown,
}
enum Job {
    Restored {
        members: Vec<Member>,
        names: HashSet<String>,
        result: Result<(), String>,
    },
    Created {
        caller: AgentId,
        name: String,
        task: String,
        cancellation: Cancellation,
        reply: Response,
        result: Result<(Agent, String), String>,
    },
    Started {
        id: AgentId,
        reply: Response,
        result: Result<(), String>,
    },
    Delivered {
        target: AgentId,
        envelope: Envelope,
        reply: Option<Response>,
        result: Result<(), String>,
    },
    Interrupted,
}

async fn load_workspace_checkpoints(
    sessions: &Sessions,
    session: SessionId,
) -> Result<(Vec<Member>, HashSet<String>), String> {
    let agents = sessions
        .list_agents(session)
        .await
        .map_err(|error| error.to_string())?;
    let mut members = Vec::new();
    let mut names = HashSet::new();

    for agent in &agents {
        names.insert(agent.name.clone());
        members.extend(load_member_checkpoint(sessions, session, agent.id).await?);
    }

    Ok((reconcile_checkpoint_members(members, &agents), names))
}

fn reconcile_checkpoint_members(members: Vec<Member>, agents: &[Agent]) -> Vec<Member> {
    let agents: HashMap<_, _> = agents.iter().map(|agent| (agent.id, agent)).collect();
    members
        .into_iter()
        .filter_map(|mut member| {
            let agent = agents.get(&member.id)?;
            match agent.state {
                psa_api::AgentState::Idle => {
                    member.status = idle();
                    member.error = None;
                }
                psa_api::AgentState::Running => {
                    member.status = "running".into();
                    member.error = None;
                }
                psa_api::AgentState::Interrupted => {
                    // A failed run persists as Interrupted. Preserve its checkpointed
                    // diagnostic; other interrupted records must not inherit stale errors.
                    if member.status != "error" {
                        member.status = "interrupted".into();
                        member.error = None;
                    }
                }
            }
            Some(member)
        })
        .collect()
}

async fn load_member_checkpoint(
    sessions: &Sessions,
    session: SessionId,
    agent: AgentId,
) -> Result<Vec<Member>, String> {
    let mut range = EventRange::default();
    let mut latest = Vec::new();

    loop {
        let events = sessions
            .load_agent_events(session, agent, range.clone())
            .await
            .map_err(|error| error.to_string())?;
        let Some(last_event) = events.last() else {
            return Ok(latest);
        };
        range.after = last_event.id;

        for event in events {
            if let Some(snapshot) = parse_checkpoint(&event, agent) {
                latest = snapshot;
            }
        }
    }
}

fn parse_checkpoint(event: &Event, agent: AgentId) -> Option<Vec<Member>> {
    // A fork inherits logs but not this workspace's identities.
    if event.source_agent_id != agent {
        return None;
    }
    let EventType::Log(log) = &event.kind else {
        return None;
    };
    let payload = log.message.strip_prefix(CHECKPOINT)?;
    serde_json::from_str(payload).ok()
}

struct FinishedRun {
    run_id: String,
    outcome: AgentRunOutcome,
}
impl FinishedRun {
    fn apply_outcome(&self, member: &mut Member) {
        match &self.outcome {
            AgentRunOutcome::Completed => {
                member.status = idle();
                member.error = None;
            }
            AgentRunOutcome::Interrupted => {
                member.status = "interrupted".into();
                member.error = None;
            }
            AgentRunOutcome::Failed { message } => {
                member.status = "error".into();
                member.error = Some(message.clone());
            }
        }
    }
}

type WaitToken = psa_api::probes::Uuid;
struct Waiter {
    token: WaitToken,
    reply: Response,
}

struct State {
    session: SessionId,
    sessions: Sessions,
    members: HashMap<AgentId, Member>,
    names: HashSet<String>,
    reserved: HashSet<String>,
    waiters: HashMap<AgentId, Waiter>,
    jobs: FuturesUnordered<BoxFuture<'static, Job>>,
    ready: bool,
    restoring: bool,
    ensures: Vec<(Agent, String, Response)>,
    finished: HashMap<AgentId, FinishedRun>,
    deliveries: HashMap<AgentId, usize>,
    active: HashMap<AgentId, String>,
    sequence: u64,
    deadlock_after: tokio::time::Instant,
}
impl State {
    fn new(session: SessionId, sessions: Sessions) -> Self {
        Self {
            session,
            sessions,
            members: HashMap::new(),
            names: HashSet::new(),
            reserved: HashSet::new(),
            waiters: HashMap::new(),
            jobs: FuturesUnordered::new(),
            ready: false,
            restoring: false,
            ensures: Vec::new(),
            finished: HashMap::new(),
            deliveries: HashMap::new(),
            active: HashMap::new(),
            sequence: 0,
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
                    Some(Command::Shutdown) | None => break,
                    Some(command) => self.handle_command(command),
                },
                Some(job) = self.jobs.next(), if !self.jobs.is_empty() => self.handle_job_result(job),
                _ = tick.tick(), if !self.waiters.is_empty() => self.check_deadlock(),
            }
        }

        // Dropping coordinator-owned futures cancels all pending RPCs. Never await
        // the session manager here: it may itself be waiting for this cleanup.
        self.jobs.clear();
        for (_, waiter) in self.waiters.drain() {
            let _ = waiter.reply.send(Err("Subagents workspace closed".into()));
        }
    }
    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Ensure {
                agent,
                template,
                reply,
            } => {
                if self.ready {
                    self.ensure_member(agent, template, reply);
                    return;
                }
                self.ensures.push((agent, template, reply));
                if self.restoring {
                    return;
                }
                self.restoring = true;
                let sessions = self.sessions.clone();
                let session = self.session;
                self.jobs.push(
                    async move {
                        match load_workspace_checkpoints(&sessions, session).await {
                            Ok((members, names)) => Job::Restored {
                                members,
                                names,
                                result: Ok(()),
                            },
                            Err(error) => Job::Restored {
                                members: Vec::new(),
                                names: HashSet::new(),
                                result: Err(error),
                            },
                        }
                    }
                    .boxed(),
                );
            }
            Command::Create {
                caller,
                allowed,
                name,
                task,
                template,
                cancellation,
                reply,
            } => {
                if !self.members.contains_key(&caller) {
                    let _ = reply.send(Err("Unknown caller".into()));
                    return;
                }
                if name.is_empty()
                    || name.len() > 80
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                {
                    let _ = reply.send(Err(
                        "Agent name must contain 1–80 letters, digits, underscores or hyphens"
                            .into(),
                    ));
                    return;
                }
                if task.trim().is_empty() {
                    let _ = reply.send(Err("task must be nonempty".into()));
                    return;
                }
                if self.names.contains(&name) || self.reserved.contains(&name) {
                    let _ = reply.send(Err(format!("Agent {name} already exists")));
                    return;
                }
                if let Err(error) = self.validate_creation_capacity(caller) {
                    let _ = reply.send(Err(error));
                    return;
                }

                self.reserved.insert(name.clone());
                let sessions = self.sessions.clone();
                let session = self.session;
                self.jobs.push(
                    async move {
                        let mut cancellation = cancellation;
                        let result = async {
                            let template = template.ok_or_else(|| {
                                "No default agent template configured".to_string()
                            })?;
                            let mut selected = None;
                            let mut seen = HashSet::new();
                            for id in allowed {
                                if !seen.insert(id) {
                                    continue;
                                }
                                let definition = sessions
                                    .get_agent_definition(id)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                if definition.name == template {
                                    if selected.is_some() {
                                        return Err(format!(
                                            "Ambiguous permitted agent template: {template}"
                                        ));
                                    }
                                    selected = Some(definition);
                                }
                            }
                            let definition = selected.ok_or_else(|| {
                                format!("Agent template not permitted: {template}")
                            })?;
                            if cancellation.is_cancelled() || reply.is_closed() {
                                return Err("Agent creation cancelled".into());
                            }
                            let agent = sessions
                                .create_agent(
                                    session,
                                    psa_api::CreateAgent {
                                        definition_id: definition.id,
                                        name: name.clone(),
                                        interactive: false,
                                    },
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok((agent, template))
                        }
                        .await;
                        // A committed record is always returned to the actor before startup;
                        // cancellation cannot lose a name/parent reservation after commit.
                        Job::Created {
                            caller,
                            name,
                            task,
                            cancellation,
                            reply,
                            result,
                        }
                    }
                    .boxed(),
                );
            }
            Command::Send {
                caller,
                name,
                message,
                cancellation,
                reply,
            } => {
                if message.trim().is_empty() {
                    let _ = reply.send(Err("message must be nonempty".into()));
                    return;
                }
                let result = self.resolve_target(caller, &name).and_then(|target| {
                    if target == caller {
                        Err("Self-messaging is not supported".into())
                    } else {
                        Ok(target)
                    }
                });
                match result {
                    Ok(target) => {
                        self.deliver(caller, target, message, Some(reply), Some(cancellation))
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Command::List { caller, all, reply } => {
                let result = self.members.get(&caller).ok_or_else(|| "Unknown caller".to_string()).map(|source| {
                    let mut members: Vec<_> = self.members.values().filter(|member| member.group == source.group && (all || member.parent == Some(caller))).collect();
                    members.sort_by(|a,b| a.name.cmp(&b.name));
                    Value::Array(members.into_iter().map(|member| {
                        let mut value = json!({"name": member.name, "parent": member.parent.and_then(|id| self.members.get(&id)).map(|m| &m.name), "template": member.template, "role": if member.parent.is_none() { "interactive" } else { "worker" }, "status": member.status});
                        if let Some(error) = &member.error { value["error"] = json!(error); }
                        value
                    }).collect())
                });
                let _ = reply.send(result);
            }
            Command::Identity { caller, reply } => {
                let identity = self.members.get(&caller).ok_or_else(|| "Unknown caller".to_string()).map(|member|json!({
                    "name":member.name, "agent_id":member.id,
                    "parent":member.parent.and_then(|parent|self.members.get(&parent)).map(|parent|&parent.name),
                    "parent_id":member.parent, "group_id":member.group,
                    "template":member.template, "role":if member.parent.is_none() { "interactive" } else { "worker" }
                }));
                let _ = reply.send(identity);
            }
            Command::Wait {
                caller,
                token,
                reply,
            } => {
                if self.waiters.contains_key(&caller) {
                    let _ = reply.send(Err("This agent is already waiting".into()));
                    return;
                }
                if let Some(member) = self.members.get_mut(&caller) {
                    member.status = "waiting".into();
                }
                self.waiters.insert(caller, Waiter { token, reply });
                self.deadlock_after = tokio::time::Instant::now() + Duration::from_millis(100);
            }
            Command::EndWait { caller, token } => {
                if !self
                    .waiters
                    .get(&caller)
                    .is_some_and(|waiter| waiter.token == token)
                {
                    return;
                }
                self.waiters.remove(&caller);
                if let Some(member) = self.members.get_mut(&caller) {
                    if member.status == "waiting" {
                        member.status = "running".into();
                    }
                }
            }
            Command::Checkpoint { reply } => {
                let records: Vec<_> = self.members.values().collect();
                let _ = reply.send(format!(
                    "{CHECKPOINT}{}",
                    serde_json::to_string(&records).unwrap()
                ));
            }
            Command::Lifecycle(event) => self.apply_lifecycle_event(event),
            Command::Shutdown => {}
        }
    }
    fn ensure_member(&mut self, agent: Agent, template: String, reply: Response) {
        if reply.is_closed() {
            return;
        }

        self.names.insert(agent.name.clone());
        let member = self.members.entry(agent.id).or_insert(Member {
            id: agent.id,
            name: agent.name,
            parent: None,
            group: agent.id,
            template,
            status: "running".into(),
            error: None,
        });
        if self.active.contains_key(&agent.id)
            && ["idle", "interrupted", "error"].contains(&member.status.as_str())
        {
            member.status = "running".into();
            member.error = None;
        }
        let _ = reply.send(Ok(Value::Null));
    }
    fn validate_worker_capacity(&self) -> Result<(), String> {
        let active = self
            .members
            .values()
            .filter(|member| {
                member.parent.is_some()
                    && ["starting", "running", "waiting"].contains(&member.status.as_str())
            })
            .count();
        if active + self.reserved.len() >= MAX_RUNNING {
            return Err(format!("Worker capacity reached ({MAX_RUNNING})"));
        }
        Ok(())
    }
    fn validate_creation_capacity(&self, caller: AgentId) -> Result<(), String> {
        let mut ancestor = caller;
        let mut depth = 0;
        while let Some(parent) = self.members.get(&ancestor).and_then(|member| member.parent) {
            depth += 1;
            if depth >= MAX_DEPTH {
                return Err("Maximum agent nesting depth reached".into());
            }
            ancestor = parent;
        }
        self.validate_worker_capacity()
    }
    fn resolve_target(&self, caller: AgentId, name: &str) -> Result<AgentId, String> {
        let source = self
            .members
            .get(&caller)
            .ok_or_else(|| "Unknown caller".to_string())?;
        self.members
            .values()
            .find(|member| member.name == name && member.group == source.group)
            .map(|member| member.id)
            .ok_or_else(|| format!("Unknown agent in your group: {name}"))
    }
    fn deliver(
        &mut self,
        caller: AgentId,
        target: AgentId,
        message: String,
        reply: Option<Response>,
        mut cancellation: Option<Cancellation>,
    ) {
        if cancellation
            .as_mut()
            .is_some_and(|token| token.is_cancelled())
        {
            if let Some(reply) = reply {
                let _ = reply.send(Err("Message send cancelled".into()));
            }
            return;
        }
        let target_member = &self.members[&target];
        let needs_slot = target_member.parent.is_some()
            && !["starting", "running", "waiting"].contains(&target_member.status.as_str());
        if needs_slot {
            if let Err(error) = self.validate_worker_capacity() {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(error));
                }
                return;
            }
            // Reserve before the RPC. Concurrent sends/creates see this worker
            // as starting and cannot race through the last available slot.
            self.members.get_mut(&target).unwrap().status = "starting".into();
        }
        self.sequence += 1;
        let envelope = Envelope {
            id: format!(
                "{}-{}-{}",
                self.session.0,
                time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
                self.sequence
            ),
            from: self.members[&caller].name.clone(),
            from_id: caller,
            message,
        };
        // Pi delivers bodies through steering and returns only acknowledgement
        // IDs/from in wait_message. Steering must be accepted before the waiter
        // resolves so the current logical run consumes it, rather than resuming
        // a second run after the wait result.
        *self.deliveries.entry(target).or_default() += 1;
        let text = format!(
            "Message from {} (agent {}), id {}:\n{}",
            envelope.from, caller.0, envelope.id, envelope.message
        );
        let sessions = self.sessions.clone();
        let session = self.session;
        self.jobs.push(
            async move {
                let result = async {
                    if let Some(mut cancellation) = cancellation {
                        if cancellation.is_cancelled() {
                            return Err("Message send cancelled".into());
                        }
                    }
                    sessions
                        .steer(session, target, prompts::text(text))
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(())
                }
                .await;
                Job::Delivered {
                    target,
                    envelope,
                    reply,
                    result,
                }
            }
            .boxed(),
        );
    }
    fn handle_job_result(&mut self, job: Job) {
        match job {
            Job::Restored {
                members,
                names,
                result,
            } => {
                if let Err(error) = result {
                    self.restoring = false;
                    for (_, _, reply) in self.ensures.drain(..) {
                        let _ = reply.send(Err(format!("Subagents recovery failed: {error}")));
                    }
                    return;
                }
                self.names.extend(names);
                for mut member in members {
                    if self.active.contains_key(&member.id) {
                        member.status = "running".into();
                        member.error = None;
                    } else if let Some(finished) = self.finished.get(&member.id) {
                        finished.apply_outcome(&mut member);
                    }
                    self.members.entry(member.id).or_insert(member);
                }
                self.ready = true;
                for (agent, template, reply) in std::mem::take(&mut self.ensures) {
                    self.ensure_member(agent, template, reply);
                }
            }
            Job::Created {
                caller,
                name,
                task,
                mut cancellation,
                reply,
                result,
            } => {
                self.reserved.remove(&name);
                let (agent, template) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let id = agent.id;
                let group = self.members[&caller].group;
                self.names.insert(name.clone());
                self.members.insert(
                    id,
                    Member {
                        id,
                        name,
                        parent: Some(caller),
                        group,
                        template,
                        status: "starting".into(),
                        error: None,
                    },
                );
                let sessions = self.sessions.clone();
                let session = self.session;
                let task = format!(
                    "Your agent name is {} (agent {}). Parent: {} (agent {}). Template: {}. You are a fresh child in the same PSA session, not a fork. Communicate only within your agent group.\n\n{}",
                    self.members[&id].name,
                    id.0,
                    self.members[&caller].name,
                    caller.0,
                    self.members[&id].template,
                    task
                );
                let cancelled = cancellation.is_cancelled() || reply.is_closed();
                self.jobs.push(async move {
                    let result = if cancelled {
                        Err("Agent creation cancelled after record creation; child retained as interrupted".into())
                    } else {
                        tokio::select! {
                            _ = cancellation.cancelled() => Err("Agent creation cancelled during startup; child retained as interrupted".into()),
                            result = sessions.steer(session,id,prompts::text(task)) => result.map(|_| ()).map_err(|e|e.to_string()),
                        }
                    };
                    if result.is_err() { let _ = sessions.interrupt(session,id).await; }
                    Job::Started { id, reply, result }
                }.boxed());
            }
            Job::Started { id, reply, result } => {
                if let Some(member) = self.members.get_mut(&id) {
                    match &result {
                        Ok(()) => {
                            if member.status == "starting" {
                                member.status = "running".into();
                            }
                        }
                        Err(error) => {
                            member.status = "interrupted".into();
                            member.error = Some(error.clone());
                        }
                    }
                }
                let _ = reply.send(result.map(|_| Value::Null));
            }
            Job::Delivered {
                target,
                envelope,
                reply,
                result,
            } => {
                let id = envelope.id.clone();
                if let Some(count) = self.deliveries.get_mut(&target) {
                    *count -= 1;
                    if *count == 0 {
                        self.deliveries.remove(&target);
                    }
                }
                if let Some(member) = self.members.get_mut(&target) {
                    if member.status == "starting" {
                        match &result {
                            Ok(()) => member.status = "running".into(),
                            Err(error) if !self.deliveries.contains_key(&target) => {
                                member.status = "error".into();
                                member.error = Some(error.clone());
                            }
                            Err(_) => {}
                        }
                    }
                }
                if result.is_ok() {
                    if let Some(waiter) = self.waiters.remove(&target) {
                        // Bodies belong only to steering. ACK metadata belongs
                        // only to the active wait, never a historical inbox.
                        let _ = waiter.reply.send(Ok(json!({"status":"received","messages":[{"id":envelope.id,"from":envelope.from}]})));
                        if let Some(member) = self.members.get_mut(&target) {
                            member.status = "running".into();
                        }
                    }
                }
                if let Some(reply) = reply {
                    let _ = reply.send(result.map(|_| json!({"id":id,"status":"queued"})));
                }
            }
            Job::Interrupted => {}
        }
    }
    fn apply_lifecycle_event(&mut self, event: AgentLifecycleEvent) {
        match event {
            AgentLifecycleEvent::Started {
                session_id,
                agent_id,
                run_id,
            } if session_id == self.session => {
                self.active.insert(agent_id, run_id);
                if let Some(member) = self.members.get_mut(&agent_id) {
                    member.status = "running".into();
                    member.error = None;
                }
            }
            AgentLifecycleEvent::Finished {
                session_id,
                agent_id,
                run_id,
                outcome,
                final_text,
            } if session_id == self.session => {
                if self
                    .finished
                    .get(&agent_id)
                    .is_some_and(|finished| finished.run_id == run_id)
                {
                    return;
                }
                if self
                    .active
                    .get(&agent_id)
                    .is_some_and(|active| active != &run_id)
                {
                    return;
                }
                if !self.active.contains_key(&agent_id) && self.finished.contains_key(&agent_id) {
                    return;
                }
                self.finished.insert(
                    agent_id,
                    FinishedRun {
                        run_id,
                        outcome: outcome.clone(),
                    },
                );
                self.active.remove(&agent_id);
                let Some(member) = self.members.get_mut(&agent_id) else {
                    return;
                };
                self.finished[&agent_id].apply_outcome(member);
                let failed = !matches!(outcome, AgentRunOutcome::Completed);
                let report = match outcome {
                    AgentRunOutcome::Completed => final_text
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| "Completed without a text answer.".into()),
                    AgentRunOutcome::Interrupted => "Agent interrupted".into(),
                    AgentRunOutcome::Failed { message } => {
                        format!("Agent failed: {message}")
                    }
                };
                let parent = member.parent;
                let report = if report.chars().count() > 30_000 {
                    format!(
                        "{}\n[Report truncated; inspect worker session for full output.]",
                        report.chars().take(30_000).collect::<String>()
                    )
                } else {
                    report
                };
                if let Some(parent) = parent.filter(|parent| {
                    self.members.get(parent).is_some_and(|member| {
                        !["interrupted", "error"].contains(&member.status.as_str())
                    })
                }) {
                    // Cascading interruption must not steer the stopped parent
                    // back into a new run. Explicit send_message may resume it.
                    self.deliver(agent_id, parent, report, None, None);
                }
                if failed {
                    let children: Vec<_> = self
                        .members
                        .values()
                        .filter(|m| {
                            m.parent == Some(agent_id)
                                && ["running", "starting", "waiting"].contains(&m.status.as_str())
                        })
                        .map(|m| m.id)
                        .collect();
                    for child in children {
                        let sessions = self.sessions.clone();
                        let session = self.session;
                        self.jobs.push(
                            async move {
                                let _ = sessions.interrupt(session, child).await;
                                Job::Interrupted
                            }
                            .boxed(),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    fn check_deadlock(&mut self) {
        self.waiters.retain(|id, waiter| {
            if !waiter.reply.is_closed() {
                return true;
            }
            if let Some(member) = self.members.get_mut(id) {
                if member.status == "waiting" {
                    member.status = "running".into();
                }
            }
            false
        });
        if !self.jobs.is_empty() || tokio::time::Instant::now() < self.deadlock_after {
            return;
        }
        let blocked: Vec<_> = self
            .waiters
            .keys()
            .copied()
            .filter(|id| {
                let Some(member) = self.members.get(id) else {
                    return true;
                };
                !self.members.values().any(|other| {
                    other.group == member.group
                        && ["running", "starting"].contains(&other.status.as_str())
                })
            })
            .collect();
        for id in blocked {
            if let Some(waiter) = self.waiters.remove(&id) {
                if let Some(member) = self.members.get_mut(&id) {
                    if member.status == "waiting" {
                        member.status = "running".into();
                    }
                }
                let _ = waiter.reply.send(Err(
                    "Cannot wait: nobody else in your agent group can make progress".into(),
                ));
            }
        }
    }
}

async fn wait_for_message_or_steering(
    acknowledgement: impl std::future::Future<Output = Result<ResultValue, actors::MailboxError>>,
    steering: impl std::future::Future<
        Output = Result<(), crate::agents::session_manager::RuntimeError>,
    >,
    timeout: Duration,
    mut cancellation: Cancellation,
) -> ResultValue {
    tokio::pin!(acknowledgement, steering);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err("Message wait cancelled".into()),
        result = &mut acknowledgement => result.map_err(|error| error.to_string())?,
        result = &mut steering => {
            result.map_err(|error| error.to_string())?;
            // Prefer an ACK already available, but never delay human steering
            // behind an unrelated or blocked group-delivery operation.
            tokio::select! {
                biased;
                result = &mut acknowledgement => result.map_err(|error| error.to_string())?,
                _ = std::future::ready(()) => Ok(json!({"status":"steering_available","messages":[]})),
            }
        },
        _ = tokio::time::sleep(timeout) => Ok(json!({"status":"timeout","messages":[]})),
    }
}

struct WaitGuard {
    sender: UnboundedMailboxHandle<Command>,
    caller: AgentId,
    token: WaitToken,
}
impl Drop for WaitGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::EndWait {
            caller: self.caller,
            token: self.token,
        });
    }
}
impl SubagentCoordinator {
    pub async fn ensure(&self, context: &AgentContext<'_>) -> Result<(), String> {
        self.sender
            .request(|reply| Command::Ensure {
                agent: context.agent().clone(),
                template: context.definition().name.clone(),
                reply,
            })
            .await
            .map_err(|e| e.to_string())??;
        Ok(())
    }
    pub async fn identity(&self, context: &AgentContext<'_>) -> Result<String, String> {
        self.ensure(context).await?;
        let identity = self
            .sender
            .request(|reply| Command::Identity {
                caller: context.agent().id,
                reply,
            })
            .await
            .map_err(|e| e.to_string())??;
        let mut permitted = Vec::new();
        for id in &context.definition().permitted_subagents {
            permitted.push(
                context
                    .sessions()
                    .get_agent_definition(*id)
                    .await
                    .map_err(|e| e.to_string())?
                    .name,
            );
        }
        Ok(format!(
            "<agent_identity>\nName: {}. Agent ID: {}. Session ID: {}.\nCoordinator identity and parent/group relationships: {}\nPermitted child templates (explicit agent_template required; no default configured): {}\nChildren run asynchronously. Final answers/errors are delivered to their parent. Idle children remain addressable and send_message resumes them. wait_message returns group messages or a steering_available notification; steering is then injected by the runtime.\n</agent_identity>",
            context.agent().name,
            context.agent().id.0,
            context.session().id.0,
            identity,
            serde_json::to_string(&permitted).unwrap()
        ))
    }
    pub async fn checkpoint(&self, context: &mut AgentContext<'_>) -> Result<(), String> {
        let message = self
            .sender
            .request(|reply| Command::Checkpoint { reply })
            .await
            .map_err(|e| e.to_string())?;
        context
            .save_events(&[NewEvent::new(EventType::Log(Log {
                error: false,
                message,
            }))])
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub async fn create(
        &self,
        caller: AgentId,
        definition: &AgentDefinition,
        name: String,
        task: String,
        template: Option<String>,
        cancellation: Cancellation,
    ) -> ResultValue {
        if definition.permitted_subagents.is_empty() {
            return Err("This agent may not create subagents".into());
        }
        self.sender
            .request(|reply| Command::Create {
                caller,
                allowed: definition.permitted_subagents.clone(),
                name,
                task,
                template,
                cancellation,
                reply,
            })
            .await
            .map_err(|e| e.to_string())?
    }
    pub async fn send(
        &self,
        caller: AgentId,
        name: String,
        message: String,
        cancellation: Cancellation,
    ) -> ResultValue {
        self.sender
            .request(|reply| Command::Send {
                caller,
                name,
                message,
                cancellation,
                reply,
            })
            .await
            .map_err(|e| e.to_string())?
    }
    pub async fn list(&self, caller: AgentId, all: bool) -> ResultValue {
        self.sender
            .request(|reply| Command::List { caller, all, reply })
            .await
            .map_err(|e| e.to_string())?
    }
    pub async fn wait(
        &self,
        context: &AgentContext<'_>,
        seconds: f64,
        cancellation: Cancellation,
    ) -> ResultValue {
        if !seconds.is_finite() || seconds <= 0.0 || seconds > 600.0 {
            return Err("timeout must be >0 and <=600 seconds".into());
        }
        let caller = context.agent().id;
        let token = WaitToken::new_v4();
        let _guard = WaitGuard {
            sender: self.sender.clone(),
            caller,
            token,
        };
        // Keep level-triggered steering available as an independent wakeup source.
        // It is intentionally non-consuming: the runtime still injects the context.
        let (_signal, mut steering_cancel) = Cancellation::channel();
        let steering = context.sessions().wait_for_steering(
            context.session().id,
            caller,
            &mut steering_cancel,
        );
        let wait = self.sender.request(|reply| Command::Wait {
            caller,
            token,
            reply,
        });
        wait_for_message_or_steering(
            wait,
            steering,
            Duration::from_secs_f64(seconds),
            cancellation,
        )
        .await
    }
}

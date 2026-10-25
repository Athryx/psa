use crate::{
    agents::agent_context::AgentContext, components::cancellation::Cancellation, db::DbError,
};
use actors::{Mailbox, MailboxHandle, Reply};
use psa_api::events::{Event, EventType, Log, NewEvent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};

const STATE_NAMESPACE: &str = "psa.goal.state";
const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Running,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalState {
    pub goal: Option<String>,
    pub status: GoalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub generation: u64,
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            goal: None,
            status: GoalStatus::Completed,
            reason: None,
            generation: 0,
        }
    }
}

impl GoalState {
    pub fn is_running(&self) -> bool {
        self.goal.is_some() && self.status == GoalStatus::Running
    }
}

#[derive(Serialize, Deserialize)]
struct StoredGoal {
    namespace: String,
    version: u32,
    state: GoalState,
}

/// Read the latest valid state on this agent's branch (including fork ancestry).
pub async fn load_goal_state(context: &AgentContext<'_>) -> Result<GoalState, DbError> {
    Ok(restore_state(&context.load_all_events().await?))
}

fn restore_state(events: &[Event]) -> GoalState {
    let mut state = GoalState::default();
    for event in events {
        let EventType::Log(log) = &event.kind else {
            continue;
        };
        if log.error {
            continue;
        }
        let Ok(stored) = serde_json::from_str::<StoredGoal>(&log.message) else {
            continue;
        };
        if stored.namespace != STATE_NAMESPACE || stored.version != STATE_VERSION {
            continue;
        }
        let mut candidate = stored.state;
        if candidate.status == GoalStatus::Completed {
            candidate.goal = None;
        } else if candidate
            .goal
            .as_deref()
            .is_none_or(|goal| trim_goal_text(goal).is_empty())
        {
            continue;
        }
        // Branch order, not generation, is authoritative after a fork.
        state = candidate;
    }
    state
}

fn state_event(state: &GoalState) -> NewEvent {
    NewEvent::new(EventType::Log(Log {
        error: false,
        message: serde_json::to_string(&StoredGoal {
            namespace: STATE_NAMESPACE.into(),
            version: STATE_VERSION,
            state: state.clone(),
        })
        .expect("goal state is serializable"),
    }))
}

/// JavaScript String.trim semantics, including BOM and excluding U+0085.
pub(crate) fn trim_goal_text(text: &str) -> &str {
    text.trim_matches(|ch| {
        matches!(ch,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

pub(crate) enum GoalOperation {
    Set(String),
    Status {
        status: GoalStatus,
        reason: Option<String>,
    },
}

struct Transition {
    state: Option<GoalState>,
    message: String,
}

fn apply_operation(state: &GoalState, operation: GoalOperation) -> Transition {
    let unchanged = |message: &str| Transition {
        state: None,
        message: message.into(),
    };
    let (goal, status, reason, message) = match operation {
        GoalOperation::Set(goal) => {
            let goal = trim_goal_text(&goal);
            if goal.is_empty() {
                return unchanged("A nonempty goal is required.");
            }
            (
                Some(goal.to_owned()),
                GoalStatus::Running,
                None,
                format!("Goal running: {goal}"),
            )
        }
        GoalOperation::Status { status, reason } => {
            let Some(goal) = &state.goal else {
                return unchanged("No current goal. Set a goal first.");
            };
            if status == GoalStatus::Running && state.status != GoalStatus::Blocked {
                return unchanged("Only a blocked goal can be resumed.");
            }
            let reason = reason
                .as_deref()
                .map(trim_goal_text)
                .filter(|reason| !reason.is_empty())
                .map(str::to_owned);
            let label = match status {
                GoalStatus::Running => "running",
                GoalStatus::Completed => "completed",
                GoalStatus::Blocked => "blocked",
            };
            if status != GoalStatus::Running && reason.is_none() {
                return unchanged(&format!(
                    "A reason explaining the evidence for {label} is required."
                ));
            }
            let message = if status == GoalStatus::Completed {
                format!("Goal completed and cleared: {goal}")
            } else {
                format!(
                    "Goal {label}: {goal}{}",
                    reason
                        .as_ref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                )
            };
            (
                (status != GoalStatus::Completed).then(|| goal.clone()),
                status,
                reason,
                message,
            )
        }
    };
    let Some(generation) = state.generation.checked_add(1) else {
        return unchanged("Goal generation limit reached.");
    };
    Transition {
        state: Some(GoalState {
            goal,
            status,
            reason,
            generation,
        }),
        message,
    }
}

enum Message {
    Hydrate {
        state: GoalState,
        reply: Reply<()>,
    },
    Apply {
        operation: GoalOperation,
        reply: Reply<Transition>,
    },
    Commit {
        state: GoalState,
        reply: Reply<()>,
    },
}

async fn run(mut mailbox: Mailbox<Message>) {
    let mut state = GoalState::default();
    while let Some(message) = mailbox.recv().await {
        match message {
            Message::Hydrate {
                state: restored,
                reply,
            }
            | Message::Commit {
                state: restored,
                reply,
            } => {
                state = restored;
                let _ = reply.send(());
            }
            Message::Apply { operation, reply } => {
                let _ = reply.send(apply_operation(&state, operation));
            }
        }
    }
}

struct Launch {
    mailbox: Option<Mailbox<Message>>,
    task: Option<JoinHandle<()>>,
}
struct Shared {
    sender: MailboxHandle<Message>,
    launch: Mutex<Launch>,
    stopped: AtomicBool,
    gate: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct GoalHandle {
    shared: Arc<Shared>,
}

pub(super) struct Runtime {
    handle: GoalHandle,
}
impl Runtime {
    pub fn new() -> Self {
        let (sender, mailbox) = actors::channel(8);
        Self {
            handle: GoalHandle {
                shared: Arc::new(Shared {
                    sender,
                    launch: Mutex::new(Launch {
                        mailbox: Some(mailbox),
                        task: None,
                    }),
                    stopped: AtomicBool::new(false),
                    gate: Arc::new(Semaphore::new(1)),
                }),
            },
        }
    }
    pub fn handle(&self) -> GoalHandle {
        self.handle.clone()
    }
    pub async fn shutdown(&mut self) {
        self.handle.shared.stopped.store(true, Ordering::SeqCst);
        self.handle.shared.gate.close();
        let task = {
            let mut launch = self.handle.shared.launch.lock().unwrap();
            launch.mailbox.take();
            launch.task.take()
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.handle.shared.stopped.store(true, Ordering::SeqCst);
        self.handle.shared.gate.close();
        let mut launch = self.handle.shared.launch.lock().unwrap();
        launch.mailbox.take();
        if let Some(task) = launch.task.take() {
            task.abort();
        }
    }
}

/// The semaphore lease spans trace hydration, pure actor logic, and context commit.
/// It is cancellation-safe and never holds a std Mutex over an await.
pub(crate) struct GoalLease {
    _permit: OwnedSemaphorePermit,
    sender: MailboxHandle<Message>,
    pub state: GoalState,
}

impl GoalHandle {
    pub(crate) async fn lease(
        &self,
        context: &AgentContext<'_>,
    ) -> Result<Option<GoalLease>, DbError> {
        let Ok(permit) = self.shared.gate.clone().acquire_owned().await else {
            return Ok(None);
        };
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Ok(None);
        }
        {
            let mut launch = self.shared.launch.lock().unwrap();
            if let Some(mailbox) = launch.mailbox.take() {
                launch.task = Some(tokio::spawn(run(mailbox)));
            }
        }
        // Refresh even for retained workers: a dropped persistence future can have
        // committed, and current invocation snapshots/branches are authoritative.
        let state = load_goal_state(context).await?;
        let sender = self.shared.sender.clone();
        if sender
            .request(|reply| Message::Hydrate {
                state: state.clone(),
                reply,
            })
            .await
            .is_err()
        {
            return Ok(None);
        }
        Ok(Some(GoalLease {
            _permit: permit,
            sender,
            state,
        }))
    }

    pub(crate) async fn execute(
        &self,
        operation: GoalOperation,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<String, String> {
        let execute = async {
            let lease = self
                .lease(context)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Goal runtime is unavailable".to_owned())?;
            let transition = lease
                .sender
                .request(|reply| Message::Apply { operation, reply })
                .await
                .map_err(|error| error.to_string())?;
            if let Some(state) = transition.state {
                context
                    .save_events(&[state_event(&state)])
                    .await
                    .map_err(|error| error.to_string())?;
                lease
                    .sender
                    .request(|reply| Message::Commit { state, reply })
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(transition.message)
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err("Goal operation interrupted".into()),
            result = execute => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn apply(state: &GoalState, op: GoalOperation) -> GoalState {
        apply_operation(state, op).state.unwrap()
    }
    fn status(status: GoalStatus, reason: Option<&str>) -> GoalOperation {
        GoalOperation::Status {
            status,
            reason: reason.map(str::to_owned),
        }
    }
    #[test]
    fn transitions_match_pi_validation_and_results() {
        let empty = GoalState::default();
        assert_eq!(
            apply_operation(&empty, GoalOperation::Set(" \u{feff}".into())).message,
            "A nonempty goal is required."
        );
        assert_eq!(
            apply_operation(&empty, status(GoalStatus::Completed, Some("done"))).message,
            "No current goal. Set a goal first."
        );
        let running = apply(&empty, GoalOperation::Set("  explicit goal  ".into()));
        assert_eq!(running.goal.as_deref(), Some("explicit goal"));
        assert_eq!(running.generation, 1);
        assert_eq!(
            apply_operation(&running, status(GoalStatus::Running, None)).message,
            "Only a blocked goal can be resumed."
        );
        for s in [GoalStatus::Completed, GoalStatus::Blocked] {
            assert!(
                apply_operation(&running, status(s, Some(" \u{feff}")))
                    .state
                    .is_none()
            );
        }
        let blocked = apply(
            &running,
            status(GoalStatus::Blocked, Some("  missing input  ")),
        );
        assert_eq!(blocked.reason.as_deref(), Some("missing input"));
        let resumed = apply(&blocked, status(GoalStatus::Running, None));
        assert!(resumed.is_running());
        assert_eq!(resumed.reason, None);
        let transition =
            apply_operation(&resumed, status(GoalStatus::Completed, Some("tests pass")));
        assert_eq!(
            transition.message,
            "Goal completed and cleared: explicit goal"
        );
        let completed = transition.state.unwrap();
        assert_eq!(completed.goal, None);
        assert_eq!(completed.generation, 4);
    }
    #[test]
    fn restoration_ignores_unknown_invalid_logs_and_uses_branch_order() {
        let first = apply(&GoalState::default(), GoalOperation::Set("first".into()));
        let mut second = apply(&first, GoalOperation::Set("fork".into()));
        second.generation = 1;
        let event = |new: NewEvent| Event {
            id: psa_api::events::EventId(1),
            source_agent_id: psa_api::AgentId(1),
            timestamp: new.timestamp,
            kind: new.kind,
        };
        let mut events = vec![event(state_event(&first)), event(state_event(&second))];
        events.push(event(NewEvent::new(EventType::Log(Log {
            error: false,
            message: r#"{"namespace":"psa.goal.state","version":2,"state":{}}"#.into(),
        }))));
        assert_eq!(restore_state(&events), second);
        second.status = GoalStatus::Completed;
        events.push(event(state_event(&second)));
        assert_eq!(restore_state(&events).goal, None);
    }
    #[tokio::test]
    async fn actor_proposes_without_mutation_and_commits_explicitly() {
        let (sender, mailbox) = actors::channel(8);
        let task = tokio::spawn(run(mailbox));
        let proposal = sender
            .request(|reply| Message::Apply {
                operation: GoalOperation::Set("evidence-backed goal".into()),
                reply,
            })
            .await
            .unwrap();
        // No context commit means no published state transition.
        let before_commit = sender
            .request(|reply| Message::Apply {
                operation: status(GoalStatus::Blocked, Some("missing prerequisite")),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(before_commit.message, "No current goal. Set a goal first.");
        let running = proposal.state.unwrap();
        sender
            .request(|reply| Message::Commit {
                state: running,
                reply,
            })
            .await
            .unwrap();
        let blocked = sender
            .request(|reply| Message::Apply {
                operation: status(GoalStatus::Blocked, Some("missing prerequisite")),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            blocked.message,
            "Goal blocked: evidence-backed goal (missing prerequisite)"
        );
        assert_eq!(blocked.state.unwrap().generation, 2);
        drop(sender);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn drop_aborts_live_actor_and_releases_waiters() {
        let runtime = Runtime::new();
        let handle = runtime.handle();
        let abort = {
            let mut launch = handle.shared.launch.lock().unwrap();
            let task = tokio::spawn(run(launch.mailbox.take().unwrap()));
            let abort = task.abort_handle();
            launch.task = Some(task);
            abort
        };
        let permit = handle.shared.gate.clone().acquire_owned().await.unwrap();
        drop(runtime);
        assert!(handle.shared.gate.clone().acquire_owned().await.is_err());
        drop(permit);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[tokio::test]
    async fn runtime_is_lazy_and_cleanup_closes_cloned_handles() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();
        assert!(handle.shared.launch.lock().unwrap().task.is_none());
        runtime.shutdown().await;
        runtime.shutdown().await;
        assert!(handle.shared.gate.clone().acquire_owned().await.is_err());
    }
}

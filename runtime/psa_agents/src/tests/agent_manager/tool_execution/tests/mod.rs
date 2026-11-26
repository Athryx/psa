use super::*;
use crate::{
    agent_manager::testing::{Fixture, finish},
    tools::{Tool, ToolId, ToolSystemPrompt, typed_tool_execute},
};
use psa_store::NewEvent;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};

mod codemode;
mod recovery;

#[derive(Debug)]
enum Notice {
    Enter(String, EventId, EventId, std::path::PathBuf),
    Saved(String, EventId),
    Rejected(String),
    Cancelled(String),
    Exit(String),
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    label: String,
    #[serde(default)]
    saves: usize,
    #[serde(default)]
    stop: bool,
    #[serde(default)]
    panic: bool,
    #[serde(default)]
    invalid_save: bool,
}

pub struct ExecutionProbe {
    pub id: ToolId,
    mode: ExecutionMode,
    notices: mpsc::UnboundedSender<Notice>,
    release: Arc<Semaphore>,
    ignore_cancellation: bool,
    grace: Duration,
    dropped: Arc<AtomicUsize>,
}

struct InvocationDrop(Arc<AtomicUsize>);

impl Drop for InvocationDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl ExecutionProbe {
    async fn execute_typed(
        &self,
        arguments: Args,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let _drop = InvocationDrop(self.dropped.clone());
        let label = arguments.label;
        let _ = self.notices.send(Notice::Enter(
            label.clone(),
            context.agent().next_event_id,
            context.projection().last_event_id,
            context.session().working_directory.clone(),
        ));
        if arguments.invalid_save {
            let batch = [
                EventBatchItem::Private(psa_store::PrivateEvent::CodeModeSnapshot(vec![7])),
                EventBatchItem::Public(NewEvent::new(EventType::Log(Log {
                    error: false,
                    message: "invalid-save-must-not-publish".into(),
                }))),
            ];
            assert!(matches!(
                context.save_event_batch(&batch).await,
                Err(AgentError::Database(DbError::InvalidInput(_)))
            ));
            let _ = self.notices.send(Notice::Rejected(label.clone()));
        }
        for index in 0..arguments.saves {
            let events = context
                .save_events(&[NewEvent::new(EventType::Log(Log {
                    error: false,
                    message: format!("{label}-{index}"),
                }))])
                .await
                .unwrap();
            assert_eq!(context.projection().last_event_id, events[0].id);
            assert_eq!(context.agent().next_event_id.0, events[0].id.0 + 1);
            let _ = self
                .notices
                .send(Notice::Saved(label.clone(), events[0].id));
        }
        if arguments.stop {
            context
                .save_events(&[status_event(AgentState::Idle)])
                .await
                .unwrap();
        }
        if arguments.panic {
            panic!("invocation panic");
        }
        tokio::select! {
            permit = self.release.acquire() => { permit.unwrap().forget(); }
            _ = cancellation.cancelled() => {
                let _ = self.notices.send(Notice::Cancelled(label.clone()));
                if self.ignore_cancellation { self.release.acquire().await.unwrap().forget(); }
                return ToolOutput::error(prompts::tool_interrupted());
            }
        }
        let _ = self.notices.send(Notice::Exit(label));
        ToolOutput::success(prompts::text("done"))
    }
}

impl Tool for ExecutionProbe {
    fn id(&self) -> ToolId {
        self.id
    }

    fn tool_definition(&self) -> api::ToolDefinition {
        let name = match self.id {
            ToolId::ListSessions => "parallel",
            ToolId::Read => "sequential",
            _ => "later",
        };
        api::ToolDefinition::for_type::<Args>(name, "Test controlled execution", Some(false))
    }
    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt::default()
    }
    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }
    fn cancellation_timeout(&self) -> Duration {
        self.grace
    }
    typed_tool_execute!(Args, execute_typed);
}

fn tool(
    id: ToolId,
    mode: ExecutionMode,
    notices: &mpsc::UnboundedSender<Notice>,
) -> (ExecutionProbe, Arc<Semaphore>) {
    let release = Arc::new(Semaphore::new(0));
    (
        ExecutionProbe {
            id,
            mode,
            notices: notices.clone(),
            release: release.clone(),
            ignore_cancellation: false,
            grace: Duration::from_secs(1),
            dropped: Arc::new(AtomicUsize::new(0)),
        },
        release,
    )
}

async fn notice(notices: &mut mpsc::UnboundedReceiver<Notice>) -> Notice {
    tokio::time::timeout(Duration::from_secs(5), notices.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn parallel_calls_overlap_with_stable_snapshots_and_call_id_results() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, release) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![Box::new(parallel) as Box<dyn crate::tools::DynTool>],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                ("b", "parallel", serde_json::json!({"label":"b"})),
            ],
        )
        .await;
    let initial = state.agent.next_event_id;
    let (controls, run) = fixture.run(state).await;
    for _ in 0..2 {
        match notice(&mut notices).await {
            Notice::Enter(_, next, last, cwd) => {
                assert_eq!(next, initial);
                assert_eq!(last.0 + 1, next.0);
                assert_eq!(cwd, std::path::Path::new("/work"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    release.add_permits(2);
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Running);
    let ids: Vec<_> = state
        .context
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"a") && ids.contains(&"b"));
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn sequential_calls_are_barriers_before_and_after() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, first) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let (sequential, middle) = tool(ToolId::Read, ExecutionMode::Sequential, &sender);
    let (later, last) = tool(ToolId::Edit, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![
                Box::new(parallel) as Box<dyn crate::tools::DynTool>,
                Box::new(sequential) as Box<dyn crate::tools::DynTool>,
                Box::new(later) as Box<dyn crate::tools::DynTool>,
            ],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                ("b", "sequential", serde_json::json!({"label":"b"})),
                ("c", "later", serde_json::json!({"label":"c"})),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    assert!(matches!(notice(&mut notices).await, Notice::Enter(label, ..) if label == "a"));
    assert!(notices.try_recv().is_err());
    first.add_permits(1);
    assert!(matches!(notice(&mut notices).await, Notice::Exit(label) if label == "a"));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(label, ..) if label == "b"));
    assert!(notices.try_recv().is_err());
    middle.add_permits(1);
    assert!(matches!(notice(&mut notices).await, Notice::Exit(label) if label == "b"));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(label, ..) if label == "c"));
    last.add_permits(1);
    finish(run).await;
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn one_control_consumer_cancels_every_active_call_and_skips_pending_calls() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, _) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let (sequential, _) = tool(ToolId::Read, ExecutionMode::Sequential, &sender);
    let state = fixture
        .state(
            vec![
                Box::new(parallel) as Box<dyn crate::tools::DynTool>,
                Box::new(sequential) as Box<dyn crate::tools::DynTool>,
            ],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                ("b", "parallel", serde_json::json!({"label":"b"})),
                ("c", "sequential", serde_json::json!({"label":"c"})),
            ],
        )
        .await;
    let mut session = state.session.clone();
    session.working_directory = "/updated".into();
    let (controls, run) = fixture.run(state).await;
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    controls
        .send(AgentManagerMessage::SessionUpdated { session })
        .unwrap();
    controls.send(AgentManagerMessage::Interrupt).unwrap();
    let mut cancelled = Vec::new();
    for _ in 0..2 {
        match notice(&mut notices).await {
            Notice::Cancelled(label) => cancelled.push(label),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(cancelled.contains(&"a".into()) && cancelled.contains(&"b".into()));
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Interrupted);
    assert_eq!(
        state.session.working_directory,
        std::path::Path::new("/updated")
    );
    assert!(notices.try_recv().is_err());
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn concurrent_saves_are_serialized_and_refresh_only_the_saving_invocation() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, release) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![Box::new(parallel) as Box<dyn crate::tools::DynTool>],
            &[
                ("a", "parallel", serde_json::json!({"label":"a", "saves":3})),
                ("b", "parallel", serde_json::json!({"label":"b", "saves":3})),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    let mut entered = 0;
    let mut saved = Vec::new();
    while saved.len() != 6 {
        match notice(&mut notices).await {
            Notice::Enter(..) => entered += 1,
            Notice::Saved(label, id) => {
                assert!(label == "a" || label == "b");
                saved.push(id.0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(entered, 2);
    saved.sort();
    assert!(saved.windows(2).all(|pair| pair[0] + 1 == pair[1]));
    release.add_permits(2);
    let state = finish(run).await;
    let reloaded = AgentManagerState::load_persisted(
        fixture.database.clone(),
        fixture.publication.clone(),
        state.agent.id,
    )
    .await
    .unwrap();
    assert_eq!(reloaded.agent.next_event_id, state.agent.next_event_id);
    assert_eq!(reloaded.context.last_event_id, state.context.last_event_id);
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn invalid_invocation_batch_replies_with_error_and_sibling_continues() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, release) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![Box::new(parallel) as Box<dyn crate::tools::DynTool>],
            &[
                (
                    "bad",
                    "parallel",
                    serde_json::json!({"label":"bad", "invalid_save":true}),
                ),
                (
                    "good",
                    "parallel",
                    serde_json::json!({"label":"good", "saves":1}),
                ),
            ],
        )
        .await;
    let mut stream = fixture.publication.lock().await.subscribe();
    let (controls, run) = fixture.run(state).await;
    let (mut rejected, mut saved) = (false, false);
    while !rejected || !saved {
        match notice(&mut notices).await {
            Notice::Enter(..) => {}
            Notice::Rejected(label) => {
                assert_eq!(label, "bad");
                rejected = true;
            }
            Notice::Saved(label, _) => {
                assert_eq!(label, "good");
                saved = true;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let mut committed = Vec::new();
    while let Ok(message) = stream.try_recv() {
        if let psa::api::StreamMessage::Committed { event, .. } = message {
            committed.push(event.event.kind);
        }
    }
    assert_eq!(committed.len(), 1);
    assert!(matches!(&committed[0], EventType::Log(log) if log.message == "good-0"));
    release.add_permits(2);
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Running);
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn a_non_running_save_cancels_peers_and_preserves_the_intentional_status() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, _) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![Box::new(parallel) as Box<dyn crate::tools::DynTool>],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                (
                    "b",
                    "parallel",
                    serde_json::json!({"label":"b", "stop":true}),
                ),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Idle);
    let mut cancelled = 0;
    while let Ok(notice) = notices.try_recv() {
        if matches!(notice, Notice::Cancelled(_)) {
            cancelled += 1;
        }
    }
    assert_eq!(cancelled, 2);
    drop(controls);
    fixture.close().await;
}

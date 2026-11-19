//! Worker-owned concurrent invocations. No invocation owns mutable worker state.

use super::*;
use crate::agents::agent_context::InvocationSnapshot;
use crate::components::{
    cancellation::Cancellation,
    tools::{ExecutionMode, Tool, ToolOutput},
};
use crate::db::DbError;
use crate::db::NewEvent;
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::collections::VecDeque;
use tokio::sync::watch;

pub(crate) struct SaveRequest {
    pub events: Vec<EventBatchItem>,
    pub reply: actors::Reply<Result<SavedEvents, AgentError>>,
}

pub(crate) struct SavedEvents {
    pub events: Vec<Event>,
    pub agent: Agent,
    pub projection: Arc<AgentProjection>,
}

type Invocation<'a> = BoxFuture<'a, (String, ToolOutput)>;

/// Validated work at a batch boundary, before invocation futures are installed.
struct PreparedCall {
    call_id: String,
    invocation: Result<(usize, serde_json::Value), ToolOutput>,
}

impl AgentManagerState {
    fn invocation_snapshot(&self) -> InvocationSnapshot {
        InvocationSnapshot {
            agent: self.agent.clone(),
            session: self.session.clone(),
            definition: self.definition.clone(),
            projection: Arc::new(self.context.clone()),
            tools: Arc::clone(&self.tools),
            codemode_functions: Arc::clone(&self.codemode_functions),
            database: self.db.clone(),
            saves: self.invocation_save_handle.clone(),
        }
    }

    fn saved_events(&self, events: Vec<Event>) -> SavedEvents {
        SavedEvents {
            events,
            agent: self.agent.clone(),
            projection: Arc::new(self.context.clone()),
        }
    }

    /// The queue belongs to the worker, not to a tool future: a dropped save
    /// waiter (including a panic or grace timeout) cannot retract an append.
    pub(super) async fn drain_invocation_saves(&mut self) -> Result<(), AgentError> {
        while let Ok(request) = self.invocation_saves.try_recv() {
            match self.persist_event_batch(request.events).await {
                Ok(events) => {
                    let _ = request.reply.send(Ok(self.saved_events(events)));
                }
                Err(error @ AgentError::Database(DbError::InvalidInput(_))) => {
                    let _ = request.reply.send(Err(error));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(super) async fn resolve_tools(
        &mut self,
        probe: &ProbeHandle,
        sessions: &RuntimeHandle,
        controls: &mut UnboundedMailbox<AgentManagerMessage>,
    ) -> Result<(), AgentError> {
        self.finish_persistence().await?;
        let mut calls: VecDeque<_> = self.outstanding_calls().into();
        let mut active: FuturesUnordered<Invocation<'_>> = FuturesUnordered::new();
        let mut completed: VecDeque<(String, ToolOutput)> = VecDeque::new();
        let mut save_reply = None;
        let (cancel, _) = watch::channel(false);
        let mut interrupted = drain_controls(self, controls);
        let mut controls_closed = controls.is_closed();

        loop {
            // Observe queued controls before crossing a batch barrier or
            // starting any additional invocation.
            interrupted |= drain_controls(self, controls);
            controls_closed |= controls.is_closed();
            if interrupted || self.agent.state != AgentState::Running {
                cancel.send_replace(true);
            }

            // Serialize all appends, but keep polling invocations and controls
            // while publication/database IO is in flight.
            if self.pending_persistence.is_none() {
                if let Ok(request) = self.invocation_saves.try_recv() {
                    self.start_persistence(request.events, &[]);
                    if self.pending_persistence.is_some() {
                        save_reply = Some(request.reply);
                    } else {
                        let _ = request.reply.send(Ok(self.saved_events(Vec::new())));
                    }
                    continue;
                }
                if let Some((call_id, output)) = completed.pop_front() {
                    let result = ToolResult {
                        delivery: output.delivery,
                        call_id,
                        content: output.content,
                        error: output.error,
                        metadata: Default::default(),
                    };
                    let events = std::iter::once(EventBatchItem::Public(NewEvent::new(
                        EventType::ToolResult(result),
                    )))
                    .chain(
                        output
                            .private_events
                            .into_iter()
                            .map(EventBatchItem::Private),
                    )
                    .collect();
                    self.start_persistence(events, &[]);
                }
            }

            if active.is_empty() && self.pending_persistence.is_none() && completed.is_empty() {
                // A sequential invocation is a barrier on both sides. All calls
                // in a parallel run receive snapshots from this same boundary.
                if calls.is_empty() {
                    break;
                }
                let snapshot = self.invocation_snapshot();
                for call in self.prepare_next_batch(&mut calls, interrupted || *cancel.borrow()) {
                    match call.invocation {
                        Err(output) => completed.push_back((call.call_id, output)),
                        Ok((index, arguments)) => active.push(
                            invoke(
                                snapshot.clone(),
                                index,
                                call.call_id,
                                arguments,
                                probe,
                                sessions,
                                cancel.subscribe(),
                            )
                            .boxed(),
                        ),
                    }
                }
                if !completed.is_empty() {
                    continue;
                }
            }

            tokio::select! {
                biased;
                message = controls.recv(), if !controls_closed => {
                    match message {
                        Some(AgentManagerMessage::SessionUpdated { session }) => self.session = session,
                        Some(AgentManagerMessage::Interrupt) => interrupted = true,
                        None => { controls_closed = true; interrupted = true; }
                    }
                }
                result = wait_for_persistence(&mut self.pending_persistence) => {
                    self.pending_persistence = None;
                    let result = result.map_err(AgentError::PersistenceTask)?;
                    match result {
                        Err(error @ AgentError::Database(DbError::InvalidInput(_))) if save_reply.is_some() => {
                            let _ = save_reply.take().expect("reply exists").send(Err(error));
                        }
                        result => {
                            let events = self.apply_persistence(result)?;
                            if let Some(reply) = save_reply.take() {
                                let _ = reply.send(Ok(self.saved_events(events)));
                            }
                        }
                    }
                }
                request = self.invocation_saves.recv(), if self.pending_persistence.is_none() => {
                    if let Some(request) = request {
                        self.start_persistence(request.events, &[]);
                        if self.pending_persistence.is_some() {
                            save_reply = Some(request.reply);
                        } else {
                            let _ = request.reply.send(Ok(self.saved_events(Vec::new())));
                        }
                    }
                }
                Some(output) = active.next(), if !active.is_empty() => completed.push_back(output),
            }
        }

        // Cancellation cleanup is complete before the final interrupt status.
        // An intentional non-running status from a tool is otherwise preserved.
        if interrupted && self.agent.state == AgentState::Running {
            self.persist_events(&[status_event(AgentState::Interrupted)])
                .await?;
        }
        Ok(())
    }

    fn prepare_next_batch(
        &self,
        calls: &mut VecDeque<api::ToolCall>,
        interrupted: bool,
    ) -> Vec<PreparedCall> {
        let mut batch = Vec::new();
        while let Some(call) = calls.front() {
            let invocation = tools::prepare_call(
                &self.tools,
                call,
                &self.context,
                !interrupted && self.agent.state == AgentState::Running,
            );
            let sequential = invocation.as_ref().is_ok_and(|(index, _)| {
                self.tools[*index].execution_mode() == ExecutionMode::Sequential
            });
            if sequential && !batch.is_empty() {
                break;
            }
            let call = calls.pop_front().expect("front exists");
            batch.push(PreparedCall {
                call_id: call.call_id,
                invocation,
            });
            if sequential {
                break;
            }
        }
        batch
    }
}

async fn wait_for_persistence(
    pending: &mut Option<PendingPersistence>,
) -> Result<PersistenceResult, tokio::task::JoinError> {
    match pending {
        Some(pending) => pending.await,
        None => std::future::pending().await,
    }
}

async fn invoke(
    snapshot: InvocationSnapshot,
    index: usize,
    call_id: String,
    arguments: serde_json::Value,
    probe: &ProbeHandle,
    sessions: &RuntimeHandle,
    mut cancelled: watch::Receiver<bool>,
) -> (String, ToolOutput) {
    let tools = Arc::clone(&snapshot.tools);
    let tool = &tools[index];
    let mut context = AgentContext::invocation(snapshot, probe, sessions);
    let (sender, cancellation) = Cancellation::channel();
    let future = tool.execute(arguments, &mut context, cancellation);
    tokio::pin!(future);
    let result = tokio::select! {
        biased;
        _ = async {
            if !*cancelled.borrow() { let _ = cancelled.changed().await; }
        } => {
            let _ = sender.send(());
            tokio::time::timeout(tool.cancellation_timeout(), &mut future).await.ok()
        }
        output = &mut future => Some(output),
    };
    (
        call_id,
        result.unwrap_or_else(|| ToolOutput::error(prompts::tool_timed_out())),
    )
}

#[cfg(test)]
pub(crate) mod tests;

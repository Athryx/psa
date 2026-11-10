//! One worker per active agent; pending requests belong to the global supervisor.

use super::{
    Agent, AgentError, AgentId,
    agent_context::AgentContext,
    agent_projection::AgentProjection,
    publication::{Publication, status_event},
    session_manager::{BoundaryReply, LifecycleHook, ManagerClient},
    sessions::Sessions,
};
use crate::{
    components::{
        bundles::InstalledComponents,
        cancellation::wait_for_interrupt,
        codemode_functions::CodeModeFunctionKind,
        hooks::{CommandOutcome, run_command_hooks, run_hooks},
        tools::{self, ToolKind, failed_tool_result},
    },
    db::{AddEventResult, Database, EventBatchItem},
    probes::{ProbeHandle, ProbeRegistry},
    prompts,
    providers::ProviderRegistry,
    sessions::Session,
};
use actors::UnboundedMailbox;
use futures_util::FutureExt;
use llm_api::api::{
    self, ApiProvider, HistoryItem, HistoryItemKind, ModelResponse, ModelTurnStream, Outcome,
    RequestConfig,
};
use psa_api::events::*;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Instant};
use tokio::{sync::Mutex, task::JoinHandle};

pub(crate) mod tool_execution;

type PersistenceResult = Result<(AddEventResult, AgentProjection), AgentError>;
type PendingPersistence = JoinHandle<PersistenceResult>;

pub use psa_api::{AgentState, MessageDelivery};

#[actors::mailbox(client = AgentClient, unbounded)]
pub enum AgentManagerMessage {
    /// Interrupt the agent's current inference or tool execution.
    Interrupt,
    /// Replace the worker's cached session settings.
    SessionUpdated { session: Session },
}

pub struct AgentManagerState {
    db: Database,
    agent: Agent,
    context: AgentProjection,
    session: Session,
    definition: psa_api::AgentDefinition,
    tools: Arc<[ToolKind]>,
    codemode_functions: Arc<[CodeModeFunctionKind]>,
    components: Option<Arc<Mutex<InstalledComponents>>>,
    publication: Publication,
    pending_persistence: Option<PendingPersistence>,
    invocation_save_handle: actors::UnboundedMailboxHandle<tool_execution::SaveRequest>,
    invocation_saves: UnboundedMailbox<tool_execution::SaveRequest>,
}

impl AgentManagerState {
    pub fn database(&self) -> &Database {
        &self.db
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn context(&self) -> &AgentProjection {
        &self.context
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn definition(&self) -> &psa_api::AgentDefinition {
        &self.definition
    }

    pub fn tools(&self) -> &[ToolKind] {
        &self.tools
    }

    pub fn codemode_functions(&self) -> &[CodeModeFunctionKind] {
        &self.codemode_functions
    }

    pub(crate) fn update_session(&mut self, session: Session) {
        self.session = session;
    }

    pub async fn load(
        db: Database,
        publication: Publication,
        id: AgentId,
    ) -> Result<Self, AgentError> {
        let mut state = Self::load_persisted(db, publication, id).await?;
        let components = state.install_components().await?;
        state
            .attach_components(Arc::new(Mutex::new(components)))
            .await;
        Ok(state)
    }

    pub(crate) async fn install_components(&self) -> Result<InstalledComponents, AgentError> {
        self.install_components_with_session(None).await
    }

    pub(crate) async fn install_components_with_session(
        &self,
        resources: Option<&crate::components::bundles::session::SessionBundleHandle>,
    ) -> Result<InstalledComponents, AgentError> {
        let components = InstalledComponents::install_with_session(
            &self.definition,
            &self.agent,
            &self.session,
            resources,
        )
        .await?;
        #[cfg(test)]
        let components = {
            use super::session_manager::lifecycle_tests::RecordLifecycle;
            use crate::components::hooks::HookKind;
            let mut components = components;
            components
                .hooks
                .agent_create
                .push(HookKind::RecordLifecycle(RecordLifecycle {
                    name: "create",
                }));
            components
                .hooks
                .session_update
                .push(HookKind::RecordLifecycle(RecordLifecycle {
                    name: "update",
                }));
            components
        };
        Ok(components)
    }

    pub(crate) async fn attach_components(&mut self, components: Arc<Mutex<InstalledComponents>>) {
        {
            let installed = components.lock().await;
            self.tools = Arc::clone(&installed.tools);
            self.codemode_functions = Arc::clone(&installed.codemode_functions);
        }
        self.components = Some(components);
    }

    /// Load event state for recovery without starting agent-runtime resources.
    pub(crate) async fn load_persisted(
        db: Database,
        publication: Publication,
        id: AgentId,
    ) -> Result<Self, AgentError> {
        let mut cursor = db.cursor().await?;
        let agent = cursor.get_agent(id).await?;
        let session = cursor.get_session(agent.session_id).await?;
        let definition = cursor.get_agent_definition(agent.definition_id).await?;
        let context = AgentProjection::replay(&cursor.load_all_events(id).await?)?;
        cursor.rollback().await?;
        let (invocation_save_handle, invocation_saves) = actors::unbounded_channel();

        Ok(Self {
            db,
            publication,
            agent,
            session,
            definition,
            tools: Arc::default(),
            codemode_functions: Arc::default(),
            components: None,
            context,
            pending_persistence: None,
            invocation_save_handle,
            invocation_saves,
        })
    }

    pub async fn persist_events(&mut self, events: &[NewEvent]) -> Result<Vec<Event>, AgentError> {
        self.persist_promoted_events(events, &[]).await
    }

    async fn persist_promoted_events(
        &mut self,
        events: &[NewEvent],
        promotions: &[(usize, String)],
    ) -> Result<Vec<Event>, AgentError> {
        let batch: Vec<_> = events.iter().cloned().map(EventBatchItem::Public).collect();
        self.persist_event_batch_with_promotions(batch, promotions)
            .await
    }

    pub(crate) async fn persist_event_batch(
        &mut self,
        events: Vec<EventBatchItem>,
    ) -> Result<Vec<Event>, AgentError> {
        self.persist_event_batch_with_promotions(events, &[]).await
    }

    async fn persist_event_batch_with_promotions(
        &mut self,
        events: Vec<EventBatchItem>,
        promotions: &[(usize, String)],
    ) -> Result<Vec<Event>, AgentError> {
        self.finish_persistence().await?;
        self.start_persistence(events, promotions);
        self.finish_persistence().await
    }

    /// Start an append without borrowing the worker while the commit runs.
    fn start_persistence(&mut self, events: Vec<EventBatchItem>, promotions: &[(usize, String)]) {
        assert!(self.pending_persistence.is_none());
        let (events, promotions) = build_persistence_batch(events, promotions, self.agent.state);
        if events.is_empty() {
            return;
        }

        // A tool/hook may be dropped after its cancellation grace period. Track
        // the spawned append independently of the worker so supervisor recovery
        // waits for it even if the worker is aborted. During normal operation,
        // reconcile memory before subsequent work or a tool response.
        let db = self.db.clone();
        let publication = self.publication.clone();
        let agent = self.agent.id;
        let context = self.context.clone();
        let finished = db.register_commit(agent);
        self.pending_persistence = Some(tokio::spawn(async move {
            // Dropping this sender also signals completion on panic or task cancellation.
            let _finished = finished;
            publication
                .append(&db, agent, &events, &promotions, &context)
                .await
        }));
    }

    pub(crate) async fn finish_persistence(&mut self) -> Result<Vec<Event>, AgentError> {
        let Some(pending) = self.pending_persistence.as_mut() else {
            return Ok(Vec::new());
        };

        let result = pending.await;
        self.pending_persistence = None;
        let result = result.map_err(AgentError::PersistenceTask)?;
        self.apply_persistence(result)
    }

    fn apply_persistence(&mut self, result: PersistenceResult) -> Result<Vec<Event>, AgentError> {
        let (result, context) = result?;
        self.context = context;
        if let Some(state) = result.new_state {
            self.agent.state = state;
        }
        if let Some(usage) = result.new_usage {
            self.agent.usage = usage;
        }
        self.agent.next_event_id.0 += result.events.len() as u64;

        Ok(result.events)
    }

    async fn interrupt(&mut self) -> Result<(), AgentError> {
        if self.agent.state != AgentState::Running && self.outstanding_calls().is_empty() {
            return Ok(());
        }

        for call in self.outstanding_calls() {
            let result = failed_tool_result(call.call_id, prompts::tool_not_run());
            self.persist_events(&[NewEvent::new(EventType::ToolResult(result))])
                .await?;
        }

        if self.agent.state != AgentState::Interrupted {
            self.persist_events(&[status_event(AgentState::Interrupted)])
                .await?;
        }

        Ok(())
    }

    fn outstanding_calls(&self) -> Vec<api::ToolCall> {
        let mut calls = Vec::new();
        for item in self.context.items() {
            match &item.kind {
                HistoryItemKind::ToolCall(call) => calls.push(call.clone()),
                HistoryItemKind::ToolResult(result) => {
                    calls.retain(|call| call.call_id != result.call_id)
                }
                _ => {}
            }
        }

        calls
    }

    #[cfg(test)]
    pub(crate) async fn save_tool_output(
        &mut self,
        call_id: String,
        output: tools::ToolOutput,
    ) -> Result<(), AgentError> {
        let result = ToolResult {
            call_id,
            content: output.content,
            error: output.error,
            metadata: Default::default(),
        };

        self.persist_events(&[NewEvent::new(EventType::ToolResult(result))])
            .await?;
        Ok(())
    }

    async fn infer(
        &mut self,
        providers: &ProviderRegistry,
        controls: &mut UnboundedMailbox<AgentManagerMessage>,
        invocation: &str,
    ) -> Result<(), AgentError> {
        let mut request = self.context.prepare_request(RequestConfig::default())?;
        let provider = providers
            .provider(request.history.config.model.provider)
            .ok_or_else(|| llm_api::Error::InvalidRequest {
                message: "provider is not configured".into(),
            })?;

        let started = Instant::now();
        let mut response = ModelResponse::default_for_model(request.history.config.clone());
        let generated = match providers.resolve_reasoning(&request.history.config).await {
            Err(error) => Some(Err(error)),
            Ok(reasoning) => {
                request.config.reasoning = reasoning;
                tokio::select! {
                    biased;
                    _ = wait_for_interrupt(&mut self.session, controls) => {
                        response.outcome = Outcome::Cancelled;
                        None
                    }
                    generated = provider.generate(&request.history, &request.config) => Some(generated),
                }
            }
        };
        match generated {
            Some(Ok(stream)) => {
                response = self.collect_response(stream, controls, invocation).await?;
            }
            Some(Err(error)) => response.outcome = Outcome::Failed(error),
            None => {}
        }

        let successful = matches!(
            response.outcome,
            Outcome::Completed { .. }
                | Outcome::Incomplete {
                    reason: api::StopReason::TokenLimit
                }
        );
        let id = self.agent.next_event_id.0;
        let mut events = vec![NewEvent::new(EventType::InferenceResultStart(
            InferenceResultStart {
                id,
                model_config: response.config,
                request_config: request.config,
            },
        ))];
        let mut promotions = Vec::new();
        for (index, item) in response.items.iter().enumerate() {
            promotions.push((events.len(), format!("{invocation}:{index}")));
            events.push(NewEvent::new(build_item_event(item)?));
        }
        events.push(NewEvent::new(EventType::InferenceResultEnd(
            InferenceResultEnd {
                id,
                outcome: response.outcome,
                usage: response.usage,
                request_time: started.elapsed(),
                response_id: response.response_id,
                provider_metadata: response.provider_metadata,
            },
        )));
        if !successful {
            events.push(status_event(AgentState::Interrupted));
        }

        self.persist_promoted_events(&events, &promotions).await?;
        self.publication.discard_pending(self.agent.id).await;
        Ok(())
    }

    async fn collect_response(
        &mut self,
        mut stream: impl ModelTurnStream,
        controls: &mut UnboundedMailbox<AgentManagerMessage>,
        invocation: &str,
    ) -> Result<ModelResponse, AgentError> {
        loop {
            let event = tokio::select! {
                biased;
                _ = wait_for_interrupt(&mut self.session, controls) => {
                    stream.close();
                    break;
                }
                event = stream.next_event() => event,
            };
            let Some(Ok(event)) = event else {
                break;
            };

            let index = match event {
                api::TurnEvent::ItemAdded { index }
                | api::TurnEvent::ItemUpdated { index, .. }
                | api::TurnEvent::ItemCompleted { index } => index,
                _ => continue,
            };
            let Some(item) = stream.current().items.get(index) else {
                continue;
            };

            self.publication
                .upsert(
                    self.agent.session_id,
                    self.agent.id,
                    format!("{invocation}:{index}"),
                    build_item_event(item)?,
                )
                .await;
        }

        Ok(stream.into_output())
    }
}

pub(crate) async fn recover_agent(
    db: &Database,
    publication: &Publication,
    id: AgentId,
) -> Result<(), AgentError> {
    let mut state = AgentManagerState::load_persisted(db.clone(), publication.clone(), id).await?;
    let events = build_recovery_events(&state.agent, &state.context)?;
    if !events.is_empty() {
        state.persist_events(&events).await?;
    }

    Ok(())
}

pub(crate) fn build_recovery_events(
    agent: &Agent,
    context: &AgentProjection,
) -> Result<Vec<NewEvent>, AgentError> {
    let mut events = Vec::new();
    let mut context = context.clone();
    if let Some(start) = context.open_inference_result() {
        events.push(NewEvent::new(EventType::InferenceResultEnd(
            InferenceResultEnd {
                id: start.id,
                outcome: Outcome::Cancelled,
                usage: Default::default(),
                request_time: Default::default(),
                response_id: None,
                provider_metadata: Default::default(),
            },
        )));
        context.apply_event(&Event {
            id: agent.next_event_id,
            source_agent_id: agent.id,
            timestamp: events[0].timestamp,
            kind: events[0].kind.clone(),
        })?;
    }
    let completed_result = agent.state == AgentState::Idle && context.has_completed_result();
    for item in context.items() {
        let HistoryItemKind::ToolCall(call) = &item.kind else {
            continue;
        };
        let has_result = context.items().iter().any(|item| {
            matches!(&item.kind, HistoryItemKind::ToolResult(result) if result.call_id == call.call_id)
        });
        if has_result {
            continue;
        }

        let result = if completed_result
            && context.result_producer_call_id() == Some(call.call_id.as_str())
        {
            // Result is the durable effect. A crash/fork can omit its later tool
            // acknowledgement; repair the first unresolved sequential producer,
            // not the artifact or any later calls that never ran.
            let output = crate::components::tools::RecordResult::recorded_output();
            ToolResult {
                call_id: call.call_id.clone(),
                content: output.content,
                error: output.error,
                metadata: Default::default(),
            }
        } else {
            failed_tool_result(
                call.call_id.clone(),
                if completed_result {
                    prompts::tool_not_run()
                } else {
                    prompts::tool_interrupted()
                },
            )
        };
        events.push(NewEvent::new(EventType::ToolResult(result)));
    }

    if !completed_result && (!events.is_empty() || agent.state == AgentState::Running) {
        events.push(status_event(AgentState::Interrupted));
    }

    Ok(events)
}

fn build_item_event(item: &HistoryItem) -> Result<EventType, AgentError> {
    Ok(match &item.kind {
        HistoryItemKind::Message(message) => EventType::Message(Message {
            source: None,
            description: None,
            message_type: MessageType::Agent,
            content: message.content.clone(),
            metadata: item.metadata.clone(),
        }),
        HistoryItemKind::Reasoning(reasoning) => EventType::Thinking(Thinking {
            contents: reasoning.clone(),
            metadata: item.metadata.clone(),
        }),
        HistoryItemKind::ToolCall(call) => EventType::ToolCall(ToolCall {
            call_id: call.call_id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            metadata: item.metadata.clone(),
        }),
        HistoryItemKind::ProviderItem(_) => EventType::ProviderItem(item.clone()),
        HistoryItemKind::ToolResult(_) => {
            return Err(llm_api::Error::Protocol {
                message: "provider returned a client tool result".into(),
            }
            .into());
        }
    })
}

/// Filter redundant statuses without detaching private events from their public anchor.
/// Promotion offsets count public events, not batch items.
fn build_persistence_batch(
    events: Vec<EventBatchItem>,
    promotions: &[(usize, String)],
    mut status: AgentState,
) -> (Vec<EventBatchItem>, Vec<(usize, String)>) {
    let mut filtered = Vec::new();
    let mut filtered_promotions = Vec::new();
    let mut public_index = 0;
    let mut filtered_public_count = 0;
    let mut events = events.into_iter().peekable();
    while let Some(item) = events.next() {
        let EventBatchItem::Public(event) = &item else {
            // Leading private items remain invalid rather than being silently dropped.
            filtered.push(item);
            continue;
        };
        let has_private = matches!(events.peek(), Some(EventBatchItem::Private(_)));
        let redundant = if let EventType::StatusChange(change) = &event.kind {
            let redundant = change.new_status == status;
            status = change.new_status;
            redundant
        } else {
            false
        };
        if !redundant || has_private {
            for (_, id) in promotions
                .iter()
                .filter(|(offset, _)| *offset == public_index)
            {
                filtered_promotions.push((filtered_public_count, id.clone()));
            }
            filtered.push(item);
            filtered_public_count += 1;
        }
        public_index += 1;
    }

    (filtered, filtered_promotions)
}

fn drain_controls(
    state: &mut AgentManagerState,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> bool {
    let mut interrupted = false;
    while let Ok(message) = controls.try_recv() {
        match message {
            AgentManagerMessage::Interrupt => interrupted = true,
            AgentManagerMessage::SessionUpdated { session } => state.session = session,
        }
    }
    interrupted || controls.is_closed()
}

pub(crate) async fn run_worker(
    mut state: AgentManagerState,
    generation: u64,
    manager: ManagerClient,
    mut controls: UnboundedMailbox<AgentManagerMessage>,
    providers: ProviderRegistry,
    probes: ProbeRegistry,
    sessions: Sessions,
) -> Result<(), AgentError> {
    let shared = Arc::clone(
        state
            .components
            .as_ref()
            .expect("worker components attached"),
    );
    let mut components = shared.lock().await;
    let hooks = &mut components.hooks;
    let outcome = AssertUnwindSafe(async {
        let mut invocation = 0u64;

        loop {
            if drain_controls(&mut state, &mut controls) {
                state.interrupt().await?;
            }

            match manager
                .boundary(state.agent.id, generation, state.agent.state)
                .await
                .map_err(|_| AgentError::SupervisorUnavailable)?
            {
                BoundaryReply::Stop => return Ok(()),
                BoundaryReply::Hook(invocation) => {
                    let hook = match invocation {
                        LifecycleHook::AgentCreate => &mut hooks.agent_create,
                        LifecycleHook::SessionUpdate(session) => {
                            state.update_session(session);
                            &mut hooks.session_update
                        }
                    };
                    let probe = probes.handle(state.session.probe_id).await.ok();
                    if run_hooks(
                        hook,
                        &mut AgentContext::with_optional_probe(
                            &mut state,
                            probe.as_ref(),
                            &sessions,
                        ),
                        &mut controls,
                    )
                    .await?
                    {
                        state.interrupt().await?;
                    }
                    continue;
                }
                BoundaryReply::Work(request) => {
                    state
                        .persist_events(&[
                            NewEvent::new(EventType::Message(Message {
                                source: request.source,
                                description: None,
                                message_type: request.message_type,
                                content: request.content,
                                metadata: Default::default(),
                            })),
                            status_event(AgentState::Running),
                        ])
                        .await?;
                    manager
                        .consumed(state.agent.id, generation, request.id)
                        .await
                        .map_err(|_| AgentError::SupervisorUnavailable)?;
                    continue;
                }
                BoundaryReply::Command(request) => {
                    // Recorded before execution, so its effects follow it in the transcript.
                    state
                        .persist_events(&[NewEvent::new(EventType::Command(
                            request.command.clone(),
                        ))])
                        .await?;
                    let probe = probes.handle(state.session.probe_id).await.ok();
                    let mut context =
                        AgentContext::with_optional_probe(&mut state, probe.as_ref(), &sessions);
                    let (outcome, interrupted) = run_command_hooks(
                        &mut hooks.command,
                        &mut context,
                        &mut controls,
                        &request.command,
                    )
                    .await?;
                    if outcome == CommandOutcome::Ignored {
                        state
                            .persist_events(&[NewEvent::new(EventType::Log(Log {
                                error: true,
                                message: "No installed hook handled this command.".into(),
                            }))])
                            .await?;
                    }
                    if interrupted {
                        state.interrupt().await?;
                    }
                    // Consumed only after execution, so a failed worker reports it.
                    manager
                        .consumed(state.agent.id, generation, request.id)
                        .await
                        .map_err(|_| AgentError::SupervisorUnavailable)?;
                    continue;
                }
                BoundaryReply::Continue => {}
            }

            if state.agent.state != AgentState::Running {
                continue;
            }
            if drain_controls(&mut state, &mut controls) {
                state.interrupt().await?;
                continue;
            }

            let Ok(probe) = probes.handle(state.session.probe_id).await else {
                state
                    .persist_events(&[NewEvent::new(EventType::StatusChange(StatusChange {
                        new_status: AgentState::Interrupted,
                        reason: Some("session probe is disconnected".into()),
                    }))])
                    .await?;
                state.interrupt().await?;
                continue;
            };

            if run_hooks(
                &mut hooks.before_request,
                &mut AgentContext::new(&mut state, &probe, &sessions),
                &mut controls,
            )
            .await?
            {
                state.interrupt().await?;
                continue;
            }

            if state.agent.state != AgentState::Running {
                state.interrupt().await?;
                continue;
            }

            invocation += 1;
            let invocation_id = format!(
                "{}:{generation}:{invocation}:{}",
                state.agent.id.0,
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            );
            state
                .infer(&providers, &mut controls, &invocation_id)
                .await?;

            if run_hooks(
                &mut hooks.after_request,
                &mut AgentContext::new(&mut state, &probe, &sessions),
                &mut controls,
            )
            .await?
            {
                state.interrupt().await?;
                continue;
            }

            state
                .resolve_tools(&probe, &sessions, &mut controls)
                .await?;
        }
    })
    .catch_unwind()
    .await;

    // Retain hooks and owners for the next worker generation. Only runtime
    // shutdown cleans resources; retirement and recovery release the borrow.
    drop(components);

    // Do not let supervisor recovery race a save detached by a panicking tool.
    let persistence = async {
        state.finish_persistence().await?;
        state.drain_invocation_saves().await
    }
    .await;
    if let Err(error) = &persistence {
        tracing::error!(agent = state.agent.id.0, %error, "worker persistence cleanup failed");
    }
    let result: Result<(), AgentError> = match outcome {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    result?;
    persistence?;
    Ok(())
}

#[cfg(test)]
mod bundle_tests;
#[cfg(test)]
mod tests;

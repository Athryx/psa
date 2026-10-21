//! One worker per active agent; pending requests belong to the global supervisor.

use super::{
    Agent, AgentError, AgentId,
    agent_context::AgentContext,
    publication::{Publication, status_event},
    session_manager::{BoundaryReply, ManagerClient},
};
use crate::{
    db::Database,
    hooks::{
        Hooks,
        cancellation::wait_for_interrupt,
        run_hook,
        tools::{self, failed_tool_result},
    },
    prompts,
    providers::ProviderRegistry,
    sessions::Session,
};
use actors::UnboundedMailbox;
use llm_api::api::{
    self, ApiProvider, HistoryItem, HistoryItemKind, ModelResponse, ModelTurnStream, Outcome,
    RequestConfig,
};
use psa_api::events::*;
use std::time::Instant;

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
    context: AgentContext,
    session: Session,
    publication: Publication,
}

impl AgentManagerState {
    pub fn database(&self) -> &Database {
        &self.db
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn context(&self) -> &AgentContext {
        &self.context
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) fn update_session(&mut self, session: Session) {
        self.session = session;
    }

    pub async fn load(
        db: Database,
        publication: Publication,
        id: AgentId,
    ) -> Result<Self, AgentError> {
        let mut cursor = db.cursor().await?;
        let agent = cursor.get_agent(id).await?;
        let session = cursor.get_session(agent.session_id).await?;
        let context = AgentContext::replay(&cursor.load_all_events(id).await?)?;
        cursor.rollback().await?;

        Ok(Self {
            db,
            publication,
            agent,
            session,
            context,
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
        let (result, context) = self
            .publication
            .append(&self.db, self.agent.id, events, promotions, &self.context)
            .await?;

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

        self.resolve_tools(true, None).await?;
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

    async fn resolve_tools(
        &mut self,
        mut interrupted: bool,
        mut controls: Option<&mut UnboundedMailbox<AgentManagerMessage>>,
    ) -> Result<(), AgentError> {
        for call in self.outstanding_calls() {
            if let Some(controls) = controls.as_deref_mut() {
                interrupted |= drain_controls(self, controls);
            }

            let result = tools::execute(
                call,
                &self.context,
                &mut self.session,
                &mut interrupted,
                controls.as_deref_mut(),
            )
            .await;
            self.persist_events(&[NewEvent::new(EventType::ToolResult(result))])
                .await?;
        }

        if interrupted && self.agent.state == AgentState::Running {
            self.persist_events(&[status_event(AgentState::Interrupted)])
                .await?;
        }

        Ok(())
    }

    async fn infer(
        &mut self,
        providers: &ProviderRegistry,
        controls: &mut UnboundedMailbox<AgentManagerMessage>,
        invocation: &str,
    ) -> Result<(), AgentError> {
        let request = self.context.prepare_request(RequestConfig::default())?;
        let provider = providers
            .provider(request.history.config.provider)
            .ok_or_else(|| llm_api::Error::InvalidRequest {
                message: "provider is not configured".into(),
            })?;

        let started = Instant::now();
        let mut response = ModelResponse::default_for_model(request.history.config.clone());
        let generated = tokio::select! {
            biased;
            _ = wait_for_interrupt(&mut self.session, controls) => {
                response.outcome = Outcome::Cancelled;
                None
            }
            generated = provider.generate(&request.history, &request.config) => Some(generated),
        };
        match generated {
            Some(Ok(stream)) => {
                response = self.collect_response(stream, controls, invocation).await?;
            }
            Some(Err(error)) => response.outcome = Outcome::Failed(error),
            None => {}
        }

        let successful = matches!(response.outcome, Outcome::Completed { .. });
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
    let mut state = AgentManagerState::load(db.clone(), publication.clone(), id).await?;
    let events = build_recovery_events(&state.agent, &state.context)?;
    if !events.is_empty() {
        state.persist_events(&events).await?;
    }

    Ok(())
}

pub(crate) fn build_recovery_events(
    agent: &Agent,
    context: &AgentContext,
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

        events.push(NewEvent::new(EventType::ToolResult(failed_tool_result(
            call.call_id.clone(),
            prompts::tool_interrupted(),
        ))));
    }

    if !events.is_empty() || agent.state == AgentState::Running {
        events.push(status_event(AgentState::Interrupted));
    }

    Ok(events)
}

fn build_item_event(item: &HistoryItem) -> Result<EventType, AgentError> {
    Ok(match &item.kind {
        HistoryItemKind::Message(message) => EventType::Message(Message {
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
) -> Result<(), AgentError> {
    let mut hooks = Hooks::default();
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
            BoundaryReply::Work(request) => {
                state
                    .persist_events(&[
                        NewEvent::new(EventType::Message(Message {
                            message_type: MessageType::Human,
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
            BoundaryReply::Continue => {}
        }

        if state.agent.state != AgentState::Running {
            continue;
        }
        if drain_controls(&mut state, &mut controls) {
            state.interrupt().await?;
            continue;
        }
        if run_hook(&mut hooks.before_request, &mut state, &mut controls).await? {
            state.interrupt().await?;
            continue;
        }

        invocation += 1;
        state
            .infer(
                &providers,
                &mut controls,
                &format!(
                    "{}:{generation}:{invocation}:{}",
                    state.agent.id.0,
                    time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                ),
            )
            .await?;

        if run_hook(&mut hooks.after_request, &mut state, &mut controls).await? {
            state.interrupt().await?;
            continue;
        }
        state.resolve_tools(false, Some(&mut controls)).await?;
    }
}

#[cfg(test)]
mod tests;

//! Per-invocation capabilities shared by hooks and tools.

use super::{
    Agent, AgentError, agent_manager::AgentManagerState, agent_projection::AgentProjection,
    client_queries::ClientQueryError, sessions::Sessions,
};
use crate::{
    components::cancellation::Cancellation,
    db::{Database, DbError, InferenceResultEvents},
    probes::ProbeHandle,
    sessions::Session,
};
use psa_api::{EventEnvelope, events::*};
use psa_shared::probe_transport::ProbeError;

#[cfg(test)]
mod tests;

/// Invocation capabilities with either borrowed hook state or owned tool snapshots.
/// Lifecycle hooks also run when the session's probe is offline.
/// Trace queries are scoped to this agent, including inherited fork history.
/// Session/agent changes made through `Sessions` follow the normal manager API.
/// Tool snapshots remain stable except for agent/projection updates from their
/// own save replies; only the worker mutates authoritative state.
pub struct AgentContext<'a> {
    state: ContextState<'a>,
    probe: Option<&'a ProbeHandle>,
    sessions: &'a Sessions,
}

enum ContextState<'a> {
    Borrowed(&'a mut AgentManagerState),
    Invocation(InvocationSnapshot),
}

/// Owned, stable invocation view. A batch shares its immutable base projection;
/// only save replies replace the saving invocation's agent/projection snapshot.
#[derive(Clone)]
pub(crate) struct InvocationSnapshot {
    pub agent: Agent,
    pub session: Session,
    pub definition: psa_api::AgentDefinition,
    pub projection: std::sync::Arc<AgentProjection>,
    pub tools: std::sync::Arc<[crate::components::tools::ToolKind]>,
    pub database: Database,
    pub saves: actors::UnboundedMailboxHandle<super::agent_manager::tool_execution::SaveRequest>,
}

impl<'a> AgentContext<'a> {
    pub(crate) fn invocation(
        snapshot: InvocationSnapshot,
        probe: &'a ProbeHandle,
        sessions: &'a Sessions,
    ) -> Self {
        Self {
            state: ContextState::Invocation(snapshot),
            probe: Some(probe),
            sessions,
        }
    }

    fn database(&self) -> &Database {
        match &self.state {
            ContextState::Borrowed(state) => state.database(),
            ContextState::Invocation(snapshot) => &snapshot.database,
        }
    }
    pub(crate) fn new(
        state: &'a mut AgentManagerState,
        probe: &'a ProbeHandle,
        sessions: &'a Sessions,
    ) -> Self {
        Self::with_optional_probe(state, Some(probe), sessions)
    }

    pub(crate) fn with_optional_probe(
        state: &'a mut AgentManagerState,
        probe: Option<&'a ProbeHandle>,
        sessions: &'a Sessions,
    ) -> Self {
        Self {
            state: ContextState::Borrowed(state),
            probe,
            sessions,
        }
    }

    pub fn agent(&self) -> &Agent {
        match &self.state {
            ContextState::Borrowed(state) => state.agent(),
            ContextState::Invocation(snapshot) => &snapshot.agent,
        }
    }

    pub fn session(&self) -> &Session {
        match &self.state {
            ContextState::Borrowed(state) => state.session(),
            ContextState::Invocation(snapshot) => &snapshot.session,
        }
    }

    pub(crate) fn update_session(&mut self, session: Session) {
        match &mut self.state {
            ContextState::Borrowed(state) => state.update_session(session),
            ContextState::Invocation(_) => {}
        }
    }

    pub fn definition(&self) -> &psa_api::AgentDefinition {
        match &self.state {
            ContextState::Borrowed(state) => state.definition(),
            ContextState::Invocation(snapshot) => &snapshot.definition,
        }
    }

    pub fn tools(&self) -> &[crate::components::tools::ToolKind] {
        match &self.state {
            ContextState::Borrowed(state) => state.tools(),
            ContextState::Invocation(snapshot) => &snapshot.tools,
        }
    }

    pub fn projection(&self) -> &AgentProjection {
        match &self.state {
            ContextState::Borrowed(state) => state.context(),
            ContextState::Invocation(snapshot) => &snapshot.projection,
        }
    }

    /// The invocation's connection, or `Disconnected` for offline lifecycle hooks.
    pub fn probe(&self) -> Result<&ProbeHandle, ProbeError> {
        self.probe.ok_or(ProbeError::Disconnected)
    }

    pub fn sessions(&self) -> &Sessions {
        self.sessions
    }

    /// Ask connected clients using this invocation's session and agent scope.
    /// Available to lifecycle hooks even when the probe is offline.
    pub async fn query_client(
        &self,
        query: psa_api::ClientQuery,
        cancellation: &mut Cancellation,
    ) -> Result<psa_api::ClientQueryResponse, ClientQueryError> {
        self.sessions
            .client_queries()
            .query(self.session().id, self.agent().id, query, cancellation)
            .await
    }

    pub(crate) async fn finish_persistence(&mut self) -> Result<(), AgentError> {
        if let ContextState::Borrowed(state) = &mut self.state {
            state.finish_persistence().await?;
        }
        Ok(())
    }

    /// Persist and publish events, updating the worker's projection and status.
    /// Tool responses are constructed and persisted separately by the worker.
    pub async fn save_events(&mut self, events: &[NewEvent]) -> Result<Vec<Event>, AgentError> {
        match &mut self.state {
            ContextState::Borrowed(state) => state.persist_events(events).await,
            ContextState::Invocation(snapshot) => {
                let saved = snapshot
                    .saves
                    .request(|reply| super::agent_manager::tool_execution::SaveRequest {
                        events: events.to_vec(),
                        reply,
                    })
                    .await
                    .map_err(|_| AgentError::SupervisorUnavailable)??;
                snapshot.agent = saved.agent;
                snapshot.projection = saved.projection;
                Ok(saved.events)
            }
        }
    }

    pub async fn load_events(&self, range: EventRange) -> Result<Vec<Event>, DbError> {
        let mut cursor = self.database().cursor().await?;
        let events = cursor.load_events(self.agent().id, range).await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_all_events(&self) -> Result<Vec<Event>, DbError> {
        let mut cursor = self.database().cursor().await?;
        let events = cursor.load_all_events(self.agent().id).await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_context_window(&self) -> Result<Vec<Event>, DbError> {
        let mut cursor = self.database().cursor().await?;
        let events = cursor.load_context_window(self.agent().id).await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_events_since_last_inference(&self) -> Result<Option<Vec<Event>>, DbError> {
        let mut cursor = self.database().cursor().await?;
        let events = cursor
            .load_events_since_last_inference(self.agent().id)
            .await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_last_inference_result(
        &self,
    ) -> Result<Option<InferenceResultEvents>, DbError> {
        let mut cursor = self.database().cursor().await?;
        let result = cursor.load_last_inference_result(self.agent().id).await?;
        cursor.rollback().await?;
        Ok(result)
    }

    pub async fn load_agent_envelopes(
        &self,
        range: EventRange,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        let mut cursor = self.database().cursor().await?;
        let events = cursor
            .load_agent_envelopes(self.session().id, self.agent().id, range)
            .await?;
        cursor.rollback().await?;
        Ok(events)
    }
}

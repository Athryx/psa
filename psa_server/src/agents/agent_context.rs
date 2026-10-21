//! Per-invocation capabilities shared by hooks and tools.

use super::{
    Agent, AgentError, agent_manager::AgentManagerState, agent_projection::AgentProjection,
    sessions::Sessions,
};
use crate::{
    db::{DbError, InferenceResultEvents},
    probes::ProbeHandle,
    sessions::Session,
};
use psa_api::{EventEnvelope, events::*, probe_transport::ProbeError};

#[cfg(test)]
mod tests;

/// Borrows the current worker state and an optional probe connection.
/// Lifecycle hooks also run when the session's probe is offline.
/// Trace queries are scoped to this agent, including inherited fork history.
/// Session/agent changes made through `Sessions` follow the normal manager API;
/// the definitions here remain the worker's current snapshots.
pub struct AgentContext<'a> {
    state: &'a mut AgentManagerState,
    probe: Option<&'a ProbeHandle>,
    sessions: &'a Sessions,
}

impl<'a> AgentContext<'a> {
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
            state,
            probe,
            sessions,
        }
    }

    pub fn agent(&self) -> &Agent {
        self.state.agent()
    }

    pub fn session(&self) -> &Session {
        self.state.session()
    }

    pub(crate) fn update_session(&mut self, session: Session) {
        self.state.update_session(session);
    }

    pub fn definition(&self) -> &psa_api::AgentDefinition {
        self.state.definition()
    }

    pub fn tools(&self) -> &[crate::hooks::tools::ToolKind] {
        self.state.tools()
    }

    pub fn projection(&self) -> &AgentProjection {
        self.state.context()
    }

    /// The invocation's connection, or `Disconnected` for offline lifecycle hooks.
    pub fn probe(&self) -> Result<&ProbeHandle, ProbeError> {
        self.probe.ok_or(ProbeError::Disconnected)
    }

    pub fn sessions(&self) -> &Sessions {
        self.sessions
    }

    pub(crate) async fn finish_persistence(&mut self) -> Result<(), AgentError> {
        self.state.finish_persistence().await?;
        Ok(())
    }

    /// Persist and publish events, updating the worker's projection and status.
    /// Tool responses are constructed and persisted separately by the worker.
    pub async fn save_events(&mut self, events: &[NewEvent]) -> Result<Vec<Event>, AgentError> {
        self.state.persist_events(events).await
    }

    pub async fn load_events(&self, range: EventRange) -> Result<Vec<Event>, DbError> {
        let mut cursor = self.state.database().cursor().await?;
        let events = cursor.load_events(self.agent().id, range).await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_all_events(&self) -> Result<Vec<Event>, DbError> {
        let mut cursor = self.state.database().cursor().await?;
        let events = cursor.load_all_events(self.agent().id).await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_context_window(&self) -> Result<Vec<Event>, DbError> {
        let mut cursor = self.state.database().cursor().await?;
        let events = cursor.load_context_window(self.agent().id).await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_events_since_last_inference(&self) -> Result<Option<Vec<Event>>, DbError> {
        let mut cursor = self.state.database().cursor().await?;
        let events = cursor
            .load_events_since_last_inference(self.agent().id)
            .await?;
        cursor.rollback().await?;
        Ok(events)
    }

    pub async fn load_last_inference_result(
        &self,
    ) -> Result<Option<InferenceResultEvents>, DbError> {
        let mut cursor = self.state.database().cursor().await?;
        let result = cursor.load_last_inference_result(self.agent().id).await?;
        cursor.rollback().await?;
        Ok(result)
    }

    pub async fn load_agent_envelopes(
        &self,
        range: EventRange,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        let mut cursor = self.state.database().cursor().await?;
        let events = cursor
            .load_agent_envelopes(self.session().id, self.agent().id, range)
            .await?;
        cursor.rollback().await?;
        Ok(events)
    }
}

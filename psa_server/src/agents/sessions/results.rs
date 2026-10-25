//! Durable result waits use publication only as a wake-up signal. The database
//! remains authoritative, including when the broadcast receiver has lagged.

use super::{RuntimeError, Sessions};
use crate::{
    agents::session_manager::EXECUTION_FAILURE_MESSAGE, components::cancellation::Cancellation,
};
use psa_api::{AgentId, AgentState, ResultRecord, SessionId, StreamMessage, events::*};
use tokio::sync::broadcast;

impl Sessions {
    /// Wait for the newest durable result strictly after `after` in this agent's
    /// logical history. `None` accepts an existing result, including fork ancestry.
    /// Capture the agent's last event ID before submitting work to exclude stale
    /// results. A new, idle agent is allowed to wait for queued work to start.
    ///
    /// With no matching result, returns `ResultUnavailable` if the agent is
    /// interrupted, a request fails, or an idle completion/fatal worker diagnostic
    /// occurs after the boundary. Nonfatal error logs do not end the wait. Fatal
    /// diagnostics also cover startup failures before a run begins. An already
    /// interrupted agent fails regardless of `after`; a
    /// queued restart must enter Running before this method can wait on it.
    /// Returns `Cancelled` on invocation cancellation and `Unavailable` on shutdown.
    /// Dropping this future only drops its subscription; it never changes agent
    /// state, consumes a result, or cancels the target agent's work.
    pub async fn await_result(
        &self,
        session: SessionId,
        agent: AgentId,
        after: Option<EventId>,
        cancellation: &mut Cancellation,
    ) -> Result<ResultRecord, RuntimeError> {
        let mut shutdown = self.publication.shutdown_receiver();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
            _ = async {
                loop {
                    if *shutdown.borrow_and_update() {
                        break;
                    }
                    if shutdown.changed().await.is_err() {
                        break;
                    }
                }
            } => Err(RuntimeError::Unavailable),
            result = self.await_result_inner(session, agent, after.unwrap_or(EventId(0))) => result,
        }
    }

    async fn await_result_inner(
        &self,
        session: SessionId,
        agent: AgentId,
        after: EventId,
    ) -> Result<ResultRecord, RuntimeError> {
        // Subscribe before reading: an append can be observed by the snapshot,
        // the subscription, or both, but can never fall between them unnoticed.
        let mut received = self.publication.lock().await.subscribe();
        let mut request_failed = false;
        loop {
            if let Some(result) = self
                .read_awaited_result(session, agent, after, request_failed)
                .await?
            {
                return Ok(result);
            }

            loop {
                match received.recv().await {
                    Ok(StreamMessage::Committed { event, .. })
                        if event.session_id == session && event.agent_id == agent =>
                    {
                        break;
                    }
                    Ok(StreamMessage::AgentUpdated { agent: updated })
                        if updated.session_id == session && updated.id == agent =>
                    {
                        break;
                    }
                    Ok(StreamMessage::RequestFailed {
                        session_id,
                        agent_id,
                        ..
                    }) if session_id == session && agent_id == agent => {
                        request_failed = true;
                        break;
                    }
                    Ok(StreamMessage::ResyncRequired)
                    | Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(RuntimeError::Unavailable);
                    }
                    _ => {}
                }
            }
        }
    }

    async fn read_awaited_result(
        &self,
        session: SessionId,
        agent: AgentId,
        after: EventId,
        request_failed: bool,
    ) -> Result<Option<ResultRecord>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let snapshot = cursor.require_agent_session(session, agent).await?;
        let result = cursor
            .load_last_result(session, agent)
            .await?
            .filter(|result| result.event_id > after);
        if result.is_some() {
            cursor.rollback().await?;
            return Ok(result);
        }

        if request_failed || snapshot.state == AgentState::Interrupted {
            return Err(RuntimeError::ResultUnavailable { session, agent });
        }

        // Idle alone is not terminal: newly created agents and accepted queued
        // work have not necessarily started. Require a completion or error event
        // newer than the boundary before declaring a result missing. A startup
        // worker failure can persist only its fatal diagnostic, without entering
        // Running. Other error logs can be harmless startup warnings.
        if snapshot.state == AgentState::Idle {
            let mut boundary = after;
            loop {
                let events = cursor
                    .load_events(
                        agent,
                        EventRange {
                            after: boundary,
                            through: Some(EventId(snapshot.next_event_id.0.saturating_sub(1))),
                            limit: 1000,
                        },
                    )
                    .await?;
                let Some(last) = events.last() else {
                    break;
                };
                boundary = last.id;
                if events.iter().any(|event| match &event.kind {
                    EventType::StatusChange(change) => change.new_status == AgentState::Idle,
                    EventType::Log(log) => log.error && log.message == EXECUTION_FAILURE_MESSAGE,
                    _ => false,
                }) {
                    return Err(RuntimeError::ResultUnavailable { session, agent });
                }
            }
        }
        cursor.rollback().await?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests;

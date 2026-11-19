//! Orders transcript commits and their publication with SSE replay snapshots.

use super::{AgentError, AgentId, agent_projection::AgentProjection};
use crate::db::NewEvent;
use crate::{
    db::{AddEventResult, Database, DbError, EventBatchItem},
    sessions::SessionId,
};
use psa::api::events::*;
use psa::api::{
    ClientQueryId, ClientQueryRequest, EventEnvelope, PendingEvent, PendingEventId, StreamMessage,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{Mutex, MutexGuard, broadcast, watch};

/// Two locks, always taken in this order. `commits` orders database writes with
/// their publication and with replay snapshots; `state` guards the published
/// projection briefly, so provisional updates never wait on a commit.
#[derive(Clone)]
pub struct Publication {
    commits: Arc<Mutex<()>>,
    state: Arc<Mutex<PublicationState>>,
    shutdown: watch::Sender<bool>,
    /// Detached appends still running, per agent. Kept outside the publication
    /// gate so registering one never waits on a commit.
    in_flight_appends: Arc<std::sync::Mutex<HashMap<AgentId, Vec<watch::Receiver<()>>>>>,
}

pub struct PublicationState {
    pub(crate) pending: HashMap<PendingEventId, PendingEvent>,
    /// Read projection of runtime queries, updated by the query actor under this gate.
    pub(crate) queries: HashMap<ClientQueryId, ClientQueryRequest>,
    /// Published activity projection, maintained by the worker supervisor.
    /// Metadata writers read it under this gate to avoid publishing stale busy state.
    busy_sessions: HashSet<SessionId>,
    sender: broadcast::Sender<StreamMessage>,
    global_sender: broadcast::Sender<psa::api::GlobalStreamMessage>,
    revision: std::sync::atomic::AtomicU64,
}

impl Default for Publication {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(1024);
        let (shutdown, _) = watch::channel(false);
        let (global_sender, _) = broadcast::channel(1024);
        Self {
            commits: Arc::default(),
            state: Arc::new(Mutex::new(PublicationState {
                pending: HashMap::new(),
                queries: HashMap::new(),
                busy_sessions: HashSet::new(),
                sender,
                global_sender,
                revision: std::sync::atomic::AtomicU64::new(0),
            })),
            shutdown,
            in_flight_appends: Arc::default(),
        }
    }
}

impl Publication {
    /// The full gate: excludes commits and other publishers. Use it for a
    /// database write published in commit order, or a consistent replay snapshot.
    pub async fn lock(&self) -> PublicationGuard<'_> {
        let commits = self.commits.lock().await;
        PublicationGuard {
            state: self.state.lock().await,
            _commits: commits,
        }
    }

    /// The published projection alone, for updates that write nothing to the
    /// database, such as provisional events and runtime notices.
    pub async fn lock_state(&self) -> MutexGuard<'_, PublicationState> {
        self.state.lock().await
    }

    /// Register a detached append before spawning it. Dropping the returned
    /// sender marks completion, including on task failure or abort. Do not send.
    #[must_use = "hold the sender until the registered append has finished"]
    pub(crate) fn register_append(&self, agent: AgentId) -> watch::Sender<()> {
        let (sender, receiver) = watch::channel(());
        let mut in_flight = self
            .in_flight_appends
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let receivers = in_flight.entry(agent).or_default();
        receivers.retain(|receiver| receiver.has_changed().is_ok());
        receivers.push(receiver);
        sender
    }

    /// Wait for an agent's registered appends after its worker has exited.
    /// Callers must serialize drains for an agent and prevent new registrations
    /// during a drain; supervisor recovery provides that ordering.
    pub(crate) async fn drain_appends(&self, agent: AgentId) {
        // Keep the original receivers registered while waiting so cancellation
        // cannot lose track of still-running appends.
        let receivers = self
            .in_flight_appends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&agent)
            .cloned()
            .unwrap_or_default();
        for mut receiver in receivers {
            while receiver.changed().await.is_ok() {}
        }

        let mut in_flight = self
            .in_flight_appends
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(receivers) = in_flight.get_mut(&agent) {
            receivers.retain(|receiver| receiver.has_changed().is_ok());
            if receivers.is_empty() {
                in_flight.remove(&agent);
            }
        }
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn close(&self) {
        self.shutdown.send_replace(true);
    }

    /// Publish a supervisor activity transition with current session metadata.
    /// The activity projection and snapshot are updated under the publication gate,
    /// in the same order as metadata writes. A failed read invalidates client state.
    pub async fn update_session_busy(
        &self,
        db: &Database,
        session_id: SessionId,
        busy: bool,
    ) -> Result<(), DbError> {
        let mut publication = self.lock().await;
        if busy {
            publication.busy_sessions.insert(session_id);
        } else {
            publication.busy_sessions.remove(&session_id);
        }

        let result = async {
            let mut cursor = db.read().await?;
            let session = cursor.get_session(session_id).await?;
            cursor.close().await?;
            Ok::<_, DbError>(session)
        }
        .await;
        let session = match result {
            Ok(session) => session,
            Err(error) => {
                publication.publish(StreamMessage::ResyncRequired);
                return Err(error);
            }
        };
        publication.publish(StreamMessage::SessionUpdated {
            session: session.into_api(busy),
        });
        Ok(())
    }

    pub async fn upsert(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        id: PendingEventId,
        kind: EventType,
    ) {
        let mut state = self.lock_state().await;
        let revision = state.pending.get(&id).map_or(1, |item| item.revision + 1);
        let pending = PendingEvent {
            id: id.clone(),
            revision,
            session_id,
            agent_id,
            kind,
        };
        state.pending.insert(id, pending.clone());
        state.publish(StreamMessage::Upsert { pending });
    }

    pub async fn discard_pending(&self, agent: AgentId) {
        let mut state = self.lock_state().await;
        let items: Vec<_> = state
            .pending
            .values()
            .filter(|item| item.agent_id == agent)
            .cloned()
            .collect();
        for item in items {
            state.pending.remove(&item.id);
            state.publish(StreamMessage::Removed {
                session_id: item.session_id,
                agent_id: agent,
                id: item.id,
            });
        }
    }

    /// Validate a cloned projection against the actual assigned events before
    /// commit. Promotions are paired by event offset, outside result markers.
    pub async fn append(
        &self,
        db: &Database,
        agent: AgentId,
        events: &[EventBatchItem],
        promotions: &[(usize, PendingEventId)],
        context: &AgentProjection,
    ) -> Result<(AddEventResult, AgentProjection), AgentError> {
        // Commit order, not the projection, is held across the write.
        let _commits = self.commits.lock().await;
        let mut cursor = db.write().await?;
        let result = cursor.add_event_batch(agent, events).await?;

        let mut context = context.clone();
        for event in &result.events {
            context.apply_event(event)?;
        }

        let snapshot = cursor.get_agent(agent).await?;
        let session = cursor.get_session(snapshot.session_id).await?;
        let first_position = session.next_position - result.events.len() as u64;
        cursor.commit().await?;

        let mut publication = self.lock_state().await;
        for (offset, event) in result.events.iter().enumerate() {
            let pending_id = promotions
                .iter()
                .find(|(index, _)| *index == offset)
                .map(|(_, id)| id.clone());
            if let Some(id) = &pending_id {
                publication.pending.remove(id);
            }
            publication.publish(StreamMessage::Committed {
                event: EventEnvelope {
                    session_id: snapshot.session_id,
                    agent_id: agent,
                    position: first_position + offset as u64,
                    event: event.clone(),
                },
                pending_id,
            });
        }
        publication.publish(StreamMessage::AgentUpdated {
            agent: snapshot.into(),
        });

        if !result.events.is_empty() {
            let busy = publication.is_session_busy(session.id);
            publication.publish(StreamMessage::SessionUpdated {
                session: session.into_api(busy),
            });
        }
        Ok((result, context))
    }
}

/// Holds both publication locks; dereferences to the published projection.
pub struct PublicationGuard<'a> {
    state: MutexGuard<'a, PublicationState>,
    _commits: MutexGuard<'a, ()>,
}

impl std::ops::Deref for PublicationGuard<'_> {
    type Target = PublicationState;

    fn deref(&self) -> &PublicationState {
        &self.state
    }
}

impl std::ops::DerefMut for PublicationGuard<'_> {
    fn deref_mut(&mut self) -> &mut PublicationState {
        &mut self.state
    }
}

impl PublicationState {
    /// Last supervisor activity state, ordered with session snapshot publication.
    pub fn is_session_busy(&self, session: SessionId) -> bool {
        self.busy_sessions.contains(&session)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamMessage> {
        self.sender.subscribe()
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn subscribe_global(&self) -> broadcast::Receiver<psa::api::GlobalStreamMessage> {
        self.global_sender.subscribe()
    }

    pub fn publish(&self, message: StreamMessage) {
        match &message {
            StreamMessage::SessionUpdated { session } => {
                let revision = self
                    .revision
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                let _ = self
                    .global_sender
                    .send(psa::api::GlobalStreamMessage::SessionUpdated {
                        revision,
                        session: session.clone(),
                    });
            }
            StreamMessage::ResyncRequired => {
                let _ = self
                    .global_sender
                    .send(psa::api::GlobalStreamMessage::ResyncRequired);
            }
            _ => {}
        }
        let _ = self.sender.send(message);
    }

    pub fn queries(&self, session: SessionId, agent: Option<AgentId>) -> Vec<ClientQueryRequest> {
        let mut queries: Vec<_> = self
            .queries
            .values()
            .filter(|query| {
                query.session_id == session && agent.is_none_or(|id| query.agent_id == id)
            })
            .cloned()
            .collect();
        queries.sort_by_key(|query| query.id);
        queries
    }

    pub fn pending(&self, session: SessionId, agent: Option<AgentId>) -> Vec<PendingEvent> {
        let mut pending: Vec<_> = self
            .pending
            .values()
            .filter(|item| item.session_id == session && agent.is_none_or(|id| item.agent_id == id))
            .cloned()
            .collect();
        pending.sort_by(|a, b| a.id.cmp(&b.id));
        pending
    }
}

/// Shared by startup/fork recovery and the worker, without duplicating contents.
pub fn status_event(status: super::AgentState) -> NewEvent {
    NewEvent::new(EventType::StatusChange(StatusChange {
        new_status: status,
        reason: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn append_tracking_is_shared_and_waits_for_completion_or_drop() {
        let publication = Publication::default();
        let agent = AgentId(1);
        let first = publication.register_append(agent);
        let second = publication.clone().register_append(agent);
        let other = publication.register_append(AgentId(999));
        let clone = publication.clone();
        let mut drain = tokio::spawn(async move { clone.drain_appends(agent).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut drain)
                .await
                .is_err()
        );
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut drain)
                .await
                .is_err()
        );
        drop(second);
        tokio::time::timeout(Duration::from_secs(1), drain)
            .await
            .unwrap()
            .unwrap();
        drop(other);
        publication.drain_appends(AgentId(999)).await;
    }

    #[tokio::test]
    async fn cancelling_an_append_drain_keeps_tracking_in_flight_appends() {
        let publication = Publication::default();
        let agent = AgentId(1);
        let in_flight = publication.register_append(agent);
        let clone = publication.clone();
        let mut first = tokio::spawn(async move { clone.drain_appends(agent).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut first)
                .await
                .is_err()
        );
        first.abort();
        let _ = first.await;

        let clone = publication.clone();
        let mut second = tokio::spawn(async move { clone.drain_appends(agent).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );
        drop(in_flight);
        tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
    }
}

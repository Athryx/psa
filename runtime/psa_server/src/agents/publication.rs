//! One gate serializes transcript commits/publication with SSE replay snapshots.

use super::{AgentError, AgentId, agent_projection::AgentProjection};
use crate::{
    db::{AddEventResult, Database, DbError},
    sessions::SessionId,
};
use psa_api::events::*;
use psa_api::{ClientQueryId, ClientQueryRequest, EventEnvelope, PendingEvent, StreamMessage};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{Mutex, MutexGuard, broadcast, watch};

#[derive(Clone)]
pub struct Publication {
    state: Arc<Mutex<PublicationState>>,
    shutdown: watch::Sender<bool>,
}

pub struct PublicationState {
    pub(crate) pending: HashMap<String, PendingEvent>,
    /// Read projection of runtime queries, updated by the query actor under this gate.
    pub(crate) queries: HashMap<ClientQueryId, ClientQueryRequest>,
    /// Published activity projection, maintained by the worker supervisor.
    /// Metadata writers read it under this gate to avoid publishing stale busy state.
    busy_sessions: HashSet<SessionId>,
    sender: broadcast::Sender<StreamMessage>,
    global_sender: broadcast::Sender<psa_api::GlobalStreamMessage>,
    revision: std::sync::atomic::AtomicU64,
}

impl Default for Publication {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(1024);
        let (shutdown, _) = watch::channel(false);
        let (global_sender, _) = broadcast::channel(1024);
        Self {
            state: Arc::new(Mutex::new(PublicationState {
                pending: HashMap::new(),
                queries: HashMap::new(),
                busy_sessions: HashSet::new(),
                sender,
                global_sender,
                revision: std::sync::atomic::AtomicU64::new(0),
            })),
            shutdown,
        }
    }
}

impl Publication {
    pub async fn lock(&self) -> MutexGuard<'_, PublicationState> {
        self.state.lock().await
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
            let mut cursor = db.cursor().await?;
            let session = cursor.get_session(session_id).await?;
            cursor.rollback().await?;
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
        id: String,
        kind: EventType,
    ) {
        let mut state = self.lock().await;
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
        let mut state = self.lock().await;
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
        events: &[NewEvent],
        promotions: &[(usize, String)],
        context: &AgentProjection,
    ) -> Result<(AddEventResult, AgentProjection), AgentError> {
        let mut publication = self.lock().await;
        let mut cursor = db.cursor().await?;
        let result = cursor.add_events(agent, events).await?;

        let mut context = context.clone();
        for event in &result.events {
            context.apply_event(event)?;
        }

        let snapshot = cursor.get_agent(agent).await?;
        let session = cursor.get_session(snapshot.session_id).await?;
        let first_position = session.next_position - result.events.len() as u64;
        cursor.commit().await?;

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

    pub fn subscribe_global(&self) -> broadcast::Receiver<psa_api::GlobalStreamMessage> {
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
                    .send(psa_api::GlobalStreamMessage::SessionUpdated {
                        revision,
                        session: session.clone(),
                    });
            }
            StreamMessage::ResyncRequired => {
                let _ = self
                    .global_sender
                    .send(psa_api::GlobalStreamMessage::ResyncRequired);
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

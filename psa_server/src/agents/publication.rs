//! One gate serializes transcript commits/publication with SSE replay snapshots.

use super::{AgentError, AgentId, agent_projection::AgentProjection};
use crate::{
    db::{AddEventResult, Database},
    sessions::SessionId,
};
use psa_api::events::*;
use psa_api::{EventEnvelope, PendingEvent, StreamMessage};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, MutexGuard, broadcast, watch};

#[derive(Clone)]
pub struct Publication {
    state: Arc<Mutex<PublicationState>>,
    shutdown: watch::Sender<bool>,
}

pub struct PublicationState {
    pub(crate) pending: HashMap<String, PendingEvent>,
    sender: broadcast::Sender<StreamMessage>,
}

impl Default for Publication {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(1024);
        let (shutdown, _) = watch::channel(false);
        Self {
            state: Arc::new(Mutex::new(PublicationState {
                pending: HashMap::new(),
                sender,
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
        let first_position = cursor.get_session(snapshot.session_id).await?.next_position
            - result.events.len() as u64;
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

        Ok((result, context))
    }
}

impl PublicationState {
    pub fn subscribe(&self) -> broadcast::Receiver<StreamMessage> {
        self.sender.subscribe()
    }

    pub fn publish(&self, message: StreamMessage) {
        let _ = self.sender.send(message);
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

//! Actor-owned, ephemeral client requests. Publication contains only a read projection.

use super::publication::{Publication, PublicationState};
use crate::components::cancellation::Cancellation;
use actors::{Reply, UnboundedMailbox, UnboundedMailboxHandle};
use psa::api::{
    AgentId, ClientQuery, ClientQueryId, ClientQueryRequest, ClientQueryResponse, SessionId,
    StreamMessage,
};
use std::collections::{HashMap, VecDeque};
use tokio::{sync::oneshot, task::JoinHandle};

const PENDING_LIMIT: usize = 1024;
const TOMBSTONE_LIMIT: usize = 1024;
type QueryResult = Result<ClientQueryResponse, ClientQueryError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientQueryError {
    #[error("invalid client query request")]
    InvalidRequest(String),
    #[error("invalid client query response")]
    InvalidResponse(String),
    #[error("client query not found")]
    NotFound,
    #[error("client query already resolved")]
    AlreadyResolved,
    #[error("client query cancelled")]
    Cancelled,
    #[error("client query service unavailable")]
    Unavailable,
    #[error("client query service overloaded")]
    Overloaded,
}

/// The non-cloneable runtime owner. Shutdown cancellation never detaches its task.
pub struct ClientQueries {
    handle: ClientQueriesHandle,
    task: Option<JoinHandle<()>>,
    stopping: bool,
}

#[derive(Clone)]
pub struct ClientQueriesHandle {
    mailbox: UnboundedMailboxHandle<Message>,
}

enum Message {
    Register {
        request: ClientQueryRequest,
        registered: Reply<Result<(), ClientQueryError>>,
        waiter: Reply<QueryResult>,
    },
    Respond {
        session: SessionId,
        agent: AgentId,
        id: ClientQueryId,
        response: ClientQueryResponse,
        reply: Reply<Result<(), ClientQueryError>>,
    },
    Cancel {
        id: ClientQueryId,
    },
    Shutdown,
}

struct PendingQuery {
    request: ClientQueryRequest,
    waiter: Reply<QueryResult>,
}

struct Tombstone {
    session: SessionId,
    agent: AgentId,
    status: Resolution,
}

#[derive(Clone, Copy)]
enum Resolution {
    Answered,
    Cancelled,
}

impl Resolution {
    fn error(self) -> ClientQueryError {
        match self {
            Self::Answered => ClientQueryError::AlreadyResolved,
            Self::Cancelled => ClientQueryError::Cancelled,
        }
    }
}

struct Registry {
    publication: Publication,
    pending: HashMap<ClientQueryId, PendingQuery>,
    tombstones: HashMap<ClientQueryId, Tombstone>,
    tombstone_order: VecDeque<ClientQueryId>,
}

struct CancelOnDrop {
    mailbox: UnboundedMailboxHandle<Message>,
    id: ClientQueryId,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.mailbox.send(Message::Cancel { id: self.id });
    }
}

impl ClientQueries {
    pub fn start(publication: Publication) -> Self {
        let (mailbox, received) = actors::unbounded_channel();
        let registry = Registry {
            publication,
            pending: HashMap::new(),
            tombstones: HashMap::new(),
            tombstone_order: VecDeque::new(),
        };
        Self {
            handle: ClientQueriesHandle { mailbox },
            task: Some(tokio::spawn(registry.run(received))),
            stopping: false,
        }
    }

    pub fn handle(&self) -> ClientQueriesHandle {
        self.handle.clone()
    }

    pub async fn shutdown(&mut self) -> Result<(), ClientQueryError> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        if !self.stopping {
            self.stopping = true;
            let _ = self.handle.mailbox.send(Message::Shutdown);
        }
        let result = task.await.map_err(|_| ClientQueryError::Unavailable);
        self.task = None;
        result
    }
}

impl Drop for ClientQueries {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl ClientQueriesHandle {
    pub async fn query(
        &self,
        session: SessionId,
        agent: AgentId,
        payload: ClientQuery,
        cancellation: &mut Cancellation,
    ) -> QueryResult {
        if cancellation.is_cancelled() {
            return Err(ClientQueryError::Cancelled);
        }
        payload
            .validate()
            .map_err(ClientQueryError::InvalidRequest)?;

        let id = ClientQueryId::new();
        // This guard precedes enqueue and outlives both registration and reply waits.
        let _cancel = CancelOnDrop {
            mailbox: self.mailbox.clone(),
            id,
        };
        let (registered, registration) = oneshot::channel();
        let (waiter, response) = oneshot::channel();
        self.mailbox
            .send(Message::Register {
                request: ClientQueryRequest {
                    id,
                    session_id: session,
                    agent_id: agent,
                    payload,
                },
                registered,
                waiter,
            })
            .map_err(|_| ClientQueryError::Unavailable)?;

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ClientQueryError::Cancelled),
            result = async {
                registration.await.map_err(|_| ClientQueryError::Unavailable)??;
                response.await.map_err(|_| ClientQueryError::Unavailable)?
            } => result,
        }
    }

    pub async fn respond(
        &self,
        session: SessionId,
        agent: AgentId,
        id: ClientQueryId,
        response: ClientQueryResponse,
    ) -> Result<(), ClientQueryError> {
        self.mailbox
            .request(|reply| Message::Respond {
                session,
                agent,
                id,
                response,
                reply,
            })
            .await
            .map_err(|_| ClientQueryError::Unavailable)?
    }
}

impl Registry {
    async fn run(mut self, mut mailbox: UnboundedMailbox<Message>) {
        while let Some(message) = mailbox.recv().await {
            match message {
                Message::Register {
                    request,
                    registered,
                    waiter,
                } => {
                    self.register(request, registered, waiter).await;
                }
                Message::Respond {
                    session,
                    agent,
                    id,
                    response,
                    reply,
                } => {
                    let result = self.respond(session, agent, id, response).await;
                    let _ = reply.send(result);
                }
                Message::Cancel { id } => self.cancel(id).await,
                Message::Shutdown => {
                    mailbox.close();
                    break;
                }
            }
        }

        let publication = self.publication.clone();
        let mut state = publication.lock_state().await;
        for (_, pending) in self.pending.drain() {
            remove_projection(&mut state, &pending.request);
            let _ = pending.waiter.send(Err(ClientQueryError::Unavailable));
        }
        // Dropping the closed mailbox also drops all queued registration/reply senders.
    }

    async fn register(
        &mut self,
        request: ClientQueryRequest,
        registered: Reply<Result<(), ClientQueryError>>,
        waiter: Reply<QueryResult>,
    ) {
        if registered.is_closed() || waiter.is_closed() {
            return;
        }
        if self.pending.len() >= PENDING_LIMIT {
            let _ = registered.send(Err(ClientQueryError::Overloaded));
            return;
        }
        if let Err(error) = request.payload.validate() {
            let _ = registered.send(Err(ClientQueryError::InvalidRequest(error)));
            return;
        }

        let publication = self.publication.clone();
        let mut state = publication.lock_state().await;
        if registered.is_closed() || waiter.is_closed() {
            return;
        }
        state.queries.insert(request.id, request.clone());
        state.publish(StreamMessage::ClientQueryRequested {
            query: request.clone(),
        });
        if registered.send(Ok(())).is_err() {
            remove_projection(&mut state, &request);
            self.remember(&request, Resolution::Cancelled);
            let _ = waiter.send(Err(ClientQueryError::Cancelled));
            return;
        }
        self.pending
            .insert(request.id, PendingQuery { request, waiter });
    }

    async fn respond(
        &mut self,
        session: SessionId,
        agent: AgentId,
        id: ClientQueryId,
        response: ClientQueryResponse,
    ) -> Result<(), ClientQueryError> {
        let Some(pending) = self.pending.get(&id) else {
            return Err(self
                .tombstones
                .get(&id)
                .filter(|item| item.session == session && item.agent == agent)
                .map_or(ClientQueryError::NotFound, |item| item.status.error()));
        };
        if pending.request.session_id != session || pending.request.agent_id != agent {
            return Err(ClientQueryError::NotFound);
        }
        if pending.waiter.is_closed() {
            self.cancel(id).await;
            return Err(ClientQueryError::Cancelled);
        }
        pending
            .request
            .payload
            .validate_response(&response)
            .map_err(ClientQueryError::InvalidResponse)?;

        let publication = self.publication.clone();
        let mut state = publication.lock_state().await;
        let pending = self
            .pending
            .remove(&id)
            .expect("pending query checked above");
        remove_projection(&mut state, &pending.request);
        // A dropped caller can race the publication gate or the final send.
        let delivered = pending.waiter.send(Ok(response)).is_ok();
        let status = if delivered {
            Resolution::Answered
        } else {
            Resolution::Cancelled
        };
        self.remember(&pending.request, status);
        if delivered {
            Ok(())
        } else {
            Err(ClientQueryError::Cancelled)
        }
    }

    async fn cancel(&mut self, id: ClientQueryId) {
        if !self.pending.contains_key(&id) {
            return;
        }
        let publication = self.publication.clone();
        let mut state = publication.lock_state().await;
        let pending = self
            .pending
            .remove(&id)
            .expect("pending query checked above");
        remove_projection(&mut state, &pending.request);
        self.remember(&pending.request, Resolution::Cancelled);
        let _ = pending.waiter.send(Err(ClientQueryError::Cancelled));
    }

    fn remember(&mut self, request: &ClientQueryRequest, status: Resolution) {
        self.tombstones.insert(
            request.id,
            Tombstone {
                session: request.session_id,
                agent: request.agent_id,
                status,
            },
        );
        self.tombstone_order.push_back(request.id);
        if self.tombstone_order.len() > TOMBSTONE_LIMIT {
            if let Some(id) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&id);
            }
        }
    }
}

fn remove_projection(state: &mut PublicationState, request: &ClientQueryRequest) {
    state.queries.remove(&request.id);
    state.publish(StreamMessage::ClientQueryRemoved {
        session_id: request.session_id,
        agent_id: request.agent_id,
        query_id: request.id,
    });
}

#[cfg(test)]
#[path = "../tests/agents/client_queries/tests.rs"]
mod tests;

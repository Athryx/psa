//! Runtime fixtures for testing components against the supervisor: a handle
//! whose commands are captured instead of run, and a lifecycle recorder.

use super::{RuntimeError, SupervisorClient, SupervisorCommand};
use crate::{
    client_queries::ClientQueries, factory::ComponentFactory, publication::Publication,
    runtime_handle::RuntimeHandle,
};
use actors::Mailbox;
use psa::api::{AgentId, AgentRequest, RequestAccepted, SessionId};
use psa_store::Database;
use std::sync::Arc;
use tokio::sync::oneshot;

#[path = "lifecycle_testing.rs"]
pub mod lifecycle;

/// The receiving side of a [`captured_runtime`] handle.
pub struct CapturedCommands {
    received: Mailbox<SupervisorCommand>,
    _queries: ClientQueries,
}

/// A conditional steering submission captured from the handle.
pub struct CapturedSteer {
    pub session: SessionId,
    pub agent: AgentId,
    pub request: AgentRequest,
    pub reply: oneshot::Sender<Result<RequestAccepted, RuntimeError>>,
}

/// A handle over `database` with no supervisor task behind it.
pub fn captured_runtime(
    database: Database,
    components: Arc<dyn ComponentFactory>,
) -> (RuntimeHandle, CapturedCommands) {
    let publication = Publication::default();
    let (commands, received) = actors::channel(256);
    let queries = ClientQueries::start(publication.clone());
    let handle = RuntimeHandle::new(
        database,
        publication,
        SupervisorClient::new(commands),
        queries.handle(),
        components,
    );
    (
        handle,
        CapturedCommands {
            received,
            _queries: queries,
        },
    )
}

impl CapturedCommands {
    /// The next command, which must be a conditional steering submission.
    pub async fn next_steer(&mut self) -> CapturedSteer {
        match self.received.recv().await {
            Some(SupervisorCommand::SubmitIfNotInterrupted {
                session,
                agent,
                request,
                reply,
            }) => CapturedSteer {
                session,
                agent,
                request,
                reply,
            },
            Some(_) => panic!("expected a conditional steering submission"),
            None => panic!("runtime handle closed"),
        }
    }
}

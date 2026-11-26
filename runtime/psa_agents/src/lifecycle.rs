//! Internal logical agent-run notifications, independent of worker generations.

use psa::api::{AgentId, SessionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRunOutcome {
    Completed,
    Interrupted,
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub enum AgentLifecycleEvent {
    Started {
        session_id: SessionId,
        agent_id: AgentId,
        run_id: String,
    },
    Finished {
        session_id: SessionId,
        agent_id: AgentId,
        run_id: String,
        outcome: AgentRunOutcome,
        final_text: Option<String>,
    },
}

/// Session-scoped bundle callbacks receive ordered logical lifecycle notifications.
/// Implementations enqueue work; they must not block the supervisor or await RPCs.
pub trait SessionLifecycleHook: Send + Sync {
    fn notify(&self, event: AgentLifecycleEvent);
}

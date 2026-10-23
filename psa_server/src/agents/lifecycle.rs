//! Internal logical agent-run notifications, independent of worker generations.

use psa_api::{AgentId, SessionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentRunOutcome {
    Completed,
    Interrupted,
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub(crate) enum AgentLifecycleEvent {
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

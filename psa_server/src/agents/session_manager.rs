//! Session manager is the task managing all sessions, starting and stopping agent workers for each session, deliverring events, etc.

use crate::agents::{AgentId, AgentState};

#[derive(Debug, Clone, Copy)]
pub enum SessionManagerMessage {
    AgentStateChanged {
        agent_id: AgentId,
        new_state: AgentState,
    },
}

async fn session_manager_worker() {}

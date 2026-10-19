//! Main logic for agent event loop.
//!
//! Each agent has a seperate tokio task managing it.

use std::collections::HashMap;

use enumset::{EnumSet, EnumSetType};

use super::Agent;
use super::AgentError;
use crate::agents::events::EventType;
use crate::db::Database;
use crate::{
    agents::{
        agent_context::AgentContext,
        events::{Event, Message, NewEvent},
    },
    db::{AddEventResult, DbError},
};

/// Current state of the agent
#[derive(Debug, EnumSetType)]
pub enum AgentState {
    /// Agent is idle and has no pending inference requests.
    ///
    /// Agent becomes idle after finishing on its own accord.
    Idle,
    /// Agent was running, but was stopped due to user interrupt, api error, or quitting
    Interrupted,
    /// Agent is currently running / ready to run
    Running,
}

/// Send a request to perform an action in the agent.
struct AgentRequest {
    /// Optionally make request only be queued if in a given agent state
    pub in_state: EnumSet<AgentState>,
    /// When the agent is in any of the given states, at the next queue point, the action will be taken.
    pub queue_state: EnumSet<AgentState>,
    /// Event type should only be certain events, like message, log, or set state
    pub event: EventType,
}

struct AgentLoopState<'a> {
    db: &'a Database,
    agent: Agent,
    context: AgentContext,
    action_queue: Vec<AgentRequest>,
}

impl AgentLoopState<'_> {
    /// Saves events to database, and syncs them with current context window state and agent state.
    async fn save_events(&mut self, events: &[NewEvent]) -> Result<Vec<Event>, AgentError> {
        let agent_id = self.agent.id;

        let mut conn = self.db.cursor().await?;
        let AddEventResult { events, new_state } = conn.add_events(agent_id, events).await?;
        for event in events.iter() {
            self.context.apply_event(event)?;
        }
        conn.commit().await?;

        if let Some(new_state) = new_state {
            self.agent.state = new_state;
        }

        Ok(events)
    }

    async fn process_queued_actions(&mut self) -> Result<Vec<Event>, AgentError> {
        let mut remove_index = Vec::new();
        let mut queued_events = Vec::new();

        for (i, request) in self.action_queue.iter().enumerate() {
            if request.queue_state.contains(self.agent.state) {
                queued_events.push(NewEvent::new(request.event.clone()));
                remove_index.push(i);
            }
        }

        for i in remove_index.iter().rev() {
            self.action_queue.remove(*i);
        }

        self.save_events(&queued_events).await
    }
}

async fn agent_loop(state: AgentLoopState<'_>) {
    loop {
        if state.agent.state == AgentState::Running {
            // steps:
            // - take request from agent context, log initial inference begin event, perform request
            // - stream output back from api to clients, do not persist anything
            //   - also wait on command queue, if interrupt occurs, write interrupted inference end event, also update status event
            // - once finished, write inference finished event
            // - parse tools, any errors record, start tool execution
            //   - if interrupt event send, interrupt tools, with a short timeout to let them finish, or record killed result
            // - persist all tool result events
            // - checks:
            //   - if agent returned no tools, call end hook, which will return some events,
            //   - if context larger then compaction threshhold, call into copmaction (pass in al events of context window, it will spit out a ton of new events, persist them)
        }
    }
}

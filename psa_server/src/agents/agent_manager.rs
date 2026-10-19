//! Main logic for agent event loop.
//!
//! Each agent has a seperate tokio task managing it.

use std::collections::VecDeque;

use tokio::sync::mpsc;

use super::Agent;
use super::AgentError;
use super::session_manager::SessionManagerMessage;
use crate::db::Database;
use crate::sessions::Session;
use crate::{
    agents::{
        agent_context::AgentContext,
        events::{Event, Message, NewEvent},
    },
    db::{AddEventResult, DbError},
};

/// Current state of the agent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Messages sent to the agent manager.
pub enum AgentManagerMessage {
    /// Stop agent if in running state
    Interrupt,
    /// Send a steering message
    SteerMessage {
        delivery: MessageDelivery,
        message: Message,
    },
    /// Internal request, just used to notify agent loop a session value has been updated.
    SessionUpdated(Session),
}

/// Determines type of steering request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDelivery {
    /// Deliver steering when agent idle or interrupted.
    Queue,
    /// Deliver steering when agent idle, interrupted, or between turn boundaries.
    Steer,
    /// Deliver steering when agent interrupted, ignore if not interrupted.
    Resume,
}

struct AgentManagerState<'a> {
    db: &'a Database,
    agent: Agent,
    context: AgentContext,
    // messages to add at next turn boundary
    steer_queue: VecDeque<Message>,
    // messages to add only during interrupted or idle states
    follow_queue: VecDeque<Message>,
    session_manager: mpsc::Sender<SessionManagerMessage>,
    sesion: Session,
}

impl AgentManagerState<'_> {
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
            self.session_manager
                .send(SessionManagerMessage::AgentStateChanged {
                    agent_id: self.agent.id,
                    new_state,
                })
                .await
                .expect("failed to send message to session manager");
        }

        Ok(events)
    }
}

async fn agent_manager_worker(
    state: AgentManagerState<'_>,
    // seperate for ownership
    msg_queue: mpsc::Receiver<AgentManagerMessage>,
) {
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
            // - if agent returned no tools, call end hook, which will return some events,
            //   - support interrupting in same manner as tools
            // - if context larger then compaction threshhold, call into copmaction (pass in al events of context window, it will spit out a ton of new events, persist them)
            //   - support interrupting in same manner as tools
            // - if messages in steer queue, save to events
        } else {
            // Idle and Interrupted just wait for steer or followup messages to come, and adds them to events, transitions state
            // additionally Interrupted waits for resume messages to come, adds to events, transitions to running
            //  - resume messages are dropped in other states
        }
    }
}

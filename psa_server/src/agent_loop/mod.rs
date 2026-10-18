//! Main agent loop

/// Current state of the agent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Agent is idle and has no pending inference requests.
    ///
    /// Agent becomes idle after finishing on its own accord.
    Idle,
    /// Agent was running, but was stopped due to user interrupt, api error, or quitting
    Interrupted,
    /// Agent is currently running / ready to run
    Running,
}

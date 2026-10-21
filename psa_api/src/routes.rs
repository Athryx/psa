use crate::{AgentId, SessionId};

pub const SESSIONS: &str = "/sessions";
pub const SESSION: &str = "/sessions/{session_id}";
pub const FORK: &str = "/sessions/{session_id}/fork";
pub const AGENTS: &str = "/sessions/{session_id}/agents";
pub const AGENT: &str = "/sessions/{session_id}/agents/{agent_id}";
pub const REQUESTS: &str = "/sessions/{session_id}/agents/{agent_id}/requests";
pub const SESSION_EVENTS: &str = "/sessions/{session_id}/events";
pub const AGENT_EVENTS: &str = "/sessions/{session_id}/agents/{agent_id}/events";
pub const PROBES: &str = "/probes";
pub const PROBE: &str = "/probes/{probe_id}";
pub const PROBES_CONNECT: &str = "/probes/connect";

pub fn session_path(route: &str, session: SessionId) -> String {
    route.replace("{session_id}", &session.0.to_string())
}

pub fn agent_path(route: &str, session: SessionId, agent: AgentId) -> String {
    session_path(route, session).replace("{agent_id}", &agent.0.to_string())
}

pub fn probe_path(id: crate::ProbeId) -> String {
    PROBE.replace("{probe_id}", &id.to_string())
}

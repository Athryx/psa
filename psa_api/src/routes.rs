use crate::{AgentDefinitionId, AgentId, ClientQueryId, SessionId};

pub const PROVIDERS: &str = "/providers";
pub const MODELS: &str = "/models";
pub const MODEL_LOOKUP: &str = "/models/lookup";
pub const MODEL_PIN: &str = "/models/pin";
pub const MODELS_RESYNC: &str = "/models/resync";

pub const AGENT_CONFIGS: &str = "/agent-configs";
pub const AGENT_CONFIG_LOOKUP: &str = "/agent-configs/lookup";
pub const AGENT_SNAPSHOT: &str = "/agent-snapshots/{definition_id}";
pub const SKILLS: &str = "/skills";

pub const SESSIONS: &str = "/sessions";
pub const SESSION: &str = "/sessions/{session_id}";
pub const FORK: &str = "/sessions/{session_id}/fork";
pub const AGENTS: &str = "/sessions/{session_id}/agents";
pub const AGENT: &str = "/sessions/{session_id}/agents/{agent_id}";
pub const REQUESTS: &str = "/sessions/{session_id}/agents/{agent_id}/requests";
pub const CLIENT_QUERY_RESPONSE: &str =
    "/sessions/{session_id}/agents/{agent_id}/queries/{query_id}/response";
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

pub fn agent_snapshot_path(id: AgentDefinitionId) -> String {
    AGENT_SNAPSHOT.replace("{definition_id}", &id.0.to_string())
}

pub fn client_query_response_path(
    session: SessionId,
    agent: AgentId,
    query: ClientQueryId,
) -> String {
    agent_path(CLIENT_QUERY_RESPONSE, session, agent).replace("{query_id}", &query.to_string())
}

pub fn probe_path(id: crate::ProbeId) -> String {
    PROBE.replace("{probe_id}", &id.to_string())
}

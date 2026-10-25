//! Shared HTTP route templates and numeric/UUID path substitution helpers.
use crate::{AgentDefinitionId, AgentId, ClientQueryId, SessionId};

/// GET the supported providers and configured-authentication flags.
pub const PROVIDERS: &str = "/providers";
/// GET model catalog entries with optional filters.
pub const MODELS: &str = "/models";
/// GET one model using flat provider/model query parameters.
pub const MODEL_LOOKUP: &str = "/models/lookup";
/// PUT a model's server-wide pin state.
pub const MODEL_PIN: &str = "/models/pin";
/// POST a manual catalog refresh for one or all providers.
pub const MODELS_RESYNC: &str = "/models/resync";

/// GET registered agent configurations or POST a batch upsert.
pub const AGENT_CONFIGS: &str = "/agent-configs";
/// POST a configuration ID to retrieve its current configuration/status.
pub const AGENT_CONFIG_LOOKUP: &str = "/agent-configs/lookup";
/// GET an immutable definition snapshot by definition ID.
pub const AGENT_SNAPSHOT: &str = "/agent-snapshots/{definition_id}";
/// GET registered skills or POST a skill archive upload.
pub const SKILLS: &str = "/skills";

/// GET root/child sessions or POST a new session.
pub const SESSIONS: &str = "/sessions";
/// GET or PATCH one session.
pub const SESSION: &str = "/sessions/{session_id}";
/// POST a session fork at a history boundary.
pub const FORK: &str = "/sessions/{session_id}/fork";
/// GET session agents or POST a new agent from a named configuration.
pub const AGENTS: &str = "/sessions/{session_id}/agents";
/// GET one agent within its session.
pub const AGENT: &str = "/sessions/{session_id}/agents/{agent_id}";
/// POST an interrupt or message request to an agent.
pub const REQUESTS: &str = "/sessions/{session_id}/agents/{agent_id}/requests";
/// POST a response to an outstanding scoped client query.
pub const CLIENT_QUERY_RESPONSE: &str =
    "/sessions/{session_id}/agents/{agent_id}/queries/{query_id}/response";
/// GET session event pages or SSE, using session-position cursors.
pub const SESSION_EVENTS: &str = "/sessions/{session_id}/events";
/// GET agent event pages or SSE, using agent-event-ID cursors.
pub const AGENT_EVENTS: &str = "/sessions/{session_id}/agents/{agent_id}/events";
/// GET durable result records newest first; `limit` defaults to one.
pub const AGENT_RESULTS: &str = "/sessions/{session_id}/agents/{agent_id}/results";
/// GET registered probes and live activity state.
pub const PROBES: &str = "/probes";
/// GET one registered probe by UUID.
pub const PROBE: &str = "/probes/{probe_id}";
/// GET the authenticated probe WebSocket upgrade endpoint.
pub const PROBES_CONNECT: &str = "/probes/connect";

/// Substitute the session ID in a route template.
pub fn session_path(route: &str, session: SessionId) -> String {
    route.replace("{session_id}", &session.0.to_string())
}

/// Substitute session and agent IDs in a route template.
pub fn agent_path(route: &str, session: SessionId, agent: AgentId) -> String {
    session_path(route, session).replace("{agent_id}", &agent.0.to_string())
}

/// Build the path for an immutable definition snapshot.
pub fn agent_snapshot_path(id: AgentDefinitionId) -> String {
    AGENT_SNAPSHOT.replace("{definition_id}", &id.0.to_string())
}

/// Build the response path for a query in its owning session and agent.
pub fn client_query_response_path(
    session: SessionId,
    agent: AgentId,
    query: ClientQueryId,
) -> String {
    agent_path(CLIENT_QUERY_RESPONSE, session, agent).replace("{query_id}", &query.to_string())
}

/// Build the discovery path for a registered probe.
pub fn probe_path(id: crate::ProbeId) -> String {
    PROBE.replace("{probe_id}", &id.to_string())
}

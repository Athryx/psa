//! Shared HTTP route templates and numeric/UUID path substitution helpers.
use crate::api::{AgentDefinitionId, AgentId, ClientQueryId, SessionId};

/// GET client-safe server configuration.
pub const CONFIG: &str = "/config";

/// GET the supported providers and configured-authentication flags.
pub const PROVIDERS: &str = "/providers";
/// PUT a provider credential or remove it without returning the secret.
pub const PROVIDER_CREDENTIAL: &str = "/providers/credential";
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
/// GET one configuration and its status, identified by `ConfigLookup` query parameters.
pub const AGENT_CONFIG_LOOKUP: &str = "/agent-configs/lookup";
/// GET an immutable definition snapshot by definition ID.
pub const AGENT_SNAPSHOT: &str = "/agent-snapshots/{definition_id}";
/// GET registered skills or POST a skill archive upload.
pub const SKILLS: &str = "/skills";
/// POST a rescan of filesystem-discovered agent configurations and skills.
pub const CONFIGS_REFRESH: &str = "/configs/refresh";

/// POST a new session.
pub const SESSIONS: &str = "/sessions";
/// POST a session query for one ordered summary page.
pub const SESSIONS_QUERY: &str = "/sessions/query";
/// GET the projects of existing sessions.
pub const PROJECTS: &str = "/projects";
/// Global summary SSE feed.
pub const GLOBAL_EVENTS: &str = "/events";
/// GET or PATCH one session.
pub const SESSION: &str = "/sessions/{session_id}";
/// POST a session fork at a history boundary.
pub const FORK: &str = "/sessions/{session_id}/fork";
/// GET session agents or POST a new agent from a named configuration.
pub const AGENTS: &str = "/sessions/{session_id}/agents";
/// GET one agent within its session.
pub const AGENT: &str = "/sessions/{session_id}/agents/{agent_id}";
/// Effective model and last inference usage at an agent event boundary.
pub const AGENT_CONTEXT_STATE: &str = "/sessions/{session_id}/agents/{agent_id}/context-state";
/// GET cumulative usage per model, including inherited calls.
pub const AGENT_USAGE: &str = "/sessions/{session_id}/agents/{agent_id}/usage";
/// POST an interrupt, message, or command record request to an agent.
pub const REQUESTS: &str = "/sessions/{session_id}/agents/{agent_id}/requests";
/// POST a command to run; responds with its result once it finishes.
pub const COMMANDS: &str = "/sessions/{session_id}/agents/{agent_id}/commands";
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
/// GET an immediate directory listing from a connected probe.
pub const PROBE_DIRECTORIES: &str = "/probes/{probe_id}/directories";
/// GET the authenticated probe WebSocket upgrade endpoint.
pub const PROBES_CONNECT: &str = "/probes/connect";
/// POST a new sandbox on a manager probe.
pub const PROBE_SANDBOXES: &str = "/probes/{probe_id}/sandboxes";
/// GET sandboxes, optionally filtered by manager probe.
pub const SANDBOXES: &str = "/sandboxes";
/// GET or DELETE one sandbox by its nested probe ID.
pub const SANDBOX: &str = "/sandboxes/{probe_id}";
/// POST to start a sandbox container.
pub const SANDBOX_START: &str = "/sandboxes/{probe_id}/start";
/// POST to stop a sandbox container.
pub const SANDBOX_STOP: &str = "/sandboxes/{probe_id}/stop";

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

/// Substitute the probe ID in a probe or sandbox route template.
pub fn probe_path(route: &str, id: crate::api::ProbeId) -> String {
    route.replace("{probe_id}", &id.to_string())
}

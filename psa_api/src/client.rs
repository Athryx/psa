//! Async HTTP/SSE client, transport errors, and probe connection URL parsing.
use crate::{
    Agent, AgentConfigInput, AgentConfigStatus, AgentDefinitionId, AgentId, AgentRequest,
    AgentResult, ApiError, ClientQueryId, ClientQueryResponse, ConfigId, CreateSession, EventsPage,
    EventsQuery, ForkSession, ForkedSession, ListSessions, NewConfiguredAgent, NewSkill,
    RequestAccepted, ResolvedAgentConfig, ResultRecord, ResultsQuery, Session, SessionId, Skill,
    StreamMessage, UpdateSession, routes,
};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::{Method, Response};
use serde::{Serialize, de::DeserializeOwned};
use std::pin::Pin;

/// Transport, server-response, or event-decoding failure for a client operation.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// HTTP transport or JSON response-body decoding failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The server returned a non-success HTTP response.
    #[error("server returned HTTP {status}: {error:?}")]
    Api {
        /// Numeric HTTP status code.
        status: u16,
        /// Structured server error, or a fallback if its body was invalid.
        error: ApiError,
    },
    /// An SSE message or typed result payload could not be decoded into its target type.
    #[error("invalid API response: {0}")]
    Json(#[from] serde_json::Error),
    /// The response was not SSE or the SSE framing/transport failed.
    #[error("invalid event stream: {0}")]
    Stream(String),
    /// The supplied base URL failed the constructor or probe URL parser checks.
    #[error("base URL must be an HTTP(S) URL without query or fragment")]
    InvalidBaseUrl,
}

/// One decoded server-sent event.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    /// SSE event ID; durable IDs are scoped resume cursors, not plain event IDs.
    pub id: String,
    /// Decoded event, snapshot, or runtime notification.
    pub message: StreamMessage,
}

/// Owned SSE stream. EOF or a resync notification requires explicit reconnection.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ClientError>> + Send>>;

/// One decoded global summary SSE notification.
#[derive(Debug, Clone)]
pub struct GlobalStreamEvent {
    /// Ephemeral revision, not a durable replay cursor.
    pub id: String,
    /// Global protocol notification.
    pub message: crate::GlobalStreamMessage,
}
/// Global summary feed; EOF/resync requires a fresh bootstrap.
pub type GlobalEventStream =
    Pin<Box<dyn Stream<Item = Result<GlobalStreamEvent, ClientError>> + Send>>;

/// Cloneable async client for PSA's trusted-client HTTP API.
///
/// Methods return [`ClientError`] for transport failures, non-success HTTP
/// responses, or invalid response bodies. Streaming may also fail while polled.
/// Every request, including SSE connections, sends the supplied client key as a
/// bearer token. This wrapper adds no retries or automatic SSE reconnections.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
    client_key: String,
}

impl Client {
    /// Create a client for an HTTP(S) server URL, optionally including a path prefix.
    /// Authenticates with `[psa].client_key` from the server's `auth.toml`.
    /// Rejects malformed URLs, unsupported schemes, queries, and fragments.
    pub fn new(base: &str, client_key: &str) -> Result<Self, ClientError> {
        Self::with_http_client(base, client_key, reqwest::Client::new())
    }

    /// Create a client using a configured HTTP transport, such as one with custom
    /// headers or timeouts. The supplied client key overrides any default
    /// Authorization header. Uses the same base-URL validation as [`Self::new`].
    pub fn with_http_client(
        base: &str,
        client_key: &str,
        http: reqwest::Client,
    ) -> Result<Self, ClientError> {
        let base = reqwest::Url::parse(base).map_err(|_| ClientError::InvalidBaseUrl)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ClientError::InvalidBaseUrl);
        }

        Ok(Self {
            http,
            base,
            client_key: client_key.to_owned(),
        })
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut url = self.base.clone();
        url.set_path(&format!(
            "{}{}",
            self.base.path().trim_end_matches('/'),
            path
        ));

        self.http.request(method, url).bearer_auth(&self.client_key)
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ClientError> {
        let response = self.request(method, path).json(body).send().await?;
        let response = require_success(response).await?;

        Ok(response.json().await?)
    }

    async fn send_no_content(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<(), ClientError> {
        let response = self.request(method, path).json(body).send().await?;
        require_success(response).await?;
        Ok(())
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, ClientError> {
        let response = self.request(Method::GET, path).query(query).send().await?;
        let response = require_success(response).await?;

        Ok(response.json().await?)
    }

    /// Fetch the server's client-safe configuration.
    pub async fn get_config(&self) -> Result<crate::ClientConfig, ClientError> {
        self.get(routes::CONFIG, &()).await
    }

    /// List supported providers and whether each has configured credentials.
    /// This does not validate credentials or model access.
    pub async fn list_providers(&self) -> Result<Vec<crate::ProviderInfo>, ClientError> {
        self.get(routes::PROVIDERS, &()).await
    }

    /// Set or remove a provider credential. Returns only its configured status.
    pub async fn set_provider_credential(
        &self,
        request: &crate::SetProviderCredential,
    ) -> Result<crate::ProviderInfo, ClientError> {
        self.send(Method::PUT, routes::PROVIDER_CREDENTIAL, request)
            .await
    }

    /// List catalog models matching the AND-combined filters in `query`.
    /// Reads cached metadata; does not trigger a catalog refresh.
    pub async fn list_models(
        &self,
        query: &crate::ListModels,
    ) -> Result<Vec<crate::ModelInfo>, ClientError> {
        self.get(routes::MODELS, query).await
    }

    /// Look up cached catalog metadata by provider and API model name.
    /// Returns an API error with HTTP 404 if the model is unknown.
    pub async fn model_info(
        &self,
        model: &llm_api::providers::Model,
    ) -> Result<crate::ModelInfo, ClientError> {
        self.get(
            routes::MODEL_LOOKUP,
            &crate::ModelLookup {
                provider: model.provider,
                model: model.api_name.clone(),
            },
        )
        .await
    }

    /// Set a model's server-wide pin, which survives catalog refreshes and restarts.
    /// Pinning an unknown model returns HTTP 404; unpinning one returns `None`.
    pub async fn set_model_pinned(
        &self,
        request: &crate::PinModel,
    ) -> Result<Option<crate::ModelInfo>, ClientError> {
        self.send(Method::PUT, routes::MODEL_PIN, request).await
    }

    /// Refresh the selected provider's catalog, or all providers when omitted.
    /// Waits for every requested refresh; returns an API error if any fails.
    /// Successful provider updates are retained even if another provider fails.
    /// Success has no response body.
    pub async fn resync_models(&self, request: &crate::ResyncModels) -> Result<(), ClientError> {
        self.send_no_content(Method::POST, routes::MODELS_RESYNC, request)
            .await
    }

    /// List registered probes, including disconnected probes and live activity state.
    pub async fn list_probes(&self) -> Result<Vec<crate::Probe>, ClientError> {
        self.get(routes::PROBES, &()).await
    }

    /// Fetch a registered probe by ID, including its live activity state.
    /// Returns HTTP 404 if the probe is unknown.
    pub async fn get_probe(&self, id: crate::ProbeId) -> Result<crate::Probe, ClientError> {
        self.get(&routes::probe_path(id), &()).await
    }

    /// List immediate children of an absolute directory on a connected probe.
    /// Paths are passed unchanged; entry metadata does not follow symlinks.
    pub async fn list_probe_dir(
        &self,
        id: crate::ProbeId,
        path: &str,
    ) -> Result<Vec<crate::probes::DirectoryEntry>, ClientError> {
        self.get(&routes::probe_directories_path(id), &[("path", path)])
            .await
    }

    /// Atomically create or replace named configurations and return their statuses.
    /// Omitted component lists inherit server defaults; explicit lists replace them.
    /// Passing normalized [`crate::AgentConfig`] values keeps all lists explicit.
    pub async fn upsert_agent_configs<I: Clone + Into<AgentConfigInput>>(
        &self,
        configs: &[I],
    ) -> Result<Vec<AgentConfigStatus>, ClientError> {
        let configs: Vec<AgentConfigInput> = configs.iter().cloned().map(Into::into).collect();
        self.send(Method::POST, routes::AGENT_CONFIGS, &configs)
            .await
    }

    /// Refresh discovery, then list all configurations and dependency statuses.
    /// Global discovery is always refreshed; `Some(refresh_session_id)` also
    /// refreshes that session's selected project. This is not a result filter.
    pub async fn refresh_and_list_agent_configs(
        &self,
        refresh_session_id: Option<SessionId>,
    ) -> Result<Vec<AgentConfigStatus>, ClientError> {
        self.get(
            routes::AGENT_CONFIGS,
            &crate::RefreshAndListAgentConfigs { refresh_session_id },
        )
        .await
    }

    /// Fetch the current named configuration and its dependency status.
    /// Returns HTTP 404 if the configuration is unknown.
    pub async fn get_agent_config(&self, id: &ConfigId) -> Result<AgentConfigStatus, ClientError> {
        self.send(Method::POST, routes::AGENT_CONFIG_LOOKUP, id)
            .await
    }

    /// Fetch an immutable definition snapshot and its source policies/references.
    /// Returns `None` if the snapshot ID is unknown.
    pub async fn get_resolved_agent_config(
        &self,
        id: AgentDefinitionId,
    ) -> Result<Option<ResolvedAgentConfig>, ClientError> {
        self.get(&routes::agent_snapshot_path(id), &()).await
    }

    /// Upload a skill's tar archive and create or replace its named registration.
    /// The server validates the archive and `SKILL.md` metadata before registration.
    pub async fn upload_skill(&self, request: &NewSkill) -> Result<Skill, ClientError> {
        self.send(Method::POST, routes::SKILLS, request).await
    }

    /// Refresh discovery, then list all registered skills.
    /// Global discovery is always refreshed; `Some(refresh_session_id)` also
    /// refreshes that session's selected project. This is not a result filter.
    pub async fn refresh_and_list_skills(
        &self,
        refresh_session_id: Option<SessionId>,
    ) -> Result<Vec<Skill>, ClientError> {
        self.get(
            routes::SKILLS,
            &crate::RefreshAndListSkills { refresh_session_id },
        )
        .await
    }

    /// Create a session with a registered probe and optional model configuration.
    /// Omitted model configuration uses the server's configured session defaults.
    /// The probe may be offline; paths refer to its filesystem.
    pub async fn create_session(&self, request: &CreateSession) -> Result<Session, ClientError> {
        self.send(Method::POST, routes::SESSIONS, request).await
    }

    /// List direct children of `parent`, or root sessions when `parent` is `None`.
    pub async fn list_sessions(
        &self,
        parent: Option<SessionId>,
    ) -> Result<Vec<Session>, ClientError> {
        self.get(routes::SESSIONS, &ListSessions { parent }).await
    }

    /// Fetch a session and its live busy state. Unknown IDs return HTTP 404.
    pub async fn get_session(&self, id: SessionId) -> Result<Session, ClientError> {
        self.get(&routes::session_path(routes::SESSION, id), &())
            .await
    }

    /// Apply the supplied session metadata changes, leaving omitted fields unchanged.
    /// Configured session-update hooks run for the session's agents.
    pub async fn update_session(
        &self,
        id: SessionId,
        request: &UpdateSession,
    ) -> Result<Session, ClientError> {
        self.send(
            Method::PATCH,
            &routes::session_path(routes::SESSION, id),
            request,
        )
        .await
    }

    /// Create a child session with inherited history through the requested boundary.
    /// Returns the new session and forked agents, with source IDs in `parent_id`.
    pub async fn fork_session(
        &self,
        id: SessionId,
        request: &ForkSession,
    ) -> Result<ForkedSession, ClientError> {
        self.send(
            Method::POST,
            &routes::session_path(routes::FORK, id),
            request,
        )
        .await
    }

    /// Create an agent from a named configuration, pinning its resolved definition.
    /// The requested model must satisfy the configuration's model-selection policy.
    pub async fn create_agent(
        &self,
        session: SessionId,
        request: &NewConfiguredAgent,
    ) -> Result<Agent, ClientError> {
        self.send(
            Method::POST,
            &routes::session_path(routes::AGENTS, session),
            request,
        )
        .await
    }

    /// List agents belonging to the session, including state and cumulative usage.
    pub async fn list_agents(&self, session: SessionId) -> Result<Vec<Agent>, ClientError> {
        self.get(&routes::session_path(routes::AGENTS, session), &())
            .await
    }

    /// Fetch an agent in the specified session.
    /// Unknown agents or a mismatched session return HTTP 404.
    pub async fn get_agent(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Agent, ClientError> {
        self.get(&routes::agent_path(routes::AGENT, session, agent), &())
            .await
    }

    /// Fetch the effective model and last inference usage for one agent.
    pub async fn get_agent_status(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<crate::AgentStatusSnapshot, ClientError> {
        self.get(
            &routes::agent_path(routes::AGENT_STATUS, session, agent),
            &(),
        )
        .await
    }

    /// Submit an interrupt or message-delivery request and return its acceptance ID.
    /// Acceptance does not mean inference or tool execution has finished.
    pub async fn submit_request(
        &self,
        session: SessionId,
        agent: AgentId,
        request: &AgentRequest,
    ) -> Result<RequestAccepted, ClientError> {
        self.send(
            Method::POST,
            &routes::agent_path(routes::REQUESTS, session, agent),
            request,
        )
        .await
    }

    /// Send attributed context using normal steering delivery.
    /// `source: None` leaves sender attribution absent; this does not create an
    /// agent or establish parentage.
    pub async fn send_subagent_message(
        &self,
        session: SessionId,
        agent: AgentId,
        source: Option<crate::AgentMessageSource>,
        content: Vec<llm_api::api::ContentBlock>,
    ) -> Result<RequestAccepted, ClientError> {
        self.submit_request(
            session,
            agent,
            &AgentRequest::SubagentMessage { source, content },
        )
        .await
    }

    /// Respond to an outstanding runtime query in the specified agent/session.
    /// The first valid response wins; invalid responses leave the query pending.
    /// Success has no response body.
    pub async fn respond_to_client_query(
        &self,
        session: SessionId,
        agent: AgentId,
        query: ClientQueryId,
        response: &ClientQueryResponse,
    ) -> Result<(), ClientError> {
        self.send_no_content(
            Method::POST,
            &routes::client_query_response_path(session, agent, query),
            response,
        )
        .await
    }

    /// Fetch up to `limit` raw durable results, newest first, from an agent's lineage.
    /// Retains each result's text-or-JSON tag and durable event provenance.
    pub async fn results(
        &self,
        session: SessionId,
        agent: AgentId,
        limit: u32,
    ) -> Result<Vec<ResultRecord<AgentResult>>, ClientError> {
        self.get(
            &routes::agent_path(routes::AGENT_RESULTS, session, agent),
            &ResultsQuery { limit },
        )
        .await
    }

    /// Fetch the newest raw durable result, or `None` if the agent has no results.
    /// Use this rather than a typed helper to preserve the [`AgentResult`] wire tag.
    pub async fn last_result_raw(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Option<ResultRecord<AgentResult>>, ClientError> {
        Ok(self.results(session, agent, 1).await?.into_iter().next())
    }

    /// Fetch the newest durable result and decode its inner payload into `T`.
    /// Text results decode as strings, not JSON source. A type mismatch returns
    /// [`ClientError::Json`]; no result returns `None`. Provenance is retained.
    /// Use [`Self::last_result_raw`] to retain the original text-or-JSON tag.
    pub async fn last_result<T: DeserializeOwned>(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Option<ResultRecord<T>>, ClientError> {
        self.last_result_raw(session, agent)
            .await?
            .map(|record| record.decode::<T>())
            .transpose()
            .map_err(ClientError::from)
    }

    /// Fetch up to `limit` durable results newest first and decode their payloads.
    /// Retains provenance and returns [`ClientError::Json`] if any payload does not
    /// match `T`, rather than skipping that result. Text results decode as strings.
    /// Use [`Self::results`] to retrieve the raw text-or-JSON representations.
    pub async fn last_results<T: DeserializeOwned>(
        &self,
        session: SessionId,
        agent: AgentId,
        limit: u32,
    ) -> Result<Vec<ResultRecord<T>>, ClientError> {
        self.results(session, agent, limit)
            .await?
            .into_iter()
            .map(|record| record.decode::<T>().map_err(ClientError::from))
            .collect()
    }

    /// Fetch ordered summaries. Subscribe globally first and reconcile updates
    /// newer than each page's revision; deduplicate by session ID.
    pub async fn query_sessions(
        &self,
        query: &crate::SessionListQuery,
    ) -> Result<crate::SessionsPage, ClientError> {
        self.get(routes::SESSION_QUERY, query).await
    }

    /// Subscribe to ephemeral global summaries. Wait for Ready before fetching
    /// pages. Reconnect/overflow requires clearing the old bootstrap and reloading.
    pub async fn stream_global_events(
        &self,
        query: &crate::GlobalEventsQuery,
        last_event_id: Option<&str>,
    ) -> Result<GlobalEventStream, ClientError> {
        let mut request = self
            .request(Method::GET, routes::GLOBAL_EVENTS)
            .header("Accept", "text/event-stream")
            .query(query);
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id);
        }
        let response = require_success(request.send().await?).await?;
        if !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            return Err(ClientError::Stream("expected text/event-stream".into()));
        }
        let stream = response.bytes_stream().eventsource().map(|event| {
            let event = event.map_err(|error| ClientError::Stream(error.to_string()))?;
            Ok(GlobalStreamEvent {
                id: event.id,
                message: serde_json::from_str(&event.data)?,
            })
        });
        Ok(Box::pin(stream))
    }

    /// Fetch one event page for a session or, with `Some(agent)`, one agent.
    /// Session cursors are session positions; agent cursors are event IDs.
    /// Requests `Accept: application/json`; paginate using the returned `next_after`.
    pub async fn events(
        &self,
        session: SessionId,
        agent: Option<AgentId>,
        query: &EventsQuery,
    ) -> Result<EventsPage, ClientError> {
        let response = self
            .request(Method::GET, &events_path(session, agent))
            .header("Accept", "application/json")
            .query(query)
            .send()
            .await?;
        let response = require_success(response).await?;

        Ok(response.json().await?)
    }

    /// Stream session events, or one agent's events when `agent` is `Some`.
    /// Requests `Accept: text/event-stream`; `last_event_id`, when supplied,
    /// overrides `query.after` and must belong to the same session/agent scope.
    ///
    /// Reconnect explicitly with the last durable ID after EOF/resync. Snapshots
    /// replace provisional state; committed items should be upserted by identity.
    /// A `query.through` bound produces a finite stream without following updates.
    pub async fn stream_events(
        &self,
        session: SessionId,
        agent: Option<AgentId>,
        query: &EventsQuery,
        last_event_id: Option<&str>,
    ) -> Result<EventStream, ClientError> {
        let mut request = self
            .request(Method::GET, &events_path(session, agent))
            .header("Accept", "text/event-stream")
            .query(query);
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id);
        }

        let response = require_success(request.send().await?).await?;
        if !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            return Err(ClientError::Stream("expected text/event-stream".into()));
        }

        let stream = response.bytes_stream().eventsource().map(|event| {
            let event = event.map_err(|error| ClientError::Stream(error.to_string()))?;
            Ok(StreamEvent {
                id: event.id,
                message: serde_json::from_str(&event.data)?,
            })
        });

        Ok(Box::pin(stream))
    }
}

fn events_path(session: SessionId, agent: Option<AgentId>) -> String {
    match agent {
        Some(agent) => routes::agent_path(routes::AGENT_EVENTS, session, agent),
        None => routes::session_path(routes::SESSION_EVENTS, session),
    }
}

async fn require_success(response: Response) -> Result<Response, ClientError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let error = response.json().await.unwrap_or(ApiError {
        code: "http_error".into(),
        message: "server returned an invalid error response".into(),
    });
    Err(ClientError::Api { status, error })
}

/// Build a probe WebSocket connection URL, preserving any server path prefix.
///
/// Accepts HTTP(S) or WS(S), converts to WS(S), and appends the probe connect path.
/// Rejects malformed URLs, unsupported schemes, credentials, missing hosts,
/// queries, and fragments with [`ClientError::InvalidBaseUrl`].
pub fn parse_probe_server_url(value: &str) -> Result<String, ClientError> {
    let mut url = reqwest::Url::parse(value).map_err(|_| ClientError::InvalidBaseUrl)?;
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(ClientError::InvalidBaseUrl);
    }
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => return Err(ClientError::InvalidBaseUrl),
    };
    url.set_scheme(scheme)
        .map_err(|_| ClientError::InvalidBaseUrl)?;
    url.set_path(&format!(
        "{}{}",
        url.path().trim_end_matches('/'),
        routes::PROBES_CONNECT
    ));
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_requests_include_bearer_key_and_preserve_base_prefix() {
        let client = Client::new("https://example.invalid/prefix/", "test-client-key").unwrap();
        let client = client.clone();

        for (method, path) in [
            (Method::GET, routes::PROVIDERS.to_owned()),
            (Method::POST, routes::SESSIONS.to_owned()),
            (
                Method::PATCH,
                routes::session_path(routes::SESSION, SessionId(1)),
            ),
            (Method::GET, events_path(SessionId(1), None)),
            (Method::GET, events_path(SessionId(1), Some(AgentId(2)))),
        ] {
            let request = client.request(method.clone(), &path).build().unwrap();
            assert_eq!(request.method(), method);
            assert_eq!(
                request.url().as_str(),
                format!("https://example.invalid/prefix{path}")
            );
            assert_eq!(request.headers()["authorization"], "Bearer test-client-key");
        }
    }

    #[test]
    fn configured_transport_uses_supplied_client_key_for_json_and_sse() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("authorization", "Bearer different-key".parse().unwrap());
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let client =
            Client::with_http_client("https://example.invalid", "test-client-key", http).unwrap();

        for accept in ["application/json", "text/event-stream"] {
            let request = client
                .request(Method::GET, &events_path(SessionId(1), None))
                .header("accept", accept)
                .build()
                .unwrap();
            assert_eq!(request.headers()["authorization"], "Bearer test-client-key");
            assert_eq!(request.headers()["accept"], accept);
        }
    }
}

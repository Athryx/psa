//! Async HTTP/SSE client, transport errors, and probe connection URL parsing.
use crate::api::{
    Agent, AgentConfigInput, AgentConfigStatus, AgentDefinitionId, AgentId, AgentRequest,
    AgentResult, ClientQueryId, ClientQueryResponse, ConfigId, CreateAgent, CreateSession,
    EventsPage, EventsQuery, ForkSession, ForkedSession, NewSkill, ProjectId, RequestAccepted,
    ResolvedAgentConfig, ResultRecord, ResultsQuery, Session, SessionId, SessionsPage,
    SessionsQuery, Skill, StreamMessage, UpdateSession, routes,
};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::{Method, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::pin::Pin;

/// Structured error returned by an API endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    /// Machine-readable error code.
    pub code: String,
    /// Human-readable error description.
    pub message: String,
}

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
    pub message: crate::api::GlobalStreamMessage,
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
    pub async fn get_config(&self) -> Result<crate::api::ClientConfig, ClientError> {
        self.get(routes::CONFIG, &()).await
    }

    /// List supported providers and whether each has configured credentials.
    /// This does not validate credentials or model access.
    pub async fn list_providers(&self) -> Result<Vec<crate::api::ProviderInfo>, ClientError> {
        self.get(routes::PROVIDERS, &()).await
    }

    /// Set or remove a provider credential. Returns only its configured status.
    pub async fn set_provider_credential(
        &self,
        request: &crate::api::SetProviderCredential,
    ) -> Result<crate::api::ProviderInfo, ClientError> {
        self.send(Method::PUT, routes::PROVIDER_CREDENTIAL, request)
            .await
    }

    /// Begin signing in to a provider. Show the returned code and URL to the
    /// user, then poll [`Self::get_provider_login`] until it finishes.
    pub async fn start_provider_login(
        &self,
        request: &crate::api::StartProviderLogin,
    ) -> Result<crate::api::ProviderLogin, ClientError> {
        self.send(Method::POST, routes::PROVIDER_LOGINS, request)
            .await
    }

    /// Fetch a sign-in's current status.
    pub async fn get_provider_login(
        &self,
        id: uuid::Uuid,
    ) -> Result<crate::api::ProviderLogin, ClientError> {
        self.get(&routes::provider_login_path(id), &()).await
    }

    /// Cancel a pending sign-in.
    pub async fn cancel_provider_login(&self, id: uuid::Uuid) -> Result<(), ClientError> {
        let response = self
            .request(Method::DELETE, &routes::provider_login_path(id))
            .send()
            .await?;
        require_success(response).await?;
        Ok(())
    }

    /// List catalog models matching the AND-combined filters in `query`.
    /// Reads cached metadata; does not trigger a catalog refresh.
    pub async fn list_models(
        &self,
        query: &crate::api::ListModels,
    ) -> Result<Vec<crate::api::ModelInfo>, ClientError> {
        self.get(routes::MODELS, query).await
    }

    /// Look up cached catalog metadata by provider and API model name.
    /// Returns an API error with HTTP 404 if the model is unknown.
    pub async fn get_model(
        &self,
        model: &llm_api::providers::Model,
    ) -> Result<crate::api::ModelInfo, ClientError> {
        self.get(
            routes::MODEL_LOOKUP,
            &crate::api::ModelLookup {
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
        request: &crate::api::PinModel,
    ) -> Result<Option<crate::api::ModelInfo>, ClientError> {
        self.send(Method::PUT, routes::MODEL_PIN, request).await
    }

    /// Refresh the selected provider's catalog, or all providers when omitted.
    /// Waits for every requested refresh; returns an API error if any fails.
    /// Successful provider updates are retained even if another provider fails.
    /// Success has no response body.
    pub async fn resync_models(
        &self,
        request: &crate::api::ResyncModels,
    ) -> Result<(), ClientError> {
        self.send_no_content(Method::POST, routes::MODELS_RESYNC, request)
            .await
    }

    /// List registered probes, including disconnected probes and live activity state.
    pub async fn list_probes(&self) -> Result<Vec<crate::api::Probe>, ClientError> {
        self.get(routes::PROBES, &()).await
    }

    /// Fetch a registered probe by ID, including its live activity state.
    /// Returns HTTP 404 if the probe is unknown.
    pub async fn get_probe(
        &self,
        id: crate::api::ProbeId,
    ) -> Result<crate::api::Probe, ClientError> {
        self.get(&routes::probe_path(routes::PROBE, id), &()).await
    }

    /// List immediate children of an absolute directory on a connected probe.
    /// Paths are passed unchanged; entry metadata does not follow symlinks.
    pub async fn list_probe_directory(
        &self,
        id: crate::api::ProbeId,
        path: &str,
    ) -> Result<Vec<crate::api::probes::DirectoryEntry>, ClientError> {
        self.get(
            &routes::probe_path(routes::PROBE_DIRECTORIES, id),
            &[("path", path)],
        )
        .await
    }

    /// List sandboxes with their container state, optionally for one manager probe.
    pub async fn list_sandboxes(
        &self,
        manager_id: Option<crate::api::ProbeId>,
    ) -> Result<Vec<crate::api::Sandbox>, ClientError> {
        self.get(routes::SANDBOXES, &crate::api::ListSandboxes { manager_id })
            .await
    }

    /// Fetch one sandbox by its nested probe ID.
    pub async fn get_sandbox(
        &self,
        id: crate::api::ProbeId,
    ) -> Result<crate::api::Sandbox, ClientError> {
        self.get(&routes::probe_path(routes::SANDBOX, id), &())
            .await
    }

    /// Create a sandbox container on a connected manager probe.
    /// The image defaults to the server's configured sandbox image.
    pub async fn create_sandbox(
        &self,
        manager_id: crate::api::ProbeId,
        request: &crate::api::CreateSandbox,
    ) -> Result<crate::api::Sandbox, ClientError> {
        let path = routes::probe_path(routes::PROBE_SANDBOXES, manager_id);
        self.send(Method::POST, &path, request).await
    }

    /// Start a sandbox container; fails with `manager_offline` if its manager is disconnected.
    pub async fn start_sandbox(
        &self,
        id: crate::api::ProbeId,
    ) -> Result<crate::api::Sandbox, ClientError> {
        let path = routes::probe_path(routes::SANDBOX_START, id);
        self.send(Method::POST, &path, &()).await
    }

    /// Stop a sandbox container; fails with `manager_offline` if its manager is disconnected.
    pub async fn stop_sandbox(
        &self,
        id: crate::api::ProbeId,
    ) -> Result<crate::api::Sandbox, ClientError> {
        let path = routes::probe_path(routes::SANDBOX_STOP, id);
        self.send(Method::POST, &path, &()).await
    }

    /// Remove a sandbox container and its record; sessions on its probe remain.
    pub async fn delete_sandbox(&self, id: crate::api::ProbeId) -> Result<(), ClientError> {
        let response = self
            .request(Method::DELETE, &routes::probe_path(routes::SANDBOX, id))
            .send()
            .await?;
        require_success(response).await?;
        Ok(())
    }

    /// Atomically create or replace named configurations and return their statuses.
    /// Omitted component lists inherit server defaults; explicit lists replace them.
    /// Passing normalized [`crate::api::AgentConfig`] values keeps all lists explicit.
    pub async fn upsert_agent_configs<I: Clone + Into<AgentConfigInput>>(
        &self,
        configs: &[I],
    ) -> Result<Vec<AgentConfigStatus>, ClientError> {
        let configs: Vec<AgentConfigInput> = configs.iter().cloned().map(Into::into).collect();
        self.send(Method::POST, routes::AGENT_CONFIGS, &configs)
            .await
    }

    /// List all registered configurations and their dependency statuses.
    /// Does not rescan the filesystem; see [`Self::refresh_configs`].
    pub async fn list_agent_configs(&self) -> Result<Vec<AgentConfigStatus>, ClientError> {
        self.get(routes::AGENT_CONFIGS, &()).await
    }

    /// Fetch the current named configuration and its dependency status.
    /// Returns HTTP 404 if the configuration is unknown.
    pub async fn get_agent_config(&self, id: &ConfigId) -> Result<AgentConfigStatus, ClientError> {
        self.get(
            routes::AGENT_CONFIG_LOOKUP,
            &crate::api::ConfigLookup::from(id),
        )
        .await
    }

    /// Fetch an immutable definition snapshot and its source policies/references.
    /// Returns HTTP 404 if the snapshot ID is unknown.
    pub async fn get_resolved_agent_config(
        &self,
        id: AgentDefinitionId,
    ) -> Result<ResolvedAgentConfig, ClientError> {
        self.get(&routes::agent_snapshot_path(id), &()).await
    }

    /// Upload a skill's tar archive and create or replace its named registration.
    /// The server validates the archive and `SKILL.md` metadata before registration.
    pub async fn upload_skill(&self, request: &NewSkill) -> Result<Skill, ClientError> {
        self.send(Method::POST, routes::SKILLS, request).await
    }

    /// List all registered skills. Does not rescan the filesystem; see
    /// [`Self::refresh_configs`].
    pub async fn list_skills(&self) -> Result<Vec<Skill>, ClientError> {
        self.get(routes::SKILLS, &()).await
    }

    /// Rescan the server's global configuration directory and the `.agents`
    /// directory of each listed session, finding a session's directory on
    /// first use. Waits for the rescan; success has no response body.
    pub async fn refresh_configs(&self, session_ids: &[SessionId]) -> Result<(), ClientError> {
        let request = crate::api::RefreshConfigs {
            session_ids: session_ids.to_vec(),
        };
        self.send_no_content(Method::POST, routes::CONFIGS_REFRESH, &request)
            .await
    }

    /// Find and rescan the `.agents` directory a session in `working_directory`
    /// on `probe_id` would use, before the session exists. Globals are
    /// rescanned too.
    pub async fn discover_configs(
        &self,
        probe_id: crate::api::ProbeId,
        working_directory: &std::path::Path,
    ) -> Result<crate::api::DiscoveredConfigs, ClientError> {
        let request = crate::api::DiscoverConfigs {
            probe_id,
            working_directory: working_directory.to_path_buf(),
        };
        self.send(Method::POST, routes::CONFIGS_DISCOVER, &request)
            .await
    }

    /// Create a session with a registered probe and optional model configuration.
    /// Omitted model configuration uses the server's configured session defaults.
    /// The probe may be offline; paths refer to its filesystem.
    pub async fn create_session(&self, request: &CreateSession) -> Result<Session, ClientError> {
        self.send(Method::POST, routes::SESSIONS, request).await
    }

    /// Fetch one page of session summaries matching `query`, newest update first.
    /// To keep a live list, subscribe with [`Self::stream_global_events`] first and
    /// apply updates newer than each page's revision, deduplicating by session ID.
    pub async fn list_sessions(&self, query: &SessionsQuery) -> Result<SessionsPage, ClientError> {
        self.send(Method::POST, routes::SESSIONS_QUERY, query).await
    }

    /// List the distinct projects of existing sessions, sorted. Global sessions
    /// have no project and are not represented.
    pub async fn list_projects(&self) -> Result<Vec<ProjectId>, ClientError> {
        self.get(routes::PROJECTS, &()).await
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
    /// Returns the new session and forked agents, with source IDs in `forked_from`.
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
        request: &CreateAgent,
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

    /// Fetch the components each of the agent's bundles installed. The server
    /// installs them if the agent has not run since it started.
    pub async fn get_agent_components(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<crate::api::AgentComponents, ClientError> {
        self.get(
            &routes::agent_path(routes::AGENT_COMPONENTS, session, agent),
            &(),
        )
        .await
    }

    /// Fetch the effective model and last inference usage for one agent.
    pub async fn get_agent_context_state(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<crate::api::AgentContextState, ClientError> {
        self.get(
            &routes::agent_path(routes::AGENT_CONTEXT_STATE, session, agent),
            &(),
        )
        .await
    }

    /// Fetch cumulative usage per model for one agent, sorted by model slug.
    /// The agent's `usage` holds the combined total.
    pub async fn get_agent_model_usage(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Vec<crate::api::ModelUsage>, ClientError> {
        self.get(
            &routes::agent_path(routes::AGENT_USAGE, session, agent),
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

    /// Request interruption of the agent's current run.
    pub async fn interrupt(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<RequestAccepted, ClientError> {
        self.submit_request(session, agent, &AgentRequest::Interrupt)
            .await
    }

    /// Send a human message. Use [`Self::submit_request`] with
    /// [`AgentRequest::Message`] to attribute it to a sending agent.
    pub async fn send_message(
        &self,
        session: SessionId,
        agent: AgentId,
        delivery: crate::api::MessageDelivery,
        content: Vec<llm_api::api::ContentBlock>,
    ) -> Result<RequestAccepted, ClientError> {
        let request = AgentRequest::Message {
            delivery,
            content,
            source: None,
        };
        self.submit_request(session, agent, &request).await
    }

    /// Run a command with the agent's installed command hooks and wait for it to
    /// finish. The server rejects commands no installed bundle supports, and
    /// records the command and its result in the agent's transcript. A command
    /// queued behind a run keeps this request open until it executes.
    pub async fn send_command(
        &self,
        session: SessionId,
        agent: AgentId,
        delivery: crate::api::CommandDelivery,
        command: crate::api::AgentCommand,
    ) -> Result<crate::api::CommandResult, ClientError> {
        self.send(
            Method::POST,
            &routes::agent_path(routes::COMMANDS, session, agent),
            &crate::api::RunCommand { delivery, command },
        )
        .await
    }

    /// Record a finished client-side slash command in the agent's transcript.
    pub async fn record_command(
        &self,
        session: SessionId,
        agent: AgentId,
        record: crate::api::events::CommandRecord,
    ) -> Result<RequestAccepted, ClientError> {
        self.submit_request(session, agent, &AgentRequest::RecordCommand(record))
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

    /// Fetch up to `limit` durable results, newest first, from an agent's lineage.
    /// Each keeps its text-or-JSON tag and event provenance.
    pub async fn list_results(
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

    /// Fetch the newest durable result, or `None` if the agent has no results.
    pub async fn get_last_result(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Option<ResultRecord<AgentResult>>, ClientError> {
        Ok(self
            .list_results(session, agent, 1)
            .await?
            .into_iter()
            .next())
    }

    /// [`Self::get_last_result`], decoding the payload into `T`. Text results
    /// decode as JSON strings. A payload that does not match `T` is
    /// [`ClientError::Json`].
    pub async fn get_last_result_as<T: DeserializeOwned>(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Option<ResultRecord<T>>, ClientError> {
        self.get_last_result(session, agent)
            .await?
            .map(|record| record.decode::<T>())
            .transpose()
            .map_err(ClientError::from)
    }

    /// [`Self::list_results`], decoding each payload into `T`. Fails with
    /// [`ClientError::Json`] if any payload does not match, rather than skipping it.
    pub async fn list_results_as<T: DeserializeOwned>(
        &self,
        session: SessionId,
        agent: AgentId,
        limit: u32,
    ) -> Result<Vec<ResultRecord<T>>, ClientError> {
        self.list_results(session, agent, limit)
            .await?
            .into_iter()
            .map(|record| record.decode::<T>().map_err(ClientError::from))
            .collect()
    }

    /// Subscribe to ephemeral global summaries. Wait for Ready before fetching
    /// pages. Reconnect/overflow requires clearing the old bootstrap and reloading.
    pub async fn stream_global_events(
        &self,
        query: &crate::api::GlobalEventsQuery,
        last_event_id: Option<&str>,
    ) -> Result<GlobalEventStream, ClientError> {
        let request = self
            .request(Method::GET, routes::GLOBAL_EVENTS)
            .query(query);
        let stream = open_event_stream(request, last_event_id).await?;
        Ok(Box::pin(stream.map(|event| {
            let (id, message) = event?;
            Ok(GlobalStreamEvent { id, message })
        })))
    }

    /// Fetch one event page for a session or, with `Some(agent)`, one agent.
    /// Session cursors are session positions; agent cursors are event IDs.
    /// Requests `Accept: application/json`; paginate using the returned `next_after`.
    pub async fn list_events(
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
        let request = self
            .request(Method::GET, &events_path(session, agent))
            .query(query);
        let stream = open_event_stream(request, last_event_id).await?;
        Ok(Box::pin(stream.map(|event| {
            let (id, message) = event?;
            Ok(StreamEvent { id, message })
        })))
    }
}

fn events_path(session: SessionId, agent: Option<AgentId>) -> String {
    match agent {
        Some(agent) => routes::agent_path(routes::AGENT_EVENTS, session, agent),
        None => routes::session_path(routes::SESSION_EVENTS, session),
    }
}

/// Open an SSE response and decode each event's data as `T`, paired with its ID.
async fn open_event_stream<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    last_event_id: Option<&str>,
) -> Result<impl Stream<Item = Result<(String, T), ClientError>> + Send + use<T>, ClientError> {
    let mut request = request.header("Accept", "text/event-stream");
    if let Some(id) = last_event_id {
        request = request.header("Last-Event-ID", id);
    }
    let response = require_success(request.send().await?).await?;
    let is_event_stream = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !is_event_stream {
        return Err(ClientError::Stream("expected text/event-stream".into()));
    }

    Ok(response.bytes_stream().eventsource().map(|event| {
        let event = event.map_err(|error| ClientError::Stream(error.to_string()))?;
        Ok((event.id, serde_json::from_str(&event.data)?))
    }))
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

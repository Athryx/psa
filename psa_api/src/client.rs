use crate::{
    Agent, AgentDefinition, AgentDefinitionId, AgentId, AgentRequest, ApiError, ClientQueryId,
    ClientQueryResponse, CreateAgent, CreateAgentDefinition, CreateSession, EventsPage,
    EventsQuery, ForkSession, ForkedSession, ListSessions, RequestAccepted, Session, SessionId,
    StreamMessage, UpdateAgentDefinition, UpdateSession, routes,
};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::{Method, Response};
use serde::{Serialize, de::DeserializeOwned};
use std::pin::Pin;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server returned HTTP {status}: {error:?}")]
    Api { status: u16, error: ApiError },
    #[error("invalid API response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid event stream: {0}")]
    Stream(String),
    #[error("base URL must be an HTTP(S) URL without query or fragment")]
    InvalidBaseUrl,
}

#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub id: String,
    pub message: StreamMessage,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ClientError>> + Send>>;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
}

impl Client {
    pub fn new(base: &str) -> Result<Self, ClientError> {
        Self::with_http_client(base, reqwest::Client::new())
    }

    pub fn with_http_client(base: &str, http: reqwest::Client) -> Result<Self, ClientError> {
        let base = reqwest::Url::parse(base).map_err(|_| ClientError::InvalidBaseUrl)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ClientError::InvalidBaseUrl);
        }

        Ok(Self { http, base })
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut url = self.base.clone();
        url.set_path(&format!(
            "{}{}",
            self.base.path().trim_end_matches('/'),
            path
        ));

        self.http.request(method, url)
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

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, ClientError> {
        let response = self.request(Method::GET, path).query(query).send().await?;
        let response = require_success(response).await?;

        Ok(response.json().await?)
    }

    pub async fn list_probes(&self) -> Result<Vec<crate::Probe>, ClientError> {
        self.get(routes::PROBES, &()).await
    }

    pub async fn get_probe(&self, id: crate::ProbeId) -> Result<crate::Probe, ClientError> {
        self.get(&routes::probe_path(id), &()).await
    }

    pub async fn create_agent_definition(
        &self,
        request: &CreateAgentDefinition,
    ) -> Result<AgentDefinition, ClientError> {
        self.send(Method::POST, routes::AGENT_DEFINITIONS, request)
            .await
    }

    pub async fn list_agent_definitions(&self) -> Result<Vec<AgentDefinition>, ClientError> {
        self.get(routes::AGENT_DEFINITIONS, &()).await
    }

    pub async fn get_agent_definition(
        &self,
        id: AgentDefinitionId,
    ) -> Result<AgentDefinition, ClientError> {
        self.get(&routes::agent_definition_path(id), &()).await
    }

    /// Creates a new immutable version; the original definition is unchanged.
    pub async fn update_agent_definition(
        &self,
        id: AgentDefinitionId,
        request: &UpdateAgentDefinition,
    ) -> Result<AgentDefinition, ClientError> {
        self.send(Method::PUT, &routes::agent_definition_path(id), request)
            .await
    }

    pub async fn create_session(&self, request: &CreateSession) -> Result<Session, ClientError> {
        self.send(Method::POST, routes::SESSIONS, request).await
    }

    pub async fn list_sessions(
        &self,
        parent: Option<SessionId>,
    ) -> Result<Vec<Session>, ClientError> {
        self.get(routes::SESSIONS, &ListSessions { parent }).await
    }

    pub async fn get_session(&self, id: SessionId) -> Result<Session, ClientError> {
        self.get(&routes::session_path(routes::SESSION, id), &())
            .await
    }

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

    pub async fn list_agents(&self, session: SessionId) -> Result<Vec<Agent>, ClientError> {
        self.get(&routes::session_path(routes::AGENTS, session), &())
            .await
    }

    pub async fn get_agent(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Agent, ClientError> {
        self.get(&routes::agent_path(routes::AGENT, session, agent), &())
            .await
    }

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

    /// Respond to an outstanding runtime query. Success has no response body.
    pub async fn respond_to_client_query(
        &self,
        session: SessionId,
        agent: AgentId,
        query: ClientQueryId,
        response: &ClientQueryResponse,
    ) -> Result<(), ClientError> {
        let response = self
            .request(
                Method::POST,
                &routes::client_query_response_path(session, agent, query),
            )
            .json(response)
            .send()
            .await?;
        require_success(response).await?;
        Ok(())
    }

    pub async fn events(
        &self,
        session: SessionId,
        agent: Option<AgentId>,
        query: &EventsQuery,
    ) -> Result<EventsPage, ClientError> {
        let mut query = query.clone();
        query.stream = false;

        self.get(&events_path(session, agent), &query).await
    }

    /// Reconnect explicitly with the last durable ID after EOF/resync. Snapshots
    /// replace provisional state; committed items should be upserted by identity.
    pub async fn stream_events(
        &self,
        session: SessionId,
        agent: Option<AgentId>,
        query: &EventsQuery,
        last_event_id: Option<&str>,
    ) -> Result<EventStream, ClientError> {
        let mut query = query.clone();
        query.stream = true;
        let mut request = self
            .request(Method::GET, &events_path(session, agent))
            .query(&query);
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

/// Normalize a configured server base URL for a probe websocket connection.
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

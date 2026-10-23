//! Trusted-client HTTP/SSE API. Transcript mutations share the publication gate.

use crate::{
    agents::{
        client_queries::ClientQueryError, publication::Publication, session_manager::RuntimeError,
        sessions::Sessions,
    },
    db::{Database, DbError},
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
    routing::{get, post},
};
use psa_api::{self as api, AgentId, SessionId, events::*, routes};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{broadcast, watch};

struct ApiJson<T>(T);
pub(crate) struct ApiQuery<T>(pub(crate) T);
pub(crate) struct ApiPath<T>(pub(crate) T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> axum::extract::FromRequest<S> for ApiJson<T> {
    type Rejection = HttpError;
    async fn from_request(
        request: axum::extract::Request,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|error| HttpError::Invalid(error.body_text()))
    }
}

impl<S: Send + Sync, T: serde::de::DeserializeOwned + Send> axum::extract::FromRequestParts<S>
    for ApiQuery<T>
{
    type Rejection = HttpError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|error| HttpError::Invalid(error.body_text()))
    }
}

impl<S: Send + Sync, T: serde::de::DeserializeOwned + Send> axum::extract::FromRequestParts<S>
    for ApiPath<T>
{
    type Rejection = HttpError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|error| HttpError::Invalid(error.body_text()))
    }
}

#[derive(Clone)]
pub struct AppState {
    pub database: Database,
    pub publication: Publication,
    pub runtime: Sessions,
    pub accepting: Arc<AtomicBool>,
    pub probes: crate::probes::ProbeRegistry,
}

impl AppState {
    fn require_accepting(&self) -> Result<(), HttpError> {
        if self.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(HttpError::Runtime(RuntimeError::Unavailable))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Agent(#[from] crate::agents::AgentError),
    #[error(transparent)]
    ClientQuery(#[from] ClientQueryError),
    #[error("{0}")]
    Invalid(String),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::ClientQuery(
                ClientQueryError::InvalidRequest(_) | ClientQueryError::InvalidResponse(_),
            ) => (StatusCode::BAD_REQUEST, "invalid_input", self.to_string()),
            Self::ClientQuery(ClientQueryError::NotFound) => {
                (StatusCode::NOT_FOUND, "not_found", self.to_string())
            }
            Self::ClientQuery(ClientQueryError::AlreadyResolved) => {
                (StatusCode::CONFLICT, "already_resolved", self.to_string())
            }
            Self::ClientQuery(ClientQueryError::Cancelled) => {
                (StatusCode::GONE, "cancelled", self.to_string())
            }
            Self::ClientQuery(ClientQueryError::Unavailable | ClientQueryError::Overloaded) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                self.to_string(),
            ),
            Self::Database(DbError::ProbeNotFound(_))
            | Self::Database(DbError::NotFound { .. })
            | Self::Runtime(RuntimeError::Database(
                DbError::NotFound { .. } | DbError::ProbeNotFound(_),
            )) => (StatusCode::NOT_FOUND, "not_found", self.to_string()),
            Self::Invalid(_)
            | Self::Database(DbError::InvalidInput(_))
            | Self::Runtime(RuntimeError::Invalid(_))
            | Self::Runtime(RuntimeError::Database(DbError::InvalidInput(_))) => {
                (StatusCode::BAD_REQUEST, "invalid_input", self.to_string())
            }
            Self::Runtime(RuntimeError::Cancelled) => {
                (StatusCode::GONE, "cancelled", self.to_string())
            }
            Self::Runtime(RuntimeError::InvalidResume) => {
                (StatusCode::CONFLICT, "invalid_state", self.to_string())
            }
            Self::Runtime(
                RuntimeError::Unavailable
                | RuntimeError::MissingProvider
                | RuntimeError::Overloaded,
            ) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                self.to_string(),
            ),
            _ => {
                tracing::error!(%self, "API operation failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal server error".into(),
                )
            }
        };
        (
            status,
            Json(api::ApiError {
                code: code.into(),
                message,
            }),
        )
            .into_response()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok\n" }))
        .route(
            routes::AGENT_DEFINITIONS,
            post(create_agent_definition).get(list_agent_definitions),
        )
        .route(
            routes::AGENT_DEFINITION,
            get(get_agent_definition).put(update_agent_definition),
        )
        .route(routes::SESSIONS, post(create_session).get(list_sessions))
        .route(routes::SESSION, get(get_session).patch(update_session))
        .route(routes::FORK, post(fork_session))
        .route(routes::AGENTS, post(create_agent).get(list_agents))
        .route(routes::AGENT, get(get_agent))
        .route(routes::REQUESTS, post(submit_request))
        .route(routes::CLIENT_QUERY_RESPONSE, post(respond_to_client_query))
        .route(routes::SESSION_EVENTS, get(session_events))
        .route(routes::AGENT_EVENTS, get(agent_events))
        .merge(crate::probes::router())
        .with_state(state)
}

async fn create_agent_definition(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<api::CreateAgentDefinition>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let definition = state.runtime.create_agent_definition(request).await?;
    Ok((StatusCode::CREATED, Json(definition)))
}

async fn list_agent_definitions(
    State(state): State<AppState>,
) -> Result<Json<Vec<api::AgentDefinition>>, HttpError> {
    Ok(Json(state.runtime.list_agent_definitions().await?))
}

async fn get_agent_definition(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
) -> Result<Json<api::AgentDefinition>, HttpError> {
    Ok(Json(
        state
            .runtime
            .get_agent_definition(api::AgentDefinitionId(id))
            .await?,
    ))
}

async fn update_agent_definition(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
    ApiJson(request): ApiJson<api::UpdateAgentDefinition>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let definition = state
        .runtime
        .update_agent_definition(api::AgentDefinitionId(id), request)
        .await?;
    Ok((StatusCode::CREATED, Json(definition)))
}

async fn create_session(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<api::CreateSession>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let session = state.runtime.create_session(request).await?;
    Ok((StatusCode::CREATED, Json(session.into_api(false))))
}

async fn list_sessions(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::ListSessions>,
) -> Result<Json<Vec<api::Session>>, HttpError> {
    let sessions = state.runtime.list_sessions(query).await?;
    let mut views = Vec::with_capacity(sessions.len());
    for session in sessions {
        let busy = state.runtime.is_busy(session.id).await?;
        views.push(session.into_api(busy));
    }
    Ok(Json(views))
}

async fn get_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
) -> Result<Json<api::Session>, HttpError> {
    let session = state.runtime.get_session(SessionId(id)).await?;
    let busy = state.runtime.is_busy(session.id).await?;
    Ok(Json(session.into_api(busy)))
}

async fn update_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
    ApiJson(request): ApiJson<api::UpdateSession>,
) -> Result<Json<api::Session>, HttpError> {
    state.require_accepting()?;

    let session = state.runtime.update_session(SessionId(id), request).await?;
    let busy = state.runtime.is_busy(session.id).await?;
    Ok(Json(session.into_api(busy)))
}

async fn fork_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
    ApiJson(request): ApiJson<api::ForkSession>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let fork = state.runtime.fork_session(SessionId(id), request).await?;
    let busy = state.runtime.is_busy(fork.session.id).await?;
    let view = api::ForkedSession {
        session: fork.session.into_api(busy),
        agents: fork
            .agents
            .into_iter()
            .map(|(parent, agent)| (parent, agent.into()))
            .collect(),
    };
    Ok((StatusCode::CREATED, Json(view)))
}

async fn create_agent(
    State(state): State<AppState>,
    ApiPath(session): ApiPath<u64>,
    ApiJson(request): ApiJson<api::CreateAgent>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let agent = state
        .runtime
        .create_agent(SessionId(session), request)
        .await?;
    Ok((StatusCode::CREATED, Json(api::Agent::from(agent))))
}

async fn list_agents(
    State(state): State<AppState>,
    ApiPath(session): ApiPath<u64>,
) -> Result<Json<Vec<api::Agent>>, HttpError> {
    let agents = state.runtime.list_agents(SessionId(session)).await?;
    Ok(Json(agents.into_iter().map(Into::into).collect()))
}

async fn get_agent(
    State(state): State<AppState>,
    ApiPath((session, agent)): ApiPath<(u64, u64)>,
) -> Result<Json<api::Agent>, HttpError> {
    let agent = state
        .runtime
        .get_agent(SessionId(session), AgentId(agent))
        .await?;
    Ok(Json(agent.into()))
}

async fn submit_request(
    State(state): State<AppState>,
    ApiPath((session, agent)): ApiPath<(u64, u64)>,
    ApiJson(request): ApiJson<api::AgentRequest>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let accepted = state
        .runtime
        .submit(SessionId(session), AgentId(agent), request)
        .await?;

    Ok((StatusCode::ACCEPTED, Json(accepted)))
}

async fn respond_to_client_query(
    State(state): State<AppState>,
    ApiPath((session, agent, query)): ApiPath<(u64, u64, api::ClientQueryId)>,
    response: Result<Json<api::ClientQueryResponse>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode, HttpError> {
    state.require_accepting()?;
    // Serde rejections can include submitted answer values; keep this boundary private.
    let Json(response) =
        response.map_err(|_| HttpError::Invalid("invalid client query response body".into()))?;

    state
        .runtime
        .respond_to_client_query(SessionId(session), AgentId(agent), query, response)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn session_events(
    State(state): State<AppState>,
    ApiPath(session): ApiPath<u64>,
    ApiQuery(query): ApiQuery<api::EventsQuery>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    events_response(state, SessionId(session), None, query, headers).await
}

async fn agent_events(
    State(state): State<AppState>,
    ApiPath((session, agent)): ApiPath<(u64, u64)>,
    ApiQuery(query): ApiQuery<api::EventsQuery>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    events_response(
        state,
        SessionId(session),
        Some(AgentId(agent)),
        query,
        headers,
    )
    .await
}

async fn load_page(
    state: &AppState,
    session: SessionId,
    agent: Option<AgentId>,
    query: &api::EventsQuery,
) -> Result<api::EventsPage, HttpError> {
    if query.limit == 0
        || query.limit > 10_000
        || query.through.is_some_and(|through| through < query.after)
    {
        return Err(HttpError::Invalid(
            "invalid event range or page limit".into(),
        ));
    }

    let mut cursor = state.database.cursor().await?;
    let (head, events) = match agent {
        Some(agent) => {
            let head = cursor
                .require_agent_session(session, agent)
                .await?
                .next_event_id
                .0
                - 1;
            let events = cursor
                .load_agent_envelopes(
                    session,
                    agent,
                    EventRange {
                        after: EventId(query.after),
                        through: query.through.map(EventId),
                        limit: query.limit,
                    },
                )
                .await?;
            (head, events)
        }
        None => {
            let head = cursor.get_session(session).await?.next_position - 1;
            let events = cursor
                .load_session_events(session, query.after, query.through, query.limit)
                .await?;
            (head, events)
        }
    };
    cursor.rollback().await?;

    let head = query.through.unwrap_or(head).min(head);
    let last_cursor = events
        .last()
        .map_or(query.after, |event| event_cursor(event, agent));
    let has_more = events.len() == query.limit as usize && last_cursor < head;

    Ok(api::EventsPage {
        events,
        pending: Vec::new(),
        queries: Vec::new(),
        next_after: if has_more {
            last_cursor
        } else {
            head.max(query.after)
        },
        head,
        has_more,
    })
}

async fn events_response(
    state: AppState,
    session: SessionId,
    agent: Option<AgentId>,
    mut query: api::EventsQuery,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    state.require_accepting()?;

    if query.stream
        && let Some(id) = headers.get("last-event-id")
    {
        query.after = decode_cursor(
            id.to_str()
                .map_err(|_| HttpError::Invalid("invalid Last-Event-ID".into()))?,
            session,
            agent,
        )?;
    }

    // Subscribe, capture a head, and snapshot drafts and queries under the same gate used by
    // every commit and provisional publication. Buffered notifications are newer.
    let publication = state.publication.lock().await;
    let mut received = publication.subscribe();
    let mut page = load_page(&state, session, agent, &query).await?;
    let pending = if query.include_pending {
        publication.pending(session, agent)
    } else {
        Vec::new()
    };
    let queries = publication.queries(session, agent);
    drop(publication);

    if !query.stream {
        page.pending = pending;
        page.queries = queries;
        return Ok(Json(page).into_response());
    }

    let mut shutdown = state.publication.shutdown_receiver();
    let head = page.head;
    let finite = query.through.is_some();
    let stream = async_stream::stream! {
        loop {
            for event in page.events {
                let cursor = event_cursor(&event, agent);
                yield encode_sse(
                    api::StreamMessage::Committed {
                        event,
                        pending_id: None,
                    },
                    Some(encode_cursor(session, agent, cursor)),
                );
            }
            if !page.has_more {
                break;
            }

            query.after = page.next_after;
            query.through = Some(head);
            match load_page(&state, session, agent, &query).await {
                Ok(next) => page = next,
                Err(_) => {
                    yield encode_sse(api::StreamMessage::ResyncRequired, None);
                    return;
                }
            }
        }

        yield encode_sse(
            api::StreamMessage::Snapshot { cursor: head, pending, queries },
            Some(encode_cursor(session, agent, head)),
        );
        if finite || *shutdown.borrow() {
            return;
        }

        while let Some(message) = next_stream_message(&mut received, &mut shutdown).await {
            let message = match message {
                Ok(message) => message,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield encode_sse(api::StreamMessage::ResyncRequired, None);
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if !matches_scope(&message, session, agent, query.include_pending) {
                continue;
            }

            let id = if let api::StreamMessage::Committed { event, .. } = &message {
                let cursor = event_cursor(event, agent);
                if cursor <= head {
                    continue;
                }
                Some(encode_cursor(session, agent, cursor))
            } else {
                None
            };
            yield encode_sse(message, id);
        }
    };
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn next_stream_message(
    received: &mut broadcast::Receiver<api::StreamMessage>,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Result<api::StreamMessage, broadcast::error::RecvError>> {
    tokio::select! {
        biased;
        message = received.recv() => Some(message),
        _ = shutdown.changed() => None,
    }
}

fn event_cursor(event: &api::EventEnvelope, agent: Option<AgentId>) -> u64 {
    if agent.is_some() {
        event.event.id.0
    } else {
        event.position
    }
}

fn matches_scope(
    message: &api::StreamMessage,
    session: SessionId,
    agent: Option<AgentId>,
    include_pending: bool,
) -> bool {
    let (owner, stream) = match message {
        api::StreamMessage::Committed { event, .. } => (event.session_id, Some(event.agent_id)),
        api::StreamMessage::Upsert { pending } => {
            if !include_pending {
                return false;
            }
            (pending.session_id, Some(pending.agent_id))
        }
        api::StreamMessage::Removed {
            session_id,
            agent_id,
            ..
        } => {
            if !include_pending {
                return false;
            }
            (*session_id, Some(*agent_id))
        }
        api::StreamMessage::ClientQueryRequested { query } => {
            (query.session_id, Some(query.agent_id))
        }
        api::StreamMessage::ClientQueryRemoved {
            session_id,
            agent_id,
            ..
        } => (*session_id, Some(*agent_id)),
        api::StreamMessage::AgentUpdated { agent } => (agent.session_id, Some(agent.id)),
        api::StreamMessage::RequestFailed {
            session_id,
            agent_id,
            ..
        } => (*session_id, Some(*agent_id)),
        api::StreamMessage::SessionBusy { session_id, .. } => (*session_id, None),
        api::StreamMessage::SessionUpdated { session } => (session.id, None),
        _ => return false,
    };
    owner == session && (stream.is_none() || agent.is_none() || stream == agent)
}

fn encode_cursor(session: SessionId, agent: Option<AgentId>, cursor: u64) -> String {
    format!("{}:{}:{cursor}", session.0, agent.map_or(0, |id| id.0))
}

fn decode_cursor(id: &str, session: SessionId, agent: Option<AgentId>) -> Result<u64, HttpError> {
    let parts: Vec<_> = id.split(':').collect();
    if parts.len() != 3
        || parts[0] != session.0.to_string()
        || parts[1] != agent.map_or(0, |id| id.0).to_string()
    {
        return Err(HttpError::Invalid(
            "Last-Event-ID belongs to a different stream".into(),
        ));
    }

    parts[2]
        .parse()
        .map_err(|_| HttpError::Invalid("invalid Last-Event-ID".into()))
}

fn encode_sse(message: api::StreamMessage, id: Option<String>) -> Result<SseEvent, axum::Error> {
    let event = SseEvent::default().json_data(message)?;
    Ok(match id {
        Some(id) => event.id(id),
        None => event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{NewAgent, session_manager::Runtime};
    use crate::{providers::ProviderRegistry, sessions};

    #[tokio::test]
    async fn lagged_subscriber_is_told_to_resync_instead_of_silently_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.sqlite3"))
            .await
            .unwrap();
        crate::test_support::register_probe(&db).await;
        let mut cursor = db.cursor().await.unwrap();
        let session = cursor
            .create_session(sessions::NewSession {
                probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
                name: "test".into(),
                working_directory: "/work".into(),
                model_config: crate::test_support::model_config(),
            })
            .await
            .unwrap();
        cursor
            .create_agent(
                session.id,
                NewAgent {
                    subagent_parent_id: None,
                    interactive: false,
                    name: "test".into(),
                    definition_id: api::AgentDefinitionId(1),
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
        let publication = Publication::default();
        let runtime = Runtime::start(
            db.clone(),
            publication.clone(),
            ProviderRegistry::default(),
            crate::probes::ProbeRegistry::start("test".into()),
        )
        .await
        .unwrap();
        let state = AppState {
            database: db.clone(),
            publication: publication.clone(),
            runtime: runtime.handle.clone(),
            accepting: Arc::new(AtomicBool::new(true)),
            probes: crate::probes::ProbeRegistry::start("test".into()),
        };
        let response = events_response(
            state,
            session.id,
            None,
            api::EventsQuery {
                stream: true,
                ..Default::default()
            },
            HeaderMap::new(),
        )
        .await
        .unwrap();
        // Do not poll the response body yet, simulating a slow connection.
        for _ in 0..1100 {
            publication
                .lock()
                .await
                .publish(api::StreamMessage::SessionBusy {
                    session_id: session.id,
                    busy: true,
                });
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains("resync_required")
        );
        runtime.shutdown().await.unwrap();
        db.close().await;
    }

    #[tokio::test]
    async fn facade_errors_preserve_http_status_codes_and_messages() {
        let cases = [
            (RuntimeError::Cancelled, StatusCode::GONE, "cancelled"),
            (
                RuntimeError::Invalid("message content must not be empty".into()),
                StatusCode::BAD_REQUEST,
                "invalid_input",
            ),
            (
                RuntimeError::Database(DbError::InvalidInput("invalid name".into())),
                StatusCode::BAD_REQUEST,
                "invalid_input",
            ),
            (
                RuntimeError::Database(DbError::NotFound {
                    entity: "agent",
                    id: 7,
                }),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                RuntimeError::Database(DbError::ProbeNotFound(psa_api::ProbeId(
                    psa_api::probes::Uuid::nil(),
                ))),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                RuntimeError::InvalidResume,
                StatusCode::CONFLICT,
                "invalid_state",
            ),
            (
                RuntimeError::MissingProvider,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
            (
                RuntimeError::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
            (
                RuntimeError::Overloaded,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
        ];
        for (error, status, code) in cases {
            let message = error.to_string();
            let response = HttpError::from(error).into_response();
            assert_eq!(response.status(), status);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let error: api::ApiError = serde_json::from_slice(&body).unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.message, message);
        }
    }

    #[tokio::test]
    async fn client_query_errors_preserve_http_status_codes_and_messages() {
        let cases = [
            (
                ClientQueryError::InvalidRequest("invalid query".into()),
                StatusCode::BAD_REQUEST,
                "invalid_input",
            ),
            (
                ClientQueryError::InvalidResponse("invalid response".into()),
                StatusCode::BAD_REQUEST,
                "invalid_input",
            ),
            (
                ClientQueryError::NotFound,
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                ClientQueryError::AlreadyResolved,
                StatusCode::CONFLICT,
                "already_resolved",
            ),
            (ClientQueryError::Cancelled, StatusCode::GONE, "cancelled"),
            (
                ClientQueryError::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
            (
                ClientQueryError::Overloaded,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
        ];
        for (error, status, code) in cases {
            let message = error.to_string();
            let response = HttpError::from(error).into_response();
            assert_eq!(response.status(), status);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let error: api::ApiError = serde_json::from_slice(&body).unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn client_query_notifications_are_scoped_independently_of_pending_events() {
        let query = api::ClientQueryRequest {
            id: api::ClientQueryId::new(),
            session_id: SessionId(1),
            agent_id: AgentId(2),
            payload: api::ClientQuery::AskQuestion(api::AskQuestionRequest {
                questions: vec![api::ClientQuestion {
                    question: "Question?".into(),
                    choices: vec![],
                }],
            }),
        };
        let removed = api::StreamMessage::ClientQueryRemoved {
            session_id: query.session_id,
            agent_id: query.agent_id,
            query_id: query.id,
        };
        for message in [api::StreamMessage::ClientQueryRequested { query }, removed] {
            for include_pending in [false, true] {
                assert!(matches_scope(&message, SessionId(1), None, include_pending));
                assert!(matches_scope(
                    &message,
                    SessionId(1),
                    Some(AgentId(2)),
                    include_pending
                ));
                assert!(!matches_scope(
                    &message,
                    SessionId(9),
                    None,
                    include_pending
                ));
                assert!(!matches_scope(
                    &message,
                    SessionId(1),
                    Some(AgentId(9)),
                    include_pending
                ));
            }
        }
    }

    #[test]
    fn resume_cursors_cannot_be_reused_for_another_session_or_agent() {
        let id = encode_cursor(SessionId(1), Some(AgentId(2)), 3);
        assert_eq!(
            decode_cursor(&id, SessionId(1), Some(AgentId(2))).unwrap(),
            3
        );
        assert!(decode_cursor(&id, SessionId(1), None).is_err());
        assert!(decode_cursor(&id, SessionId(2), Some(AgentId(2))).is_err());
    }
}

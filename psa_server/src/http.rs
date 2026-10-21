//! Trusted-client HTTP/SSE API. Transcript mutations share the publication gate.

use crate::{
    agents::{
        NewAgent, ToolDefinitionExt,
        agent_context::AgentContext,
        agent_manager::build_recovery_events,
        publication::Publication,
        session_manager::{RuntimeError, RuntimeHandle},
    },
    db::{Database, DbError},
    sessions,
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
    pub runtime: RuntimeHandle,
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
    #[error("{0}")]
    Invalid(String),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::Database(DbError::ProbeNotFound(_))
            | Self::Database(DbError::NotFound { .. })
            | Self::Runtime(RuntimeError::Database(DbError::NotFound { .. })) => {
                (StatusCode::NOT_FOUND, "not_found", self.to_string())
            }
            Self::Invalid(_) | Self::Database(DbError::InvalidInput(_)) => {
                (StatusCode::BAD_REQUEST, "invalid_input", self.to_string())
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
        .route(routes::SESSIONS, post(create_session).get(list_sessions))
        .route(routes::SESSION, get(get_session).patch(update_session))
        .route(routes::FORK, post(fork_session))
        .route(routes::AGENTS, post(create_agent).get(list_agents))
        .route(routes::AGENT, get(get_agent))
        .route(routes::REQUESTS, post(submit_request))
        .route(routes::SESSION_EVENTS, get(session_events))
        .route(routes::AGENT_EVENTS, get(agent_events))
        .merge(crate::probes::router())
        .with_state(state)
}

async fn create_session(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<api::CreateSession>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let publication = state.publication.lock().await;
    let mut cursor = state.database.cursor().await?;
    let session = cursor
        .create_session(sessions::NewSession {
            name: request.name,
            working_directory: request.working_directory,
            probe_id: request.probe_id,
        })
        .await?;
    cursor.commit().await?;

    let session = session.into_api(false);
    publication.publish(api::StreamMessage::SessionUpdated {
        session: session.clone(),
    });

    Ok((StatusCode::CREATED, Json(session)))
}

async fn list_sessions(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::ListSessions>,
) -> Result<Json<Vec<api::Session>>, HttpError> {
    let mut cursor = state.database.cursor().await?;
    let sessions = cursor.list_sessions(query.parent).await?;
    cursor.rollback().await?;

    let mut result = Vec::new();
    for session in sessions {
        let busy = state.runtime.is_busy(session.id).await?;
        result.push(session.into_api(busy));
    }

    Ok(Json(result))
}

async fn get_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
) -> Result<Json<api::Session>, HttpError> {
    let mut cursor = state.database.cursor().await?;
    let session = cursor.get_session(SessionId(id)).await?;
    cursor.rollback().await?;

    Ok(Json(
        session.into_api(state.runtime.is_busy(SessionId(id)).await?),
    ))
}

async fn update_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
    ApiJson(request): ApiJson<api::UpdateSession>,
) -> Result<Json<api::Session>, HttpError> {
    state.require_accepting()?;

    // Never wait on the supervisor while holding the publication gate.
    let busy = state.runtime.is_busy(SessionId(id)).await?;
    let publication = state.publication.lock().await;
    let mut cursor = state.database.cursor().await?;
    let session = cursor
        .update_session(
            SessionId(id),
            sessions::SessionUpdate {
                name: request.name,
                working_directory: request.working_directory,
            },
        )
        .await?;
    cursor.commit().await?;

    let result = session.clone().into_api(busy);
    publication.publish(api::StreamMessage::SessionUpdated {
        session: result.clone(),
    });
    drop(publication);

    state.runtime.update_session(session).await?;
    Ok(Json(result))
}

async fn fork_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
    ApiJson(request): ApiJson<api::ForkSession>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let publication = state.publication.lock().await;
    let mut cursor = state.database.cursor().await?;
    let fork = cursor
        .fork_session(
            SessionId(id),
            sessions::SessionFork {
                point: match request.point {
                    api::ForkPoint::Head => sessions::ForkPoint::Head,
                    api::ForkPoint::ThroughEvent { agent_id, event_id } => {
                        sessions::ForkPoint::ThroughEvent { agent_id, event_id }
                    }
                },
                metadata: sessions::SessionUpdate {
                    name: request.metadata.name,
                    working_directory: request.metadata.working_directory,
                },
            },
        )
        .await?;

    let mut agents = Vec::new();
    let mut committed = Vec::new();
    for (parent, agent) in fork.agents {
        let context = AgentContext::replay(&cursor.load_all_events(agent.id).await?)
            .map_err(crate::agents::AgentError::from)?;
        let events = build_recovery_events(&agent, &context)?;
        let result = cursor.add_events(agent.id, &events).await?;
        let first =
            cursor.get_session(agent.session_id).await?.next_position - result.events.len() as u64;
        committed.extend(
            result
                .events
                .into_iter()
                .enumerate()
                .map(|(offset, event)| api::StreamMessage::Committed {
                    event: api::EventEnvelope {
                        session_id: agent.session_id,
                        agent_id: agent.id,
                        position: first + offset as u64,
                        event,
                    },
                    pending_id: None,
                }),
        );
        agents.push((parent, cursor.get_agent(agent.id).await?.into()));
    }
    cursor.commit().await?;

    for message in committed {
        publication.publish(message);
    }
    let session = fork.session.into_api(false);
    publication.publish(api::StreamMessage::SessionUpdated {
        session: session.clone(),
    });

    Ok((
        StatusCode::CREATED,
        Json(api::ForkedSession { session, agents }),
    ))
}

async fn create_agent(
    State(state): State<AppState>,
    ApiPath(session): ApiPath<u64>,
    ApiJson(request): ApiJson<api::CreateAgent>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let mut history = llm_api::api::AgentHistory::new(request.model.clone());
    history.tools = request
        .tools
        .iter()
        .map(|id| crate::hooks::tools::tool_definition(*id))
        .collect();
    llm_api::api::validate_request(&history, request.model.provider, &Default::default())
        .map_err(|error| HttpError::Invalid(error.to_string()))?;

    let publication = state.publication.lock().await;
    let mut cursor = state.database.cursor().await?;
    let agent = cursor
        .create_agent(SessionId(session), NewAgent { name: request.name })
        .await?;
    let mut events = vec![NewEvent::new(EventType::Model(request.model))];
    events.extend(
        request
            .tools
            .into_iter()
            .map(|id| NewEvent::new(EventType::ToolDefinition(ToolDefinition::for_tool(id)))),
    );
    let result = cursor.add_events(agent.id, &events).await?;
    AgentContext::replay(&result.events).map_err(crate::agents::AgentError::from)?;
    let agent = cursor.get_agent(agent.id).await?;
    let first_position =
        cursor.get_session(agent.session_id).await?.next_position - result.events.len() as u64;
    cursor.commit().await?;

    for (offset, event) in result.events.into_iter().enumerate() {
        publication.publish(api::StreamMessage::Committed {
            event: api::EventEnvelope {
                session_id: agent.session_id,
                agent_id: agent.id,
                position: first_position + offset as u64,
                event,
            },
            pending_id: None,
        });
    }
    publication.publish(api::StreamMessage::AgentUpdated {
        agent: agent.clone().into(),
    });
    drop(publication);

    Ok((StatusCode::CREATED, Json(api::Agent::from(agent))))
}

async fn list_agents(
    State(state): State<AppState>,
    ApiPath(session): ApiPath<u64>,
) -> Result<Json<Vec<api::Agent>>, HttpError> {
    let mut cursor = state.database.cursor().await?;
    let agents = cursor.list_agents(SessionId(session)).await?;
    cursor.rollback().await?;

    Ok(Json(agents.into_iter().map(Into::into).collect()))
}

async fn get_agent(
    State(state): State<AppState>,
    ApiPath((session, agent)): ApiPath<(u64, u64)>,
) -> Result<Json<api::Agent>, HttpError> {
    let mut cursor = state.database.cursor().await?;
    let agent = cursor
        .require_agent_session(SessionId(session), AgentId(agent))
        .await?;
    cursor.rollback().await?;
    Ok(Json(agent.into()))
}

async fn submit_request(
    State(state): State<AppState>,
    ApiPath((session, agent)): ApiPath<(u64, u64)>,
    ApiJson(request): ApiJson<api::AgentRequest>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;
    if matches!(&request, api::AgentRequest::Message { content, .. } if content.is_empty()) {
        return Err(HttpError::Invalid(
            "message content must not be empty".into(),
        ));
    }

    let accepted = state
        .runtime
        .submit(SessionId(session), AgentId(agent), request)
        .await?;

    Ok((StatusCode::ACCEPTED, Json(accepted)))
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
    let next = events
        .last()
        .map_or(query.after, |event| event_cursor(event, agent));
    let has_more = events.len() == query.limit as usize && next < head;

    Ok(api::EventsPage {
        events,
        pending: Vec::new(),
        next_after: if has_more {
            next
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

    // Subscribe, capture a head, and snapshot drafts under the same gate used by
    // every commit and provisional publication. Buffered notifications are newer.
    let publication = state.publication.lock().await;
    let mut received = publication.subscribe();
    let mut page = load_page(&state, session, agent, &query).await?;
    let pending = if query.include_pending {
        publication.pending(session, agent)
    } else {
        Vec::new()
    };
    drop(publication);

    if !query.stream {
        page.pending = pending;
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
            api::StreamMessage::Snapshot { cursor: head, pending },
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
    use crate::providers::ProviderRegistry;

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
            })
            .await
            .unwrap();
        cursor
            .create_agent(
                session.id,
                NewAgent {
                    name: "test".into(),
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
        let publication = Publication::default();
        let runtime = Runtime::start(db.clone(), publication.clone(), ProviderRegistry::default())
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

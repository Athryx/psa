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
    pub client_key: Arc<str>,
    pub client_config: api::ClientConfig,
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
    #[error(transparent)]
    Catalog(#[from] crate::models::CatalogError),
    #[error(transparent)]
    ProviderAuth(#[from] crate::providers::AuthError),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::ProviderAuth(crate::providers::AuthError::InvalidCredential) => {
                (StatusCode::BAD_REQUEST, "invalid_input", self.to_string())
            }
            Self::ProviderAuth(crate::providers::AuthError::Unavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                self.to_string(),
            ),
            Self::Catalog(crate::models::CatalogError::NotFound) => {
                (StatusCode::NOT_FOUND, "not_found", self.to_string())
            }
            Self::Catalog(crate::models::CatalogError::Invalid(_)) => {
                (StatusCode::BAD_REQUEST, "invalid_input", self.to_string())
            }
            Self::Catalog(crate::models::CatalogError::SyncFailed(_)) => (
                StatusCode::BAD_GATEWAY,
                "catalog_sync_failed",
                self.to_string(),
            ),
            Self::Catalog(crate::models::CatalogError::Unavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                self.to_string(),
            ),
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
            | Self::Database(DbError::ConfigNotFound(_))
            | Self::Database(DbError::NotFound { .. })
            | Self::Runtime(RuntimeError::Database(
                DbError::NotFound { .. } | DbError::ProbeNotFound(_) | DbError::ConfigNotFound(_),
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
        .route(routes::CONFIG, get(get_config))
        .route(
            routes::AGENT_CONFIGS,
            post(upsert_agent_configs).get(refresh_and_list_agent_configs),
        )
        .route(routes::AGENT_CONFIG_LOOKUP, post(get_agent_config))
        .route(routes::AGENT_SNAPSHOT, get(get_resolved_agent_config))
        .route(
            routes::SKILLS,
            post(upload_skill).get(refresh_and_list_skills).layer(
                // A 64 MiB archive encoded as a JSON byte array uses up to
                // four bytes per element, plus configuration metadata.
                axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024 + 64 * 1024),
            ),
        )
        .route(routes::SESSIONS, post(create_session).get(list_sessions))
        .route(routes::SESSION_QUERY, get(query_sessions))
        .route(routes::GLOBAL_EVENTS, get(global_events))
        .route(routes::SESSION, get(get_session).patch(update_session))
        .route(routes::FORK, post(fork_session))
        .route(routes::AGENTS, post(create_agent).get(list_agents))
        .route(routes::AGENT, get(get_agent))
        .route(routes::AGENT_STATUS, get(get_agent_status))
        .route(routes::AGENT_RESULTS, get(get_agent_results))
        .route(routes::REQUESTS, post(submit_request))
        .route(routes::CLIENT_QUERY_RESPONSE, post(respond_to_client_query))
        .route(routes::SESSION_EVENTS, get(session_events))
        .route(routes::AGENT_EVENTS, get(agent_events))
        .route(routes::PROVIDERS, get(list_providers))
        .route(
            routes::PROVIDER_CREDENTIAL,
            axum::routing::put(set_provider_credential)
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route(routes::MODELS, get(list_models))
        .route(routes::MODEL_LOOKUP, get(lookup_model))
        .route(routes::MODEL_PIN, axum::routing::put(pin_model))
        .route(routes::MODELS_RESYNC, post(resync_models))
        .merge(crate::probes::client_router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_client_key,
        ))
        .merge(crate::probes::connection_router())
        .with_state(state)
}

async fn require_client_key(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !crate::auth::has_bearer_key(request.headers(), &state.client_key) {
        return (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            Json(api::ApiError {
                code: "unauthorized".into(),
                message: "invalid client credentials".into(),
            }),
        )
            .into_response();
    }

    next.run(request).await
}

async fn get_config(State(state): State<AppState>) -> Json<api::ClientConfig> {
    Json(state.client_config)
}

fn require_catalog(state: &AppState) -> Result<&crate::models::ModelCatalog, HttpError> {
    state
        .runtime
        .model_catalog()
        .ok_or_else(|| crate::models::CatalogError::Unavailable.into())
}

async fn list_providers(
    State(state): State<AppState>,
) -> Result<Json<Vec<api::ProviderInfo>>, HttpError> {
    Ok(Json(require_catalog(&state)?.providers()))
}

async fn set_provider_credential(
    State(state): State<AppState>,
    request: Result<Json<api::SetProviderCredential>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<api::ProviderInfo>, HttpError> {
    state.require_accepting()?;
    // Never echo deserialization errors, which may contain submitted secrets.
    let Json(request) =
        request.map_err(|_| HttpError::Invalid("invalid provider credential request".into()))?;
    Ok(Json(
        require_catalog(&state)?
            .set_provider_credential(request)
            .await?,
    ))
}

async fn list_models(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::ListModels>,
) -> Result<Json<Vec<api::ModelInfo>>, HttpError> {
    Ok(Json(require_catalog(&state)?.list_models(query).await?))
}

async fn lookup_model(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::ModelLookup>,
) -> Result<Json<api::ModelInfo>, HttpError> {
    let model = llm_api::providers::Model::new(query.provider, query.model)
        .map_err(|error| HttpError::Invalid(error.to_string()))?;
    Ok(Json(
        require_catalog(&state)?
            .model_info(&model)
            .await?
            .ok_or(crate::models::CatalogError::NotFound)?,
    ))
}

async fn pin_model(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<api::PinModel>,
) -> Result<Json<Option<api::ModelInfo>>, HttpError> {
    state.require_accepting()?;
    Ok(Json(require_catalog(&state)?.set_pinned(request).await?))
}

async fn resync_models(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<api::ResyncModels>,
) -> Result<StatusCode, HttpError> {
    state.require_accepting()?;
    require_catalog(&state)?.resync(request.provider).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn upsert_agent_configs(
    State(state): State<AppState>,
    ApiJson(configs): ApiJson<Vec<api::AgentConfigInput>>,
) -> Result<Json<Vec<api::AgentConfigStatus>>, HttpError> {
    state.require_accepting()?;
    Ok(Json(state.runtime.upsert_agent_configs(configs).await?))
}

async fn refresh_and_list_agent_configs(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::RefreshAndListAgentConfigs>,
) -> Result<Json<Vec<api::AgentConfigStatus>>, HttpError> {
    Ok(Json(
        state
            .runtime
            .refresh_and_list_agent_configs(query.refresh_session_id)
            .await?,
    ))
}

async fn get_agent_config(
    State(state): State<AppState>,
    ApiJson(id): ApiJson<api::ConfigId>,
) -> Result<Json<api::AgentConfigStatus>, HttpError> {
    Ok(Json(state.runtime.get_agent_config(id).await?))
}

async fn get_resolved_agent_config(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<u64>,
) -> Result<Json<Option<api::ResolvedAgentConfig>>, HttpError> {
    Ok(Json(
        state
            .runtime
            .get_resolved_agent_config(api::AgentDefinitionId(id))
            .await?,
    ))
}

async fn upload_skill(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<api::NewSkill>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;
    let skill = state.runtime.upload_skill(request).await?;
    Ok((StatusCode::CREATED, Json(skill)))
}

async fn refresh_and_list_skills(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::RefreshAndListSkills>,
) -> Result<Json<Vec<api::Skill>>, HttpError> {
    Ok(Json(
        state
            .runtime
            .refresh_and_list_skills(query.refresh_session_id)
            .await?,
    ))
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

// Cursors carry their filter scope so accidentally reusing one cannot omit rows.
type SummaryCursor = (i64, i64, bool, Option<SessionId>, Option<api::ProbeId>);

async fn query_sessions(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::SessionListQuery>,
) -> Result<Json<api::SessionsPage>, HttpError> {
    state.require_accepting()?;
    if query.limit == 0 || query.limit > 1000 || (query.flat && query.parent.is_some()) {
        return Err(HttpError::Invalid(
            "invalid session list scope or limit".into(),
        ));
    }
    let before = query
        .before
        .as_ref()
        .map(|value| {
            let cursor: SummaryCursor = serde_json::from_str(value)
                .map_err(|_| HttpError::Invalid("invalid session cursor".into()))?;
            if cursor.0 < 0
                || cursor.1 <= 0
                || (cursor.2, cursor.3, cursor.4) != (query.flat, query.parent, query.probe_id)
            {
                return Err(HttpError::Invalid(
                    "session cursor belongs to another scope".into(),
                ));
            }
            Ok((cursor.0, cursor.1))
        })
        .transpose()?;
    let publication = state.publication.lock().await;
    let mut cursor = state.database.cursor().await?;
    let mut sessions = cursor.query_sessions(&query, before).await?;
    cursor.rollback().await?;
    let more = sessions.len() > query.limit as usize;
    sessions.truncate(query.limit as usize);
    let next_before = if more {
        let last = sessions.last().expect("nonempty bounded page");
        Some(
            serde_json::to_string(&(
                i64::try_from(last.updated_at.unix_timestamp_nanos() / 1000)
                    .map_err(|_| HttpError::Invalid("invalid session timestamp".into()))?,
                i64::try_from(last.id.0)
                    .map_err(|_| HttpError::Invalid("invalid session ID".into()))?,
                query.flat,
                query.parent,
                query.probe_id,
            ))
            .expect("summary cursor is serializable"),
        )
    } else {
        None
    };
    let sessions = sessions
        .into_iter()
        .map(|session| {
            let busy = publication.is_session_busy(session.id);
            session.into_api(busy)
        })
        .collect();
    Ok(Json(api::SessionsPage {
        sessions,
        next_before,
        revision: publication.revision(),
    }))
}

async fn global_events(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<api::GlobalEventsQuery>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    state.require_accepting()?;
    let publication = state.publication.lock().await;
    let mut received = publication.subscribe_global();
    let revision = publication.revision();
    drop(publication);
    let mut shutdown = state.publication.shutdown_receiver();
    let reconnect = headers.contains_key("last-event-id");
    let stream = async_stream::stream! {
        if reconnect {
            yield SseEvent::default().json_data(api::GlobalStreamMessage::ResyncRequired);
            return;
        }
        yield SseEvent::default().id(revision.to_string())
            .json_data(api::GlobalStreamMessage::Ready { revision });
        if *shutdown.borrow() { return; }
        loop {
            let message = tokio::select! {
                biased;
                message = received.recv() => message,
                _ = shutdown.changed() => break,
            };
            match message {
                Ok(api::GlobalStreamMessage::SessionUpdated { revision, session }) => {
                    if query.probe_id.is_some_and(|probe| probe != session.probe_id) { continue; }
                    yield SseEvent::default().id(revision.to_string())
                        .json_data(api::GlobalStreamMessage::SessionUpdated { revision, session });
                }
                Ok(api::GlobalStreamMessage::ResyncRequired) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield SseEvent::default().json_data(api::GlobalStreamMessage::ResyncRequired);
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Ok(api::GlobalStreamMessage::Ready { .. }) => {}
            }
        }
    };
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
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
        agents: fork.agents.into_iter().map(Into::into).collect(),
    };
    Ok((StatusCode::CREATED, Json(view)))
}

async fn create_agent(
    State(state): State<AppState>,
    ApiPath(session): ApiPath<u64>,
    ApiJson(request): ApiJson<api::NewConfiguredAgent>,
) -> Result<impl IntoResponse, HttpError> {
    state.require_accepting()?;

    let agent = state
        .runtime
        .create_configured_agent(SessionId(session), request)
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

async fn get_agent_status(
    State(state): State<AppState>,
    ApiPath((session, agent)): ApiPath<(u64, u64)>,
) -> Result<Json<api::AgentStatusSnapshot>, HttpError> {
    let mut cursor = state.database.cursor().await?;
    let status = cursor
        .load_agent_status(SessionId(session), AgentId(agent))
        .await?;
    Ok(Json(status))
}

async fn get_agent_results(
    State(state): State<AppState>,
    ApiPath((session, agent)): ApiPath<(u64, u64)>,
    ApiQuery(query): ApiQuery<api::ResultsQuery>,
) -> Result<Json<Vec<api::ResultRecord>>, HttpError> {
    let results = state
        .runtime
        .load_last_results(SessionId(session), AgentId(agent), query.limit)
        .await?;
    Ok(Json(results))
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
        || ((query.recent || query.before.is_some())
            && (query.after != 0 || query.through.is_some()))
        || (query.recent && query.before.is_some())
    {
        return Err(HttpError::Invalid(
            "invalid event range or page limit".into(),
        ));
    }

    let mut cursor = state.database.cursor().await?;
    let backward = query.recent || query.before.is_some();
    if backward {
        let head = match agent {
            Some(agent) => {
                cursor
                    .require_agent_session(session, agent)
                    .await?
                    .next_event_id
                    .0
                    - 1
            }
            None => cursor.get_session(session).await?.next_position - 1,
        };
        let through = query
            .before
            .map_or(head, |before| before.saturating_sub(1).min(head));
        let events = match agent {
            Some(agent) => {
                cursor
                    .load_recent_agent_events(
                        session,
                        agent,
                        through,
                        query.limit,
                        query.results_only,
                    )
                    .await?
            }
            None => {
                cursor
                    .load_recent_session_events(session, through, query.limit, query.results_only)
                    .await?
            }
        };
        let older_through = events
            .first()
            .map(|event| event_cursor(event, agent).saturating_sub(1));
        let has_older = if let Some(older_through) = older_through {
            let older = match agent {
                Some(agent) => {
                    cursor
                        .load_recent_agent_events(
                            session,
                            agent,
                            older_through,
                            1,
                            query.results_only,
                        )
                        .await?
                }
                None => {
                    cursor
                        .load_recent_session_events(session, older_through, 1, query.results_only)
                        .await?
                }
            };
            !older.is_empty()
        } else {
            false
        };
        cursor.rollback().await?;
        let next_before = if has_older {
            events.first().map(|event| event_cursor(event, agent))
        } else {
            None
        };
        return Ok(api::EventsPage {
            events,
            pending: Vec::new(),
            queries: Vec::new(),
            next_after: head,
            next_before,
            has_older,
            head,
            has_more: false,
        });
    }
    let (head, events) = match agent {
        Some(agent) => {
            let head = cursor
                .require_agent_session(session, agent)
                .await?
                .next_event_id
                .0
                - 1;
            let range = EventRange {
                after: EventId(query.after),
                through: query.through.map(EventId),
                limit: query.limit,
            };
            let events = if query.results_only {
                cursor
                    .load_agent_result_envelopes(session, agent, range)
                    .await?
            } else {
                cursor.load_agent_envelopes(session, agent, range).await?
            };
            (head, events)
        }
        None => {
            let head = cursor.get_session(session).await?.next_position - 1;
            let events = if query.results_only {
                cursor
                    .load_session_result_events(session, query.after, query.through, query.limit)
                    .await?
            } else {
                cursor
                    .load_session_events(session, query.after, query.through, query.limit)
                    .await?
            };
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
        next_before: None,
        has_older: false,
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

    // JSON is the default; SSE requires an explicitly accepted event-stream MIME type.
    let stream_events = headers
        .get_all("accept")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        });

    if stream_events && (query.recent || query.before.is_some()) {
        return Err(HttpError::Invalid(
            "SSE requires forward event pagination".into(),
        ));
    }

    if stream_events && let Some(id) = headers.get("last-event-id") {
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
    let pending = if query.include_pending && !query.results_only {
        publication.pending(session, agent)
    } else {
        Vec::new()
    };
    let queries = publication.queries(session, agent);
    drop(publication);

    if !stream_events {
        if query.before.is_none() {
            page.pending = pending;
            page.queries = queries;
        }
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
            if matches!(message, api::StreamMessage::ResyncRequired) {
                yield encode_sse(message, None);
                break;
            }
            if !matches_scope(&message, session, agent, query.include_pending)
                || !matches_result_filter(&message, query.results_only)
            {
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

fn matches_result_filter(message: &api::StreamMessage, results_only: bool) -> bool {
    if !results_only {
        return true;
    }
    match message {
        api::StreamMessage::Committed { event, .. } => {
            matches!(event.event.kind, EventType::Result(_))
        }
        api::StreamMessage::Upsert { .. } | api::StreamMessage::Removed { .. } => false,
        // Snapshot/cursor recovery and runtime control messages retain their normal scope.
        _ => true,
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
mod desktop_stream_tests;
#[cfg(test)]
mod desktop_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{NewAgent, session_manager::Runtime};
    use crate::{providers::ProviderRegistry, sessions};

    #[test]
    fn old_probe_connection_query_accepts_new_metadata() {
        fn legacy_host<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<bool, D::Error> {
            let value = <String as serde::Deserialize>::deserialize(deserializer)?;
            value.parse().map_err(serde::de::Error::custom)
        }

        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyMetadata {
            id: api::ProbeId,
            hostname: String,
            os: String,
            architecture: String,
            label: Option<String>,
            data_dir: std::path::PathBuf,
            #[serde(default, deserialize_with = "legacy_host")]
            host: bool,
        }
        #[derive(serde::Deserialize)]
        struct LegacyConnect {
            version: u32,
            #[serde(flatten)]
            metadata: LegacyMetadata,
        }
        let uri = format!("/probes/connect?version=2&id={}&hostname=test&os=linux&architecture=x86_64&data_dir=%2Ftmp&host=false&home_dir=%2Fhome", api::probes::Uuid::nil()).parse().unwrap();
        let Query(legacy) = Query::<LegacyConnect>::try_from_uri(&uri).unwrap();
        assert_eq!(legacy.version, 2);
        assert_eq!(legacy.metadata.hostname, "test");
        let _ = (
            legacy.metadata.id,
            legacy.metadata.os,
            legacy.metadata.architecture,
            legacy.metadata.label,
            legacy.metadata.data_dir,
            legacy.metadata.host,
        );
    }

    #[test]
    fn probe_connection_query_decodes_host_and_legacy_metadata() {
        let base = format!(
            "/probes/connect?version={}&id={}&hostname=test&os=linux&architecture=x86_64&data_dir=%2Ftmp",
            api::probes::PROTOCOL_VERSION,
            api::probes::Uuid::nil()
        );
        for (suffix, host) in [("", false), ("&host=false", false), ("&host=true", true)] {
            let uri = format!("{base}{suffix}").parse().unwrap();
            let Query(query) = Query::<api::probes::ConnectProbe>::try_from_uri(&uri).unwrap();
            assert_eq!(query.metadata.host, host);
            assert_eq!(query.metadata.home_dir, None);
        }
        let uri = format!("{base}&home_dir=%2Fhome%2Ftest+user")
            .parse()
            .unwrap();
        let Query(query) = Query::<api::probes::ConnectProbe>::try_from_uri(&uri).unwrap();
        assert_eq!(query.metadata.home_dir.as_deref(), Some("/home/test user"));
        assert!(query.metadata.validate().is_ok());
        let uri = format!("{base}&home_dir=relative").parse().unwrap();
        let Query(query) = Query::<api::probes::ConnectProbe>::try_from_uri(&uri).unwrap();
        assert!(query.metadata.validate().is_err());
        for value in ["invalid", "1", "", "TRUE"] {
            let uri = format!("{base}&host={value}").parse().unwrap();
            assert!(Query::<api::probes::ConnectProbe>::try_from_uri(&uri).is_err());
        }
    }

    async fn assert_client_authentication(base: &str) {
        let http = reqwest::Client::new();
        let endpoints = [
            ("GET", "/health"),
            ("GET", routes::PROVIDERS),
            ("GET", routes::MODELS),
            ("GET", routes::MODEL_LOOKUP),
            ("PUT", routes::MODEL_PIN),
            ("POST", routes::MODELS_RESYNC),
            ("GET", routes::PROBES),
            ("GET", routes::PROBE),
            ("GET", routes::PROBE_DIRECTORIES),
            ("GET", routes::CONFIG),
            ("GET", routes::AGENT_CONFIGS),
            ("POST", routes::AGENT_CONFIGS),
            ("POST", routes::AGENT_CONFIG_LOOKUP),
            ("GET", routes::AGENT_SNAPSHOT),
            ("GET", routes::SKILLS),
            ("POST", routes::SKILLS),
            ("GET", routes::SESSIONS),
            ("POST", routes::SESSIONS),
            ("GET", routes::SESSION_QUERY),
            ("GET", routes::GLOBAL_EVENTS),
            ("GET", routes::SESSION),
            ("PATCH", routes::SESSION),
            ("POST", routes::FORK),
            ("GET", routes::AGENTS),
            ("POST", routes::AGENTS),
            ("GET", routes::AGENT),
            ("GET", routes::AGENT_STATUS),
            ("POST", routes::REQUESTS),
            ("POST", routes::CLIENT_QUERY_RESPONSE),
            ("GET", routes::SESSION_EVENTS),
            ("GET", routes::AGENT_EVENTS),
            ("GET", routes::AGENT_RESULTS),
        ];
        for (method, route) in endpoints {
            let path = route
                .replace("{session_id}", "1")
                .replace("{agent_id}", "1")
                .replace("{definition_id}", "1")
                .replace("{probe_id}", "00000000-0000-0000-0000-000000000000")
                .replace("{query_id}", "00000000-0000-0000-0000-000000000000");
            for credentials in [None, Some("Bearer wrong-key"), Some("Bearer test")] {
                let mut request = http.request(method.parse().unwrap(), format!("{base}{path}"));
                if let Some(credentials) = credentials {
                    request = request.header("authorization", credentials);
                }
                // Authentication precedes payload/path parsing and SSE setup.
                let response = request
                    .header("accept", "text/event-stream")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::UNAUTHORIZED,
                    "{method} {path}"
                );
                assert_eq!(response.headers()["www-authenticate"], "Bearer");
                let error: api::ApiError = response.json().await.unwrap();
                assert_eq!(error.code, "unauthorized");
            }
        }
        for credentials in [
            "Basic test-client-key",
            "Bearer ",
            "Bearer test-client-key-extra",
        ] {
            let response = http
                .get(format!("{base}/health"))
                .header("authorization", credentials)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = http
            .get(format!("{base}/health"))
            .bearer_auth("test-client-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn config_routes_and_client_replace_legacy_definition_crud() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.sqlite3"))
            .await
            .unwrap();
        let publication = Publication::default();
        let probes = crate::probes::ProbeRegistry::start("test".into());
        let defaults = api::AgentDefaults {
            bundles: vec![
                api::BundleConfig::FileTools { read_only: false },
                api::BundleConfig::Noop,
            ],
            hooks: api::HookConfigs {
                agent_create: vec![api::HookConfig::DefaultStartup],
                after_request: vec![api::HookConfig::StopWithoutTools],
                ..Default::default()
            },
        };
        let runtime = Runtime::start_with_configuration(
            db.clone(),
            publication.clone(),
            ProviderRegistry::default(),
            probes.clone(),
            Some(
                crate::config_registry::ConfigRegistry::new(dir.path().join("config"))
                    .with_agent_defaults(defaults.clone()),
            ),
            crate::config::default_model_config(),
        )
        .await
        .unwrap();
        let state = AppState {
            database: db.clone(),
            publication,
            runtime: runtime.handle.clone(),
            accepting: Arc::new(AtomicBool::new(true)),
            client_key: "test-client-key".into(),
            client_config: api::ClientConfig {
                default_agent_name: "worker".into(),
            },
            probes,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server =
            tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        assert_client_authentication(&base).await;
        let client = api::Client::new(&base, "test-client-key").unwrap();
        assert_eq!(
            client.get_config().await.unwrap().default_agent_name,
            "worker"
        );
        let response = reqwest::Client::new()
            .get(format!("{base}/config"))
            .bearer_auth("test-client-key")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!({"default_agent_name": "worker"})
        );
        let config: api::AgentConfig = serde_json::from_value(serde_json::json!({
            "config_id": {"config_group": {"Custom": "http-test"}, "name": "worker"},
            "models": ["Global"]
        }))
        .unwrap();
        let id = config.config_id.clone();
        let upserted = client.upsert_agent_configs(&[config]).await.unwrap();
        assert_eq!(upserted.len(), 1);
        assert_eq!(upserted[0].config.config_id, id);

        let listed = client.refresh_and_list_agent_configs(None).await.unwrap();
        assert!(listed.iter().any(|config| config.config.config_id == id));
        let found = client.get_agent_config(&id).await.unwrap();
        assert_eq!(found.config.config_id, id);
        let mut updated = found.config;
        updated.description = "updated".into();
        let upserted = client.upsert_agent_configs(&[updated]).await.unwrap();
        assert_eq!(upserted[0].config.description, "updated");
        assert_eq!(
            client
                .refresh_and_list_agent_configs(None)
                .await
                .unwrap()
                .len(),
            listed.len()
        );
        let snapshot = client
            .get_resolved_agent_config(api::AgentDefinitionId(999_999))
            .await
            .unwrap();
        assert!(snapshot.is_none());
        assert!(
            client
                .refresh_and_list_skills(None)
                .await
                .unwrap()
                .is_empty()
        );

        // The typed input preserves omissions over the client wire, while a
        // normalized AgentConfig (as above) remains fully explicit.
        assert!(upserted[0].config.bundles.is_empty());
        let inherited_id = api::ConfigId {
            config_group: api::ConfigGroup::Custom("http-test".into()),
            name: "inherited".into(),
        };
        let inherited = client
            .upsert_agent_configs(&[api::AgentConfigInput::new(
                inherited_id.clone(),
                vec![api::ModelSelection::Global],
            )])
            .await
            .unwrap();
        assert_eq!(inherited[0].config.bundles, defaults.bundles);
        assert_eq!(inherited[0].config.hooks, defaults.hooks);

        let http = reqwest::Client::new();
        let response = http
            .post(format!("{base}/agent-configs"))
            .bearer_auth("test-client-key")
            .json(&serde_json::json!([{
                "config_id": inherited_id,
                "models": ["Global"],
                "bundles": [],
                "hooks": {"agent_create": []}
            }]))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let explicit: Vec<api::AgentConfigStatus> = response.json().await.unwrap();
        assert!(explicit[0].config.bundles.is_empty());
        assert!(explicit[0].config.hooks.agent_create.is_empty());
        assert_eq!(
            explicit[0].config.hooks.after_request,
            defaults.hooks.after_request
        );
        assert_eq!(
            client
                .get_agent_config(&inherited_id)
                .await
                .unwrap()
                .config
                .hooks,
            explicit[0].config.hooks
        );

        for tools in [serde_json::json!([]), serde_json::json!(["Noop"])] {
            let response = http.post(format!("{base}/agent-configs"))
                .bearer_auth("test-client-key")
                .json(&serde_json::json!([{"config_id": inherited_id, "models": ["Global"], "tools": tools}]))
                .send().await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let legacy_creation = http
            .post(format!("{base}/sessions/1/agents"))
            .bearer_auth("test-client-key")
            .json(&serde_json::json!({"definition_id": 1, "name": "legacy"}))
            .send()
            .await
            .unwrap();
        assert_eq!(legacy_creation.status(), StatusCode::BAD_REQUEST);
        for path in ["/agent-definitions", "/agent-definitions/1"] {
            for method in [
                reqwest::Method::GET,
                reqwest::Method::POST,
                reqwest::Method::PUT,
            ] {
                let response = http
                    .request(method, format!("{base}{path}"))
                    .bearer_auth("test-client-key")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
            }
        }

        server.abort();
        let _ = server.await;
        runtime.shutdown().await.unwrap();
        db.close().await;
    }

    #[tokio::test]
    async fn accept_headers_select_transport_and_lagged_subscribers_are_told_to_resync() {
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
                    hidden: false,
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
            client_key: "test-client-key".into(),
            client_config: api::ClientConfig {
                default_agent_name: "default".into(),
            },
            probes: crate::probes::ProbeRegistry::start("test".into()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for legacy in ["true", "false"] {
            let response = reqwest::Client::new()
                .get(format!(
                    "{base}/sessions/{}/events?stream={legacy}",
                    session.id.0
                ))
                .header("accept", "text/event-stream")
                .bearer_auth("test-client-key")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        server.abort();
        let _ = server.await;

        // Last-Event-ID is ignored in JSON mode, including the default Accept behavior.
        for accept in [None, Some("application/json"), Some("*/*")] {
            let mut headers = HeaderMap::new();
            if let Some(accept) = accept {
                headers.insert("accept", accept.parse().unwrap());
            }
            headers.insert("last-event-id", "invalid-cursor".parse().unwrap());
            let response = events_response(
                state.clone(),
                session.id,
                None,
                api::EventsQuery::default(),
                headers,
            )
            .await
            .unwrap();
            assert!(
                response.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("application/json")
            );
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            serde_json::from_slice::<api::EventsPage>(&bytes).unwrap();
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            "application/json, text/event-stream; charset=utf-8"
                .parse()
                .unwrap(),
        );
        headers.insert("last-event-id", "invalid-cursor".parse().unwrap());
        assert!(matches!(
            events_response(
                state.clone(),
                session.id,
                None,
                api::EventsQuery::default(),
                headers.clone(),
            )
            .await,
            Err(HttpError::Invalid(_))
        ));
        headers.insert(
            "last-event-id",
            encode_cursor(session.id, None, 0).parse().unwrap(),
        );
        let mut repeated = HeaderMap::new();
        repeated.append("accept", "application/json".parse().unwrap());
        repeated.append(
            "accept",
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        repeated.insert(
            "last-event-id",
            encode_cursor(session.id, None, 0).parse().unwrap(),
        );
        for accepted in [headers.clone(), repeated] {
            let finite = events_response(
                state.clone(),
                session.id,
                None,
                api::EventsQuery {
                    through: Some(0),
                    ..Default::default()
                },
                accepted,
            )
            .await
            .unwrap();
            assert!(
                finite.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("text/event-stream")
            );
            let bytes = axum::body::to_bytes(finite.into_body(), 1_000_000)
                .await
                .unwrap();
            assert!(std::str::from_utf8(&bytes).unwrap().contains("snapshot"));
        }

        let response = events_response(
            state,
            session.id,
            None,
            api::EventsQuery::default(),
            headers,
        )
        .await
        .unwrap();
        // Do not poll the response body yet, simulating a slow connection.
        for _ in 0..1100 {
            publication
                .lock()
                .await
                .publish(api::StreamMessage::SessionUpdated {
                    session: session.clone().into_api(true),
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

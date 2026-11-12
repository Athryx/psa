//! Docker sandboxes on manager probes, each running a nested probe.
//!
//! The server stores sandbox records; container state is queried from the
//! manager probe and reported as unknown while the manager is disconnected.
use crate::{
    db::{DbError, SandboxRecord},
    http::{ApiJson, ApiPath, ApiQuery, AppState, HttpError},
    probes::ProbeHandle,
};
use axum::{
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use psa_api::{
    CreateSandbox, ListSandboxes, Probe, ProbeId, Sandbox, SandboxState,
    probes::{ContainerStatus, SandboxRequest, SandboxSpec, Uuid},
    routes,
    sandboxes::is_valid_sandbox_name,
};
use psa_shared::probe_transport::ProbeError;
use std::{collections::HashMap, time::Duration};
use time::OffsetDateTime;

const INSPECT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn client_router() -> Router<AppState> {
    Router::new()
        .route(routes::SANDBOXES, get(list_sandboxes))
        .route(routes::PROBE_SANDBOXES, post(create_sandbox))
        .route(routes::SANDBOX, get(get_sandbox).delete(delete_sandbox))
        .route(routes::SANDBOX_START, post(start_sandbox))
        .route(routes::SANDBOX_STOP, post(stop_sandbox))
}

#[derive(Debug, thiserror::Error)]
enum SandboxError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("probe {0} does not support sandboxes")]
    Unsupported(String),
    #[error("manager probe {0} is not connected")]
    ManagerOffline(String),
    #[error("no image given and the server has no default sandbox image")]
    ImageRequired,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Probe(#[from] ProbeError),
}

impl From<DbError> for SandboxError {
    fn from(error: DbError) -> Self {
        Self::Http(error.into())
    }
}

impl IntoResponse for SandboxError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::Http(_) => {
                let Self::Http(error) = self else {
                    unreachable!()
                };
                return error.into_response();
            }
            Self::Unsupported(_) => (StatusCode::BAD_REQUEST, "sandboxes_unsupported"),
            Self::ManagerOffline(_) => (StatusCode::CONFLICT, "manager_offline"),
            Self::ImageRequired => (StatusCode::BAD_REQUEST, "image_required"),
            Self::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_input"),
            Self::Probe(ProbeError::Disconnected | ProbeError::Overloaded) => {
                (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
            }
            Self::Probe(_) => (StatusCode::BAD_GATEWAY, "probe_error"),
        };
        let body = psa_api::ApiError {
            code: code.into(),
            message: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

async fn list_sandboxes(
    axum::extract::State(state): axum::extract::State<AppState>,
    ApiQuery(query): ApiQuery<ListSandboxes>,
) -> Result<Json<Vec<Sandbox>>, SandboxError> {
    let mut cursor = state.database.cursor().await?;
    let records = cursor.list_sandboxes(query.manager_id).await?;
    cursor.rollback().await?;

    let mut states = HashMap::new();
    for record in &records {
        if let std::collections::hash_map::Entry::Vacant(entry) = states.entry(record.manager_id) {
            entry.insert(inspect_manager(&state, record.manager_id).await);
        }
    }

    let mut sandboxes = Vec::with_capacity(records.len());
    for record in records {
        let manager_states = &states[&record.manager_id];
        sandboxes.push(build_sandbox(&state, record, manager_states.as_ref()).await);
    }
    Ok(Json(sandboxes))
}

async fn get_sandbox(
    axum::extract::State(state): axum::extract::State<AppState>,
    ApiPath(probe_id): ApiPath<ProbeId>,
) -> Result<Json<Sandbox>, SandboxError> {
    let record = load_sandbox(&state, probe_id).await?;
    Ok(Json(refresh_sandbox(&state, record).await))
}

async fn create_sandbox(
    axum::extract::State(state): axum::extract::State<AppState>,
    ApiPath(manager_id): ApiPath<ProbeId>,
    ApiJson(request): ApiJson<CreateSandbox>,
) -> Result<(StatusCode, Json<Sandbox>), SandboxError> {
    let mut cursor = state.database.cursor().await?;
    let manager = cursor.get_probe(manager_id).await?;
    let existing = cursor.list_sandboxes(Some(manager_id)).await?;
    cursor.rollback().await?;

    if !manager.metadata.sandboxes {
        return Err(SandboxError::Unsupported(probe_name(&manager)));
    }
    let handle = require_manager(&state, &manager).await?;

    let image = request
        .image
        .filter(|image| !image.trim().is_empty())
        .or_else(|| state.client_config.default_sandbox_image.clone())
        .ok_or(SandboxError::ImageRequired)?;
    let probe_id = ProbeId(Uuid::new_v4());
    let name = request
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("sandbox-{}", &probe_id.to_string()[..8]));
    if !is_valid_sandbox_name(&name) {
        return Err(SandboxError::Invalid(
            "sandbox name must be 1-64 ASCII letters, digits, '-', '_' or '.'".into(),
        ));
    }
    if existing.iter().any(|sandbox| sandbox.name == name) {
        return Err(SandboxError::Invalid(format!(
            "sandbox name {name:?} is already in use"
        )));
    }

    let record = SandboxRecord {
        probe_id,
        manager_id,
        name,
        image,
        mounts: request.mounts,
        created_at: OffsetDateTime::now_utc(),
    };
    let spec = SandboxSpec {
        probe_id,
        name: record.name.clone(),
        image: record.image.clone(),
        mounts: record.mounts.clone(),
    };
    handle.sandbox(SandboxRequest::Create { spec }).await?;

    // Remove the container again if the record cannot be stored, so no orphan remains.
    if let Err(error) = insert_sandbox(&state, &record).await {
        let _ = handle.sandbox(SandboxRequest::Remove { probe_id }).await;
        return Err(error.into());
    }

    if request.start {
        handle.sandbox(SandboxRequest::Start { probe_id }).await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(refresh_sandbox(&state, record).await),
    ))
}

async fn start_sandbox(
    axum::extract::State(state): axum::extract::State<AppState>,
    ApiPath(probe_id): ApiPath<ProbeId>,
) -> Result<Json<Sandbox>, SandboxError> {
    let (record, handle) = load_managed_sandbox(&state, probe_id).await?;
    handle.sandbox(SandboxRequest::Start { probe_id }).await?;
    Ok(Json(refresh_sandbox(&state, record).await))
}

async fn stop_sandbox(
    axum::extract::State(state): axum::extract::State<AppState>,
    ApiPath(probe_id): ApiPath<ProbeId>,
) -> Result<Json<Sandbox>, SandboxError> {
    let (record, handle) = load_managed_sandbox(&state, probe_id).await?;
    handle.sandbox(SandboxRequest::Stop { probe_id }).await?;
    Ok(Json(refresh_sandbox(&state, record).await))
}

async fn delete_sandbox(
    axum::extract::State(state): axum::extract::State<AppState>,
    ApiPath(probe_id): ApiPath<ProbeId>,
) -> Result<StatusCode, SandboxError> {
    let (_, handle) = load_managed_sandbox(&state, probe_id).await?;
    handle.sandbox(SandboxRequest::Remove { probe_id }).await?;

    let mut cursor = state.database.cursor().await?;
    cursor.delete_sandbox(probe_id).await?;
    cursor.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn insert_sandbox(state: &AppState, record: &SandboxRecord) -> Result<(), DbError> {
    let mut cursor = state.database.cursor().await?;
    cursor.insert_sandbox(record).await?;
    cursor.commit().await
}

async fn load_sandbox(state: &AppState, probe_id: ProbeId) -> Result<SandboxRecord, SandboxError> {
    let mut cursor = state.database.cursor().await?;
    let record = cursor.get_sandbox(probe_id).await?;
    cursor.rollback().await?;
    Ok(record)
}

/// Load a sandbox and a live handle to its manager, failing if the manager is offline.
async fn load_managed_sandbox(
    state: &AppState,
    probe_id: ProbeId,
) -> Result<(SandboxRecord, ProbeHandle), SandboxError> {
    let mut cursor = state.database.cursor().await?;
    let record = cursor.get_sandbox(probe_id).await?;
    let manager = cursor.get_probe(record.manager_id).await?;
    cursor.rollback().await?;

    let handle = require_manager(state, &manager).await?;
    Ok((record, handle))
}

async fn require_manager(state: &AppState, manager: &Probe) -> Result<ProbeHandle, SandboxError> {
    state
        .probes
        .handle(manager.metadata.id)
        .await
        .map_err(|_| SandboxError::ManagerOffline(probe_name(manager)))
}

fn probe_name(probe: &Probe) -> String {
    probe
        .metadata
        .label
        .clone()
        .filter(|label| !label.trim().is_empty())
        .unwrap_or_else(|| probe.metadata.hostname.clone())
}

/// Container states for a manager, or `None` when it cannot be queried.
async fn inspect_manager(
    state: &AppState,
    manager_id: ProbeId,
) -> Option<HashMap<ProbeId, ContainerStatus>> {
    let handle = state.probes.handle(manager_id).await.ok()?;
    let inspection = handle.sandbox(SandboxRequest::Inspect);
    let containers = match tokio::time::timeout(INSPECT_TIMEOUT, inspection).await {
        Ok(Ok(containers)) => containers.unwrap_or_default(),
        Ok(Err(error)) => {
            tracing::warn!(%manager_id, %error, "sandbox inspection failed");
            return None;
        }
        Err(_) => {
            tracing::warn!(%manager_id, "sandbox inspection timed out");
            return None;
        }
    };

    let states = containers
        .into_iter()
        .map(|container| (container.probe_id, container))
        .collect();
    Some(states)
}

async fn refresh_sandbox(state: &AppState, record: SandboxRecord) -> Sandbox {
    let states = inspect_manager(state, record.manager_id).await;
    build_sandbox(state, record, states.as_ref()).await
}

async fn build_sandbox(
    state: &AppState,
    record: SandboxRecord,
    containers: Option<&HashMap<ProbeId, ContainerStatus>>,
) -> Sandbox {
    let sandbox_state = match containers {
        None => SandboxState::Unknown,
        Some(containers) => containers
            .get(&record.probe_id)
            .map_or(SandboxState::Missing, |container| container.state),
    };
    Sandbox {
        probe_active: state.probes.is_active(record.probe_id).await,
        probe_id: record.probe_id,
        manager_id: record.manager_id,
        name: record.name,
        image: record.image,
        mounts: record.mounts,
        state: sandbox_state,
        created_at: record.created_at,
    }
}

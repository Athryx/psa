//! Probe discovery routes and the WebSocket transport probes connect over.
use crate::http::AppState;
use axum::{
    Json, Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message as WsMessage, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt, future::ready};
use psa::api::{
    ProbeId,
    probes::{ConnectProbe, PROTOCOL_VERSION},
};
pub use psa_agents::probes::{ProbeHandle, ProbeRegistry, RemoteChild};
use psa_shared::probe_transport::{Frame, ProbeError, spawn_connection};
use psa_store::Database;

/// Probe discovery belongs to the client-authenticated API.
pub fn client_router() -> Router<AppState> {
    Router::new()
        .route(psa::api::routes::PROBES, get(list_probes))
        .route(psa::api::routes::PROBE, get(get_probe))
        .route(
            psa::api::routes::PROBE_DIRECTORIES,
            get(list_probe_directory),
        )
}

/// Probe transport authenticates independently of the client API.
pub fn connection_router() -> Router<AppState> {
    Router::new().route(psa::api::routes::PROBES_CONNECT, get(connect_probe))
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(psa::api::ApiError {
            code: code.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

async fn list_probes(
    State(state): State<AppState>,
) -> Result<Json<Vec<psa::api::Probe>>, crate::http::HttpError> {
    let mut cursor = state.database.read().await?;
    let mut probes = cursor.list_probes().await?;
    cursor.close().await?;

    for probe in &mut probes {
        probe.is_active = state.probes.is_active(probe.metadata.id).await;
    }
    Ok(Json(probes))
}

async fn get_probe(
    State(state): State<AppState>,
    crate::http::ApiPath(id): crate::http::ApiPath<ProbeId>,
) -> Result<Json<psa::api::Probe>, crate::http::HttpError> {
    let mut cursor = state.database.read().await?;
    let mut probe = cursor.get_probe(id).await?;
    cursor.close().await?;

    probe.is_active = state.probes.is_active(id).await;
    Ok(Json(probe))
}

#[derive(serde::Deserialize)]
struct ListDirectoryQuery {
    path: String,
}

async fn list_probe_directory(
    State(state): State<AppState>,
    crate::http::ApiPath(id): crate::http::ApiPath<ProbeId>,
    crate::http::ApiQuery(query): crate::http::ApiQuery<ListDirectoryQuery>,
) -> Result<Json<Vec<psa::api::probes::DirectoryEntry>>, Response> {
    if !query.path.starts_with('/') || query.path.contains('\0') {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "path must be absolute UTF-8 without NUL",
        ));
    }

    // Distinguish an unknown probe from a registered but disconnected one.
    let mut cursor = state
        .database
        .read()
        .await
        .map_err(|error| crate::http::HttpError::from(error).into_response())?;
    cursor
        .get_probe(id)
        .await
        .map_err(|error| crate::http::HttpError::from(error).into_response())?;
    cursor
        .close()
        .await
        .map_err(|error| crate::http::HttpError::from(error).into_response())?;

    let probe = state.probes.handle(id).await.map_err(probe_listing_error)?;
    let entries = probe
        .list_dir(query.path)
        .await
        .map_err(probe_listing_error)?;
    Ok(Json(entries))
}

fn probe_listing_error(error: ProbeError) -> Response {
    match error {
        ProbeError::Disconnected | ProbeError::Overloaded => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            &error.to_string(),
        ),
        ProbeError::Remote { .. } => {
            api_error(StatusCode::BAD_GATEWAY, "probe_error", &error.to_string())
        }
        ProbeError::Protocol(_) => {
            tracing::warn!(%error, "probe directory listing failed");
            api_error(
                StatusCode::BAD_GATEWAY,
                "probe_error",
                "invalid response from probe",
            )
        }
    }
}

async fn connect_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::http::ApiQuery(query): crate::http::ApiQuery<ConnectProbe>,
    websocket: WebSocketUpgrade,
) -> Response {
    if !state.accepting.load(std::sync::atomic::Ordering::Acquire) {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "server is shutting down",
        );
    }

    if !crate::auth::has_bearer_key(&headers, state.probes.key()) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid probe credentials",
        );
    }
    if query.version != PROTOCOL_VERSION {
        return api_error(
            StatusCode::BAD_REQUEST,
            "incompatible_protocol",
            "unsupported probe protocol version",
        );
    }
    if query.metadata.validate().is_err() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "invalid probe metadata",
        );
    }

    let id = query.metadata.id;
    let token = rand::random::<u64>();
    if !state.probes.reserve(id, token).await {
        return api_error(
            StatusCode::CONFLICT,
            "already_connected",
            "probe UUID is already connected",
        );
    }

    let failed = state.probes.clone();
    websocket
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_failed_upgrade(move |_| {
            tokio::spawn(async move {
                failed.remove(id, token).await;
            });
        })
        .on_upgrade(move |socket| handle_probe_connection(socket, state, query.metadata, token))
        .into_response()
}

async fn handle_probe_connection(
    socket: WebSocket,
    state: AppState,
    metadata: psa::api::ProbeMetadata,
    token: u64,
) {
    let id = metadata.id;
    let socket = socket
        .with(|frame| ready(Ok::<_, axum::Error>(encode_socket_frame(frame))))
        .map(|result| result.map(decode_socket_message));
    let (connection, _requests) = spawn_connection(socket, false);

    let registered = register_probe(&state.database, &metadata).await;
    let activated =
        registered.is_ok() && state.probes.activate(id, token, connection.clone()).await;
    if !activated {
        connection.close();
    }

    connection.closed().await;
    state.probes.remove(id, token).await;
}

fn encode_socket_frame(frame: Frame) -> WsMessage {
    match frame {
        Frame::Text(text) => WsMessage::Text(text.into()),
        Frame::Binary(bytes) => WsMessage::Binary(bytes.into()),
        Frame::Ping(bytes) => WsMessage::Ping(bytes.into()),
        Frame::Pong(bytes) => WsMessage::Pong(bytes.into()),
        Frame::Close => WsMessage::Close(None),
    }
}

fn decode_socket_message(message: WsMessage) -> Frame {
    match message {
        WsMessage::Text(text) => Frame::Text(text.to_string()),
        WsMessage::Binary(bytes) => Frame::Binary(bytes.to_vec()),
        WsMessage::Ping(bytes) => Frame::Ping(bytes.to_vec()),
        WsMessage::Pong(bytes) => Frame::Pong(bytes.to_vec()),
        WsMessage::Close(_) => Frame::Close,
    }
}

async fn register_probe(
    database: &Database,
    metadata: &psa::api::ProbeMetadata,
) -> Result<(), psa_store::DbError> {
    let mut cursor = database.write().await?;
    cursor.upsert_probe(metadata).await?;
    cursor.commit().await
}

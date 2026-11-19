//! Probe registration, discovery, and live connection ownership.
use crate::{db::Database, http::AppState};
use actors::{Mailbox, mailbox};
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
    probes::{
        Command, ConnectProbe, ContainerStatus, Event, Operation, PROTOCOL_VERSION, SandboxRequest,
    },
};
use psa_shared::probe_transport::{Call, Connection, Frame, ProbeError, spawn_connection};
use std::collections::HashMap;
use tokio::sync::oneshot;

#[mailbox(client = RegistryClient)]
enum RegistryMessage {
    /// Reserve an unused probe ID for a connection attempt.
    Reserve {
        id: ProbeId,
        token: u64,
        #[reply]
        reply: oneshot::Sender<bool>,
    },
    /// Attach a live connection if its reservation token matches.
    Activate {
        id: ProbeId,
        token: u64,
        connection: Connection,
        #[reply]
        reply: oneshot::Sender<bool>,
    },
    /// Remove a probe entry if its reservation token matches.
    Remove { id: ProbeId, token: u64 },
    /// Check whether a probe has a live connection.
    IsActive {
        id: ProbeId,
        #[reply]
        reply: oneshot::Sender<bool>,
    },
    /// Return the probe's live connection, if any.
    Handle {
        id: ProbeId,
        #[reply]
        reply: oneshot::Sender<Option<Connection>>,
    },
    /// Reject new connections and wait for all active connections to close.
    Shutdown {
        #[reply]
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
pub struct ProbeRegistry {
    client: RegistryClient,
    key: std::sync::Arc<str>,
}

impl ProbeRegistry {
    pub fn start(key: String) -> Self {
        let (handle, mailbox) = actors::channel(64);
        tokio::spawn(run_registry(mailbox));

        Self {
            client: RegistryClient::new(handle),
            key: key.into(),
        }
    }

    pub async fn is_active(&self, id: ProbeId) -> bool {
        self.client.is_active(id).await.unwrap_or(false)
    }

    pub async fn handle(&self, id: ProbeId) -> Result<ProbeHandle, ProbeError> {
        let connection = self
            .client
            .handle(id)
            .await
            .map_err(|_| ProbeError::Disconnected)?
            .ok_or(ProbeError::Disconnected)?;
        Ok(ProbeHandle { connection })
    }

    pub async fn disconnect(&self, id: ProbeId) -> Result<(), ProbeError> {
        let connection = self
            .client
            .handle(id)
            .await
            .map_err(|_| ProbeError::Disconnected)?
            .ok_or(ProbeError::Disconnected)?;
        connection.close();
        connection.closed().await;
        Ok(())
    }

    pub async fn shutdown(&self) {
        let _ = self.client.shutdown().await;
    }
}

async fn run_registry(mut mailbox: Mailbox<RegistryMessage>) {
    let mut connections = HashMap::<ProbeId, (u64, Option<Connection>)>::new();
    let mut stopping = false;

    while let Some(message) = mailbox.recv().await {
        match message {
            RegistryMessage::Reserve { id, token, reply } => {
                let accepted = !stopping && !connections.contains_key(&id);
                if accepted {
                    connections.insert(id, (token, None));
                }
                let _ = reply.send(accepted);
            }
            RegistryMessage::Activate {
                id,
                token,
                connection,
                reply,
            } => {
                let entry = connections
                    .get_mut(&id)
                    .filter(|entry| !stopping && entry.0 == token);
                let accepted = match entry {
                    Some((_, active)) => {
                        *active = Some(connection);
                        true
                    }
                    None => {
                        connection.close();
                        false
                    }
                };
                let _ = reply.send(accepted);
            }
            RegistryMessage::Remove { id, token } => {
                if connections.get(&id).is_some_and(|entry| entry.0 == token) {
                    connections.remove(&id);
                }
            }
            RegistryMessage::IsActive { id, reply } => {
                let active = active_connection(&connections, id).is_some();
                let _ = reply.send(active);
            }
            RegistryMessage::Handle { id, reply } => {
                let connection = active_connection(&connections, id).cloned();
                let _ = reply.send(connection);
            }
            RegistryMessage::Shutdown { reply } => {
                stopping = true;
                for connection in connections
                    .values()
                    .filter_map(|(_, connection)| connection.as_ref())
                {
                    connection.close();
                }
                for connection in connections
                    .values()
                    .filter_map(|(_, connection)| connection.as_ref())
                {
                    connection.closed().await;
                }
                connections.clear();
                let _ = reply.send(());
            }
        }
    }
}

fn active_connection(
    connections: &HashMap<ProbeId, (u64, Option<Connection>)>,
    id: ProbeId,
) -> Option<&Connection> {
    connections
        .get(&id)?
        .1
        .as_ref()
        .filter(|connection| !connection.is_closed())
}

/// A process call exposing streaming events, stdin, signals, and wait methods.
pub type RemoteChild = Call;

/// A handle refers to one connection. Obtain a new handle after reconnection.
#[derive(Clone)]
pub struct ProbeHandle {
    connection: Connection,
}

impl ProbeHandle {
    pub async fn request(&self, operation: Operation) -> Result<Call, ProbeError> {
        self.connection.start(operation).await
    }

    pub async fn read_file(&self, path: impl Into<std::path::PathBuf>) -> Result<Call, ProbeError> {
        self.request(Operation::ReadFile { path: path.into() })
            .await
    }

    pub async fn write_file(
        &self,
        path: impl Into<std::path::PathBuf>,
        append: bool,
    ) -> Result<Call, ProbeError> {
        self.request(Operation::WriteFile {
            path: path.into(),
            append,
        })
        .await
    }

    pub async fn spawn(&self, command: Command) -> Result<RemoteChild, ProbeError> {
        self.request(Operation::Spawn { command }).await
    }

    pub async fn create_dir(&self, path: impl Into<std::path::PathBuf>) -> Result<(), ProbeError> {
        let mut call = self
            .request(Operation::CreateDir { path: path.into() })
            .await?;
        match call.next_event().await? {
            Event::Complete { .. } => Ok(()),
            _ => Err(ProbeError::Protocol("expected completion".into())),
        }
    }

    pub async fn list_dir(
        &self,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<Vec<psa::api::probes::DirectoryEntry>, ProbeError> {
        let mut call = self
            .request(Operation::ListDir { path: path.into() })
            .await?;
        let mut result = Vec::new();
        loop {
            match call.next_event().await? {
                Event::Entries { entries } => result.extend(entries),
                Event::Complete { .. } => return Ok(result),
                _ => return Err(ProbeError::Protocol("unexpected directory response".into())),
            }
        }
    }

    /// Execute a sandbox request on a manager probe; inspections return container states.
    pub async fn sandbox(
        &self,
        request: SandboxRequest,
    ) -> Result<Option<Vec<ContainerStatus>>, ProbeError> {
        let mut call = self.request(Operation::Sandbox { request }).await?;
        let mut containers = None;
        loop {
            match call.next_event().await? {
                Event::Containers { containers: found } => containers = Some(found),
                Event::Complete { .. } => return Ok(containers),
                _ => return Err(ProbeError::Protocol("unexpected sandbox response".into())),
            }
        }
    }

    pub async fn upload_dir(
        &self,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<Call, ProbeError> {
        self.request(Operation::UploadDir { path: path.into() })
            .await
    }

    /// Stream a tar archive to atomically replace (or create) a probe directory.
    pub async fn sync_dir(&self, path: impl Into<std::path::PathBuf>) -> Result<Call, ProbeError> {
        self.request(Operation::SyncDir { path: path.into() }).await
    }

    pub async fn download_dir(
        &self,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<Call, ProbeError> {
        self.request(Operation::DownloadDir { path: path.into() })
            .await
    }
}

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

    if !crate::auth::has_bearer_key(&headers, &state.probes.key) {
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
    if !state
        .probes
        .client
        .reserve(id, token)
        .await
        .unwrap_or(false)
    {
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
                let _ = failed.client.remove(id, token).await;
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
    let activated = registered.is_ok()
        && state
            .probes
            .client
            .activate(id, token, connection.clone())
            .await
            .unwrap_or(false);
    if !activated {
        connection.close();
    }

    connection.closed().await;
    let _ = state.probes.client.remove(id, token).await;
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
) -> Result<(), crate::db::DbError> {
    let mut cursor = database.write().await?;
    cursor.upsert_probe(metadata).await?;
    cursor.commit().await
}

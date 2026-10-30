//! Linux probe runtime. Remote operations execute with this process's permissions.
pub mod config;
mod operations;
pub mod tools;

use config::ResolvedConfig;
use futures_util::{SinkExt, StreamExt, future::ready};
use psa_api::probes::PROTOCOL_VERSION;
use psa_shared::probe_transport::{Frame, spawn_connection};
use std::{future::Future, path::Path, pin::Pin, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{
        self, Message, client::IntoClientRequest, handshake::client::Request,
        protocol::WebSocketConfig,
    },
};

type ProbeSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("invalid server URL or authentication header")]
    InvalidConfiguration,
    #[error("server rejected probe authentication or protocol")]
    Rejected,
    #[error("probe I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Runs until shutdown. Network failures and duplicate UUIDs are retried.
pub async fn run(
    config: ResolvedConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ProbeError> {
    tools::ensure_tools(&config.metadata.data_dir).await?;
    let request = build_connect_request(&config)?;
    let bin = config.metadata.data_dir.join("bin");
    tokio::pin!(shutdown);

    let mut delay = 1u64;
    loop {
        let socket_config = WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let connect = connect_async_with_config(request.clone(), Some(socket_config), false);
        let attempt = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            result = tokio::time::timeout(Duration::from_secs(30), connect) => result,
        };

        match attempt {
            Ok(Ok((socket, _))) => {
                let connected = tokio::time::Instant::now();
                if run_connection(socket, &bin, shutdown.as_mut()).await {
                    return Ok(());
                }
                if connected.elapsed() >= Duration::from_secs(30) {
                    delay = 1;
                }
            }
            Ok(Err(tungstenite::Error::Http(response)))
                if response.status().is_client_error() && response.status().as_u16() != 409 =>
            {
                return Err(ProbeError::Rejected);
            }
            _ => {}
        }

        // UUID randomness supplies jitter without another random-number dependency.
        let random = psa_api::probes::Uuid::new_v4().as_u128() as u64;
        let pause = Duration::from_millis((delay * 1000 + random % (delay * 1000 + 1)).min(30_000));
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tokio::time::sleep(pause) => {}
        }
        delay = (delay * 2).min(30);
    }
}

fn build_connect_request(config: &ResolvedConfig) -> Result<Request, ProbeError> {
    let mut url =
        url::Url::parse(&config.server_url).map_err(|_| ProbeError::InvalidConfiguration)?;
    {
        let metadata = &config.metadata;
        let mut query = url.query_pairs_mut();
        query
            .append_pair("version", &PROTOCOL_VERSION.to_string())
            .append_pair("id", &metadata.id.to_string())
            .append_pair("hostname", &metadata.hostname)
            .append_pair("os", &metadata.os)
            .append_pair("architecture", &metadata.architecture)
            .append_pair("host", if metadata.host { "true" } else { "false" })
            .append_pair(
                "data_dir",
                metadata
                    .data_dir
                    .to_str()
                    .ok_or(ProbeError::InvalidConfiguration)?,
            );
        if let Some(label) = &metadata.label {
            query.append_pair("label", label);
        }
        if let Some(home_dir) = &metadata.home_dir {
            query.append_pair("home_dir", home_dir);
        }
    }

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| ProbeError::InvalidConfiguration)?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", config.key)
            .parse()
            .map_err(|_| ProbeError::InvalidConfiguration)?,
    );
    Ok(request)
}

/// Returns true on application shutdown, false when the connection was lost.
async fn run_connection(
    socket: ProbeSocket,
    bin: &Path,
    mut shutdown: Pin<&mut impl Future<Output = ()>>,
) -> bool {
    let socket = socket
        .with(|frame| ready(Ok::<_, tungstenite::Error>(encode_socket_frame(frame))))
        .map(|result| result.map(decode_socket_message));
    let (connection, mut incoming) = spawn_connection(socket, true);
    let mut tasks = tokio::task::JoinSet::new();

    let shutting_down = loop {
        tokio::select! {
            _ = &mut shutdown => break true,
            _ = connection.closed() => break false,
            request = incoming.recv() => {
                let Some((operation, call)) = request else { break false; };
                tasks.spawn(operations::execute(operation, call, bin.to_owned()));
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    };

    connection.close();
    connection.closed().await;
    while tasks.join_next().await.is_some() {}

    shutting_down
}

fn encode_socket_frame(frame: Frame) -> Message {
    match frame {
        Frame::Text(text) => Message::Text(text.into()),
        Frame::Binary(bytes) => Message::Binary(bytes.into()),
        Frame::Ping(bytes) => Message::Ping(bytes.into()),
        Frame::Pong(bytes) => Message::Pong(bytes.into()),
        Frame::Close => Message::Close(None),
    }
}

fn decode_socket_message(message: Message) -> Frame {
    match message {
        Message::Text(text) => Frame::Text(text.to_string()),
        Message::Binary(bytes) => Frame::Binary(bytes.to_vec()),
        Message::Ping(bytes) => Frame::Ping(bytes.to_vec()),
        Message::Pong(bytes) => Frame::Pong(bytes.to_vec()),
        Message::Close(_) | Message::Frame(_) => Frame::Close,
    }
}

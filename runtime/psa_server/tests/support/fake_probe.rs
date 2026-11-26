//! Connects the runtime's fake probe through the server's authenticated
//! WebSocket route, as a real probe would.

use futures_util::{SinkExt, StreamExt};
use psa::api::probes::PROTOCOL_VERSION;
pub use psa_agents::probes::fake_probe::{FakeFiles, FakeProbe, PROBE_KEY};
use psa_server::probes::ProbeRegistry;
use psa_shared::probe_transport::Frame;
use std::{future::ready, net::SocketAddr, sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

#[allow(dead_code)]
pub async fn connect(address: SocketAddr, probes: &ProbeRegistry) -> FakeProbe {
    connect_inner(address, probes, None).await
}

#[allow(dead_code)]
pub async fn connect_with_files(
    address: SocketAddr,
    probes: &ProbeRegistry,
    files: FakeFiles,
) -> FakeProbe {
    connect_inner(address, probes, Some(Arc::new(files))).await
}

async fn connect_inner(
    address: SocketAddr,
    probes: &ProbeRegistry,
    files: Option<Arc<FakeFiles>>,
) -> FakeProbe {
    let metadata = psa_store::test_support::probe_metadata();
    let sandboxes = files.as_ref().is_some_and(|files| files.serves_sandboxes());
    let mut request = format!(
        "ws://{address}/probes/connect?version={PROTOCOL_VERSION}&id={}&hostname={}&os={}&architecture={}&data_dir=%2Ftmp%2Fpsa-test&sandboxes={sandboxes}",
        metadata.id, metadata.hostname, metadata.os, metadata.architecture,
    )
    .into_client_request()
    .unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {PROBE_KEY}").parse().unwrap(),
    );

    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .unwrap()
    .unwrap();
    let socket = socket
        .with(|frame| {
            ready(Ok::<_, tokio_tungstenite::tungstenite::Error>(
                match frame {
                    Frame::Text(text) => Message::Text(text.into()),
                    Frame::Binary(bytes) => Message::Binary(bytes.into()),
                    Frame::Ping(bytes) => Message::Ping(bytes.into()),
                    Frame::Pong(bytes) => Message::Pong(bytes.into()),
                    Frame::Close => Message::Close(None),
                },
            ))
        })
        .map(|message| {
            message.map(|message| match message {
                Message::Text(text) => Frame::Text(text.to_string()),
                Message::Binary(bytes) => Frame::Binary(bytes.to_vec()),
                Message::Ping(bytes) => Frame::Ping(bytes.to_vec()),
                Message::Pong(bytes) => Frame::Pong(bytes.to_vec()),
                Message::Close(_) | Message::Frame(_) => Frame::Close,
            })
        });
    FakeProbe::serve(socket, probes, files).await
}

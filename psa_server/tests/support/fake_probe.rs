use futures_util::{SinkExt, StreamExt};
use psa_api::probes::PROTOCOL_VERSION;
use psa_server::probes::ProbeRegistry;
use std::{net::SocketAddr, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

pub const PROBE_KEY: &str = "test-key";

/// Keep a real authenticated probe connection alive without a psa_probe dependency.
pub struct FakeProbe {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl FakeProbe {
    pub async fn connect(address: SocketAddr, probes: &ProbeRegistry) -> Self {
        let metadata = super::support::probe_metadata();
        let mut request = format!(
            "ws://{address}/probes/connect?version={PROTOCOL_VERSION}&id={}&hostname={}&os={}&architecture={}&data_dir=%2Ftmp%2Fpsa-test",
            metadata.id, metadata.hostname, metadata.os, metadata.architecture,
        )
        .into_client_request()
        .unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {PROBE_KEY}").parse().unwrap(),
        );

        let (mut socket, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .unwrap()
        .unwrap();

        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = &mut stopped => {
                        let _ = socket.close(None).await;
                        break;
                    }
                    message = socket.next() => message,
                };
                match message {
                    Some(Ok(Message::Ping(bytes))) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        });

        Self::wait_active(probes, true).await;
        Self { stop, task }
    }

    pub async fn disconnect(self, probes: &ProbeRegistry) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap();
        Self::wait_active(probes, false).await;
    }

    async fn wait_active(probes: &ProbeRegistry, expected_active: bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while probes.is_active(super::support::probe_metadata().id).await != expected_active {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

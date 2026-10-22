use futures_util::{SinkExt, StreamExt};
use psa_api::{
    probe_transport::{Call, Frame, spawn_connection},
    probes::{DirectoryEntry, Event, Operation, PROTOCOL_VERSION},
};
use psa_server::probes::ProbeRegistry;
use std::{
    collections::BTreeMap, future::ready, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

pub const PROBE_KEY: &str = "test-key";

/// In-memory remote filesystem, including explicitly injected remote failures.
#[derive(Default)]
pub struct FakeFiles {
    files: BTreeMap<PathBuf, Result<Vec<u8>, String>>,
    directories: BTreeMap<PathBuf, Result<Vec<DirectoryEntry>, String>>,
}

#[allow(dead_code)] // Builders are used by context unit tests, not every integration test.
impl FakeFiles {
    pub fn file(mut self, path: &str, content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        self.add_entry(path, "file", content.len() as u64);
        self.files.insert(path.into(), Ok(content));
        self
    }

    pub fn symlink(mut self, path: &str, content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        self.add_entry(path, "symlink", content.len() as u64);
        self.files.insert(path.into(), Ok(content));
        self
    }

    pub fn unreadable(mut self, path: &str) -> Self {
        self.add_entry(path, "file", 0);
        self.files
            .insert(path.into(), Err("permission denied".into()));
        self
    }

    pub fn directory(mut self, path: &str) -> Self {
        self.add_entry(path, "directory", 0);
        self.directories
            .entry(path.into())
            .or_insert_with(|| Ok(Vec::new()));
        self
    }

    pub fn list_error(mut self, path: &str) -> Self {
        self.directories
            .insert(path.into(), Err("permission denied".into()));
        self
    }

    fn add_entry(&mut self, path: &str, kind: &str, size: u64) {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            self.directories
                .entry(parent.into())
                .or_insert_with(|| Ok(Vec::new()))
                .as_mut()
                .expect("cannot add entries to an unreadable directory")
                .push(DirectoryEntry {
                    name: path.file_name().unwrap().to_str().unwrap().into(),
                    kind: kind.into(),
                    size,
                });
            for ancestor in parent.ancestors() {
                self.directories
                    .entry(ancestor.into())
                    .or_insert_with(|| Ok(Vec::new()));
            }
        }
    }

    async fn execute(&self, operation: Operation, call: Call) {
        let result = match operation {
            Operation::ListDir { path } => {
                let entries = self
                    .directories
                    .get(&path)
                    .cloned()
                    .unwrap_or_else(|| Ok(Vec::new()));
                match entries {
                    Ok(entries) => call
                        .sender
                        .event(Event::Entries { entries })
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error),
                }
            }
            Operation::ReadFile { path } => match self.files.get(&path) {
                Some(Ok(bytes)) => match call.sender.write(1, bytes).await {
                    Ok(()) => call
                        .sender
                        .event(Event::Eof { stream: 1 })
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                },
                Some(Err(error)) => Err(error.clone()),
                None => Err("file not found".into()),
            },
            _ => Err("unsupported operation".into()),
        };
        let event = match result {
            Ok(()) => Event::Complete { status: None },
            Err(message) => Event::Error {
                code: "io".into(),
                message,
            },
        };
        let _ = call.sender.finish(event).await;
    }
}

/// Keep a real authenticated probe connection alive without a psa_probe dependency.
pub struct FakeProbe {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl FakeProbe {
    pub async fn connect(address: SocketAddr, probes: &ProbeRegistry) -> Self {
        Self::connect_inner(address, probes, None).await
    }

    #[allow(dead_code)]
    pub async fn connect_with_files(
        address: SocketAddr,
        probes: &ProbeRegistry,
        files: FakeFiles,
    ) -> Self {
        Self::connect_inner(address, probes, Some(Arc::new(files))).await
    }

    async fn connect_inner(
        address: SocketAddr,
        probes: &ProbeRegistry,
        files: Option<Arc<FakeFiles>>,
    ) -> Self {
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

        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .unwrap()
        .unwrap();

        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            // Keep the original connection-only fake inert for existing callers.
            let Some(files) = files else {
                let mut socket = socket;
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
                return;
            };
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
            let (connection, mut incoming) = spawn_connection(socket, true);
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = connection.closed() => break,
                    request = incoming.recv() => {
                        let Some((operation, call)) = request else { break; };
                        let files = files.clone();
                        tasks.spawn(async move { files.execute(operation, call).await; });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
            connection.close();
            connection.closed().await;
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
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

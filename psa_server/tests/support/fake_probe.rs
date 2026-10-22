use futures_util::{SinkExt, StreamExt};
use psa_api::{
    probe_transport::{Call, Frame, spawn_connection},
    probes::{DirectoryEntry, Event, Operation, PROTOCOL_VERSION},
};
use psa_server::probes::ProbeRegistry;
use std::{
    collections::BTreeMap,
    future::ready,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

pub const PROBE_KEY: &str = "test-key";

type ProbeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// In-memory remote filesystem, including explicitly injected remote failures.
#[derive(Default)]
pub struct FakeFiles {
    files: Mutex<BTreeMap<PathBuf, Result<Vec<u8>, String>>>,
    directories: Mutex<BTreeMap<PathBuf, Result<Vec<DirectoryEntry>, String>>>,
}

#[allow(dead_code)] // Builders are used by context unit tests, not every integration test.
impl FakeFiles {
    pub fn file(mut self, path: &str, content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        self.add_entry(path, "file", content.len() as u64);
        self.files.get_mut().insert(path.into(), Ok(content));
        self
    }

    pub fn symlink(mut self, path: &str, content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        self.add_entry(path, "symlink", content.len() as u64);
        self.files.get_mut().insert(path.into(), Ok(content));
        self
    }

    pub fn unreadable(mut self, path: &str) -> Self {
        self.add_entry(path, "file", 0);
        self.files
            .get_mut()
            .insert(path.into(), Err("permission denied".into()));
        self
    }

    pub fn directory(mut self, path: &str) -> Self {
        self.add_entry(path, "directory", 0);
        self.directories
            .get_mut()
            .entry(path.into())
            .or_insert_with(|| Ok(Vec::new()));
        self
    }

    pub fn list_error(mut self, path: &str) -> Self {
        self.directories
            .get_mut()
            .insert(path.into(), Err("permission denied".into()));
        self
    }

    fn add_entry(&mut self, path: &str, kind: &str, size: u64) {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            self.directories
                .get_mut()
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
                    .get_mut()
                    .entry(ancestor.into())
                    .or_insert_with(|| Ok(Vec::new()));
            }
        }
    }

    async fn create_directories(&self, path: PathBuf) -> Result<(), String> {
        let files = self.files.lock().await;
        let mut directories = self.directories.lock().await;
        let ancestors: Vec<_> = path.ancestors().collect();
        for ancestor in ancestors.into_iter().rev() {
            if files.contains_key(ancestor) {
                return Err("not a directory".into());
            }
            if let Some(Err(error)) = directories.get(ancestor) {
                return Err(error.clone());
            }
            directories
                .entry(ancestor.into())
                .or_insert_with(|| Ok(Vec::new()));
            if let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) {
                let entries = directories.get_mut(parent).unwrap().as_mut().unwrap();
                let name = name.to_str().unwrap();
                if !entries.iter().any(|entry| entry.name == name) {
                    entries.push(DirectoryEntry {
                        name: name.into(),
                        kind: "directory".into(),
                        size: 0,
                    });
                }
            }
        }
        Ok(())
    }

    async fn write_file(&self, path: PathBuf, append: bool, call: &mut Call) -> Result<(), String> {
        {
            let files = self.files.lock().await;
            let directories = self.directories.lock().await;
            if let Some(Err(error)) = files.get(&path) {
                return Err(error.clone());
            }
            if directories.contains_key(&path) {
                return Err("is a directory".into());
            }
            let parent = path.parent().ok_or("file not found")?;
            match directories.get(parent) {
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.clone()),
                None => return Err("file not found".into()),
            }
        }

        let mut bytes = Vec::new();
        loop {
            match call.next_event().await.map_err(|error| error.to_string())? {
                Event::Data {
                    stream: 0,
                    bytes: chunk,
                } => bytes.extend(chunk),
                Event::Eof { stream: 0 } => break,
                _ => return Err("unexpected write event".into()),
            }
        }

        let mut files = self.files.lock().await;
        if append && let Some(Ok(previous)) = files.get(&path) {
            let mut combined = previous.clone();
            combined.extend(bytes);
            bytes = combined;
        }
        let size = bytes.len() as u64;
        files.insert(path.clone(), Ok(bytes));
        let mut directories = self.directories.lock().await;
        let entries = directories
            .get_mut(path.parent().unwrap())
            .unwrap()
            .as_mut()
            .unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.name == name) {
            entry.size = size;
        } else {
            entries.push(DirectoryEntry {
                name: name.into(),
                kind: "file".into(),
                size,
            });
        }
        Ok(())
    }

    async fn read_file(&self, path: &Path, call: &Call) -> Result<(), String> {
        let bytes = self
            .files
            .lock()
            .await
            .get(path)
            .cloned()
            .unwrap_or_else(|| Err("file not found".into()))?;

        call.sender
            .write(1, &bytes)
            .await
            .map_err(|error| error.to_string())?;
        call.sender
            .event(Event::Eof { stream: 1 })
            .await
            .map_err(|error| error.to_string())
    }

    async fn execute(&self, operation: Operation, mut call: Call) {
        let result = match operation {
            Operation::ListDir { path } => {
                let entries = self
                    .directories
                    .lock()
                    .await
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
            Operation::ReadFile { path } => self.read_file(&path, &call).await,
            Operation::Spawn { command }
                if command.program == "sh"
                    && command.args.len() == 4
                    && command.args[..3] == ["-c", "test -e \"$1\"", "read"] =>
            {
                let path = PathBuf::from(&command.args[3]);
                let exists = self.files.lock().await.contains_key(&path)
                    || self.directories.lock().await.contains_key(&path);
                let _ = call
                    .sender
                    .finish(Event::Complete {
                        status: Some(psa_api::probes::ExitStatus {
                            code: Some(if exists { 0 } else { 1 }),
                            signal: None,
                        }),
                    })
                    .await;
                return;
            }
            Operation::CreateDir { path } => self.create_directories(path).await,
            Operation::WriteFile { path, append } => self.write_file(path, append, &mut call).await,
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
                run_inert_connection(socket, stopped).await;
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

async fn run_inert_connection(mut socket: ProbeSocket, mut stopped: oneshot::Receiver<()>) {
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
}

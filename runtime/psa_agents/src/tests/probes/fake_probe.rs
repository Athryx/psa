//! A fake probe serving an in-memory filesystem, connected to a
//! [`ProbeRegistry`] directly or over any frame transport.

use crate::probes::ProbeRegistry;
use futures_util::{Sink, Stream};
use psa::api::{
    ProbeId, SandboxState,
    probes::{ContainerStatus, DirectoryEntry, Event, Operation, SandboxRequest},
};
use psa_shared::probe_transport::{Call, Frame, spawn_connection};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};

pub const PROBE_KEY: &str = "test-key";

/// In-memory remote filesystem, including explicitly injected remote failures.
#[derive(Default)]
pub struct FakeFiles {
    files: Mutex<BTreeMap<PathBuf, Result<Vec<u8>, String>>>,
    directories: Mutex<BTreeMap<PathBuf, Result<Vec<DirectoryEntry>, String>>>,
    /// Simulated sandbox containers; `None` when sandbox support is not advertised.
    sandboxes: Option<Mutex<BTreeMap<String, ContainerStatus>>>,
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

    /// Advertise sandbox support and simulate containers in memory.
    /// Whether the probe advertises sandbox support.
    pub fn serves_sandboxes(&self) -> bool {
        self.sandboxes.is_some()
    }

    pub fn with_sandboxes(mut self) -> Self {
        self.sandboxes = Some(Mutex::new(BTreeMap::new()));
        self
    }

    async fn execute_sandbox(
        &self,
        request: SandboxRequest,
        call: &mut Call,
    ) -> Result<(), String> {
        let containers = self.sandboxes.as_ref().ok_or("sandboxes disabled")?;
        let mut containers = containers.lock().await;
        let mut set_state = |probe_id: ProbeId, state| {
            containers
                .get_mut(&probe_id.to_string())
                .map(|container| container.state = state)
                .ok_or_else(|| "sandbox container does not exist".to_owned())
        };
        match request {
            SandboxRequest::Create { spec } => {
                // Seeds stream after the request; they must arrive whole and in order.
                if !spec.seeds.is_empty() {
                    let received = receive_input(call).await?;
                    let announced: u64 = spec.seeds.iter().map(|seed| seed.size).sum();
                    if received.len() as u64 != announced {
                        return Err(format!(
                            "received {} seed bytes, announced {announced}",
                            received.len()
                        ));
                    }
                }
                if spec.mounts.iter().any(|mount| mount.seed.is_some()) {
                    return Err("seeds must not be inlined in the request".into());
                }
                let container = ContainerStatus {
                    probe_id: spec.probe_id,
                    container_name: format!("psa-sandbox-{}", spec.name),
                    state: SandboxState::Stopped,
                };
                containers.insert(spec.probe_id.to_string(), container);
            }
            SandboxRequest::Start { probe_id } => set_state(probe_id, SandboxState::Running)?,
            SandboxRequest::Stop { probe_id } => set_state(probe_id, SandboxState::Stopped)?,
            SandboxRequest::Remove { probe_id } => {
                containers.remove(&probe_id.to_string());
            }
            SandboxRequest::Inspect => {
                let containers = containers.values().cloned().collect();
                call.sender
                    .event(Event::Containers { containers })
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
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
                        status: Some(psa::api::probes::ExitStatus {
                            code: Some(if exists { 0 } else { 1 }),
                            signal: None,
                        }),
                    })
                    .await;
                return;
            }
            Operation::CreateDir { path } => self.create_directories(path).await,
            Operation::Sandbox { request } => self.execute_sandbox(request, &mut call).await,
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

/// Read input stream 0 until its end.
async fn receive_input(call: &mut Call) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    loop {
        match call.next_event().await.map_err(|error| error.to_string())? {
            Event::Data {
                stream: 0,
                bytes: chunk,
            } => bytes.extend(chunk),
            Event::Eof { stream: 0 } => return Ok(bytes),
            _ => return Err("unexpected input event".into()),
        }
    }
}

/// Keeps a fake probe connection alive without a psa_probe dependency. With
/// no files, requests are left unanswered.
pub struct FakeProbe {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl FakeProbe {
    /// Connect directly to `probes`, as the test fixture probe.
    pub async fn attach(probes: &ProbeRegistry) -> Self {
        Self::attach_inner(probes, None).await
    }

    /// Connect directly to `probes`, serving `files`.
    pub async fn attach_with_files(probes: &ProbeRegistry, files: FakeFiles) -> Self {
        Self::attach_inner(probes, Some(Arc::new(files))).await
    }

    async fn attach_inner(probes: &ProbeRegistry, files: Option<Arc<FakeFiles>>) -> Self {
        let (server_end, probe_end) = duplex();
        let (connection, _requests) = spawn_connection(server_end, false);
        let id = crate::test_support::probe_metadata().id;
        let token = 0;
        assert!(
            probes.reserve(id, token).await,
            "fake probe already connected"
        );
        assert!(probes.activate(id, token, connection).await);
        Self::serve(probe_end, probes, files).await
    }

    /// Serve `files` over an established transport, returning once `probes`
    /// reports the fixture probe connected.
    pub async fn serve<S, E>(
        socket: S,
        probes: &ProbeRegistry,
        files: Option<Arc<FakeFiles>>,
    ) -> Self
    where
        S: Sink<Frame, Error = E> + Stream<Item = Result<Frame, E>> + Send + Unpin + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (connection, mut incoming) = spawn_connection(socket, true);
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = connection.closed() => break,
                    request = incoming.recv() => {
                        let Some((operation, call)) = request else { break; };
                        let Some(files) = files.clone() else { continue; };
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
            while probes
                .is_active(crate::test_support::probe_metadata().id)
                .await
                != expected_active
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

/// One end of an in-memory frame transport.
struct Duplex {
    sender: mpsc::UnboundedSender<Frame>,
    receiver: mpsc::UnboundedReceiver<Frame>,
}

fn duplex() -> (Duplex, Duplex) {
    let (to_probe, from_server) = mpsc::unbounded_channel();
    let (to_server, from_probe) = mpsc::unbounded_channel();
    (
        Duplex {
            sender: to_probe,
            receiver: from_probe,
        },
        Duplex {
            sender: to_server,
            receiver: from_server,
        },
    )
}

impl Stream for Duplex {
    type Item = Result<Frame, String>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context).map(|frame| frame.map(Ok))
    }
}

impl Sink<Frame> for Duplex {
    type Error = String;

    fn poll_ready(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), String>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, frame: Frame) -> Result<(), String> {
        self.sender
            .send(frame)
            .map_err(|_| "fake probe transport closed".to_owned())
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), String>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), String>> {
        self.receiver.close();
        Poll::Ready(Ok(()))
    }
}

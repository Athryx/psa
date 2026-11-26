//! Live probe connection ownership, independent of the transport that
//! established each connection.
use actors::{Mailbox, mailbox};
use psa::api::{
    ProbeId,
    probes::{Command, ContainerStatus, Event, Operation, SandboxRequest, SandboxSpec},
};
use psa_shared::probe_transport::{Call, Connection, ProbeError};
use std::collections::HashMap;
use tokio::sync::oneshot;

#[cfg(any(test, feature = "testing"))]
#[path = "tests/probes/fake_probe.rs"]
pub mod fake_probe;

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

    /// The key probes present to connect.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Reserve `id` for a connection attempt identified by `token`. Fails if
    /// the probe is already connected or the registry is shutting down.
    pub async fn reserve(&self, id: ProbeId, token: u64) -> bool {
        self.client.reserve(id, token).await.unwrap_or(false)
    }

    /// Attach a live connection to a reservation made with the same token.
    pub async fn activate(&self, id: ProbeId, token: u64, connection: Connection) -> bool {
        self.client
            .activate(id, token, connection)
            .await
            .unwrap_or(false)
    }

    /// Release a reservation or connection made with `token`.
    pub async fn remove(&self, id: ProbeId, token: u64) {
        let _ = self.client.remove(id, token).await;
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

    /// Create a sandbox on a manager probe, streaming `seeds` (the `tar_data` of
    /// each transfer in `spec.seeds`, in order) after the request.
    pub async fn create_sandbox(
        &self,
        spec: SandboxSpec,
        seeds: &[Vec<u8>],
    ) -> Result<(), ProbeError> {
        let mut call = self
            .request(Operation::Sandbox {
                request: SandboxRequest::Create { spec },
            })
            .await?;
        let sender = call.sender.clone();
        let send = async {
            for seed in seeds {
                sender.write(0, seed).await?;
            }
            sender.close_stdin().await
        };
        let receive = async {
            match call.next_event().await? {
                Event::Complete { .. } => Ok(()),
                _ => Err(ProbeError::Protocol("unexpected sandbox response".into())),
            }
        };
        tokio::pin!(send, receive);

        // The probe may finish (usually with an error) before reading all input;
        // its result then wins and the remaining input is dropped.
        tokio::select! {
            received = &mut receive => received,
            sent = &mut send => {
                let received = receive.await;
                received.and(sent)
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

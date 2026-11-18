//! Probes sessions run on: plain probes, and sandboxes whose nested probe
//! runs in a container managed by another probe.
use crate::{
    Context, Error, Result,
    api::{self, ClientError, ProbeId, SandboxState},
};
use std::time::Duration;

/// How long [`ProbeHandle::ensure_up`] waits for a sandbox's probe to connect.
const SANDBOX_START_TIMEOUT: Duration = Duration::from_secs(120);
const SANDBOX_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Selects one probe among those a context may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSelector {
    /// The probe that shares the server's filesystem.
    Host,
    /// A probe or sandbox by its probe ID.
    Id(ProbeId),
    /// A probe whose display label matches exactly.
    Label(String),
    /// A probe whose hostname matches exactly.
    Hostname(String),
    /// A sandbox by name.
    Sandbox(String),
}

impl ProbeSelector {
    pub(crate) fn matches(&self, probe: &ProbeHandle) -> bool {
        let metadata = probe.probe.as_ref().map(|probe| &probe.metadata);
        match self {
            Self::Host => metadata.is_some_and(|metadata| metadata.host),
            Self::Id(id) => probe.id == *id,
            Self::Label(label) => {
                metadata.is_some_and(|metadata| metadata.label.as_deref() == Some(label.as_str()))
            }
            Self::Hostname(hostname) => {
                metadata.is_some_and(|metadata| metadata.hostname == *hostname)
            }
            Self::Sandbox(name) => probe
                .sandbox
                .as_ref()
                .is_some_and(|sandbox| sandbox.name == *name),
        }
    }
}

/// A probe or sandbox, holding the records it was last loaded with.
///
/// A sandbox's nested probe has no probe record until it first connects, so
/// [`Self::info`] can be `None` for a new sandbox.
#[derive(Clone)]
pub struct ProbeHandle {
    context: Context,
    id: ProbeId,
    probe: Option<api::Probe>,
    sandbox: Option<api::Sandbox>,
}

impl ProbeHandle {
    pub(crate) fn new(
        context: Context,
        id: ProbeId,
        probe: Option<api::Probe>,
        sandbox: Option<api::Sandbox>,
    ) -> Self {
        Self {
            context,
            id,
            probe,
            sandbox,
        }
    }

    /// Load a probe by ID, with its sandbox record when it is a sandbox.
    pub(crate) async fn load(context: Context, id: ProbeId) -> Result<Self> {
        let probe = optional(context.client().get_probe(id).await)?;
        let sandbox = optional(context.client().get_sandbox(id).await)?;
        if probe.is_none() && sandbox.is_none() {
            return Err(Error::ProbeNotFound(ProbeSelector::Id(id)));
        }
        Ok(Self::new(context, id, probe, sandbox))
    }

    /// Probe ID; for a sandbox, its nested probe's ID.
    pub fn id(&self) -> ProbeId {
        self.id
    }

    /// Probe record, absent for a sandbox whose probe has never connected.
    pub fn info(&self) -> Option<&api::Probe> {
        self.probe.as_ref()
    }

    /// Sandbox record, when this probe runs in a sandbox.
    pub fn sandbox(&self) -> Option<&api::Sandbox> {
        self.sandbox.as_ref()
    }

    /// Whether this probe runs in a sandbox.
    pub fn is_sandbox(&self) -> bool {
        self.sandbox.is_some()
    }

    /// Whether the probe was connected when last loaded.
    pub fn is_live(&self) -> bool {
        match &self.sandbox {
            Some(sandbox) => sandbox.probe_active,
            None => self.probe.as_ref().is_some_and(|probe| probe.is_active),
        }
    }

    /// Reload the probe and sandbox records.
    pub async fn reload(&mut self) -> Result<()> {
        *self = Self::load(self.context.clone(), self.id).await?;
        Ok(())
    }

    /// Make sure the probe is connected. A stopped sandbox is started and
    /// waited for; a plain probe cannot be started, so an offline one fails
    /// with [`Error::ProbeOffline`].
    pub async fn ensure_up(&mut self) -> Result<()> {
        self.reload().await?;
        if self.is_live() {
            return Ok(());
        }
        let Some(sandbox) = &self.sandbox else {
            return Err(Error::ProbeOffline(self.id));
        };

        match sandbox.state {
            SandboxState::Running => {}
            SandboxState::Stopped => {
                self.sandbox = Some(self.context.client().start_sandbox(self.id).await?);
            }
            state => {
                return Err(Error::SandboxUnavailable {
                    probe_id: self.id,
                    state,
                });
            }
        }
        self.wait_until_live().await
    }

    /// Stop the probe's sandbox container. Does nothing for a plain probe.
    pub async fn stop(&mut self) -> Result<()> {
        if self.sandbox.is_none() {
            return Ok(());
        }
        self.sandbox = Some(self.context.client().stop_sandbox(self.id).await?);
        Ok(())
    }

    async fn wait_until_live(&mut self) -> Result<()> {
        let id = self.id;
        let waiting = async {
            loop {
                self.reload().await?;
                if self.is_live() {
                    return Ok(());
                }
                tokio::time::sleep(SANDBOX_POLL_INTERVAL).await;
            }
        };
        tokio::time::timeout(SANDBOX_START_TIMEOUT, waiting)
            .await
            .map_err(|_| Error::ProbeStartTimeout(id))?
    }
}

/// Treat HTTP 404 as an absent record.
fn optional<T>(result: Result<T, ClientError>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ClientError::Api { status: 404, .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

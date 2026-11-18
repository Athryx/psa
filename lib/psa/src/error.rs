//! Errors returned by the high-level client layer.
use crate::{ProbeSelector, api};
use std::path::PathBuf;

/// Failure of a high-level operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport, HTTP, or response-decoding failure from the wire client.
    #[error(transparent)]
    Api(#[from] api::ClientError),
    /// The project name is not a valid [`api::ProjectId`].
    #[error("invalid project: {0}")]
    InvalidProject(String),
    /// No registered probe matches the context's selector.
    #[error("no registered probe matches {0:?}")]
    ProbeNotFound(ProbeSelector),
    /// Several registered probes match the context's selector.
    #[error("more than one probe matches {0:?}; select one by ID")]
    AmbiguousProbe(ProbeSelector),
    /// A probe exists but is outside the context's probe restriction.
    #[error("probe {0:?} is outside this context's probe restriction")]
    ProbeNotAllowed(api::ProbeId),
    /// A plain probe is not connected; only sandboxes can be started.
    #[error("probe {0:?} is offline")]
    ProbeOffline(api::ProbeId),
    /// A sandbox cannot be started in its current state.
    #[error("sandbox {probe_id:?} cannot be started while {state:?}")]
    SandboxUnavailable {
        /// Sandbox probe.
        probe_id: api::ProbeId,
        /// Container state that prevents starting it.
        state: api::SandboxState,
    },
    /// A started sandbox's probe did not connect in time.
    #[error("sandbox {0:?} did not connect in time")]
    ProbeStartTimeout(api::ProbeId),
    /// A session was created without a working directory on the context, the
    /// spec, or the probe's home directory.
    #[error("no working directory: set one on the context or the session spec")]
    MissingWorkingDirectory,
    /// A named agent config is in neither the context's group nor the global group.
    #[error("no agent config named `{0}` in this context's group or the global group")]
    UnknownConfig(String),
    /// A named skill is in neither the context's group nor the global group.
    #[error("no skill named `{0}` in this context's group or the global group")]
    UnknownSkill(String),
    /// A layout tree defines two different agent layouts with this name.
    #[error("agent layouts named `{0}` differ within one layout tree")]
    ConflictingConfig(String),
    /// A layout tree defines two different skills with this name.
    #[error("skills named `{0}` differ within one layout tree")]
    ConflictingSkill(String),
    /// A skill directory could not be read or packaged.
    #[error("invalid skill at {}: {reason}", path.display())]
    InvalidSkill {
        /// Skill directory as given.
        path: PathBuf,
        /// What was wrong with it.
        reason: String,
    },
    /// The event range cannot be read the requested way.
    #[error("invalid event range: {0}")]
    InvalidRange(&'static str),
    /// The server accepted a request and later reported that it failed.
    #[error("agent request failed: {0}")]
    RequestFailed(String),
    /// An event stream ended while waiting for a run.
    #[error("event stream ended before the run finished")]
    StreamEnded,
    /// A run finished without recording a result.
    #[error("the run finished without a result")]
    NoResult,
    /// A result payload does not match the requested type.
    #[error("result does not match the requested type: {0}")]
    InvalidResult(#[from] serde_json::Error),
}

/// Result of a high-level operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;

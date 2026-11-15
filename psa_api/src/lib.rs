//! Shared wire types and a small asynchronous client for trusted PSA clients.
//!
//! Use [`Client`] for HTTP requests and server-sent events. The [`probes`] module
//! defines the separate server/probe protocol; shared runtime utilities live in
//! the `psa_shared` crate.

#![warn(missing_docs)]

pub mod agent_defaults;
pub mod client;
pub mod client_queries;
pub mod completion;
pub mod components;
pub mod decisions;
pub mod definitions;
pub mod events;
pub mod models;
pub mod probes;
pub mod results;
pub mod routes;
pub mod sandboxes;
mod types;

#[cfg(test)]
mod tests;

pub use agent_defaults::{AgentConfigInput, AgentDefaults, HookOverrides};
pub use client::{
    Client, ClientError, EventStream, GlobalEventStream, GlobalStreamEvent, StreamEvent,
};
pub use client_queries::*;
pub use completion::{CompletionConfigurationError, validate_completion_modes};
pub use components::{BundleConfig, HookConfig, HookConfigs};
pub use decisions::*;
pub use definitions::*;
pub use models::*;
pub use probes::{Probe, ProbeId, ProbeMetadata};
pub use results::*;
pub use sandboxes::{CreateSandbox, ListSandboxes, Mount, Sandbox, SandboxState};
pub use types::*;

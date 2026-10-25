//! Shared wire types and a small asynchronous client for trusted PSA clients.
//!
//! Use [`Client`] for HTTP requests and server-sent events. The [`probes`] module
//! defines the separate server/probe protocol; shared runtime utilities live in
//! the `psa_shared` crate.

#![warn(missing_docs)]

pub mod agent_defaults;
pub mod client;
pub use agent_defaults::{AgentConfigInput, AgentDefaults, HookOverrides};
pub mod client_queries;
pub mod completion;
pub mod components;
pub use completion::{CompletionConfigurationError, validate_completion_config};
pub use components::{BundleConfig, HookConfig, HookConfigs};
pub mod definitions;
pub use definitions::*;
pub mod events;
pub mod models;
pub mod results;
pub mod routes;
mod types;

pub use client::{Client, ClientError, EventStream, StreamEvent};
pub use client_queries::*;
pub use models::*;
pub use results::*;
pub use types::*;

#[cfg(test)]
mod tests;

pub mod probes;
pub use probes::{Probe, ProbeId, ProbeMetadata};

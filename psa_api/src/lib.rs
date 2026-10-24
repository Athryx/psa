//! Shared wire types and a small asynchronous client for trusted PSA clients.

pub mod client;
pub mod client_queries;
pub mod definitions;
pub use definitions::*;
pub mod events;
pub mod models;
pub mod routes;
mod types;

pub use client::{Client, ClientError, EventStream, StreamEvent};
pub use client_queries::*;
pub use models::*;
pub use types::*;

#[cfg(test)]
mod tests;

pub mod probe_transport;
pub mod probes;
pub use probes::{Probe, ProbeId, ProbeMetadata};

pub mod local_config;

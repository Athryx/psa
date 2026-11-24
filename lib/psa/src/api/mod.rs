//! Wire types shared by the PSA server, its clients and probes, and an async
//! HTTP/SSE [`Client`].
//!
//! Most types are re-exported at this module's root. Three modules stay namespaced:
//! [`events`] (transcript events), [`probes`] (the server/probe protocol), and
//! [`routes`] (HTTP route templates).

pub mod events;
pub mod probes;
pub mod routes;

mod agents;
pub(crate) mod archive;
mod client;
mod client_queries;
mod components;
mod configs;
mod decisions;
mod definitions;
mod feeds;
mod models;
mod results;
mod sandboxes;
mod sessions;
mod skills;

#[cfg(test)]
mod tests;

pub use agents::*;
pub use archive::{Archive, ArchiveBuilder, ArchiveCompression};
pub use client::*;
pub use client_queries::*;
pub use components::*;
pub use configs::*;
pub use decisions::*;
pub use definitions::*;
pub use feeds::*;
pub use models::*;
pub use probes::{Probe, ProbeId, ProbeMetadata};
pub use results::*;
pub use sandboxes::*;
pub use sessions::*;
pub use skills::*;

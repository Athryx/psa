//! Shared wire types and a small asynchronous client for trusted PSA clients.

pub mod client;
pub mod events;
pub mod routes;
mod types;

pub use client::{Client, ClientError, EventStream, StreamEvent};
pub use types::*;

#[cfg(test)]
mod tests;

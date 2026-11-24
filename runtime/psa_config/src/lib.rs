#![doc = include_str!("../README.md")]

pub mod agent_definition;
pub mod auth;
pub mod paths;
pub mod server;

use std::path::PathBuf;

/// A filesystem operation on a configuration file failed.
#[derive(Debug, thiserror::Error)]
#[error("cannot access {path}: {source}")]
pub struct PathIoError {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

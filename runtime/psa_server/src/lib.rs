#![doc = include_str!("../README.md")]

pub mod agents;
pub mod components;
pub mod config;
pub mod config_registry;
pub mod http;
mod local_control;
pub mod prompts;
pub mod server;
pub mod trace_export;

pub mod auth;
pub mod probes;
pub mod sandboxes;

#[cfg(test)]
extern crate self as psa_server;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
pub(crate) mod test_support;

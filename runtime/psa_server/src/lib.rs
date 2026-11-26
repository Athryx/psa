#![doc = include_str!("../README.md")]

pub mod config;
pub mod config_registry;
pub mod http;
mod local_control;
pub mod server;

pub mod auth;
pub mod probes;
pub mod sandboxes;

#[cfg(test)]
extern crate self as psa_server;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
pub(crate) mod test_support;

/// Tests of agent invocations against the HTTP API and a connected fake probe.
#[cfg(test)]
#[path = "tests/agent_context_tests/mod.rs"]
mod agent_context_tests;
/// CodeMode and Bash sharing sessions on a real probe.
#[cfg(test)]
#[path = "tests/codemode_probe_tests.rs"]
mod codemode_probe_tests;

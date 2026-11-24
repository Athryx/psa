//! What the engine needs from the agent that runs a script.

use psa_shared::cancellation::Cancellation;
use serde_json::Value;
use std::{future::Future, time::Duration};

/// A Python-level failure raised inside the script, e.g. `TypeError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeModeError {
    pub exception_type: String,
    pub message: String,
}

impl CodeModeError {
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            exception_type: "RuntimeError".into(),
            message: message.into(),
        }
    }
}

/// A host function as scripts see it.
#[derive(Debug, Clone)]
pub struct HostFunction {
    pub name: String,
    pub description: String,
    /// Returns a Python future, so scripts can run calls concurrently. Other
    /// functions suspend the worker and resume with a plain value.
    pub is_async: bool,
    /// Field that a single positional argument is bound to, for lookups such
    /// as `prior_results(-1)`. Other functions take keywords or one object.
    pub positional_field: Option<&'static str>,
    /// Cooperative cleanup allowed after cancellation, capped by the engine.
    pub cancellation_timeout: Duration,
}

/// Checkpoint storage and host functions for one script invocation.
pub trait CodeModeHost: Sync {
    /// The latest committed interpreter checkpoint, if any.
    fn load_snapshot(&self) -> impl Future<Output = Result<Option<Vec<u8>>, String>> + Send;

    /// Python fed before every script, such as result classes and wrappers.
    fn prelude(&self) -> Option<String>;

    fn functions(&self) -> Vec<HostFunction>;

    /// Run `name`, one of [`Self::functions`], with JSON arguments.
    fn call(
        &self,
        name: &str,
        arguments: Value,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<Value, CodeModeError>> + Send;
}

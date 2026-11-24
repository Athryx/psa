//! CodeMode: run model-written Python in an isolated Monty worker.
//!
//! [`MontyEngine`] owns the worker pool and the script protocol. The agent
//! runtime supplies checkpoints and host functions through [`CodeModeHost`].
//! [`python_stub`] and [`result_classes`] render the Python the model sees.

mod engine;
mod host;
pub mod python_stub;
pub mod result_classes;

pub use engine::{EngineOutput, MAX_SAVED_OUTPUT_CHARS, MontyEngine};
pub use host::{CodeModeError, CodeModeHost, HostFunction};

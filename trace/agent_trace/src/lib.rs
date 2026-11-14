//! Harness-neutral agent session traces.
//!
//! A trace directory holds `session.json` (metadata, agents, tool definitions), one
//! JSON-lines event log per agent under `agents/`, and images under `blobs/`. The
//! format keeps each agent's active conversation branch as a linear list of events,
//! trading exact fidelity for a shape that is simple to analyze across harnesses.

pub mod import;
pub mod model;
pub mod render;
pub mod stats;
pub mod store;

pub use model::{
    AgentMeta, AgentTrace, Event, EventKind, FORMAT_VERSION, FinishOutcome, ImageRef, Inference,
    SessionMeta, SessionTrace, Source, ToolDefinition, ToolResult, attach_tool_result,
};
pub use store::{StoreError, TraceFile};

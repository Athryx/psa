//! UI-independent desktop state. Persist settings/layout, not live caches or drafts.
pub mod agents;
pub mod commands;
pub mod connection;
pub mod context;
pub mod decisions;
pub mod links;
pub mod local;
pub mod sessions;
pub mod settings;
pub mod status;
pub mod transcript;
pub mod usage;
pub mod workspace;

pub use agents::*;
pub use commands::*;
pub use connection::*;
pub use context::*;
pub use decisions::*;
pub use links::*;
pub use local::*;
pub use sessions::*;
pub use settings::*;
pub use status::*;
pub use transcript::*;
pub use usage::*;
pub use workspace::*;

#[cfg(test)]
mod tests;

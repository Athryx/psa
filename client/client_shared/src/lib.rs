//! UI-independent desktop state. Persist settings/layout, not live caches or drafts.
pub mod agents;
pub mod commands;
pub mod connection;
pub mod local;
pub mod sessions;
pub mod settings;
pub mod transcript;
pub mod workspace;

pub use agents::*;
pub use commands::*;
pub use connection::*;
pub use local::*;
pub use sessions::*;
pub use settings::*;
pub use transcript::*;
pub use workspace::*;

#[cfg(test)]
mod tests;

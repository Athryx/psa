//! SQLite is private to this module; callers exchange domain sessions/agents/events.

mod agent_definitions;
mod agents;
mod configurations;
mod error;
mod events;
mod models;
mod payloads;
mod probes;
mod queries;
mod sandboxes;
mod schema;
mod sessions;
mod snapshots;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

use crate::agents::AgentId;

pub use agent_definitions::{CreateAgentDefinition, UpdateAgentDefinition};
pub use configurations::{AgentConfigGraph, SkillRegistration};
pub use error::DbError;
pub use events::{
    AddEventResult, EventBatchItem, EventRange, MAX_CODEMODE_SNAPSHOT_BYTES, NewEvent,
    PrivateEvent, PrivateEventKind,
};
pub use queries::InferenceResultEvents;
pub use sandboxes::SandboxRecord;

#[derive(Debug, Clone)]
pub struct Database {
    pool: sqlx::SqlitePool,
    pending_commits: Arc<Mutex<HashMap<AgentId, Vec<watch::Receiver<()>>>>>,
}

/// A transaction-scoped view of the database. Dropping it rolls back all changes.
#[must_use = "a database cursor must be committed to persist changes"]
pub struct DatabaseCursor {
    transaction: sqlx::Transaction<'static, sqlx::Sqlite>,
}

impl Database {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self {
            pool,
            pending_commits: Arc::default(),
        })
    }

    /// Begin a transaction, reserving the SQLite writer lock up front.
    pub async fn cursor(&self) -> Result<DatabaseCursor, DbError> {
        let transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Ok(DatabaseCursor { transaction })
    }

    /// Register an in-flight append before it is spawned. Dropping the returned
    /// sender marks completion, including on task failure or abort. Do not send.
    #[must_use = "hold the sender until the registered append has finished"]
    pub(crate) fn register_commit(&self, agent: AgentId) -> watch::Sender<()> {
        let (sender, receiver) = watch::channel(());
        let mut pending = self
            .pending_commits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let receivers = pending.entry(agent).or_default();
        receivers.retain(|receiver| receiver.has_changed().is_ok());
        receivers.push(receiver);
        sender
    }

    /// Wait for registered appends after the agent's worker has exited. Callers
    /// must serialize drains for an agent and prevent new registrations during a
    /// drain; supervisor recovery provides that ordering.
    pub(crate) async fn drain_commits(&self, agent: AgentId) {
        // Keep the original receivers registered while waiting so cancellation
        // cannot lose track of still-running appends.
        let receivers = self
            .pending_commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&agent)
            .cloned()
            .unwrap_or_default();
        for mut receiver in receivers {
            while receiver.changed().await.is_ok() {}
        }
        let mut pending = self
            .pending_commits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(receivers) = pending.get_mut(&agent) {
            receivers.retain(|receiver| receiver.has_changed().is_ok());
            if receivers.is_empty() {
                pending.remove(&agent);
            }
        }
    }

    /// Close the pool after all active cursors have been committed or dropped.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

impl DatabaseCursor {
    fn connection(&mut self) -> &mut sqlx::SqliteConnection {
        &mut self.transaction
    }

    pub async fn commit(self) -> Result<(), DbError> {
        self.transaction.commit().await?;
        Ok(())
    }

    pub async fn rollback(self) -> Result<(), DbError> {
        self.transaction.rollback().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

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
use std::{path::Path, time::Duration};

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
        Ok(Self { pool })
    }

    /// Begin a transaction, reserving the SQLite writer lock up front.
    pub async fn cursor(&self) -> Result<DatabaseCursor, DbError> {
        let transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Ok(DatabaseCursor { transaction })
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

//! SQLite is private to this module; callers exchange domain sessions/agents/events.

mod agent_definitions;
mod agents;
mod cache_usage;
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
    ops::{Deref, DerefMut},
    path::Path,
    time::Duration,
};

pub use agent_definitions::CreateAgentDefinition;
pub use cache_usage::CacheTracking;
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

/// A read-only transaction over a consistent snapshot of the database.
///
/// It is opened deferred, so under WAL it never takes the SQLite writer lock and
/// does not wait on writers. Keep it short-lived: an open read transaction
/// prevents WAL checkpoints from completing. Dropping it ends the transaction.
pub struct ReadCursor {
    transaction: sqlx::Transaction<'static, sqlx::Sqlite>,
}

/// A write transaction that reserves the SQLite writer lock when it begins.
///
/// It dereferences to [`ReadCursor`], so every read is also available while
/// writing. Taking the lock up front means a transaction that reads and then
/// writes can never fail to upgrade. Dropping it rolls back all changes.
#[must_use = "a write cursor must be committed to persist changes"]
pub struct WriteCursor {
    cursor: ReadCursor,
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

    /// Begin a read-only transaction that does not take the writer lock.
    pub async fn read(&self) -> Result<ReadCursor, DbError> {
        let transaction = self.pool.begin_with("BEGIN DEFERRED").await?;
        Ok(ReadCursor { transaction })
    }

    /// Begin a write transaction, reserving the SQLite writer lock up front.
    pub async fn write(&self) -> Result<WriteCursor, DbError> {
        let transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Ok(WriteCursor {
            cursor: ReadCursor { transaction },
        })
    }

    /// Close the pool after all active cursors have been closed, committed or dropped.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

impl ReadCursor {
    fn connection(&mut self) -> &mut sqlx::SqliteConnection {
        &mut self.transaction
    }

    /// End the transaction now instead of when the cursor is dropped.
    pub async fn close(self) -> Result<(), DbError> {
        self.transaction.rollback().await?;
        Ok(())
    }
}

impl WriteCursor {
    pub async fn commit(self) -> Result<(), DbError> {
        self.cursor.transaction.commit().await?;
        Ok(())
    }

    pub async fn rollback(self) -> Result<(), DbError> {
        self.cursor.transaction.rollback().await?;
        Ok(())
    }
}

impl Deref for WriteCursor {
    type Target = ReadCursor;

    fn deref(&self) -> &ReadCursor {
        &self.cursor
    }
}

impl DerefMut for WriteCursor {
    fn deref_mut(&mut self) -> &mut ReadCursor {
        &mut self.cursor
    }
}

#[cfg(test)]
mod tests;

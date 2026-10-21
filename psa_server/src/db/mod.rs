//! SQLite is private to this module; callers exchange domain sessions/agents/events.

mod agents;
mod error;
mod events;
mod payloads;
mod probes;
mod queries;
mod schema;
mod sessions;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::{path::Path, time::Duration};

pub use error::DbError;
pub use events::AddEventResult;
pub use queries::InferenceResultEvents;

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

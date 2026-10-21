#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("invalid stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{entity} {id} does not exist")]
    NotFound { entity: &'static str, id: u64 },
    #[error("probe {0} does not exist")]
    ProbeNotFound(psa_api::ProbeId),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid stored data: {0}")]
    Corrupt(String),
    #[error("inference result {0} has no visible end marker")]
    IncompleteInference(u64),
}

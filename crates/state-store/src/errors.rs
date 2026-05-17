//! State store error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("record not found: {entity} with id {id}")]
    NotFound { entity: String, id: String },

    #[error("integrity check failed: {0}")]
    IntegrityCheckFailed(String),
}

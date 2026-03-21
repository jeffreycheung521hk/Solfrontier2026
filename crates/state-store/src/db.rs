//! Database connection pool initialization and migration runner.
//!
//! The `Database` struct is the entry point for all state-store operations.
//! It wraps `SqlitePool` and exposes it to all repositories.

use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use tracing::{info, warn};

use crate::errors::StoreError;

/// Configuration for the database connection.
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// Path to the SQLite database file. Use `:memory:` for tests.
    pub path: String,
    /// Maximum number of connections in the pool.
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: "./data/claw.db".to_string(),
            max_connections: 5,
        }
    }
}

/// The primary database handle. Clone freely — it wraps an `Arc<Pool>`.
#[derive(Clone, Debug)]
pub struct Database {
    pub(crate) pool: SqlitePool,
}

impl Database {
    /// Opens the database, applies pending migrations, and runs an integrity
    /// check. This is the only way to create a `Database` instance.
    pub async fn open(config: &DatabaseConfig) -> Result<Self, StoreError> {
        // Ensure the parent directory exists for file-based databases.
        if config.path != ":memory:" {
            if let Some(parent) = Path::new(&config.path).parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    StoreError::IntegrityCheckFailed(format!(
                        "cannot create data directory: {e}"
                    ))
                })?;
            }
        }

        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", config.path))
            .map_err(StoreError::Sqlx)?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .foreign_keys(true);

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(config.max_connections)
            .connect_with(options)
            .await
            .map_err(StoreError::Sqlx)?;

        let db = Self { pool };
        db.run_migrations().await?;
        db.integrity_check().await?;

        info!(path = %config.path, "database opened");
        Ok(db)
    }

    /// Opens an in-memory database for testing.
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        Self::open(&DatabaseConfig {
            path: ":memory:".to_string(),
            max_connections: 1,
        })
        .await
    }

    async fn run_migrations(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./src/migrations")
            .run(&self.pool)
            .await
            .map_err(StoreError::Migration)?;
        info!("database migrations applied");
        Ok(())
    }

    async fn integrity_check(&self) -> Result<(), StoreError> {
        let result: (String,) = sqlx::query_as("PRAGMA integrity_check")
            .fetch_one(&self.pool)
            .await
            .map_err(StoreError::Sqlx)?;

        if result.0 != "ok" {
            warn!(result = %result.0, "database integrity check failed");
            return Err(StoreError::IntegrityCheckFailed(result.0));
        }
        Ok(())
    }

    /// Returns the underlying pool for repositories.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

//! Wallet registry repository.

use chrono::Utc;
use sqlx::{FromRow, SqlitePool};

use crate::errors::StoreError;

#[derive(Clone, Debug)]
pub struct WalletRepository {
    pool: SqlitePool,
}

impl WalletRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn register(
        &self,
        pubkey:      &str,
        label:       Option<&str>,
        signer_type: &str,
        network:     &str,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT OR REPLACE INTO wallets (pubkey, label, signer_type, network, created_at, is_active)
             VALUES (?, ?, ?, ?, ?, 1)",
        )
        .bind(pubkey)
        .bind(label)
        .bind(signer_type)
        .bind(network)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_active(&self) -> Result<Vec<WalletRow>, StoreError> {
        let rows = sqlx::query_as::<_, WalletRow>(
            "SELECT pubkey, label, signer_type, network, created_at, is_active
             FROM wallets
             WHERE is_active = 1
             ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    pub async fn find(&self, pubkey: &str) -> Result<Option<WalletRow>, StoreError> {
        let row = sqlx::query_as::<_, WalletRow>(
            "SELECT pubkey, label, signer_type, network, created_at, is_active
             FROM wallets
             WHERE pubkey = ?",
        )
        .bind(pubkey)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }
}

#[derive(Debug, FromRow)]
pub struct WalletRow {
    pub pubkey:      String,
    pub label:       Option<String>,
    pub signer_type: String,
    pub network:     String,
    pub created_at:  i64,
    pub is_active:   i64,
}

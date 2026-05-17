//! Repository for wallet ownership challenge-response (Phase 1).
//!
//! Persists nonce challenges so that the server can verify that a
//! client actually controls the private key of the claimed wallet.

use chrono::Utc;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::errors::StoreError;

/// A persisted wallet bind challenge.
#[derive(Debug, Clone)]
pub struct WalletChallengeRow {
    /// Unique challenge ID.
    pub id:            String,
    /// Session that requested the challenge.
    pub session_id:    String,
    /// Claimed wallet pubkey (base58).
    pub wallet_pubkey: String,
    /// Random nonce (hex-encoded).
    pub nonce:         String,
    /// Canonical message that must be signed.
    pub message:       String,
    /// Unix epoch milliseconds when created.
    pub created_at:    i64,
    /// Unix epoch milliseconds when this challenge expires.
    pub expires_at:    i64,
    /// Unix epoch milliseconds when consumed (None if unused).
    pub used_at:       Option<i64>,
}

/// Repository for wallet bind challenges.
#[derive(Clone, Debug)]
pub struct WalletChallengeRepository {
    pool: SqlitePool,
}

impl WalletChallengeRepository {
    /// Create a new repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new challenge. Returns the challenge ID.
    pub async fn create_challenge(
        &self,
        session_id:    &str,
        wallet_pubkey: &str,
        nonce:         &str,
        message:       &str,
        expires_at:    i64,
    ) -> Result<String, StoreError> {
        let id  = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO wallet_bind_challenges
                 (id, session_id, wallet_pubkey, nonce, message, created_at, expires_at, used_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(&id)
        .bind(session_id)
        .bind(wallet_pubkey)
        .bind(nonce)
        .bind(message)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Load a challenge by ID. Returns None if not found.
    pub async fn get_challenge(&self, challenge_id: &str) -> Result<Option<WalletChallengeRow>, StoreError> {
        let row = sqlx::query_as::<_, (String, String, String, String, String, i64, i64, Option<i64>)>(
            "SELECT id, session_id, wallet_pubkey, nonce, message, created_at, expires_at, used_at
             FROM wallet_bind_challenges
             WHERE id = ?",
        )
        .bind(challenge_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(id, session_id, wallet_pubkey, nonce, message, created_at, expires_at, used_at)| {
            WalletChallengeRow { id, session_id, wallet_pubkey, nonce, message, created_at, expires_at, used_at }
        }))
    }

    /// Mark a challenge as used. Returns true if updated.
    /// Only updates if the challenge exists and has not been used yet.
    pub async fn mark_used(&self, challenge_id: &str) -> Result<bool, StoreError> {
        let now = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE wallet_bind_challenges SET used_at = ? WHERE id = ? AND used_at IS NULL",
        )
        .bind(now)
        .bind(challenge_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Delete expired and used challenges older than the given retention (ms).
    pub async fn purge_old(&self, retention_ms: i64) -> Result<u64, StoreError> {
        let cutoff = Utc::now().timestamp_millis() - retention_ms;
        let result = sqlx::query(
            "DELETE FROM wallet_bind_challenges WHERE created_at < ?",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

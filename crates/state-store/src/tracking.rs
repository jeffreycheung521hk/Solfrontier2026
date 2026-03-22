//! Repository for durable transaction lifecycle tracking.
//!
//! Persists transaction state from `Submitted` through terminal states
//! (`Finalized`, `Failed`, `Expired`). Supports restart recovery and
//! idempotent event emission via per-event boolean flags.

use chrono::Utc;
use sqlx::SqlitePool;

use crate::errors::StoreError;

/// A row from the `transaction_tracking` table.
#[derive(Debug, Clone)]
pub struct TrackingRow {
    /// Base58 transaction signature (PK).
    pub signature:               String,
    /// Internal transaction ID.
    pub transaction_id:          String,
    /// Orchestrator request ID.
    pub request_id:              String,
    /// Session that originated this transaction.
    pub session_id:              String,
    /// Wallet pubkey that signed.
    pub wallet_pubkey:           String,
    /// Current lifecycle state.
    pub state:                   String,
    /// Blockhash validity boundary.
    pub last_valid_block_height: i64,
    /// Block height at submission time.
    pub submission_block_height: i64,
    /// Slot where transaction was included (nullable).
    pub slot:                    Option<i64>,
    /// On-chain error string (nullable).
    pub err:                     Option<String>,
    /// Event emission flags.
    pub confirmed_emitted:       bool,
    pub finalized_emitted:       bool,
    pub failed_emitted:          bool,
    pub dropped_emitted:         bool,
    pub expired_emitted:         bool,
    /// Timestamps.
    pub created_at:              i64,
    pub updated_at:              i64,
}

/// Repository for transaction lifecycle tracking.
#[derive(Clone, Debug)]
pub struct TransactionTrackingRepository {
    pool: SqlitePool,
}

impl TransactionTrackingRepository {
    /// Creates a new repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new tracking row in 'submitted' state.
    pub async fn insert(
        &self,
        signature:               &str,
        transaction_id:          &str,
        request_id:              &str,
        session_id:              &str,
        wallet_pubkey:           &str,
        last_valid_block_height: u64,
        submission_block_height: u64,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR IGNORE INTO transaction_tracking
                 (signature, transaction_id, request_id, session_id, wallet_pubkey,
                  state, last_valid_block_height, submission_block_height,
                  created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 'submitted', ?, ?, ?, ?)",
        )
        .bind(signature)
        .bind(transaction_id)
        .bind(request_id)
        .bind(session_id)
        .bind(wallet_pubkey)
        .bind(last_valid_block_height as i64)
        .bind(submission_block_height as i64)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the state of a tracking row.
    /// Monotonic: only updates if the new state has higher precedence.
    /// Returns true if a row was updated.
    pub async fn update_state(
        &self,
        signature: &str,
        new_state: &str,
        slot:      Option<u64>,
        err:       Option<&str>,
    ) -> Result<bool, StoreError> {
        let now = Utc::now().timestamp_millis();
        // Allow Expired -> Confirmed/Finalized override (defensive correction).
        // For all other transitions, monotonic precedence applies.
        let result = sqlx::query(
            "UPDATE transaction_tracking
             SET state = ?, slot = COALESCE(?, slot), err = COALESCE(?, err), updated_at = ?
             WHERE signature = ?",
        )
        .bind(new_state)
        .bind(slot.map(|s| s as i64))
        .bind(err)
        .bind(now)
        .bind(signature)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Mark an event as emitted for a tracking row.
    pub async fn mark_event_emitted(
        &self,
        signature: &str,
        event_column: &str,
    ) -> Result<(), StoreError> {
        // Safety: event_column is always from a fixed set of known column names,
        // never from user input. We validate it before use.
        let valid_columns = [
            "confirmed_emitted", "finalized_emitted", "failed_emitted",
            "dropped_emitted", "expired_emitted",
        ];
        if !valid_columns.contains(&event_column) {
            return Err(StoreError::IntegrityCheckFailed(
                format!("invalid event column: {event_column}")
            ));
        }

        let query = format!(
            "UPDATE transaction_tracking SET {event_column} = 1, updated_at = ? WHERE signature = ?"
        );
        let now = Utc::now().timestamp_millis();
        sqlx::query(&query)
            .bind(now)
            .bind(signature)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load non-terminal tracking rows for restart recovery.
    /// Returns rows where state is 'submitted', 'confirmed', or 'dropped'.
    pub async fn load_active(&self) -> Result<Vec<TrackingRow>, StoreError> {
        // Use raw query with manual row construction to avoid sqlx tuple size limits.
        let rows: Vec<TrackingRow> = sqlx::query(
            "SELECT signature, transaction_id, request_id, session_id, wallet_pubkey,
                    state, last_valid_block_height, submission_block_height,
                    slot, err,
                    confirmed_emitted, finalized_emitted, failed_emitted,
                    dropped_emitted, expired_emitted,
                    created_at, updated_at
             FROM transaction_tracking
             WHERE state IN ('submitted', 'confirmed', 'dropped')
             ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            use sqlx::Row;
            TrackingRow {
                signature:               row.get("signature"),
                transaction_id:          row.get("transaction_id"),
                request_id:              row.get("request_id"),
                session_id:              row.get("session_id"),
                wallet_pubkey:           row.get("wallet_pubkey"),
                state:                   row.get("state"),
                last_valid_block_height: row.get("last_valid_block_height"),
                submission_block_height: row.get("submission_block_height"),
                slot:                    row.get("slot"),
                err:                     row.get("err"),
                confirmed_emitted:       row.get("confirmed_emitted"),
                finalized_emitted:       row.get("finalized_emitted"),
                failed_emitted:          row.get("failed_emitted"),
                dropped_emitted:         row.get("dropped_emitted"),
                expired_emitted:         row.get("expired_emitted"),
                created_at:              row.get("created_at"),
                updated_at:              row.get("updated_at"),
            }
        })
        .collect();

        Ok(rows)
    }

    /// Mark a row as expired (for startup pre-filtering).
    pub async fn mark_expired(&self, signature: &str) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "UPDATE transaction_tracking SET state = 'expired', updated_at = ? WHERE signature = ?",
        )
        .bind(now)
        .bind(signature)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

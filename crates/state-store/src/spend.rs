//! Spend tracking repository — append-only ledger.
//!
//! Records actual spend (lamports) when transactions are signed.
//! Provides accumulation queries for policy evaluation.
//!
//! # Recording point
//!
//! Spend is recorded at the `Signed` stage — when the signer has produced
//! a signature. This is the last stage before on-chain submission and
//! represents an irreversible commitment of the keypair.
//!
//! # Double-count protection
//!
//! The `spend_ledger.transaction_id` column has a UNIQUE constraint.
//! `record_spend()` uses `INSERT OR IGNORE` — duplicate calls for the
//! same `transaction_id` are silently ignored. This makes the recording
//! call idempotent.
//!
//! # Spend unit
//!
//! Lamports. Specifically `fee_lamports` from the simulation result.
//! This represents the actual on-chain cost to the wallet (transaction
//! fee + any value transferred). For V1, `fee_lamports` is the best
//! available approximation of total wallet debit.

use chrono::Utc;
use sqlx::SqlitePool;
use tracing::instrument;
use uuid::Uuid;

use crate::errors::StoreError;

/// Repository for the spend ledger.
#[derive(Clone, Debug)]
pub struct SpendRepository {
    pool: SqlitePool,
}

impl SpendRepository {
    /// Creates a new spend repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Records a spend entry. Idempotent — duplicate `transaction_id`
    /// inserts are silently ignored via `INSERT OR IGNORE`.
    ///
    /// Returns `true` if the row was inserted, `false` if it was a duplicate.
    #[instrument(skip(self), fields(wallet = %wallet_pubkey, tx = %transaction_id))]
    pub async fn record_spend(
        &self,
        wallet_pubkey:  &str,
        session_id:     &str,
        transaction_id: &str,
        lamports_spent: u64,
    ) -> Result<bool, StoreError> {
        let id   = Uuid::new_v4().to_string();
        let now  = Utc::now().timestamp_millis();
        let date = Utc::now().format("%Y-%m-%d").to_string();

        let result = sqlx::query(
            "INSERT OR IGNORE INTO spend_ledger
                 (id, wallet_pubkey, session_id, transaction_id, lamports_spent, recorded_date, recorded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(wallet_pubkey)
        .bind(session_id)
        .bind(transaction_id)
        .bind(lamports_spent as i64)
        .bind(&date)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Returns the total lamports spent by a wallet on a given UTC date.
    ///
    /// Used by policy evaluation to check daily spend caps.
    #[instrument(skip(self), fields(wallet = %wallet_pubkey, date = %date))]
    pub async fn wallet_daily_spend(
        &self,
        wallet_pubkey: &str,
        date:          &str,
    ) -> Result<u64, StoreError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(lamports_spent), 0)
             FROM spend_ledger
             WHERE wallet_pubkey = ? AND recorded_date = ?",
        )
        .bind(wallet_pubkey)
        .bind(date)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0 as u64)
    }

    /// Returns the total lamports spent in a specific session.
    ///
    /// Used by policy evaluation to check per-session spend caps.
    #[instrument(skip(self), fields(session = %session_id))]
    pub async fn session_spend(
        &self,
        session_id: &str,
    ) -> Result<u64, StoreError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(lamports_spent), 0)
             FROM spend_ledger
             WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0 as u64)
    }

    /// Returns the number of spend entries for a wallet on a given date.
    ///
    /// Useful for audit and testing.
    pub async fn wallet_daily_count(
        &self,
        wallet_pubkey: &str,
        date:          &str,
    ) -> Result<i64, StoreError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM spend_ledger
             WHERE wallet_pubkey = ? AND recorded_date = ?",
        )
        .bind(wallet_pubkey)
        .bind(date)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }
}

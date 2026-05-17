//! Repository for durable pending workflow state (P1-1).
//!
//! Persists live pending state to SQLite so that daemon restarts do not lose
//! in-flight approval / signing workflows.
//!
//! # Tables
//!
//! - `pending_approvals` — approval requests awaiting human decision, plus the
//!   PendingSigningStore data needed to reconstruct the resume path.
//! - `pending_signatures` — orchestrator execution-plane pending entries
//!   (external wallet signatures in progress).
//! - `pending_completion_meta` — fee metadata for spend recording at completion.
//!
//! # Lifecycle state
//!
//! Each row carries an explicit `state` column:
//! - `pending`  — live, recoverable on restart
//! - `consumed` — terminal: decision made / signature completed
//! - `expired`  — terminal: TTL exceeded
//! - `rejected` — terminal: verification failed or operator rejected
//!
//! Startup recovery only loads `state = 'pending'` rows. Terminal rows remain
//! in the DB for post-mortem / audit correlation and are cleaned up lazily.
//!
//! # Mutation ordering
//!
//! - **CREATE**: DB write must succeed BEFORE the request is acknowledged as live.
//! - **CONSUME**: DB state must be marked terminal BEFORE in-memory removal.
//!   This prevents stale resurrection if the process crashes between the two.

use chrono::Utc;
use sqlx::SqlitePool;
use tracing::instrument;

use crate::errors::StoreError;

/// Row from `pending_approvals` table.
#[derive(Debug, Clone)]
pub struct PendingApprovalRow {
    /// The approval request ID (UUID string).
    pub request_id:      String,
    /// Session ID.
    pub session_id:      String,
    /// Transaction proposal ID.
    pub transaction_id:  String,
    /// Human-readable description.
    pub description:     String,
    /// Wallet pubkey string.
    pub wallet_pubkey:   String,
    /// Full ApprovalRequest as JSON.
    pub approval_json:   String,
    /// Finalized transaction bytes (hex-encoded bincode).
    pub tx_bytes_hex:    String,
    /// SimulationResult as JSON.
    pub simulation_json: String,
    /// PolicyVerdict as JSON.
    pub verdict_json:    String,
    /// Unix epoch milliseconds when created.
    pub created_at:      i64,
}

/// Row from `pending_signatures` table.
#[derive(Debug, Clone)]
pub struct PendingSignatureRow {
    /// The signature request ID (UUID string).
    pub request_id:         String,
    /// Session ID.
    pub session_id:         String,
    /// Transaction proposal ID.
    pub transaction_id:     String,
    /// Expected signer pubkey.
    pub expected_signer:    String,
    /// Human-readable description.
    pub description:        String,
    /// Finalized unsigned transaction bytes (raw bincode).
    pub finalized_tx_bytes: Vec<u8>,
    /// Adapter type (e.g. "external").
    pub adapter_type:       String,
    /// Unix epoch milliseconds when created.
    pub created_at:         i64,
}

/// Row from `pending_completion_meta` table.
#[derive(Debug, Clone)]
pub struct PendingCompletionMetaRow {
    /// The signature request ID (UUID string).
    pub request_id:   String,
    /// Fee from simulation (nullable).
    pub fee_lamports: Option<i64>,
    /// Unix epoch milliseconds when created.
    pub created_at:   i64,
}

/// Repository for durable pending state.
#[derive(Clone, Debug)]
pub struct PendingStateRepository {
    pool: SqlitePool,
}

impl PendingStateRepository {
    /// Creates a new repository wrapping the given pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── Pending approvals ────────────────────────────────────────────────

    /// Insert a pending approval row in 'pending' state.
    /// Idempotent (INSERT OR REPLACE).
    #[instrument(skip(self, approval_json, tx_bytes_hex, simulation_json, verdict_json))]
    pub async fn insert_pending_approval(
        &self,
        request_id:      &str,
        session_id:      &str,
        transaction_id:  &str,
        description:     &str,
        wallet_pubkey:   &str,
        approval_json:   &str,
        tx_bytes_hex:    &str,
        simulation_json: &str,
        verdict_json:    &str,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR REPLACE INTO pending_approvals
                 (request_id, session_id, transaction_id, description, wallet_pubkey,
                  approval_json, tx_bytes_hex, simulation_json, verdict_json, created_at, state)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(request_id)
        .bind(session_id)
        .bind(transaction_id)
        .bind(description)
        .bind(wallet_pubkey)
        .bind(approval_json)
        .bind(tx_bytes_hex)
        .bind(simulation_json)
        .bind(verdict_json)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a pending approval as terminal. Returns true if a row was updated.
    /// The row is NOT deleted — it stays for audit correlation and crash safety.
    pub async fn mark_approval_terminal(
        &self,
        request_id: &str,
        terminal_state: &str,
        reason: Option<&str>,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE pending_approvals SET state = ?, terminal_reason = ? WHERE request_id = ? AND state = 'pending'"
        )
        .bind(terminal_state)
        .bind(reason)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove a pending approval row entirely (for cleanup of corrupt rows).
    pub async fn remove_pending_approval(&self, request_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM pending_approvals WHERE request_id = ?")
            .bind(request_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Load only 'pending' approval rows (for startup recovery).
    pub async fn load_pending_approvals(&self) -> Result<Vec<PendingApprovalRow>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, String, String, String, String, String, String, String, i64)>(
            "SELECT request_id, session_id, transaction_id, description, wallet_pubkey,
                    approval_json, tx_bytes_hex, simulation_json, verdict_json, created_at
             FROM pending_approvals
             WHERE state = 'pending'
             ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(request_id, session_id, transaction_id, description, wallet_pubkey,
                   approval_json, tx_bytes_hex, simulation_json, verdict_json, created_at)| {
                PendingApprovalRow {
                    request_id, session_id, transaction_id, description, wallet_pubkey,
                    approval_json, tx_bytes_hex, simulation_json, verdict_json, created_at,
                }
            })
            .collect())
    }

    // ── Pending signatures ───────────────────────────────────────────────

    /// Insert a pending signature row in 'pending' state.
    #[instrument(skip(self, finalized_tx_bytes))]
    pub async fn insert_pending_signature(
        &self,
        request_id:         &str,
        session_id:         &str,
        transaction_id:     &str,
        expected_signer:    &str,
        description:        &str,
        finalized_tx_bytes: &[u8],
        adapter_type:       &str,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR REPLACE INTO pending_signatures
                 (request_id, session_id, transaction_id, expected_signer, description,
                  finalized_tx_bytes, adapter_type, created_at, state)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(request_id)
        .bind(session_id)
        .bind(transaction_id)
        .bind(expected_signer)
        .bind(description)
        .bind(finalized_tx_bytes)
        .bind(adapter_type)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a pending signature as terminal.
    pub async fn mark_signature_terminal(
        &self,
        request_id: &str,
        terminal_state: &str,
        reason: Option<&str>,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE pending_signatures SET state = ?, terminal_reason = ? WHERE request_id = ? AND state = 'pending'"
        )
        .bind(terminal_state)
        .bind(reason)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove a pending signature row entirely (for cleanup).
    pub async fn remove_pending_signature(&self, request_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM pending_signatures WHERE request_id = ?")
            .bind(request_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Load only 'pending' signature rows (for startup recovery).
    pub async fn load_pending_signatures(&self) -> Result<Vec<PendingSignatureRow>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, String, String, String, Vec<u8>, String, i64)>(
            "SELECT request_id, session_id, transaction_id, expected_signer, description,
                    finalized_tx_bytes, adapter_type, created_at
             FROM pending_signatures
             WHERE state = 'pending'
             ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(request_id, session_id, transaction_id, expected_signer,
                   description, finalized_tx_bytes, adapter_type, created_at)| {
                PendingSignatureRow {
                    request_id, session_id, transaction_id, expected_signer,
                    description, finalized_tx_bytes, adapter_type, created_at,
                }
            })
            .collect())
    }

    // ── Atomic signature + completion meta ──────────────────────────────

    /// Atomically insert both a pending signature row and its completion
    /// metadata in a single SQLite transaction. If either INSERT fails,
    /// both are rolled back. This prevents half-persisted durable state
    /// that could be recovered as a valid pending entry on restart.
    #[instrument(skip(self, finalized_tx_bytes))]
    pub async fn insert_signature_and_meta(
        &self,
        request_id:         &str,
        session_id:         &str,
        transaction_id:     &str,
        expected_signer:    &str,
        description:        &str,
        finalized_tx_bytes: &[u8],
        adapter_type:       &str,
        fee_lamports:       Option<u64>,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let mut tx = self.pool.begin().await.map_err(StoreError::Sqlx)?;

        sqlx::query(
            "INSERT OR REPLACE INTO pending_signatures
                 (request_id, session_id, transaction_id, expected_signer, description,
                  finalized_tx_bytes, adapter_type, created_at, state)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(request_id)
        .bind(session_id)
        .bind(transaction_id)
        .bind(expected_signer)
        .bind(description)
        .bind(finalized_tx_bytes)
        .bind(adapter_type)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT OR REPLACE INTO pending_completion_meta
                 (request_id, fee_lamports, created_at, state)
             VALUES (?, ?, ?, 'pending')",
        )
        .bind(request_id)
        .bind(fee_lamports.map(|f| f as i64))
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await.map_err(StoreError::Sqlx)?;
        Ok(())
    }

    // ── Pending completion metadata ──────────────────────────────────────

    /// Insert completion metadata in 'pending' state.
    pub async fn insert_completion_meta(
        &self,
        request_id:   &str,
        fee_lamports: Option<u64>,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR REPLACE INTO pending_completion_meta
                 (request_id, fee_lamports, created_at, state)
             VALUES (?, ?, ?, 'pending')",
        )
        .bind(request_id)
        .bind(fee_lamports.map(|f| f as i64))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark completion metadata as terminal.
    pub async fn mark_completion_meta_terminal(
        &self,
        request_id: &str,
        terminal_state: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE pending_completion_meta SET state = ? WHERE request_id = ? AND state = 'pending'"
        )
        .bind(terminal_state)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove completion metadata row entirely (for cleanup).
    pub async fn remove_completion_meta(&self, request_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM pending_completion_meta WHERE request_id = ?")
            .bind(request_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Load only 'pending' completion metadata (for startup recovery).
    pub async fn load_completion_meta(&self) -> Result<Vec<PendingCompletionMetaRow>, StoreError> {
        let rows = sqlx::query_as::<_, (String, Option<i64>, i64)>(
            "SELECT request_id, fee_lamports, created_at
             FROM pending_completion_meta
             WHERE state = 'pending'
             ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(request_id, fee_lamports, created_at)| {
                PendingCompletionMetaRow { request_id, fee_lamports, created_at }
            })
            .collect())
    }

    // ── Bulk cleanup ─────────────────────────────────────────────────────

    /// Mark all pending state older than the given age as 'expired'.
    /// Used during startup recovery to expire stale records.
    pub async fn expire_older_than(&self, max_age_ms: i64) -> Result<ExpiredCounts, StoreError> {
        let cutoff = Utc::now().timestamp_millis() - max_age_ms;

        let approvals = sqlx::query(
            "UPDATE pending_approvals SET state = 'expired', terminal_reason = 'TTL exceeded on startup'
             WHERE state = 'pending' AND created_at <= ?"
        )
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();

        let signatures = sqlx::query(
            "UPDATE pending_signatures SET state = 'expired', terminal_reason = 'TTL exceeded on startup'
             WHERE state = 'pending' AND created_at <= ?"
        )
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();

        let meta = sqlx::query(
            "UPDATE pending_completion_meta SET state = 'expired'
             WHERE state = 'pending' AND created_at <= ?"
        )
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();

        Ok(ExpiredCounts { approvals, signatures, meta })
    }

    /// Delete terminal rows older than the given retention period.
    ///
    /// Only removes rows where `state != 'pending'` AND `created_at` is
    /// older than the retention cutoff. This preserves recent terminal
    /// rows for post-mortem / audit correlation.
    pub async fn purge_terminal(&self, retention_ms: i64) -> Result<u64, StoreError> {
        let cutoff = Utc::now().timestamp_millis() - retention_ms;
        let mut total = 0u64;
        total += sqlx::query(
            "DELETE FROM pending_approvals WHERE state != 'pending' AND created_at <= ?"
        ).bind(cutoff).execute(&self.pool).await?.rows_affected();
        total += sqlx::query(
            "DELETE FROM pending_signatures WHERE state != 'pending' AND created_at <= ?"
        ).bind(cutoff).execute(&self.pool).await?.rows_affected();
        total += sqlx::query(
            "DELETE FROM pending_completion_meta WHERE state != 'pending' AND created_at <= ?"
        ).bind(cutoff).execute(&self.pool).await?.rows_affected();
        Ok(total)
    }
}

/// Counts of expired records during startup cleanup.
#[derive(Debug, Clone)]
pub struct ExpiredCounts {
    /// Number of expired approval rows.
    pub approvals:  u64,
    /// Number of expired signature rows.
    pub signatures: u64,
    /// Number of expired completion metadata rows.
    pub meta:       u64,
}

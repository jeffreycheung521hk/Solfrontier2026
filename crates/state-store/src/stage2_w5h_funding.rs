//! W5h — durable storage for chat-budget funding intents.
//!
//! A W5h intent is the operator-approved chat-route conditional-deposit
//! order that requires user funding before it is eligible to execute.
//! Lifecycle:
//!
//! ```text
//!  funding_required ──► funding_submitted ──► budget_reserved ──► executing ──► completed
//!                            │                      │                  │
//!                            ▼                      ▼                  └── failed
//!                       funding_invalid        expired ──► refunding ──► refunded
//! ```
//!
//! Execution and refund are competing terminal paths over the same
//! 0.25 USDC budget — the CAS transitions in this module guarantee
//! at-most-one path wins:
//!
//! - [`Stage2W5hFundingIntentRepository::lease_execution_if_budget_reserved`]
//!   moves `budget_reserved → executing` only if the intent has not
//!   yet expired. The W5g executor MUST call this before signing.
//! - [`Stage2W5hFundingIntentRepository::lease_refund_if_expired_or_past`]
//!   moves `budget_reserved | expired → refunding` only if `now_ms >=
//!   expires_at_ms`. The W5h refund route MUST call this before
//!   signing.
//!
//! Schema: `migrations/0012_stage2_w5h_funding_intents.sql`.

use chrono::Utc;
use sqlx::SqlitePool;

use crate::errors::StoreError;

/// Canonical string values for `stage2_w5h_funding_intents.status`.
pub mod status {
    /// Intent persisted; user has NOT yet paid the funding tx.
    pub const FUNDING_REQUIRED: &str = "funding_required";
    /// User submitted a funding signature; daemon is verifying.
    pub const FUNDING_SUBMITTED: &str = "funding_submitted";
    /// Funding signature was finalized but on-chain delta failed
    /// the +250 000-raw-USDC check. Terminal; the user must mint a
    /// new intent.
    pub const FUNDING_INVALID: &str = "funding_invalid";
    /// Funding verified; budget is held in the controlled wallet and
    /// the intent is eligible for execution OR (after expiry) refund.
    pub const BUDGET_RESERVED: &str = "budget_reserved";
    /// W5g execution lease acquired; daemon is building/sending the
    /// Solend deposit tx. Refund cannot lease during this window.
    pub const EXECUTING: &str = "executing";
    /// W5g deposit finalized. Terminal.
    pub const COMPLETED: &str = "completed";
    /// `expires_at_ms` reached without execution. Refund is now
    /// eligible.
    pub const EXPIRED: &str = "expired";
    /// W5h refund lease acquired; daemon is building/sending the
    /// USDC TransferChecked back to the user.
    pub const REFUNDING: &str = "refunding";
    /// Refund tx finalized. Terminal.
    pub const REFUNDED: &str = "refunded";
    /// Unrecoverable error mid-execute / mid-refund. Terminal.
    pub const FAILED: &str = "failed";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum W5hIntentStatus {
    FundingRequired,
    FundingSubmitted,
    FundingInvalid,
    BudgetReserved,
    Executing,
    Completed,
    Expired,
    Refunding,
    Refunded,
    Failed,
}

impl W5hIntentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FundingRequired => status::FUNDING_REQUIRED,
            Self::FundingSubmitted => status::FUNDING_SUBMITTED,
            Self::FundingInvalid => status::FUNDING_INVALID,
            Self::BudgetReserved => status::BUDGET_RESERVED,
            Self::Executing => status::EXECUTING,
            Self::Completed => status::COMPLETED,
            Self::Expired => status::EXPIRED,
            Self::Refunding => status::REFUNDING,
            Self::Refunded => status::REFUNDED,
            Self::Failed => status::FAILED,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            status::FUNDING_REQUIRED => Self::FundingRequired,
            status::FUNDING_SUBMITTED => Self::FundingSubmitted,
            status::FUNDING_INVALID => Self::FundingInvalid,
            status::BUDGET_RESERVED => Self::BudgetReserved,
            status::EXECUTING => Self::Executing,
            status::COMPLETED => Self::Completed,
            status::EXPIRED => Self::Expired,
            status::REFUNDING => Self::Refunding,
            status::REFUNDED => Self::Refunded,
            status::FAILED => Self::Failed,
            _ => return None,
        })
    }

    /// Terminal statuses: the intent will not change again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::FundingInvalid | Self::Completed | Self::Refunded | Self::Failed
        )
    }
}

// ── Stored row ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct W5hFundingIntent {
    pub intent_id: String,
    pub rule_id_hex: String,
    pub canonical_rule_hash_hex: String,

    pub user_wallet: String,
    pub user_usdc_ata: String,
    pub controlled_wallet: String,
    pub controlled_usdc_ata: String,

    pub amount_raw: u64,
    pub threshold_bps: u32,
    pub save_display_apy_bps_at_creation: u32,
    pub native_onchain_apr_bps_at_creation: u32,

    pub created_at_ms: i64,
    pub expires_at_ms: i64,

    pub status: W5hIntentStatus,

    pub funding_signature: Option<String>,
    pub funding_finalized_slot: Option<u64>,
    pub execution_signature: Option<String>,
    pub refund_signature: Option<String>,

    pub last_error: Option<String>,

    pub updated_at_ms: i64,
}

/// Inputs the bridge uses to insert a fresh `funding_required` intent.
#[derive(Debug, Clone)]
pub struct NewW5hFundingIntent {
    pub intent_id: String,
    pub rule_id_hex: String,
    pub canonical_rule_hash_hex: String,
    pub user_wallet: String,
    pub user_usdc_ata: String,
    pub controlled_wallet: String,
    pub controlled_usdc_ata: String,
    pub amount_raw: u64,
    pub threshold_bps: u32,
    pub save_display_apy_bps_at_creation: u32,
    pub native_onchain_apr_bps_at_creation: u32,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

// ── Repository ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Stage2W5hFundingIntentRepository {
    pool: SqlitePool,
}

impl Stage2W5hFundingIntentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Insert a fresh intent at `funding_required`. Returns
    /// `StoreError::AlreadyExists` (mapped from a UNIQUE-violation)
    /// when a row with the same `intent_id` is already present —
    /// callers treat that as "the W5e/W5f rule was re-typed; the
    /// W5h funding intent already exists; surface its current state".
    pub async fn insert(
        &self,
        intent: &NewW5hFundingIntent,
    ) -> Result<(), StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "INSERT INTO stage2_w5h_funding_intents
                 (intent_id, rule_id_hex, canonical_rule_hash_hex,
                  user_wallet, user_usdc_ata, controlled_wallet, controlled_usdc_ata,
                  amount_raw, threshold_bps,
                  save_display_apy_bps_at_creation, native_onchain_apr_bps_at_creation,
                  created_at_ms, expires_at_ms,
                  status,
                  funding_signature, funding_finalized_slot,
                  execution_signature, refund_signature, last_error,
                  updated_at_ms)
             VALUES (?, ?, ?,
                     ?, ?, ?, ?,
                     ?, ?,
                     ?, ?,
                     ?, ?,
                     ?,
                     NULL, NULL,
                     NULL, NULL, NULL,
                     ?)",
        )
        .bind(&intent.intent_id)
        .bind(&intent.rule_id_hex)
        .bind(&intent.canonical_rule_hash_hex)
        .bind(&intent.user_wallet)
        .bind(&intent.user_usdc_ata)
        .bind(&intent.controlled_wallet)
        .bind(&intent.controlled_usdc_ata)
        .bind(intent.amount_raw as i64)
        .bind(intent.threshold_bps as i64)
        .bind(intent.save_display_apy_bps_at_creation as i64)
        .bind(intent.native_onchain_apr_bps_at_creation as i64)
        .bind(intent.created_at_ms)
        .bind(intent.expires_at_ms)
        .bind(status::FUNDING_REQUIRED)
        .bind(now_ms)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                // sqlx::Error::Database wraps the underlying sqlite
                // constraint failure. We surface the generic StoreError
                // here; the caller's "already-exists" detection is
                // structural (subsequent `get` returns Some).
                Err(StoreError::Sqlx(e))
            }
        }
    }

    /// Look up an intent by `intent_id`. Returns `Ok(None)` when no
    /// row matches.
    pub async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<W5hFundingIntent>, StoreError> {
        let row = sqlx::query_as::<_, RowRaw>(SELECT_ALL_SQL)
            .bind(intent_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_intent).transpose()
    }

    /// Sweep helper: list intents whose `expires_at_ms <= now_ms` AND
    /// whose status is in {budget_reserved, watching-equivalent}.
    /// Bounded by `limit`.
    pub async fn list_eligible_for_refund(
        &self,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<W5hFundingIntent>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, RowRaw>(&format!(
            "{} WHERE expires_at_ms <= ?
                AND status IN (?, ?)
              ORDER BY created_at_ms ASC
              LIMIT ?",
            SELECT_ALL_SQL_PREFIX
        ))
        .bind(now_ms)
        .bind(status::BUDGET_RESERVED)
        .bind(status::EXPIRED)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_intent).collect()
    }

    // ── State transitions (all CAS-guarded by status predicates) ────────

    /// `funding_required → funding_submitted`. The user POSTed a
    /// signature; the daemon will verify it on-chain. Idempotent:
    /// re-submitting the same signature on an already-submitted
    /// intent is a no-op (still returns 1 row affected). Re-submitting
    /// a DIFFERENT signature on an already-submitted intent (which
    /// would imply the user paid twice) is rejected with 0 rows.
    pub async fn mark_funding_submitted_if_required(
        &self,
        intent_id: &str,
        signature: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, funding_signature = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND (status = ?
                    OR (status = ? AND (funding_signature IS NULL OR funding_signature = ?)))",
        )
        .bind(status::FUNDING_SUBMITTED)
        .bind(signature)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::FUNDING_REQUIRED)
        .bind(status::FUNDING_SUBMITTED)
        .bind(signature)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `funding_submitted → budget_reserved`. Only fires if the
    /// submitted signature matches the one we expect (the one the
    /// verifier just confirmed on-chain). 0 rows = signature mismatch
    /// or wrong status.
    pub async fn mark_budget_reserved_if_submitted(
        &self,
        intent_id: &str,
        expected_signature: &str,
        finalized_slot: u64,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, funding_finalized_slot = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status = ?
               AND funding_signature = ?",
        )
        .bind(status::BUDGET_RESERVED)
        .bind(finalized_slot as i64)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::FUNDING_SUBMITTED)
        .bind(expected_signature)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `funding_submitted → funding_invalid`. Terminal — the user
    /// must mint a new intent (different rule_id) to retry.
    pub async fn mark_funding_invalid_if_submitted(
        &self,
        intent_id: &str,
        expected_signature: &str,
        reason: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, last_error = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status = ?
               AND funding_signature = ?",
        )
        .bind(status::FUNDING_INVALID)
        .bind(reason)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::FUNDING_SUBMITTED)
        .bind(expected_signature)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `budget_reserved → executing` (CAS, time-bounded). The W5g
    /// executor MUST call this BEFORE building / signing / sending
    /// the Solend deposit tx. A 0-row return means the lease failed
    /// (intent is already executing, refunding, terminal, expired,
    /// or in a wrong status); callers MUST NOT proceed to send.
    pub async fn lease_execution_if_budget_reserved(
        &self,
        intent_id: &str,
        now_ms: i64,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status = ?
               AND expires_at_ms > ?",
        )
        .bind(status::EXECUTING)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::BUDGET_RESERVED)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `executing → completed`. Only fires from the `executing`
    /// status — defends against a misordered call from W5g.
    pub async fn mark_completed_if_executing(
        &self,
        intent_id: &str,
        execution_signature: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, execution_signature = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status = ?",
        )
        .bind(status::COMPLETED)
        .bind(execution_signature)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::EXECUTING)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `executing → budget_reserved` if the W5g tx failed before
    /// reaching `completed`. Lets the budget either re-attempt (if
    /// not expired) or be refunded (after expiry). Idempotent.
    pub async fn release_execution_lease_to_budget_reserved(
        &self,
        intent_id: &str,
        reason: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, last_error = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status = ?",
        )
        .bind(status::BUDGET_RESERVED)
        .bind(reason)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::EXECUTING)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `budget_reserved → expired` if `now_ms >= expires_at_ms`.
    /// Idempotent on already-expired rows. Does NOT touch rows in
    /// any other status (refund/execute leases protected).
    pub async fn mark_expired_if_past(
        &self,
        intent_id: &str,
        now_ms: i64,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status = ?
               AND expires_at_ms <= ?",
        )
        .bind(status::EXPIRED)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::BUDGET_RESERVED)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `budget_reserved | expired → refunding` if `now_ms >= expires_at_ms`.
    /// The W5h refund route MUST call this BEFORE building / signing.
    /// 0 rows = lease failed (executing, completed, refunded, not yet
    /// expired, etc.).
    pub async fn lease_refund_if_expired_or_past(
        &self,
        intent_id: &str,
        now_ms: i64,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status IN (?, ?)
               AND expires_at_ms <= ?",
        )
        .bind(status::REFUNDING)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::BUDGET_RESERVED)
        .bind(status::EXPIRED)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `refunding → refunded`. Only fires from the `refunding`
    /// status — defends against a misordered call.
    pub async fn mark_refunded_if_refunding(
        &self,
        intent_id: &str,
        refund_signature: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, refund_signature = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status = ?",
        )
        .bind(status::REFUNDED)
        .bind(refund_signature)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::REFUNDING)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Terminal-failure record. Stamps `last_error` from any
    /// non-terminal status.
    pub async fn mark_failed(
        &self,
        intent_id: &str,
        reason: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_w5h_funding_intents
             SET status = ?, last_error = ?, updated_at_ms = ?
             WHERE intent_id = ?
               AND status NOT IN (?, ?, ?, ?)",
        )
        .bind(status::FAILED)
        .bind(reason)
        .bind(now_ms)
        .bind(intent_id)
        .bind(status::COMPLETED)
        .bind(status::REFUNDED)
        .bind(status::FAILED)
        .bind(status::FUNDING_INVALID)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

// ── Row mapping ─────────────────────────────────────────────────────────────

const SELECT_ALL_SQL_PREFIX: &str = "SELECT \
    intent_id, rule_id_hex, canonical_rule_hash_hex, \
    user_wallet, user_usdc_ata, controlled_wallet, controlled_usdc_ata, \
    amount_raw, threshold_bps, \
    save_display_apy_bps_at_creation, native_onchain_apr_bps_at_creation, \
    created_at_ms, expires_at_ms, \
    status, \
    funding_signature, funding_finalized_slot, \
    execution_signature, refund_signature, last_error, \
    updated_at_ms \
FROM stage2_w5h_funding_intents";

const SELECT_ALL_SQL: &str = "SELECT \
    intent_id, rule_id_hex, canonical_rule_hash_hex, \
    user_wallet, user_usdc_ata, controlled_wallet, controlled_usdc_ata, \
    amount_raw, threshold_bps, \
    save_display_apy_bps_at_creation, native_onchain_apr_bps_at_creation, \
    created_at_ms, expires_at_ms, \
    status, \
    funding_signature, funding_finalized_slot, \
    execution_signature, refund_signature, last_error, \
    updated_at_ms \
FROM stage2_w5h_funding_intents \
WHERE intent_id = ?";

#[derive(sqlx::FromRow)]
struct RowRaw {
    intent_id: String,
    rule_id_hex: String,
    canonical_rule_hash_hex: String,
    user_wallet: String,
    user_usdc_ata: String,
    controlled_wallet: String,
    controlled_usdc_ata: String,
    amount_raw: i64,
    threshold_bps: i64,
    save_display_apy_bps_at_creation: i64,
    native_onchain_apr_bps_at_creation: i64,
    created_at_ms: i64,
    expires_at_ms: i64,
    status: String,
    funding_signature: Option<String>,
    funding_finalized_slot: Option<i64>,
    execution_signature: Option<String>,
    refund_signature: Option<String>,
    last_error: Option<String>,
    updated_at_ms: i64,
}

fn row_to_intent(r: RowRaw) -> Result<W5hFundingIntent, StoreError> {
    let status = W5hIntentStatus::parse(&r.status).ok_or_else(|| {
        StoreError::IntegrityCheckFailed(format!(
            "stage2_w5h_funding_intents: unknown status {:?} for intent_id {}",
            r.status, r.intent_id
        ))
    })?;
    let amount_raw: u64 = r.amount_raw.try_into().map_err(|_| {
        StoreError::IntegrityCheckFailed(format!(
            "stage2_w5h_funding_intents: amount_raw out of u64 range: {}",
            r.amount_raw
        ))
    })?;
    Ok(W5hFundingIntent {
        intent_id: r.intent_id,
        rule_id_hex: r.rule_id_hex,
        canonical_rule_hash_hex: r.canonical_rule_hash_hex,
        user_wallet: r.user_wallet,
        user_usdc_ata: r.user_usdc_ata,
        controlled_wallet: r.controlled_wallet,
        controlled_usdc_ata: r.controlled_usdc_ata,
        amount_raw,
        threshold_bps: r.threshold_bps as u32,
        save_display_apy_bps_at_creation: r.save_display_apy_bps_at_creation as u32,
        native_onchain_apr_bps_at_creation: r.native_onchain_apr_bps_at_creation as u32,
        created_at_ms: r.created_at_ms,
        expires_at_ms: r.expires_at_ms,
        status,
        funding_signature: r.funding_signature,
        funding_finalized_slot: r.funding_finalized_slot.map(|s| s as u64),
        execution_signature: r.execution_signature,
        refund_signature: r.refund_signature,
        last_error: r.last_error,
        updated_at_ms: r.updated_at_ms,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    async fn test_repo() -> (Database, Stage2W5hFundingIntentRepository) {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        let repo = Stage2W5hFundingIntentRepository::new(db.pool().clone());
        (db, repo)
    }

    fn fixture(intent_id: &str, now_ms: i64) -> NewW5hFundingIntent {
        NewW5hFundingIntent {
            intent_id: intent_id.to_string(),
            rule_id_hex: intent_id.to_string(),
            canonical_rule_hash_hex:
                "0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            user_wallet: "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string(),
            user_usdc_ata: "TestUserUsdcAta1111111111111111111111111111".to_string(),
            controlled_wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L"
                .to_string(),
            controlled_usdc_ata: "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3"
                .to_string(),
            amount_raw: 250_000,
            threshold_bps: 100,
            save_display_apy_bps_at_creation: 210,
            native_onchain_apr_bps_at_creation: 165,
            created_at_ms: now_ms,
            expires_at_ms: now_ms + 180_000,
        }
    }

    #[tokio::test]
    async fn w5h_insert_and_get_round_trips() {
        let (_db, repo) = test_repo().await;
        let intent = fixture("a".repeat(32).as_str(), 1_000_000);
        repo.insert(&intent).await.unwrap();
        let got = repo.get(&intent.intent_id).await.unwrap().unwrap();
        assert_eq!(got.status, W5hIntentStatus::FundingRequired);
        assert_eq!(got.amount_raw, 250_000);
        assert_eq!(got.threshold_bps, 100);
        assert_eq!(got.save_display_apy_bps_at_creation, 210);
        assert!(got.funding_signature.is_none());
        assert!(got.last_error.is_none());
    }

    #[tokio::test]
    async fn w5h_duplicate_insert_returns_error() {
        let (_db, repo) = test_repo().await;
        let intent = fixture(&"a".repeat(32), 1_000_000);
        repo.insert(&intent).await.unwrap();
        let err = repo.insert(&intent).await.unwrap_err();
        // UNIQUE PK violation surfaces as StoreError::Db (the caller's
        // structural check uses `get` to confirm presence).
        assert!(format!("{err}").contains("UNIQUE") || format!("{err}").contains("constraint"));
    }

    #[tokio::test]
    async fn w5h_required_to_submitted_transition() {
        let (_db, repo) = test_repo().await;
        let intent = fixture(&"a".repeat(32), 1_000_000);
        repo.insert(&intent).await.unwrap();
        let sig = "Sig1aaaa".to_string();
        let n = repo
            .mark_funding_submitted_if_required(&intent.intent_id, &sig)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let got = repo.get(&intent.intent_id).await.unwrap().unwrap();
        assert_eq!(got.status, W5hIntentStatus::FundingSubmitted);
        assert_eq!(got.funding_signature, Some(sig));
    }

    #[tokio::test]
    async fn w5h_submit_is_idempotent_for_same_signature() {
        let (_db, repo) = test_repo().await;
        let intent = fixture(&"a".repeat(32), 1_000_000);
        repo.insert(&intent).await.unwrap();
        let sig = "Sig1aaaa";
        assert_eq!(
            repo.mark_funding_submitted_if_required(&intent.intent_id, sig)
                .await
                .unwrap(),
            1
        );
        // Same signature again — still allowed (idempotent re-POST).
        assert_eq!(
            repo.mark_funding_submitted_if_required(&intent.intent_id, sig)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn w5h_submit_rejects_different_signature_after_first() {
        let (_db, repo) = test_repo().await;
        let intent = fixture(&"a".repeat(32), 1_000_000);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        // A SECOND submission with a different signature → 0 rows
        // (this would imply the user paid twice; we ignore the
        // second tx and stay bound to Sig1).
        let n = repo
            .mark_funding_submitted_if_required(&intent.intent_id, "Sig2")
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn w5h_budget_reserved_requires_matching_signature() {
        let (_db, repo) = test_repo().await;
        let intent = fixture(&"a".repeat(32), 1_000_000);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        // Wrong signature → 0 rows.
        assert_eq!(
            repo.mark_budget_reserved_if_submitted(&intent.intent_id, "WRONG", 42)
                .await
                .unwrap(),
            0
        );
        // Correct signature → 1 row, status flips.
        assert_eq!(
            repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 42)
                .await
                .unwrap(),
            1
        );
        let got = repo.get(&intent.intent_id).await.unwrap().unwrap();
        assert_eq!(got.status, W5hIntentStatus::BudgetReserved);
        assert_eq!(got.funding_finalized_slot, Some(42));
    }

    #[tokio::test]
    async fn w5h_execution_lease_only_from_budget_reserved_and_not_expired() {
        let (_db, repo) = test_repo().await;
        let now = 1_000_000_i64;
        let intent = fixture(&"a".repeat(32), now);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
            .await
            .unwrap();
        // Lease at a not-yet-expired time → success.
        assert_eq!(
            repo.lease_execution_if_budget_reserved(&intent.intent_id, now + 1000)
                .await
                .unwrap(),
            1
        );
        let got = repo.get(&intent.intent_id).await.unwrap().unwrap();
        assert_eq!(got.status, W5hIntentStatus::Executing);

        // Second lease attempt — already executing → 0 rows.
        assert_eq!(
            repo.lease_execution_if_budget_reserved(&intent.intent_id, now + 2000)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn w5h_execution_lease_rejects_expired() {
        let (_db, repo) = test_repo().await;
        let now = 1_000_000_i64;
        let intent = fixture(&"a".repeat(32), now);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
            .await
            .unwrap();
        // Lease at expires_at_ms (or later) → 0 rows (refund window).
        let expires = intent.expires_at_ms;
        assert_eq!(
            repo.lease_execution_if_budget_reserved(&intent.intent_id, expires)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            repo.lease_execution_if_budget_reserved(&intent.intent_id, expires + 1)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn w5h_completed_transition_blocks_refund() {
        let (_db, repo) = test_repo().await;
        let now = 1_000_000_i64;
        let intent = fixture(&"a".repeat(32), now);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
            .await
            .unwrap();
        repo.lease_execution_if_budget_reserved(&intent.intent_id, now + 1000)
            .await
            .unwrap();
        repo.mark_completed_if_executing(&intent.intent_id, "ExecSig1")
            .await
            .unwrap();
        // Refund lease must NOT fire on a completed intent, even after
        // expires_at_ms.
        let expires_past = intent.expires_at_ms + 10_000;
        assert_eq!(
            repo.lease_refund_if_expired_or_past(&intent.intent_id, expires_past)
                .await
                .unwrap(),
            0
        );
        let got = repo.get(&intent.intent_id).await.unwrap().unwrap();
        assert_eq!(got.status, W5hIntentStatus::Completed);
        assert_eq!(got.execution_signature.as_deref(), Some("ExecSig1"));
    }

    #[tokio::test]
    async fn w5h_refund_lease_blocks_execution_after_winning() {
        let (_db, repo) = test_repo().await;
        let now = 1_000_000_i64;
        let intent = fixture(&"a".repeat(32), now);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
            .await
            .unwrap();
        // Past expiry — refund lease wins.
        let after_expiry = intent.expires_at_ms + 100;
        assert_eq!(
            repo.lease_refund_if_expired_or_past(&intent.intent_id, after_expiry)
                .await
                .unwrap(),
            1
        );
        // Execution lease now rejected (status is `refunding`, not
        // `budget_reserved`).
        assert_eq!(
            repo.lease_execution_if_budget_reserved(&intent.intent_id, after_expiry)
                .await
                .unwrap(),
            0
        );
        // Mark refunded.
        assert_eq!(
            repo.mark_refunded_if_refunding(&intent.intent_id, "RefundSig1")
                .await
                .unwrap(),
            1
        );
        let got = repo.get(&intent.intent_id).await.unwrap().unwrap();
        assert_eq!(got.status, W5hIntentStatus::Refunded);
        assert_eq!(got.refund_signature.as_deref(), Some("RefundSig1"));
    }

    #[tokio::test]
    async fn w5h_execution_lease_blocks_refund_after_winning() {
        // Symmetric: execution wins first → refund must not fire even
        // if expiry passes mid-execution.
        let (_db, repo) = test_repo().await;
        let now = 1_000_000_i64;
        let intent = fixture(&"a".repeat(32), now);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
            .await
            .unwrap();
        // Execute at not-yet-expired moment.
        assert_eq!(
            repo.lease_execution_if_budget_reserved(&intent.intent_id, now + 1000)
                .await
                .unwrap(),
            1
        );
        // Expiry passes mid-execution — refund still cannot lease
        // (status is `executing`, not `budget_reserved | expired`).
        assert_eq!(
            repo.lease_refund_if_expired_or_past(
                &intent.intent_id,
                intent.expires_at_ms + 5000
            )
            .await
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn w5h_mark_expired_if_past_idempotent() {
        let (_db, repo) = test_repo().await;
        let now = 1_000_000_i64;
        let intent = fixture(&"a".repeat(32), now);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
            .await
            .unwrap();
        // Not yet expired — no transition.
        assert_eq!(
            repo.mark_expired_if_past(&intent.intent_id, now + 100)
                .await
                .unwrap(),
            0
        );
        // Past expiry — transitions to expired.
        assert_eq!(
            repo.mark_expired_if_past(&intent.intent_id, intent.expires_at_ms + 1)
                .await
                .unwrap(),
            1
        );
        // Already expired — second call is no-op (status is expired,
        // not budget_reserved).
        assert_eq!(
            repo.mark_expired_if_past(&intent.intent_id, intent.expires_at_ms + 2)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn w5h_funding_invalid_terminal() {
        let (_db, repo) = test_repo().await;
        let now = 1_000_000_i64;
        let intent = fixture(&"a".repeat(32), now);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        // Verifier discovers the on-chain delta doesn't match.
        assert_eq!(
            repo.mark_funding_invalid_if_submitted(
                &intent.intent_id,
                "Sig1",
                "controlled USDC ATA delta = +1 raw, expected +250000"
            )
            .await
            .unwrap(),
            1
        );
        let got = repo.get(&intent.intent_id).await.unwrap().unwrap();
        assert_eq!(got.status, W5hIntentStatus::FundingInvalid);
        assert!(got.status.is_terminal());
        // Subsequent transitions blocked.
        assert_eq!(
            repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
                .await
                .unwrap(),
            0
        );
    }
}

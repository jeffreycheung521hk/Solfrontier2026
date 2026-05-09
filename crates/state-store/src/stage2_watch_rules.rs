//! Durable persistence for Stage 2 watch rules (delegated conditional
//! execution).
//!
//! Substrate only. The watcher / scheduler / executor are NOT here —
//! this crate just owns the off-chain row that the daemon will later
//! tick against, plus the canonical bytes + hash so the on-chain
//! comparator and the daemon agree on what the user signed.
//!
//! # What we store
//!
//! - `WatchRule` itself (as canonical JSON for operator/debug visibility),
//! - `canonical_rule_bytes` (hex) and `canonical_rule_hash` (hex) so the
//!   stored row is bit-identical to what an Authorization PDA would bind,
//! - lifecycle status + bookkeeping (last-checked slot, exec nonce,
//!   completed / revoked flags, last error).
//!
//! Schema lives in `migrations/0011_stage2_watch_rules.sql`. Status
//! constants live in [`status`]; the typed projection is [`WatchRuleStatus`].

use chrono::Utc;
use sqlx::SqlitePool;

use claw_types::stage2_watch_rule::{
    canonical_rule_bytes, canonical_rule_hash, ConditionLogic, WatchRule,
    WatchRuleActionType,
};

use crate::errors::StoreError;

// ── Status constants + typed projection ─────────────────────────────────────

/// Canonical string values for `stage2_watch_rules.status`.
pub mod status {
    /// Rule was just inserted; conditions have not yet evaluated true.
    pub const ACTIVE: &str = "active";
    /// At least one tick saw the rule's condition logic evaluate true;
    /// the executor has not yet started building the action tx.
    pub const CONDITION_MET: &str = "condition_met";
    /// Executor has begun building / submitting the action tx.
    pub const EXECUTING: &str = "executing";
    /// Action tx confirmed; rule retired.
    pub const COMPLETED: &str = "completed";
    /// `expires_at_slot` reached without execution.
    pub const EXPIRED: &str = "expired";
    /// User revoked the rule (off-chain or via on-chain revoke ix).
    pub const REVOKED: &str = "revoked";
    /// Execution attempted but failed; see `last_error`.
    pub const FAILED: &str = "failed";
}

/// Strongly-typed projection of [`status`]. Unknown strings in the DB
/// surface as [`StoreError::IntegrityCheckFailed`] when loading rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchRuleStatus {
    Active,
    ConditionMet,
    Executing,
    Completed,
    Expired,
    Revoked,
    Failed,
}

impl WatchRuleStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => status::ACTIVE,
            Self::ConditionMet => status::CONDITION_MET,
            Self::Executing => status::EXECUTING,
            Self::Completed => status::COMPLETED,
            Self::Expired => status::EXPIRED,
            Self::Revoked => status::REVOKED,
            Self::Failed => status::FAILED,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            status::ACTIVE => Self::Active,
            status::CONDITION_MET => Self::ConditionMet,
            status::EXECUTING => Self::Executing,
            status::COMPLETED => Self::Completed,
            status::EXPIRED => Self::Expired,
            status::REVOKED => Self::Revoked,
            status::FAILED => Self::Failed,
            _ => return None,
        })
    }
}

// ── Stored row ──────────────────────────────────────────────────────────────

/// A persisted Stage 2 watch rule with its lifecycle metadata.
#[derive(Debug, Clone)]
pub struct StoredWatchRule {
    /// The full canonical rule, deserialized from `rule_json`.
    pub rule: WatchRule,
    /// SHA-256 of the canonical Borsh bytes. Recomputed on load and
    /// compared with the stored value; mismatch surfaces as
    /// [`StoreError::IntegrityCheckFailed`].
    pub canonical_rule_hash: [u8; 32],
    /// Canonical Borsh bytes (decoded from the hex column).
    pub canonical_rule_bytes: Vec<u8>,
    pub status: WatchRuleStatus,
    pub action_type: WatchRuleActionType,
    pub condition_logic: ConditionLogic,
    pub last_checked_slot: Option<u64>,
    pub last_successful_tick_at_ms: Option<i64>,
    pub last_error: Option<String>,
    pub execution_nonce: u64,
    pub completed: bool,
    pub revoked: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

// ── Repository ──────────────────────────────────────────────────────────────

/// Repository for the `stage2_watch_rules` table.
#[derive(Clone, Debug)]
pub struct Stage2WatchRuleRepository {
    pool: SqlitePool,
}

impl Stage2WatchRuleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Read-only access to the underlying pool. Used by integration
    /// tests that need to assert backdoor side-effects (e.g.
    /// rewriting `created_at_ms` to fake a stale row), and by other
    /// crates whose adjacency to this repo predates the public
    /// helper surface.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Insert a new rule row in the [`status::ACTIVE`] state.
    ///
    /// The `rule.created_at_slot` is recorded verbatim; `created_at_ms`
    /// and `updated_at_ms` are taken from the local wall clock.
    pub async fn insert(&self, rule: &WatchRule) -> Result<(), StoreError> {
        let bytes = canonical_rule_bytes(rule);
        let hash = canonical_rule_hash(rule);
        let bytes_hex = hex_encode(&bytes);
        let hash_hex = hex_encode(&hash);
        let rule_json = serde_json::to_string(rule)?;
        let action_type = rule.action.action_type();
        let condition_logic_str = condition_logic_str(rule.condition_logic);
        let now_ms = Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO stage2_watch_rules
                 (rule_id, user_pubkey, executor_pubkey, delegated_wallet_pubkey,
                  canonical_rule_hash, canonical_rule_bytes_hex, rule_json,
                  action_type, condition_logic, status,
                  max_input_amount_raw, used_amount_raw, destination_pubkey,
                  expires_at_slot, created_at_slot,
                  last_checked_slot, last_successful_tick_at_ms, last_error,
                  execution_nonce, completed, revoked,
                  created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                     ?, ?, ?, ?, ?,
                     NULL, NULL, NULL,
                     0, 0, 0,
                     ?, ?)",
        )
        .bind(rule_id_hex(&rule.rule_id))
        .bind(rule.user.to_base58())
        .bind(rule.executor.to_base58())
        .bind(rule.delegated_wallet.to_base58())
        .bind(hash_hex)
        .bind(bytes_hex)
        .bind(rule_json)
        .bind(action_type.label())
        .bind(condition_logic_str)
        .bind(status::ACTIVE)
        .bind(rule.max_input_amount_raw as i64)
        .bind(rule.used_amount_raw as i64)
        .bind(rule.destination.to_base58())
        .bind(rule.expires_at_slot as i64)
        .bind(rule.created_at_slot as i64)
        .bind(now_ms)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load a single rule by its `rule_id` (16-byte canonical id).
    pub async fn get(
        &self,
        rule_id: &[u8; 16],
    ) -> Result<Option<StoredWatchRule>, StoreError> {
        let row = sqlx::query_as::<_, RowRaw>(SELECT_ALL_SQL)
            .bind(rule_id_hex(rule_id))
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_stored).transpose()
    }

    /// List all non-terminal rules whose `expires_at_slot > current_slot`.
    /// Excludes rows whose `completed` or `revoked` flag is set, and
    /// rows whose `status` is one of `completed`/`expired`/`revoked`/`failed`.
    pub async fn list_active(
        &self,
        current_slot: u64,
    ) -> Result<Vec<StoredWatchRule>, StoreError> {
        let rows = sqlx::query_as::<_, RowRaw>(
            "SELECT rule_id, user_pubkey, executor_pubkey, delegated_wallet_pubkey,
                    canonical_rule_hash, canonical_rule_bytes_hex, rule_json,
                    action_type, condition_logic, status,
                    max_input_amount_raw, used_amount_raw, destination_pubkey,
                    expires_at_slot, created_at_slot,
                    last_checked_slot, last_successful_tick_at_ms, last_error,
                    execution_nonce, completed, revoked,
                    created_at_ms, updated_at_ms
             FROM stage2_watch_rules
             WHERE expires_at_slot > ?
               AND completed = 0
               AND revoked = 0
               AND status NOT IN ('completed', 'expired', 'revoked', 'failed')
             ORDER BY created_at_ms ASC",
        )
        .bind(current_slot as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_stored).collect()
    }

    /// Bounded variant of [`Self::list_active`]. Returns at most
    /// `limit` non-terminal rows, oldest-first. Designed for the W2
    /// watcher's batch-tick path so a single tick cannot
    /// unbounded-load the entire active set.
    ///
    /// `limit == 0` returns an empty Vec (cheap caller-side
    /// bounded-tick assertion).
    pub async fn list_active_limit(
        &self,
        current_slot: u64,
        limit: u32,
    ) -> Result<Vec<StoredWatchRule>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, RowRaw>(
            "SELECT rule_id, user_pubkey, executor_pubkey, delegated_wallet_pubkey,
                    canonical_rule_hash, canonical_rule_bytes_hex, rule_json,
                    action_type, condition_logic, status,
                    max_input_amount_raw, used_amount_raw, destination_pubkey,
                    expires_at_slot, created_at_slot,
                    last_checked_slot, last_successful_tick_at_ms, last_error,
                    execution_nonce, completed, revoked,
                    created_at_ms, updated_at_ms
             FROM stage2_watch_rules
             WHERE expires_at_slot > ?
               AND completed = 0
               AND revoked = 0
               AND status NOT IN ('completed', 'expired', 'revoked', 'failed')
             ORDER BY created_at_ms ASC
             LIMIT ?",
        )
        .bind(current_slot as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_stored).collect()
    }

    /// Bounded "lifecycle scan" — returns rules in non-terminal
    /// status (`active`, `condition_met`, `executing`) **regardless
    /// of `expires_at_slot`**, oldest-first, capped by `limit`.
    ///
    /// This is the W2 watcher's tick query: it deliberately
    /// includes rows whose slot-expiry has elapsed, because the
    /// watcher's first job each tick is to flip those rows to
    /// `expired` via [`Self::mark_expired_if_not_terminal`]. If we
    /// used the slot-filtered [`Self::list_active`] /
    /// [`Self::list_active_limit`], expired-by-slot rows would
    /// silently linger in `active` status forever.
    ///
    /// `limit == 0` returns an empty Vec.
    pub async fn list_pending_lifecycle_limit(
        &self,
        limit: u32,
    ) -> Result<Vec<StoredWatchRule>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, RowRaw>(
            "SELECT rule_id, user_pubkey, executor_pubkey, delegated_wallet_pubkey,
                    canonical_rule_hash, canonical_rule_bytes_hex, rule_json,
                    action_type, condition_logic, status,
                    max_input_amount_raw, used_amount_raw, destination_pubkey,
                    expires_at_slot, created_at_slot,
                    last_checked_slot, last_successful_tick_at_ms, last_error,
                    execution_nonce, completed, revoked,
                    created_at_ms, updated_at_ms
             FROM stage2_watch_rules
             WHERE completed = 0
               AND revoked = 0
               AND status NOT IN ('completed', 'expired', 'revoked', 'failed')
             ORDER BY created_at_ms ASC
             LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_stored).collect()
    }

    /// List all rules belonging to a given user pubkey, oldest first.
    /// Includes terminal states (callers who only want non-terminal
    /// should use [`Self::list_active`]).
    pub async fn list_by_user(
        &self,
        user_pubkey: &str,
    ) -> Result<Vec<StoredWatchRule>, StoreError> {
        let rows = sqlx::query_as::<_, RowRaw>(
            "SELECT rule_id, user_pubkey, executor_pubkey, delegated_wallet_pubkey,
                    canonical_rule_hash, canonical_rule_bytes_hex, rule_json,
                    action_type, condition_logic, status,
                    max_input_amount_raw, used_amount_raw, destination_pubkey,
                    expires_at_slot, created_at_slot,
                    last_checked_slot, last_successful_tick_at_ms, last_error,
                    execution_nonce, completed, revoked,
                    created_at_ms, updated_at_ms
             FROM stage2_watch_rules
             WHERE user_pubkey = ?
             ORDER BY created_at_ms ASC",
        )
        .bind(user_pubkey)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_stored).collect()
    }

    /// Record a successful tick attempt: bump `last_checked_slot` and
    /// `last_successful_tick_at_ms`. Does NOT change `status`.
    pub async fn mark_checked(
        &self,
        rule_id: &[u8; 16],
        slot: u64,
        ts_ms: i64,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET last_checked_slot = ?, last_successful_tick_at_ms = ?, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(slot as i64)
        .bind(ts_ms)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Transition `active` → `condition_met`.
    pub async fn mark_condition_met(
        &self,
        rule_id: &[u8; 16],
    ) -> Result<u64, StoreError> {
        self.set_status(rule_id, status::CONDITION_MET).await
    }

    /// TOCTOU-safe variant: only flip `status = active` →
    /// `status = condition_met`. If the row is already in any other
    /// state (e.g. revoked, expired, completed, failed,
    /// condition_met) the UPDATE WHERE-clause matches zero rows and
    /// the call returns `Ok(0)`.
    ///
    /// Watcher contract (W2): caller treats `Ok(0)` as "another
    /// actor advanced the lifecycle between our load and our
    /// transition; do nothing further this tick."
    pub async fn mark_condition_met_if_active(
        &self,
        rule_id: &[u8; 16],
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, updated_at_ms = ?
             WHERE rule_id = ? AND status = ?",
        )
        .bind(status::CONDITION_MET)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .bind(status::ACTIVE)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Transition to `executing` and stamp the on-chain replay-protection
    /// nonce so a crash mid-flight is recoverable.
    pub async fn mark_executing(
        &self,
        rule_id: &[u8; 16],
        nonce: u64,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, execution_nonce = ?, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(status::EXECUTING)
        .bind(nonce as i64)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Terminal-success transition: set `status = completed`, set
    /// `completed = 1`, record the consumed amount, and stamp the
    /// completion slot in `last_checked_slot`.
    pub async fn mark_completed(
        &self,
        rule_id: &[u8; 16],
        used_amount: u64,
        slot: u64,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, completed = 1, used_amount_raw = ?, last_checked_slot = ?, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(status::COMPLETED)
        .bind(used_amount as i64)
        .bind(slot as i64)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Terminal-failure transition: set `status = failed` and record
    /// `error` in `last_error`. The row is NOT marked `completed` —
    /// failure ≠ consumption.
    pub async fn mark_failed(
        &self,
        rule_id: &[u8; 16],
        error: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, last_error = ?, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(status::FAILED)
        .bind(error)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// TOCTOU-safe variant: only flip to `failed` if the row is
    /// currently in a non-terminal status (`active` or
    /// `condition_met`). Already-`revoked`, `completed`, `expired`,
    /// or already-`failed` rows are not overwritten.
    pub async fn mark_failed_if_not_terminal(
        &self,
        rule_id: &[u8; 16],
        error: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, last_error = ?, updated_at_ms = ?
             WHERE rule_id = ?
               AND status IN (?, ?)",
        )
        .bind(status::FAILED)
        .bind(error)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .bind(status::ACTIVE)
        .bind(status::CONDITION_MET)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Record `last_error` on the rule WITHOUT changing its status.
    /// Used by the W2 watcher when an evaluator or simulator returns
    /// a transient error: the rule remains active for retry next
    /// tick, but the error is preserved for health surfaces.
    pub async fn record_last_error(
        &self,
        rule_id: &[u8; 16],
        error: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET last_error = ?, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(error)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Terminal: set `status = expired`. `expires_at_slot` is unchanged.
    pub async fn mark_expired(&self, rule_id: &[u8; 16]) -> Result<u64, StoreError> {
        self.set_status(rule_id, status::EXPIRED).await
    }

    /// TOCTOU-safe variant: only flip to `expired` if the row is
    /// currently in a non-terminal status (`active` or
    /// `condition_met`). Avoids overwriting a row that became
    /// revoked / completed / failed between the watcher's load and
    /// its expiry transition.
    pub async fn mark_expired_if_not_terminal(
        &self,
        rule_id: &[u8; 16],
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, updated_at_ms = ?
             WHERE rule_id = ?
               AND status IN (?, ?)",
        )
        .bind(status::EXPIRED)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .bind(status::ACTIVE)
        .bind(status::CONDITION_MET)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Terminal: set `status = revoked` and `revoked = 1`.
    pub async fn mark_revoked(&self, rule_id: &[u8; 16]) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, revoked = 1, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(status::REVOKED)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Liveness-only update: bump `last_successful_tick_at_ms` so a
    /// watcher restart can tell stale rows from healthy ones, without
    /// claiming a new condition evaluation.
    pub async fn update_health_tick(
        &self,
        rule_id: &[u8; 16],
        ts_ms: i64,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET last_successful_tick_at_ms = ?, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(ts_ms)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn set_status(
        &self,
        rule_id: &[u8; 16],
        new_status: &str,
    ) -> Result<u64, StoreError> {
        let now_ms = Utc::now().timestamp_millis();
        let result = sqlx::query(
            "UPDATE stage2_watch_rules
             SET status = ?, updated_at_ms = ?
             WHERE rule_id = ?",
        )
        .bind(new_status)
        .bind(now_ms)
        .bind(rule_id_hex(rule_id))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

const SELECT_ALL_SQL: &str = "SELECT rule_id, user_pubkey, executor_pubkey, delegated_wallet_pubkey,
            canonical_rule_hash, canonical_rule_bytes_hex, rule_json,
            action_type, condition_logic, status,
            max_input_amount_raw, used_amount_raw, destination_pubkey,
            expires_at_slot, created_at_slot,
            last_checked_slot, last_successful_tick_at_ms, last_error,
            execution_nonce, completed, revoked,
            created_at_ms, updated_at_ms
     FROM stage2_watch_rules
     WHERE rule_id = ?";

fn rule_id_hex(id: &[u8; 16]) -> String {
    hex_encode(id)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        std::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}")).unwrap();
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>, StoreError> {
    if s.len() % 2 != 0 {
        return Err(StoreError::IntegrityCheckFailed(format!(
            "hex string has odd length: {} chars",
            s.len()
        )));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks_exact(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, StoreError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(StoreError::IntegrityCheckFailed(format!(
            "invalid hex character: 0x{b:02x}"
        ))),
    }
}

fn condition_logic_str(c: ConditionLogic) -> &'static str {
    match c {
        ConditionLogic::All => "all",
        ConditionLogic::Any => "any",
    }
}

fn parse_condition_logic(s: &str) -> Result<ConditionLogic, StoreError> {
    match s {
        "all" => Ok(ConditionLogic::All),
        "any" => Ok(ConditionLogic::Any),
        other => Err(StoreError::IntegrityCheckFailed(format!(
            "unknown condition_logic '{other}'"
        ))),
    }
}

fn parse_action_type(s: &str) -> Result<WatchRuleActionType, StoreError> {
    match s {
        "solend_withdraw_all_delegated" => {
            Ok(WatchRuleActionType::SolendWithdrawAllDelegated)
        }
        "jupiter_buy_sol_with_usdc" => Ok(WatchRuleActionType::JupiterBuySolWithUsdc),
        other => Err(StoreError::IntegrityCheckFailed(format!(
            "unknown action_type '{other}'"
        ))),
    }
}

#[derive(sqlx::FromRow)]
struct RowRaw {
    rule_id: String,
    #[allow(dead_code)]
    user_pubkey: String,
    #[allow(dead_code)]
    executor_pubkey: String,
    #[allow(dead_code)]
    delegated_wallet_pubkey: String,
    canonical_rule_hash: String,
    canonical_rule_bytes_hex: String,
    rule_json: String,
    action_type: String,
    condition_logic: String,
    status: String,
    #[allow(dead_code)]
    max_input_amount_raw: i64,
    #[allow(dead_code)]
    used_amount_raw: i64,
    #[allow(dead_code)]
    destination_pubkey: String,
    #[allow(dead_code)]
    expires_at_slot: i64,
    #[allow(dead_code)]
    created_at_slot: i64,
    last_checked_slot: Option<i64>,
    last_successful_tick_at_ms: Option<i64>,
    last_error: Option<String>,
    execution_nonce: i64,
    completed: i64,
    revoked: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
}

fn row_to_stored(row: RowRaw) -> Result<StoredWatchRule, StoreError> {
    // rule_id sanity (32 hex chars / 16 bytes).
    let _rule_id_bytes = hex_decode(&row.rule_id)?;
    // canonical_rule_hash hex → 32 bytes.
    let hash_vec = hex_decode(&row.canonical_rule_hash)?;
    let canonical_rule_hash: [u8; 32] = hash_vec.as_slice().try_into().map_err(|_| {
        StoreError::IntegrityCheckFailed(format!(
            "canonical_rule_hash must decode to 32 bytes, got {}",
            hash_vec.len()
        ))
    })?;
    let canonical_rule_bytes = hex_decode(&row.canonical_rule_bytes_hex)?;

    let rule: WatchRule = serde_json::from_str(&row.rule_json)?;

    // Recompute hash to detect rule_json / canonical_rule_hash drift.
    let recomputed = canonical_rule_hash_inner(&rule);
    if recomputed != canonical_rule_hash {
        return Err(StoreError::IntegrityCheckFailed(format!(
            "canonical hash mismatch for rule_id {}: stored={} recomputed={}",
            row.rule_id,
            hex_encode(&canonical_rule_hash),
            hex_encode(&recomputed),
        )));
    }
    // rule.rule_id must match the row's primary key.
    let stored_id_lower = row.rule_id.to_lowercase();
    let rule_id_lower = hex_encode(&rule.rule_id);
    if stored_id_lower != rule_id_lower {
        return Err(StoreError::IntegrityCheckFailed(format!(
            "rule_id mismatch: column={stored_id_lower} json={rule_id_lower}"
        )));
    }

    let status = WatchRuleStatus::parse(&row.status).ok_or_else(|| {
        StoreError::IntegrityCheckFailed(format!("unknown status '{}'", row.status))
    })?;
    let action_type = parse_action_type(&row.action_type)?;
    let condition_logic = parse_condition_logic(&row.condition_logic)?;

    Ok(StoredWatchRule {
        rule,
        canonical_rule_hash,
        canonical_rule_bytes,
        status,
        action_type,
        condition_logic,
        last_checked_slot: row.last_checked_slot.map(|v| v as u64),
        last_successful_tick_at_ms: row.last_successful_tick_at_ms,
        last_error: row.last_error,
        execution_nonce: row.execution_nonce as u64,
        completed: row.completed != 0,
        revoked: row.revoked != 0,
        created_at_ms: row.created_at_ms,
        updated_at_ms: row.updated_at_ms,
    })
}

fn canonical_rule_hash_inner(rule: &WatchRule) -> [u8; 32] {
    canonical_rule_hash(rule)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use claw_types::canonical_intent::PubkeyBytes;
    use claw_types::stage2_watch_rule::{
        ActionSpec, BoundMode, Comparison, Condition, JupiterApiVersion, RateKind,
        VerificationLevel, WithdrawMode, STAGE2_WATCH_RULE_SCHEMA_VERSION,
    };

    async fn test_repo() -> (Database, Stage2WatchRuleRepository) {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        (db, repo)
    }

    fn pk(b: u8) -> PubkeyBytes {
        PubkeyBytes::new([b; 32])
    }

    fn pk_from_str(s: &str) -> PubkeyBytes {
        PubkeyBytes::from_base58(s).expect("test pubkey parses")
    }

    const FIXTURE_RULE_ID_A: [u8; 16] = [
        0x52, 0x55, 0x4c, 0x45, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
    ];
    const FIXTURE_RULE_ID_B: [u8; 16] = [
        0x52, 0x55, 0x4c, 0x45, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
    ];

    const BTC_USD_FEED_ID: [u8; 32] = [
        0xe6, 0x2d, 0xf6, 0xc8, 0xb4, 0xa8, 0x5f, 0xe1, 0xa6, 0x7d, 0xb4, 0x4d, 0xc1,
        0x2d, 0xe5, 0xdb, 0x33, 0x0f, 0x7a, 0xc6, 0x6b, 0x72, 0xdc, 0x65, 0x8a, 0xfe,
        0xdf, 0x0f, 0x4a, 0x41, 0x5b, 0x43,
    ];
    const ETH_USD_FEED_ID: [u8; 32] = [
        0xff, 0x61, 0x49, 0x1a, 0x93, 0x11, 0x12, 0xdd, 0xf1, 0xbd, 0x81, 0x47, 0xcd,
        0x1b, 0x64, 0x13, 0x75, 0xf7, 0x9f, 0x58, 0x25, 0x12, 0x6d, 0x66, 0x54, 0x80,
        0x87, 0x46, 0x34, 0xfd, 0x0a, 0xce,
    ];
    const SOL_USD_FEED_ID: [u8; 32] = [
        0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95,
        0xd1, 0xda, 0x39, 0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c,
        0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
    ];

    const SOLEND_USDC_RESERVE: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
    const SOLEND_LENDING_MARKET: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
    const SOLEND_PROGRAM_ID: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
    const TEST_USER: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

    const FIXTURE_CREATED_AT: u64 = 415_500_000;
    const FIXTURE_EXPIRES_AT: u64 = 415_700_000;

    fn fixture_a_solend_apr_below_10() -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id: FIXTURE_RULE_ID_A,
            user: pk_from_str(TEST_USER),
            executor: pk(0x02),
            delegated_wallet: pk(0x03),
            created_at_slot: FIXTURE_CREATED_AT,
            expires_at_slot: FIXTURE_EXPIRES_AT,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![Condition::SolendReserveSupplyRate {
                reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
                lending_market: pk_from_str(SOLEND_LENDING_MARKET),
                solend_program_id: pk_from_str(SOLEND_PROGRAM_ID),
                comparison: Comparison::Lt,
                threshold_bps: 1_000,
                rate_kind: RateKind::Apr,
                formula_version: 1,
                max_reserve_staleness_slots: 16,
                required_refresh_same_tx: true,
            }],
            action: ActionSpec::SolendWithdrawAllDelegated {
                target_obligation: pk(0x04),
                reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
                lending_market: pk_from_str(SOLEND_LENDING_MARKET),
                destination_wallet: pk_from_str(TEST_USER),
                withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
            },
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk_from_str(TEST_USER),
            slippage_bps: 0,
        }
    }

    fn fixture_b_basket_buy_sol() -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id: FIXTURE_RULE_ID_B,
            user: pk_from_str(TEST_USER),
            executor: pk(0x02),
            delegated_wallet: pk(0x03),
            created_at_slot: FIXTURE_CREATED_AT,
            expires_at_slot: FIXTURE_EXPIRES_AT,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![
                Condition::PythPrice {
                    feed_id: BTC_USD_FEED_ID,
                    price_update_account: pk(0x10),
                    comparison: Comparison::Gt,
                    threshold_mantissa: 7_500_000,
                    threshold_exponent: -2,
                    max_age_seconds: 30,
                    max_confidence_bps: 50,
                    verification_level_required: VerificationLevel::Full,
                    bound_mode: BoundMode::AdverseLowerForGt,
                },
                Condition::PythPrice {
                    feed_id: ETH_USD_FEED_ID,
                    price_update_account: pk(0x11),
                    comparison: Comparison::Gt,
                    threshold_mantissa: 230_000,
                    threshold_exponent: -2,
                    max_age_seconds: 30,
                    max_confidence_bps: 50,
                    verification_level_required: VerificationLevel::Full,
                    bound_mode: BoundMode::AdverseLowerForGt,
                },
                Condition::PythPrice {
                    feed_id: SOL_USD_FEED_ID,
                    price_update_account: pk(0x12),
                    comparison: Comparison::Lt,
                    threshold_mantissa: 9_000,
                    threshold_exponent: -2,
                    max_age_seconds: 30,
                    max_confidence_bps: 50,
                    verification_level_required: VerificationLevel::Full,
                    bound_mode: BoundMode::AdverseUpperForLt,
                },
            ],
            action: ActionSpec::JupiterBuySolWithUsdc {
                input_mint: pk_from_str(USDC_MINT),
                output_mint: pk_from_str(WSOL_MINT),
                input_amount_raw: 5_000_000,
                min_output_amount_raw: None,
                jupiter_api_version: JupiterApiVersion::V2,
                max_accounts_hint: 48,
                require_pre_post_bracket: true,
            },
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk_from_str(TEST_USER),
            slippage_bps: 50,
        }
    }

    // ── Migration / table creation ──────────────────────────────────────

    #[tokio::test]
    async fn migration_creates_table() {
        let (db, _repo) = test_repo().await;
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='stage2_watch_rules'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, 1, "stage2_watch_rules table should exist");
    }

    // ── Insert / get round-trip ─────────────────────────────────────────

    #[tokio::test]
    async fn insert_and_get_roundtrip_scenario_a() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();

        repo.insert(&rule).await.unwrap();
        let loaded = repo.get(&rule.rule_id).await.unwrap().expect("row exists");

        assert_eq!(loaded.rule, rule);
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert_eq!(loaded.action_type, WatchRuleActionType::SolendWithdrawAllDelegated);
        assert_eq!(loaded.condition_logic, ConditionLogic::All);
        assert_eq!(loaded.execution_nonce, 0);
        assert!(!loaded.completed);
        assert!(!loaded.revoked);
        assert!(loaded.last_error.is_none());
        assert!(loaded.last_checked_slot.is_none());
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip_scenario_b() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_b_basket_buy_sol();

        repo.insert(&rule).await.unwrap();
        let loaded = repo.get(&rule.rule_id).await.unwrap().expect("row exists");

        assert_eq!(loaded.rule, rule);
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert_eq!(loaded.action_type, WatchRuleActionType::JupiterBuySolWithUsdc);
        assert_eq!(loaded.condition_logic, ConditionLogic::All);
        assert_eq!(loaded.rule.conditions.len(), 3);
    }

    #[tokio::test]
    async fn stored_canonical_hash_matches_recomputed_hash() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();

        repo.insert(&rule).await.unwrap();
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();

        assert_eq!(loaded.canonical_rule_hash, canonical_rule_hash(&rule));
        assert_eq!(loaded.canonical_rule_bytes, canonical_rule_bytes(&rule));
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_rule_id() {
        let (_db, repo) = test_repo().await;
        let unknown = [0xAB; 16];
        assert!(repo.get(&unknown).await.unwrap().is_none());
    }

    // ── list_active filtering ───────────────────────────────────────────

    #[tokio::test]
    async fn list_active_excludes_expired_revoked_completed() {
        let (_db, repo) = test_repo().await;

        let active_rule = fixture_a_solend_apr_below_10();
        let mut completed_rule = fixture_a_solend_apr_below_10();
        completed_rule.rule_id = [0x10; 16];
        let mut revoked_rule = fixture_a_solend_apr_below_10();
        revoked_rule.rule_id = [0x11; 16];
        let mut slot_expired_rule = fixture_a_solend_apr_below_10();
        slot_expired_rule.rule_id = [0x12; 16];
        slot_expired_rule.expires_at_slot = FIXTURE_CREATED_AT + 100; // tight expiry
        let mut status_expired_rule = fixture_a_solend_apr_below_10();
        status_expired_rule.rule_id = [0x13; 16];

        repo.insert(&active_rule).await.unwrap();
        repo.insert(&completed_rule).await.unwrap();
        repo.insert(&revoked_rule).await.unwrap();
        repo.insert(&slot_expired_rule).await.unwrap();
        repo.insert(&status_expired_rule).await.unwrap();

        repo.mark_completed(&completed_rule.rule_id, 1_000, FIXTURE_CREATED_AT + 1)
            .await
            .unwrap();
        repo.mark_revoked(&revoked_rule.rule_id).await.unwrap();
        repo.mark_expired(&status_expired_rule.rule_id).await.unwrap();

        // current_slot is well past slot_expired_rule's expiry but before active_rule's.
        let current_slot = FIXTURE_CREATED_AT + 200;
        let listed = repo.list_active(current_slot).await.unwrap();
        let listed_ids: Vec<[u8; 16]> = listed.iter().map(|r| r.rule.rule_id).collect();
        assert_eq!(
            listed_ids,
            vec![active_rule.rule_id],
            "only the still-active row should be listed"
        );
    }

    // ── Status transitions ──────────────────────────────────────────────

    #[tokio::test]
    async fn mark_condition_met_transitions_status() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let n = repo.mark_condition_met(&rule.rule_id).await.unwrap();
        assert_eq!(n, 1);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::ConditionMet);
    }

    #[tokio::test]
    async fn mark_executing_stores_nonce_and_status() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let nonce = 42_u64;
        let n = repo.mark_executing(&rule.rule_id, nonce).await.unwrap();
        assert_eq!(n, 1);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Executing);
        assert_eq!(loaded.execution_nonce, nonce);
    }

    #[tokio::test]
    async fn mark_completed_sets_completed_flag_and_used_amount() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let used = 5_000_000_u64;
        let slot = FIXTURE_CREATED_AT + 50;
        let n = repo.mark_completed(&rule.rule_id, used, slot).await.unwrap();
        assert_eq!(n, 1);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Completed);
        assert!(loaded.completed);
        assert_eq!(loaded.last_checked_slot, Some(slot));
        // used_amount_raw is reflected on the canonical rule via re-deser only
        // if we rewrote rule_json; we don't, so the column reflects state-store
        // bookkeeping. Verify via direct column read.
        let row: (i64,) = sqlx::query_as(
            "SELECT used_amount_raw FROM stage2_watch_rules WHERE rule_id = ?",
        )
        .bind(rule_id_hex(&rule.rule_id))
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        assert_eq!(row.0 as u64, used);
    }

    #[tokio::test]
    async fn mark_failed_preserves_last_error() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let err_msg = "tx simulate failed: insufficient compute";
        let n = repo.mark_failed(&rule.rule_id, err_msg).await.unwrap();
        assert_eq!(n, 1);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
        assert_eq!(loaded.last_error.as_deref(), Some(err_msg));
        assert!(!loaded.completed, "failed != completed");
    }

    #[tokio::test]
    async fn mark_revoked_excludes_from_active_list() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        repo.mark_revoked(&rule.rule_id).await.unwrap();
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Revoked);
        assert!(loaded.revoked);

        let active = repo.list_active(FIXTURE_CREATED_AT + 1).await.unwrap();
        assert!(active.is_empty(), "revoked rule must not appear in list_active");
    }

    #[tokio::test]
    async fn mark_checked_updates_slot_and_tick_ts() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        repo.mark_checked(&rule.rule_id, FIXTURE_CREATED_AT + 5, 1_700_000_000_000)
            .await
            .unwrap();

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.last_checked_slot, Some(FIXTURE_CREATED_AT + 5));
        assert_eq!(loaded.last_successful_tick_at_ms, Some(1_700_000_000_000));
        assert_eq!(loaded.status, WatchRuleStatus::Active);
    }

    #[tokio::test]
    async fn update_health_tick_only_touches_tick_ts() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        repo.update_health_tick(&rule.rule_id, 1_800_000_000_000)
            .await
            .unwrap();

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.last_successful_tick_at_ms, Some(1_800_000_000_000));
        assert!(loaded.last_checked_slot.is_none());
    }

    // ── list_by_user ────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_by_user_filters_correctly() {
        let (_db, repo) = test_repo().await;

        let rule_user_a = fixture_a_solend_apr_below_10();
        let mut rule_user_b = fixture_b_basket_buy_sol();
        rule_user_b.user = pk(0xAB);
        rule_user_b.destination = pk(0xAB);

        repo.insert(&rule_user_a).await.unwrap();
        repo.insert(&rule_user_b).await.unwrap();

        let list_a = repo.list_by_user(&rule_user_a.user.to_base58()).await.unwrap();
        assert_eq!(list_a.len(), 1);
        assert_eq!(list_a[0].rule.rule_id, rule_user_a.rule_id);

        let list_b = repo.list_by_user(&rule_user_b.user.to_base58()).await.unwrap();
        assert_eq!(list_b.len(), 1);
        assert_eq!(list_b[0].rule.rule_id, rule_user_b.rule_id);
    }

    // ── u64 / i64 boundary ──────────────────────────────────────────────

    #[tokio::test]
    async fn large_u64_amount_roundtrip() {
        // i64::MAX as u64 is the largest amount the INTEGER column can hold
        // without sign-bit corruption. This test pins that boundary.
        let (_db, repo) = test_repo().await;
        let mut rule = fixture_a_solend_apr_below_10();
        rule.max_input_amount_raw = i64::MAX as u64;
        rule.rule_id = [0x55; 16];

        repo.insert(&rule).await.unwrap();
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.rule.max_input_amount_raw, i64::MAX as u64);

        // Also exercise the bookkeeping column written via mark_completed.
        let completed_used = (i64::MAX as u64) - 1;
        repo.mark_completed(&rule.rule_id, completed_used, 0).await.unwrap();
        let row: (i64,) = sqlx::query_as(
            "SELECT used_amount_raw FROM stage2_watch_rules WHERE rule_id = ?",
        )
        .bind(rule_id_hex(&rule.rule_id))
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        assert_eq!(row.0 as u64, completed_used);
    }

    // ── Duplicate / unknown ─────────────────────────────────────────────

    #[tokio::test]
    async fn duplicate_rule_id_rejected() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();
        let err = repo.insert(&rule).await.expect_err("second insert must fail");
        // sqlx::Error::Database carries the UNIQUE constraint violation.
        assert!(matches!(err, StoreError::Sqlx(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn unknown_status_string_in_db_fails_to_load() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        // Inject an out-of-band status value.
        sqlx::query("UPDATE stage2_watch_rules SET status = 'mystery' WHERE rule_id = ?")
            .bind(rule_id_hex(&rule.rule_id))
            .execute(&repo.pool)
            .await
            .unwrap();

        let err = repo.get(&rule.rule_id).await.expect_err("must reject unknown status");
        assert!(
            matches!(err, StoreError::IntegrityCheckFailed(ref s) if s.contains("status") && s.contains("mystery")),
            "got {err:?}",
        );

        // And the Rust enum cannot be constructed from the unknown string.
        assert!(WatchRuleStatus::parse("mystery").is_none());
    }

    #[tokio::test]
    async fn canonical_hash_drift_detected_on_load() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        // Corrupt the rule_json so its hash no longer matches the stored hash.
        let mut tampered: WatchRule = rule.clone();
        tampered.slippage_bps = tampered.slippage_bps.wrapping_add(1);
        let tampered_json = serde_json::to_string(&tampered).unwrap();
        sqlx::query("UPDATE stage2_watch_rules SET rule_json = ? WHERE rule_id = ?")
            .bind(tampered_json)
            .bind(rule_id_hex(&rule.rule_id))
            .execute(&repo.pool)
            .await
            .unwrap();

        let err = repo.get(&rule.rule_id).await.expect_err("hash drift must be caught");
        assert!(
            matches!(err, StoreError::IntegrityCheckFailed(ref s) if s.contains("canonical hash mismatch")),
            "got {err:?}",
        );
    }

    // ── W2 status-guarded helpers ───────────────────────────────────────

    #[tokio::test]
    async fn mark_condition_met_if_active_succeeds_on_active() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let n = repo.mark_condition_met_if_active(&rule.rule_id).await.unwrap();
        assert_eq!(n, 1, "active row should flip to condition_met");

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::ConditionMet);
    }

    #[tokio::test]
    async fn mark_condition_met_if_active_no_op_on_revoked() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();
        repo.mark_revoked(&rule.rule_id).await.unwrap();

        let n = repo.mark_condition_met_if_active(&rule.rule_id).await.unwrap();
        assert_eq!(n, 0, "revoked row must not be overwritten");

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Revoked);
    }

    #[tokio::test]
    async fn mark_condition_met_if_active_idempotent_on_condition_met() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        repo.mark_condition_met(&rule.rule_id).await.unwrap();

        let n = repo.mark_condition_met_if_active(&rule.rule_id).await.unwrap();
        assert_eq!(n, 0, "second flip from condition_met must no-op");
    }

    #[tokio::test]
    async fn mark_failed_if_not_terminal_does_not_overwrite_revoked() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();
        repo.mark_revoked(&rule.rule_id).await.unwrap();

        let n = repo
            .mark_failed_if_not_terminal(&rule.rule_id, "evaluator timeout")
            .await
            .unwrap();
        assert_eq!(n, 0, "revoked row must not be flipped to failed");

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Revoked);
        assert!(
            loaded.last_error.is_none(),
            "no last_error should be written when guard rejects"
        );
    }

    #[tokio::test]
    async fn mark_failed_if_not_terminal_writes_error_on_active() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let n = repo
            .mark_failed_if_not_terminal(&rule.rule_id, "evaluator unsupported action")
            .await
            .unwrap();
        assert_eq!(n, 1);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
        assert_eq!(
            loaded.last_error.as_deref(),
            Some("evaluator unsupported action")
        );
    }

    #[tokio::test]
    async fn mark_failed_if_not_terminal_does_not_overwrite_completed() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();
        repo.mark_completed(&rule.rule_id, 5_000_000, FIXTURE_CREATED_AT + 5)
            .await
            .unwrap();

        let n = repo
            .mark_failed_if_not_terminal(&rule.rule_id, "x")
            .await
            .unwrap();
        assert_eq!(n, 0);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Completed);
        assert!(loaded.completed);
    }

    #[tokio::test]
    async fn record_last_error_does_not_change_status() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let n = repo
            .record_last_error(&rule.rule_id, "RPC timeout, will retry")
            .await
            .unwrap();
        assert_eq!(n, 1);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert_eq!(
            loaded.last_error.as_deref(),
            Some("RPC timeout, will retry")
        );
    }

    #[tokio::test]
    async fn mark_expired_if_not_terminal_does_not_overwrite_revoked() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();
        repo.mark_revoked(&rule.rule_id).await.unwrap();

        let n = repo.mark_expired_if_not_terminal(&rule.rule_id).await.unwrap();
        assert_eq!(n, 0);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Revoked);
    }

    #[tokio::test]
    async fn mark_expired_if_not_terminal_flips_active() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let n = repo.mark_expired_if_not_terminal(&rule.rule_id).await.unwrap();
        assert_eq!(n, 1);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Expired);
    }

    #[tokio::test]
    async fn list_active_limit_caps_results() {
        let (_db, repo) = test_repo().await;

        // Insert 5 distinct active rules.
        for i in 0..5_u8 {
            let mut r = fixture_a_solend_apr_below_10();
            r.rule_id = [0xC0 + i; 16];
            repo.insert(&r).await.unwrap();
        }

        let two = repo
            .list_active_limit(FIXTURE_CREATED_AT + 1, 2)
            .await
            .unwrap();
        assert_eq!(two.len(), 2);

        let all_with_huge_limit = repo
            .list_active_limit(FIXTURE_CREATED_AT + 1, 999)
            .await
            .unwrap();
        assert_eq!(all_with_huge_limit.len(), 5);
    }

    #[tokio::test]
    async fn list_active_limit_zero_returns_empty() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_a_solend_apr_below_10();
        repo.insert(&rule).await.unwrap();

        let none = repo
            .list_active_limit(FIXTURE_CREATED_AT + 1, 0)
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn list_pending_lifecycle_includes_expired_by_slot_active_status() {
        let (_db, repo) = test_repo().await;

        // Tight-expiry rule whose slot has already passed but
        // status is still `active` — the W2 watcher's
        // garbage-collect-first branch needs to see this.
        let mut tight = fixture_a_solend_apr_below_10();
        tight.rule_id = [0xE0; 16];
        tight.expires_at_slot = FIXTURE_CREATED_AT + 100;
        repo.insert(&tight).await.unwrap();

        // list_active_limit at a higher slot would NOT see this
        // row (slot filter excludes it).
        let active_at_higher_slot = repo
            .list_active_limit(FIXTURE_CREATED_AT + 200, 100)
            .await
            .unwrap();
        assert!(active_at_higher_slot.is_empty());

        // list_pending_lifecycle_limit DOES see it — that's the
        // whole point.
        let pending = repo.list_pending_lifecycle_limit(100).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].rule.rule_id, tight.rule_id);
        assert_eq!(pending[0].status, WatchRuleStatus::Active);
    }

    #[tokio::test]
    async fn list_pending_lifecycle_excludes_terminal() {
        let (_db, repo) = test_repo().await;
        let active = fixture_a_solend_apr_below_10();
        let mut revoked = fixture_a_solend_apr_below_10();
        revoked.rule_id = [0xE1; 16];
        let mut completed = fixture_a_solend_apr_below_10();
        completed.rule_id = [0xE2; 16];
        let mut expired_by_status = fixture_a_solend_apr_below_10();
        expired_by_status.rule_id = [0xE3; 16];

        for r in [&active, &revoked, &completed, &expired_by_status] {
            repo.insert(r).await.unwrap();
        }
        repo.mark_revoked(&revoked.rule_id).await.unwrap();
        repo.mark_completed(&completed.rule_id, 5_000_000, 0)
            .await
            .unwrap();
        repo.mark_expired(&expired_by_status.rule_id).await.unwrap();

        let pending = repo.list_pending_lifecycle_limit(100).await.unwrap();
        let ids: Vec<[u8; 16]> = pending.iter().map(|r| r.rule.rule_id).collect();
        assert_eq!(ids, vec![active.rule_id]);
    }

    #[tokio::test]
    async fn list_pending_lifecycle_caps_at_limit() {
        let (_db, repo) = test_repo().await;
        for i in 0..4_u8 {
            let mut r = fixture_a_solend_apr_below_10();
            r.rule_id = [0xF0 + i; 16];
            repo.insert(&r).await.unwrap();
        }
        let two = repo.list_pending_lifecycle_limit(2).await.unwrap();
        assert_eq!(two.len(), 2);

        let zero = repo.list_pending_lifecycle_limit(0).await.unwrap();
        assert!(zero.is_empty());
    }
}

//! Durable persistence for Jupiter swap intents.
//!
//! Stores intents flowing through the approval workflow. The stored
//! shape is intentionally minimal:
//!
//! - `SwapIntent` (the declarative "what to swap")
//! - `PolicyVerdict` (what the swap policy ruled)
//! - `SwapPreview` (optional, informational-only quote preview)
//! - `state` + `last_error` (lifecycle tracking)
//!
//! Transaction bytes, blockhashes, lookup tables, and instruction payloads
//! are NOT stored here — those are JIT build-time concerns.

use chrono::{DateTime, TimeZone, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

use claw_types::{
    policy::PolicyVerdict,
    swap::{SwapIntent, SwapPreview},
};

use crate::errors::StoreError;

/// Lifecycle states for a pending Jupiter swap intent.
///
/// Valid transitions:
///
/// ```text
/// PENDING_APPROVAL ─┬─► APPROVED_WAITING_JIT ─► JIT_BUILDING ─┬─► SUBMITTED
///                   │                                          │
///                   │                                          ├─► AWAITING_WALLET_SIGNATURE ─► SUBMITTED
///                   │                                          │
///                   │                                          └─► JIT_BUILD_FAILED
///                   │
///                   ├─► REJECTED   (operator rejection, terminal)
///                   └─► EXPIRED    (lease timeout, terminal)
/// ```
///
/// `JIT_BUILD_FAILED` covers all non-network failures during JIT assembly
/// (build API error, simulate error, sign error). `last_error` distinguishes.
pub mod state {
    /// Awaiting operator decision.
    pub const PENDING_APPROVAL: &str = "pending_approval";
    /// Operator approved; JIT build has not yet started.
    pub const APPROVED_WAITING_JIT: &str = "approved_waiting_jit";
    /// Actively fetching a fresh Jupiter route, assembling tx, simulating.
    pub const JIT_BUILDING: &str = "jit_building";
    /// JIT assembly failed (build API, simulate, or sign).
    pub const JIT_BUILD_FAILED: &str = "jit_build_failed";
    /// External wallet is signing.
    pub const AWAITING_WALLET_SIGNATURE: &str = "awaiting_wallet_signature";
    /// Signed and submitted to the network.
    pub const SUBMITTED: &str = "submitted";
    /// Operator rejected.
    pub const REJECTED: &str = "rejected";
    /// Lease timeout or blockhash expiry.
    pub const EXPIRED: &str = "expired";
}

/// A persisted Jupiter swap intent row.
#[derive(Debug, Clone)]
pub struct StoredJupiterIntent {
    pub intent: SwapIntent,
    pub request_id: Option<Uuid>,
    pub state: String,
    pub policy_verdict: Option<PolicyVerdict>,
    pub preview: Option<SwapPreview>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Repository for the `pending_jupiter_swaps` table.
#[derive(Clone, Debug)]
pub struct PendingJupiterRepository {
    pool: SqlitePool,
}

impl PendingJupiterRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new intent row.
    ///
    /// `request_id` is optional — `None` when the intent was auto-approved or
    /// rejected without a human-approval step.
    pub async fn insert(
        &self,
        intent: &SwapIntent,
        request_id: Option<Uuid>,
        state: &str,
        policy_verdict: Option<&PolicyVerdict>,
        preview: Option<&SwapPreview>,
    ) -> Result<(), StoreError> {
        let verdict_json = policy_verdict
            .map(serde_json::to_string)
            .transpose()?;
        let preview_json = preview
            .map(serde_json::to_string)
            .transpose()?;
        let now = Utc::now().timestamp();

        sqlx::query(
            "INSERT INTO pending_jupiter_swaps
                 (intent_id, session_id, request_id, wallet_pubkey, network,
                  input_mint, output_mint, input_amount, slippage_bps, description,
                  state, policy_verdict_json, preview_json, last_error,
                  created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
        )
        .bind(intent.id.to_string())
        .bind(intent.session_id.to_string())
        .bind(request_id.map(|r| r.to_string()))
        .bind(&intent.wallet_pubkey)
        .bind(network_str(&intent.network))
        .bind(&intent.input_mint)
        .bind(&intent.output_mint)
        .bind(intent.input_amount as i64)
        .bind(intent.slippage_bps as i64)
        .bind(&intent.description)
        .bind(state)
        .bind(verdict_json)
        .bind(preview_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Attach a newly created approval request ID to an intent.
    pub async fn attach_request_id(
        &self,
        intent_id: Uuid,
        request_id: Uuid,
    ) -> Result<u64, StoreError> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            "UPDATE pending_jupiter_swaps
             SET request_id = ?, updated_at = ?
             WHERE intent_id = ?",
        )
        .bind(request_id.to_string())
        .bind(now)
        .bind(intent_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Transition an intent to a new state, optionally recording an error.
    pub async fn mark_state(
        &self,
        intent_id: Uuid,
        state: &str,
        last_error: Option<&str>,
    ) -> Result<u64, StoreError> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            "UPDATE pending_jupiter_swaps
             SET state = ?, last_error = ?, updated_at = ?
             WHERE intent_id = ?",
        )
        .bind(state)
        .bind(last_error)
        .bind(now)
        .bind(intent_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Load a single intent by ID.
    pub async fn get(&self, intent_id: Uuid) -> Result<Option<StoredJupiterIntent>, StoreError> {
        let row = sqlx::query_as::<_, JupiterRowRaw>(
            "SELECT intent_id, session_id, request_id, wallet_pubkey, network,
                    input_mint, output_mint, input_amount, slippage_bps, description,
                    state, policy_verdict_json, preview_json, last_error,
                    created_at, updated_at
             FROM pending_jupiter_swaps
             WHERE intent_id = ?",
        )
        .bind(intent_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_stored).transpose()
    }

    /// Load an intent by its approval request ID.
    pub async fn get_by_request_id(
        &self,
        request_id: Uuid,
    ) -> Result<Option<StoredJupiterIntent>, StoreError> {
        let row = sqlx::query_as::<_, JupiterRowRaw>(
            "SELECT intent_id, session_id, request_id, wallet_pubkey, network,
                    input_mint, output_mint, input_amount, slippage_bps, description,
                    state, policy_verdict_json, preview_json, last_error,
                    created_at, updated_at
             FROM pending_jupiter_swaps
             WHERE request_id = ?",
        )
        .bind(request_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_stored).transpose()
    }

    /// Bulk-expire all intents whose state is in `states`, recording `reason`
    /// as `last_error`.
    ///
    /// Used on daemon restart to drain rows that were mid-flight (e.g.
    /// `approved_waiting_jit`, `jit_building`) and have no in-memory resume task.
    /// Returns the total number of rows updated.
    pub async fn expire_stuck(
        &self,
        states: &[&str],
        reason: &str,
    ) -> Result<u64, StoreError> {
        let now = Utc::now().timestamp();
        let mut total = 0u64;
        for &state in states {
            let result = sqlx::query(
                "UPDATE pending_jupiter_swaps
                 SET state = 'expired', last_error = ?, updated_at = ?
                 WHERE state = ?",
            )
            .bind(reason)
            .bind(now)
            .bind(state)
            .execute(&self.pool)
            .await?;
            total += result.rows_affected();
        }
        Ok(total)
    }

    /// List all pending intents for a session, oldest first.
    pub async fn list_pending_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredJupiterIntent>, StoreError> {
        let rows = sqlx::query_as::<_, JupiterRowRaw>(
            "SELECT intent_id, session_id, request_id, wallet_pubkey, network,
                    input_mint, output_mint, input_amount, slippage_bps, description,
                    state, policy_verdict_json, preview_json, last_error,
                    created_at, updated_at
             FROM pending_jupiter_swaps
             WHERE session_id = ? AND state = 'pending_approval'
             ORDER BY created_at ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_stored).collect()
    }
}

// ── Row → domain type ──────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct JupiterRowRaw {
    intent_id: String,
    session_id: String,
    request_id: Option<String>,
    wallet_pubkey: String,
    network: String,
    input_mint: String,
    output_mint: String,
    input_amount: i64,
    slippage_bps: i64,
    description: Option<String>,
    state: String,
    policy_verdict_json: Option<String>,
    preview_json: Option<String>,
    last_error: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn row_to_stored(row: JupiterRowRaw) -> Result<StoredJupiterIntent, StoreError> {
    let intent_id = Uuid::parse_str(&row.intent_id).map_err(|e| {
        StoreError::IntegrityCheckFailed(format!("invalid intent_id '{}': {e}", row.intent_id))
    })?;
    let session_id_uuid = Uuid::parse_str(&row.session_id).map_err(|e| {
        StoreError::IntegrityCheckFailed(format!("invalid session_id '{}': {e}", row.session_id))
    })?;
    let request_id = row
        .request_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| StoreError::IntegrityCheckFailed(format!("invalid request_id: {e}")))?;

    let network = parse_network(&row.network).ok_or_else(|| {
        StoreError::IntegrityCheckFailed(format!("unknown network '{}'", row.network))
    })?;

    let created_at = Utc.timestamp_opt(row.created_at, 0).single().ok_or_else(|| {
        StoreError::IntegrityCheckFailed(format!("invalid created_at {}", row.created_at))
    })?;
    let updated_at = Utc.timestamp_opt(row.updated_at, 0).single().ok_or_else(|| {
        StoreError::IntegrityCheckFailed(format!("invalid updated_at {}", row.updated_at))
    })?;

    let intent = SwapIntent {
        id: intent_id,
        session_id: claw_types::session::SessionId::from(session_id_uuid),
        wallet_pubkey: row.wallet_pubkey,
        network,
        input_mint: row.input_mint,
        output_mint: row.output_mint,
        input_amount: row.input_amount as u64,
        slippage_bps: row.slippage_bps as u16,
        description: row.description.unwrap_or_default(),
        created_at,
    };

    let policy_verdict = row
        .policy_verdict_json
        .as_deref()
        .map(serde_json::from_str::<PolicyVerdict>)
        .transpose()?;
    let preview = row
        .preview_json
        .as_deref()
        .map(serde_json::from_str::<SwapPreview>)
        .transpose()?;

    Ok(StoredJupiterIntent {
        intent,
        request_id,
        state: row.state,
        policy_verdict,
        preview,
        last_error: row.last_error,
        created_at,
        updated_at,
    })
}

fn network_str(network: &claw_types::solana::SolanaNetwork) -> &'static str {
    use claw_types::solana::SolanaNetwork;
    match network {
        SolanaNetwork::MainnetBeta => "mainnet-beta",
        SolanaNetwork::Devnet => "devnet",
        SolanaNetwork::Testnet => "testnet",
        SolanaNetwork::Localnet => "localnet",
    }
}

fn parse_network(s: &str) -> Option<claw_types::solana::SolanaNetwork> {
    use claw_types::solana::SolanaNetwork;
    match s {
        "mainnet" | "mainnet-beta" => Some(SolanaNetwork::MainnetBeta),
        "devnet" => Some(SolanaNetwork::Devnet),
        "testnet" => Some(SolanaNetwork::Testnet),
        "localnet" | "localhost" => Some(SolanaNetwork::Localnet),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use claw_types::{
        policy::PolicyVerdict,
        session::SessionId,
        solana::SolanaNetwork,
    };

    async fn test_repo() -> PendingJupiterRepository {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        PendingJupiterRepository::new(db.pool().clone())
    }

    fn test_intent() -> SwapIntent {
        SwapIntent::new(
            SessionId::from(Uuid::new_v4()),
            "CL4w7VNnCpGh3oJ8qfPFR4RjV6R3K4fZ1234567890ab",
            SolanaNetwork::Devnet,
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "So11111111111111111111111111111111111111112",
            100_000_000,
            50,
            "buy SOL with USDC",
        )
    }

    fn test_verdict() -> PolicyVerdict {
        PolicyVerdict::RequiresHumanApproval {
            reason: "input amount exceeds cap".to_string(),
            rule_name: "swap-input-amount-exceeds-cap".to_string(),
            required_approver_role: Some("treasury".to_string()),
            approval_chain: None,
        }
    }

    fn test_preview() -> SwapPreview {
        SwapPreview {
            expected_output_amount: 670_000_000,
            price_impact_pct: Some("0.0012".to_string()),
            route_labels: vec!["Orca".to_string()],
            fetched_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn insert_and_get_round_trips() {
        let repo = test_repo().await;
        let intent = test_intent();
        let verdict = test_verdict();
        let preview = test_preview();

        repo.insert(&intent, None, state::PENDING_APPROVAL, Some(&verdict), Some(&preview))
            .await
            .unwrap();

        let loaded = repo.get(intent.id).await.unwrap().expect("should exist");
        assert_eq!(loaded.intent.id, intent.id);
        assert_eq!(loaded.intent.input_mint, intent.input_mint);
        assert_eq!(loaded.intent.input_amount, intent.input_amount);
        assert_eq!(loaded.intent.slippage_bps, intent.slippage_bps);
        assert_eq!(loaded.state, state::PENDING_APPROVAL);
        assert!(loaded.request_id.is_none());
        assert_eq!(loaded.last_error, None);
        assert!(loaded.policy_verdict.is_some());
        assert!(loaded.preview.is_some());
    }

    #[tokio::test]
    async fn insert_without_verdict_or_preview() {
        let repo = test_repo().await;
        let intent = test_intent();

        repo.insert(&intent, None, state::APPROVED_WAITING_JIT, None, None).await.unwrap();

        let loaded = repo.get(intent.id).await.unwrap().unwrap();
        assert!(loaded.policy_verdict.is_none());
        assert!(loaded.preview.is_none());
        assert_eq!(loaded.state, state::APPROVED_WAITING_JIT);
    }

    #[tokio::test]
    async fn attach_request_id_links_intent_to_approval() {
        let repo = test_repo().await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();

        repo.insert(&intent, None, state::PENDING_APPROVAL, None, None).await.unwrap();
        let affected = repo.attach_request_id(intent.id, request_id).await.unwrap();
        assert_eq!(affected, 1);

        let loaded = repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(loaded.request_id, Some(request_id));

        // Also findable by request_id
        let by_req = repo.get_by_request_id(request_id).await.unwrap().unwrap();
        assert_eq!(by_req.intent.id, intent.id);
    }

    #[tokio::test]
    async fn mark_state_transitions_and_records_error() {
        let repo = test_repo().await;
        let intent = test_intent();

        repo.insert(&intent, None, state::PENDING_APPROVAL, None, None).await.unwrap();

        let affected = repo
            .mark_state(intent.id, state::REJECTED, Some("operator rejected"))
            .await
            .unwrap();
        assert_eq!(affected, 1);

        let loaded = repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(loaded.state, state::REJECTED);
        assert_eq!(loaded.last_error.as_deref(), Some("operator rejected"));
    }

    #[tokio::test]
    async fn mark_state_clears_error_when_none_provided() {
        let repo = test_repo().await;
        let intent = test_intent();

        repo.insert(&intent, None, state::PENDING_APPROVAL, None, None).await.unwrap();
        repo.mark_state(intent.id, state::REJECTED, Some("err")).await.unwrap();
        repo.mark_state(intent.id, state::SUBMITTED, None).await.unwrap();

        let loaded = repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(loaded.state, state::SUBMITTED);
        assert!(loaded.last_error.is_none());
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let repo = test_repo().await;
        assert!(repo.get(Uuid::new_v4()).await.unwrap().is_none());
        assert!(repo.get_by_request_id(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_pending_for_session_filters_by_state() {
        let repo = test_repo().await;
        let session = SessionId::from(Uuid::new_v4());
        let other_session = SessionId::from(Uuid::new_v4());

        let mut pending1 = test_intent();
        pending1.session_id = session.clone();
        let mut approved = test_intent();
        approved.session_id = session.clone();
        let mut other = test_intent();
        other.session_id = other_session.clone();

        repo.insert(&pending1, None, state::PENDING_APPROVAL, None, None).await.unwrap();
        repo.insert(&approved, None, state::APPROVED_WAITING_JIT, None, None).await.unwrap();
        repo.insert(&other, None, state::PENDING_APPROVAL, None, None).await.unwrap();

        let listed = repo.list_pending_for_session(&session.to_string()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].intent.id, pending1.id);
    }

    #[tokio::test]
    async fn persistence_preserves_all_intent_fields() {
        // Guards against future migrations dropping intent fields.
        let repo = test_repo().await;
        let intent = test_intent();

        repo.insert(&intent, None, state::PENDING_APPROVAL, None, None).await.unwrap();
        let loaded = repo.get(intent.id).await.unwrap().unwrap();

        assert_eq!(loaded.intent.id, intent.id);
        assert_eq!(loaded.intent.wallet_pubkey, intent.wallet_pubkey);
        assert_eq!(loaded.intent.input_mint, intent.input_mint);
        assert_eq!(loaded.intent.output_mint, intent.output_mint);
        assert_eq!(loaded.intent.input_amount, intent.input_amount);
        assert_eq!(loaded.intent.slippage_bps, intent.slippage_bps);
        assert_eq!(loaded.intent.description, intent.description);
    }

    #[tokio::test]
    async fn expire_stuck_marks_target_states_and_leaves_others_untouched() {
        let repo = test_repo().await;

        // Insert one row in each of the states we care about plus two bystanders.
        let mut waiting_jit = test_intent();
        let mut building = test_intent();
        let mut pending = test_intent();
        let mut submitted = test_intent();
        let mut rejected = test_intent();

        repo.insert(&waiting_jit, None, state::APPROVED_WAITING_JIT, None, None).await.unwrap();
        repo.insert(&building,    None, state::JIT_BUILDING,          None, None).await.unwrap();
        repo.insert(&pending,     None, state::PENDING_APPROVAL,      None, None).await.unwrap();
        repo.insert(&submitted,   None, state::SUBMITTED,             None, None).await.unwrap();
        repo.insert(&rejected,    None, state::REJECTED,              None, None).await.unwrap();

        let count = repo
            .expire_stuck(
                &[state::APPROVED_WAITING_JIT, state::JIT_BUILDING],
                "daemon restart",
            )
            .await
            .unwrap();

        assert_eq!(count, 2, "exactly the two stuck rows should be expired");

        // Stuck rows are now EXPIRED with the right reason.
        let row = repo.get(waiting_jit.id).await.unwrap().unwrap();
        assert_eq!(row.state, state::EXPIRED);
        assert_eq!(row.last_error.as_deref(), Some("daemon restart"));

        let row = repo.get(building.id).await.unwrap().unwrap();
        assert_eq!(row.state, state::EXPIRED);
        assert_eq!(row.last_error.as_deref(), Some("daemon restart"));

        // Bystander states are unchanged.
        let row = repo.get(pending.id).await.unwrap().unwrap();
        assert_eq!(row.state, state::PENDING_APPROVAL);

        let row = repo.get(submitted.id).await.unwrap().unwrap();
        assert_eq!(row.state, state::SUBMITTED);

        let row = repo.get(rejected.id).await.unwrap().unwrap();
        assert_eq!(row.state, state::REJECTED);
    }

    #[tokio::test]
    async fn expire_stuck_with_no_matching_rows_returns_zero() {
        let repo = test_repo().await;
        // Insert only a SUBMITTED row — target states have no entries.
        let intent = test_intent();
        repo.insert(&intent, None, state::SUBMITTED, None, None).await.unwrap();

        let count = repo
            .expire_stuck(
                &[state::APPROVED_WAITING_JIT, state::JIT_BUILDING],
                "daemon restart",
            )
            .await
            .unwrap();

        assert_eq!(count, 0);
        // SUBMITTED row unchanged.
        let row = repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(row.state, state::SUBMITTED);
    }
}

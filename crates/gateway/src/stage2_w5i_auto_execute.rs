//! Stage 2 W5i — demo-only background watcher that auto-executes a
//! W5h funded conditional order when the Save APY condition is met.
//!
//! # Scope
//!
//! This is **demo polish**, NOT a production scheduler. It handles
//! exactly the pinned W5h demo shape:
//!
//!   amount_raw            = 250_000 (0.25 USDC)
//!   controlled_wallet     = BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L
//!   controlled_usdc_ata   = 7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3
//!
//! Anything else is left untouched (the watcher silently skips). No
//! refund, no Jupiter, no LLM, no generic multi-rule scheduling, no
//! retry queue, no in-memory dedup.
//!
//! # Architecture
//!
//! On daemon startup, **if every env gate is set**, [`spawn_supervised`]
//! starts a tokio task that runs [`Stage2W5iAutoExecuteWatcher::run`]
//! — a `tokio::time::interval` loop ticking every 30 s. Each tick:
//!
//!   1. Lists W5h intents currently in `budget_reserved`.
//!   2. Filters to the pinned demo shape.
//!   3. Re-fetches Save display APY (cheap optimisation: avoids burning
//!      a CAS lease when the condition is clearly false).
//!   4. If `save_display_apy_bps > intent.threshold_bps`, dispatches
//!      through the **existing [`Stage2ChatExecutor`]** with the W5g
//!      approval phrase — the executor is already wired with
//!      `.with_w5h_intent_repo(...)` so it does the
//!      `budget_reserved → executing` CAS internally. Same gate as the
//!      manual W5g approval command — so the watcher and a racing
//!      manual command can both call `execute()` and at most ONE wins
//!      the CAS.
//!   5. Logs the outcome (Completed / BroadcastedTimeout / PrechecksFailed
//!      / ExecutionFailed). State-store transitions are handled by the
//!      executor itself.
//!
//! # Hardening (addendum)
//!
//!   - **Async safety:** `tokio::spawn` / `tokio::time::interval` only.
//!     No blocking-thread sleep call anywhere in this file (proven
//!     by [`tests::source_guard_no_std_thread_sleep`]).
//!
//!   - **CAS:** the watcher does NOT pre-lease. The executor's step 11a
//!     calls `lease_execution_if_budget_reserved` immediately before
//!     tx construction. Watcher + manual W5g approval share that CAS.
//!
//!   - **Resiliency:** [`tick`] returns `Result`. [`run`] matches on
//!     the result; an `Err` is `warn!`-logged and the loop continues.
//!     A single failed poll cycle does NOT kill the daemon. No
//!     `.unwrap()` / `.expect()` / `panic!()` on any live path.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::interval;
use tracing::{debug, info, warn};

use claw_state_store::stage2_w5h_funding::{
    Stage2W5hFundingIntentRepository, W5hFundingIntent,
};

use crate::stage2_chat_execute::{
    ChatExecuteOutcome, ChatExecuteRequest, ChatExecuteStatus, Stage2ChatExecutor,
    W5G_ENV_APPROVAL_PHRASE_OWNER, W5G_ENV_CLUSTER, W5G_ENV_DELEGATED_KEYPAIR_PATH,
    W5G_ENV_MASTER_GATE, W5G_REQUIRED_APPROVAL_PHRASE,
};
use crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher;

// ── Pinned demo identifiers ──────────────────────────────────────────────

/// Deterministic regex / W5h path canonical amount. The watcher used
/// to require equality; Phase 5c-lite relaxes that to a band check
/// (any amount in [PHASE5C_MIN_AMOUNT_RAW, PHASE5C_MAX_AMOUNT_RAW] is
/// considered a valid funded W5h intent). The constant is kept for
/// test fixtures so the W5h-lite deterministic path remains
/// indistinguishable from its pre-Phase-5c shape.
const PINNED_AMOUNT_RAW: u64 = 250_000;
const PINNED_CONTROLLED_WALLET: &str =
    "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";
const PINNED_CONTROLLED_USDC_ATA: &str =
    "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3";

// ── Tuning constants ─────────────────────────────────────────────────────

/// Poll cadence between watcher ticks. Demo-only; production schedulers
/// would use a smarter interval + jitter.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Maximum intents inspected per tick. The pinned demo only ever has
/// ~1 intent at a time; this is a defense-in-depth bound.
const MAX_INTENTS_PER_TICK: u32 = 16;

/// W5i master gate env var name. Required IN ADDITION TO the full W5g
/// env-gate chain. Missing or != "1" → watcher not started.
pub const W5I_ENV_MASTER_GATE: &str = "CLAW_STAGE2_W5I_AUTO_EXECUTION";

// ── Errors ───────────────────────────────────────────────────────────────

/// Non-fatal errors a single tick can surface. The outer [`run`] loop
/// catches every variant, logs it, and continues — a single failed
/// poll cycle MUST NOT kill the daemon.
#[derive(Debug, thiserror::Error)]
pub enum TickError {
    #[error("repo.list_budget_reserved failed: {0}")]
    RepoListFailed(String),
    #[error("save apy fetch failed: {0}")]
    SaveApyFailed(String),
}

// ── Config ───────────────────────────────────────────────────────────────

/// Snapshot of every env gate the W5i watcher requires. Use
/// [`Self::from_std_env`] to read all of them.
#[derive(Debug, Clone)]
pub struct Stage2W5iAutoExecuteConfig {
    /// `CLAW_STAGE2_W5I_AUTO_EXECUTION == "1"`.
    pub w5i_master_gate_on: bool,
    /// `CLAW_STAGE2_LIVE_CHAT_EXECUTION == "1"` (W5g master gate).
    pub w5g_master_gate_on: bool,
    /// `CLAW_STAGE2_CHAT_EXECUTION_APPROVED` matches the required
    /// approval phrase verbatim.
    pub w5g_env_approval_matches: bool,
    /// `CLAW_STAGE2_CLUSTER == "mainnet-beta"`.
    pub w5g_cluster_mainnet_beta: bool,
    /// `CLAW_STAGE2_DELEGATED_KEYPAIR_PATH` non-blank.
    pub w5g_keypair_path_present: bool,
    /// `HELIUS_RPC_URL` or `CLAW_RPC_URL` non-blank.
    pub rpc_url_present: bool,
}

impl Stage2W5iAutoExecuteConfig {
    pub fn from_std_env() -> Self {
        let env = |key: &str| std::env::var(key).ok().filter(|s| !s.trim().is_empty());
        let w5i_master_gate_on =
            env(W5I_ENV_MASTER_GATE).as_deref() == Some("1");
        let w5g_master_gate_on =
            env(W5G_ENV_MASTER_GATE).as_deref() == Some("1");
        let w5g_env_approval_matches = env(W5G_ENV_APPROVAL_PHRASE_OWNER)
            .as_deref()
            == Some(W5G_REQUIRED_APPROVAL_PHRASE);
        let w5g_cluster_mainnet_beta =
            env(W5G_ENV_CLUSTER).as_deref() == Some("mainnet-beta");
        let w5g_keypair_path_present =
            env(W5G_ENV_DELEGATED_KEYPAIR_PATH).is_some();
        let rpc_url_present = env("HELIUS_RPC_URL").is_some()
            || env("CLAW_RPC_URL").is_some();
        Self {
            w5i_master_gate_on,
            w5g_master_gate_on,
            w5g_env_approval_matches,
            w5g_cluster_mainnet_beta,
            w5g_keypair_path_present,
            rpc_url_present,
        }
    }

    /// Returns `true` only when EVERY gate is satisfied. If any gate
    /// is missing, the daemon must NOT spawn the watcher.
    pub fn fully_enabled(&self) -> bool {
        self.w5i_master_gate_on
            && self.w5g_master_gate_on
            && self.w5g_env_approval_matches
            && self.w5g_cluster_mainnet_beta
            && self.w5g_keypair_path_present
            && self.rpc_url_present
    }

    /// One-line reason string for the operator log when not fully
    /// enabled.
    pub fn disabled_reason(&self) -> String {
        let mut missing: Vec<&str> = Vec::new();
        if !self.w5i_master_gate_on {
            missing.push("CLAW_STAGE2_W5I_AUTO_EXECUTION");
        }
        if !self.w5g_master_gate_on {
            missing.push("CLAW_STAGE2_LIVE_CHAT_EXECUTION");
        }
        if !self.w5g_env_approval_matches {
            missing.push("CLAW_STAGE2_CHAT_EXECUTION_APPROVED");
        }
        if !self.w5g_cluster_mainnet_beta {
            missing.push("CLAW_STAGE2_CLUSTER (must be mainnet-beta)");
        }
        if !self.w5g_keypair_path_present {
            missing.push("CLAW_STAGE2_DELEGATED_KEYPAIR_PATH");
        }
        if !self.rpc_url_present {
            missing.push("HELIUS_RPC_URL or CLAW_RPC_URL");
        }
        if missing.is_empty() {
            "all gates satisfied".to_string()
        } else {
            format!("missing: {}", missing.join(", "))
        }
    }
}

// ── Executor seam (mocked in tests) ──────────────────────────────────────

/// Trait that abstracts the per-rule execution call so tests can
/// inject a counter / panic stub without spinning up the live
/// `Stage2ChatExecutor`.
///
/// Production uses the blanket impl over `Stage2ChatExecutor` below.
#[async_trait]
pub trait W5iExecutionDispatcher: Send + Sync + std::fmt::Debug {
    async fn execute(&self, request: ChatExecuteRequest) -> ChatExecuteOutcome;
}

#[async_trait]
impl W5iExecutionDispatcher for Stage2ChatExecutor {
    async fn execute(&self, request: ChatExecuteRequest) -> ChatExecuteOutcome {
        Stage2ChatExecutor::execute(self, request).await
    }
}

// ── Watcher ──────────────────────────────────────────────────────────────

/// Per-tick result summary. Useful for tests + telemetry; the live
/// loop only logs it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub scanned: usize,
    pub skipped_not_pinned_shape: usize,
    pub skipped_condition_false: usize,
    pub dispatched: usize,
    pub completed: usize,
    pub broadcasted_timeout: usize,
    pub prechecks_failed: usize,
    pub execution_failed: usize,
}

pub struct Stage2W5iAutoExecuteWatcher {
    intent_repo: Arc<Stage2W5hFundingIntentRepository>,
    save_apy_fetcher: Arc<dyn SaveDisplayApyFetcher>,
    dispatcher: Arc<dyn W5iExecutionDispatcher>,
}

impl std::fmt::Debug for Stage2W5iAutoExecuteWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage2W5iAutoExecuteWatcher").finish()
    }
}

impl Stage2W5iAutoExecuteWatcher {
    pub fn new(
        intent_repo: Arc<Stage2W5hFundingIntentRepository>,
        save_apy_fetcher: Arc<dyn SaveDisplayApyFetcher>,
        dispatcher: Arc<dyn W5iExecutionDispatcher>,
    ) -> Self {
        Self {
            intent_repo,
            save_apy_fetcher,
            dispatcher,
        }
    }

    /// Single tick. Returns `Result` so the outer loop can `warn!`
    /// and continue on any failure — a tick error must NEVER panic
    /// or kill the daemon.
    pub async fn tick(&self) -> Result<TickReport, TickError> {
        let mut report = TickReport::default();

        let intents = self
            .intent_repo
            .list_budget_reserved(MAX_INTENTS_PER_TICK)
            .await
            .map_err(|e| TickError::RepoListFailed(e.to_string()))?;

        report.scanned = intents.len();
        if intents.is_empty() {
            return Ok(report);
        }

        // Fetch Save APY ONCE per tick. The pinned demo only has the
        // Solend Main Pool USDC reserve, so one reading covers all
        // intents this tick.
        let save = self
            .save_apy_fetcher
            .fetch_main_pool_usdc()
            .await
            .map_err(|e| TickError::SaveApyFailed(e.to_string()))?;

        for intent in intents {
            if !is_pinned_demo_shape(&intent) {
                report.skipped_not_pinned_shape += 1;
                debug!(
                    rule = %intent.rule_id_hex,
                    amount = %intent.amount_raw,
                    "W5i tick: skipped (not pinned demo shape)"
                );
                continue;
            }

            if save.save_display_apy_bps <= intent.threshold_bps {
                report.skipped_condition_false += 1;
                debug!(
                    rule = %intent.rule_id_hex,
                    apy = save.save_display_apy_bps,
                    threshold = intent.threshold_bps,
                    "W5i tick: condition false (apy <= threshold); keep watching"
                );
                continue;
            }

            // Condition true. Dispatch to the existing executor — its
            // step 11a does the budget_reserved -> executing CAS,
            // shared with the manual W5g approval command.
            report.dispatched += 1;
            let request = ChatExecuteRequest {
                rule_id_hex: intent.rule_id_hex.clone(),
                canonical_rule_hash_hex: intent.canonical_rule_hash_hex.clone(),
                approval_phrase: W5G_REQUIRED_APPROVAL_PHRASE.to_string(),
            };
            let outcome = self.dispatcher.execute(request).await;
            match outcome.status {
                ChatExecuteStatus::Completed => {
                    report.completed += 1;
                    info!(
                        rule = %intent.rule_id_hex,
                        tx = ?outcome.tx_signature,
                        slot = ?outcome.confirmation_slot,
                        "W5i: auto-execute COMPLETED"
                    );
                }
                ChatExecuteStatus::BroadcastedTimeout => {
                    report.broadcasted_timeout += 1;
                    warn!(
                        rule = %intent.rule_id_hex,
                        tx = ?outcome.tx_signature,
                        "W5i: broadcasted but timed-out waiting for finality; \
                         leaving intent in executing (manual check via Solscan)"
                    );
                }
                ChatExecuteStatus::PrechecksFailed => {
                    report.prechecks_failed += 1;
                    debug!(
                        rule = %intent.rule_id_hex,
                        err = ?outcome.error,
                        reason = ?outcome.error_reason,
                        "W5i: precheck refused (expected if Save APY drifted \
                         below threshold between watcher fetch and executor \
                         re-check, or another tick already won the CAS)"
                    );
                }
                ChatExecuteStatus::ExecutionFailed => {
                    report.execution_failed += 1;
                    warn!(
                        rule = %intent.rule_id_hex,
                        err = ?outcome.error,
                        reason = ?outcome.error_reason,
                        "W5i: execution failed live (tx build / size / broadcast)"
                    );
                }
            }
        }

        Ok(report)
    }

    /// Outer loop. Catches every `tick()` error, logs it, and
    /// continues. Never panics, never blocks the runtime thread.
    /// Designed to be spawned with `spawn_supervised` so any internal
    /// abort still surfaces a supervisor restart, but in normal
    /// operation it should run forever without restart.
    pub async fn run(self: Arc<Self>) {
        info!(
            interval_secs = POLL_INTERVAL.as_secs(),
            "W5i auto-execution watcher started"
        );
        let mut ticker = interval(POLL_INTERVAL);
        // First `tick()` on tokio::interval fires immediately. Skip
        // it so we don't race the daemon startup banner; the first
        // useful poll happens POLL_INTERVAL later.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match self.tick().await {
                Ok(report) => {
                    if report.dispatched > 0 || report.completed > 0 {
                        info!(?report, "W5i tick");
                    } else {
                        debug!(?report, "W5i tick");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "W5i tick failed (continuing)");
                }
            }
        }
    }
}

/// Pinned demo shape predicate. Anything that doesn't match is
/// silently skipped by the watcher.
///
/// Phase 5c-lite: the amount check is now a BAND check rather than
/// strict equality with `PINNED_AMOUNT_RAW`. The intent's
/// `amount_raw` was written into the DB by the bridge from a
/// schema-validated source (either the deterministic 0.25 USDC regex
/// or the Phase 5c-lite LLM extractor whose schema bounds the value
/// to `[PHASE5C_MIN_AMOUNT_RAW, PHASE5C_MAX_AMOUNT_RAW]`). Any value
/// outside that band must have been written by a path we don't
/// recognise — silently skip it (the watcher is defense-in-depth;
/// the bridge enforces the band at insertion time).
fn is_pinned_demo_shape(intent: &W5hFundingIntent) -> bool {
    use crate::stage2_phase5c_draft::{
        PHASE5C_MAX_AMOUNT_RAW, PHASE5C_MIN_AMOUNT_RAW,
    };
    (PHASE5C_MIN_AMOUNT_RAW..=PHASE5C_MAX_AMOUNT_RAW).contains(&intent.amount_raw)
        && intent.controlled_wallet == PINNED_CONTROLLED_WALLET
        && intent.controlled_usdc_ata == PINNED_CONTROLLED_USDC_ATA
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use claw_state_store::db::Database;
    use claw_state_store::stage2_w5h_funding::{
        NewW5hFundingIntent, W5hIntentStatus,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::stage2_chat_execute::ChatExecuteErrorCode;
    use crate::stage2_demo_apr_bridge::{EvaluationError, SaveDisplayApyReading};

    // ── Test fixtures ──────────────────────────────────────────────────

    fn pinned_intent(intent_id: &str, threshold_bps: u32, now_ms: i64) -> NewW5hFundingIntent {
        NewW5hFundingIntent {
            intent_id: intent_id.to_string(),
            rule_id_hex: intent_id.to_string(),
            canonical_rule_hash_hex: "00".repeat(32),
            user_wallet: "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string(),
            user_usdc_ata: "TestUserUsdcAta1111111111111111111111111111".to_string(),
            controlled_wallet: PINNED_CONTROLLED_WALLET.to_string(),
            controlled_usdc_ata: PINNED_CONTROLLED_USDC_ATA.to_string(),
            amount_raw: PINNED_AMOUNT_RAW,
            threshold_bps,
            save_display_apy_bps_at_creation: 312,
            native_onchain_apr_bps_at_creation: 287,
            created_at_ms: now_ms,
            expires_at_ms: now_ms + 180_000,
        }
    }

    async fn fixture_repo_with_budget_reserved(
        intent_id: &str,
        threshold_bps: u32,
    ) -> (Database, Arc<Stage2W5hFundingIntentRepository>) {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        // CRITICAL: use chrono::Utc::now() so the fixture's
        // expires_at_ms is in the future relative to the
        // production `lease_execution_if_budget_reserved` CAS (which
        // also reads chrono::Utc::now()). A unix-epoch-1970
        // timestamp would make the lease see the intent as expired.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let intent = pinned_intent(intent_id, threshold_bps, now_ms);
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "Sig1")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "Sig1", 100)
            .await
            .unwrap();
        (db, repo)
    }

    /// Save APY fetcher returning a fixed value.
    #[derive(Debug, Clone)]
    struct StubSaveApy {
        bps: u32,
    }
    #[async_trait]
    impl SaveDisplayApyFetcher for StubSaveApy {
        async fn fetch_main_pool_usdc(
            &self,
        ) -> Result<SaveDisplayApyReading, EvaluationError> {
            Ok(SaveDisplayApyReading {
                save_display_apy_bps: self.bps,
                raw_supply_interest_str: format!("{:.2}", self.bps as f64 / 100.0),
                reserve_pubkey: "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw".to_string(),
                lending_market: "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY".to_string(),
                liquidity_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                collateral_mint: "993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk".to_string(),
                rewards_present: false,
            })
        }
    }

    /// Save APY fetcher that always errors. Used by the resiliency
    /// tests to prove the watcher logs and continues.
    #[derive(Debug, Clone)]
    struct FailingSaveApy;
    #[async_trait]
    impl SaveDisplayApyFetcher for FailingSaveApy {
        async fn fetch_main_pool_usdc(
            &self,
        ) -> Result<SaveDisplayApyReading, EvaluationError> {
            Err(EvaluationError::RpcFetchFailed {
                detail: "simulated RPC failure".to_string(),
            })
        }
    }

    /// Dispatcher stub. Counts calls + returns a configurable outcome.
    /// Lets us assert "dispatched exactly N times" + drive every
    /// `ChatExecuteStatus` path. Also runs the real repo state
    /// transitions so we can verify CAS semantics.
    #[derive(Debug)]
    struct StubDispatcher {
        calls: AtomicUsize,
        outcome_status: ChatExecuteStatus,
        intent_repo: Arc<Stage2W5hFundingIntentRepository>,
    }

    impl StubDispatcher {
        fn new(
            outcome_status: ChatExecuteStatus,
            intent_repo: Arc<Stage2W5hFundingIntentRepository>,
        ) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                outcome_status,
                intent_repo,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl W5iExecutionDispatcher for StubDispatcher {
        async fn execute(&self, request: ChatExecuteRequest) -> ChatExecuteOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Simulate the executor's step 11a CAS — this is what makes
            // the watcher and a racing manual W5g approval share the
            // same gate.
            let now_ms = chrono::Utc::now().timestamp_millis();
            let leased = self
                .intent_repo
                .lease_execution_if_budget_reserved(&request.rule_id_hex, now_ms)
                .await
                .unwrap_or(0);
            if leased == 0 {
                return ChatExecuteOutcome {
                    status: ChatExecuteStatus::PrechecksFailed,
                    rule_id_hex: request.rule_id_hex,
                    canonical_rule_hash_hex: request.canonical_rule_hash_hex,
                    tx_signature: None,
                    solscan_url: None,
                    confirmation_slot: None,
                    used_save_display_apy_bps: None,
                    used_native_onchain_apr_bps: None,
                    used_threshold_bps: None,
                    before_usdc_raw: None,
                    after_usdc_raw: None,
                    usdc_delta_raw: None,
                    before_ctoken_amount: None,
                    after_ctoken_amount: None,
                    ctoken_delta_raw: None,
                    serialized_tx_bytes: None,
                    instruction_count: None,
                    ctoken_ata_create_included: None,
                    error: Some(ChatExecuteErrorCode::RuleNotExecutable),
                    error_reason: Some("CAS lease lost — already executing".into()),
                };
            }
            // Lease won. Drive the requested terminal status.
            let tx_signature = Some("StubTxSig11111111111111111111111111111111".to_string());
            match self.outcome_status {
                ChatExecuteStatus::Completed => {
                    let _ = self
                        .intent_repo
                        .mark_completed_if_executing(
                            &request.rule_id_hex,
                            tx_signature.as_ref().unwrap(),
                        )
                        .await;
                    ChatExecuteOutcome {
                        status: ChatExecuteStatus::Completed,
                        rule_id_hex: request.rule_id_hex,
                        canonical_rule_hash_hex: request.canonical_rule_hash_hex,
                        tx_signature,
                        solscan_url: None,
                        confirmation_slot: Some(123_456_789),
                        used_save_display_apy_bps: None,
                        used_native_onchain_apr_bps: None,
                        used_threshold_bps: None,
                        before_usdc_raw: None,
                        after_usdc_raw: None,
                        usdc_delta_raw: None,
                        before_ctoken_amount: None,
                        after_ctoken_amount: None,
                        ctoken_delta_raw: None,
                        serialized_tx_bytes: None,
                        instruction_count: None,
                        ctoken_ata_create_included: None,
                        error: None,
                        error_reason: None,
                    }
                }
                ChatExecuteStatus::BroadcastedTimeout => ChatExecuteOutcome {
                    status: ChatExecuteStatus::BroadcastedTimeout,
                    rule_id_hex: request.rule_id_hex,
                    canonical_rule_hash_hex: request.canonical_rule_hash_hex,
                    tx_signature,
                    solscan_url: None,
                    confirmation_slot: None,
                    used_save_display_apy_bps: None,
                    used_native_onchain_apr_bps: None,
                    used_threshold_bps: None,
                    before_usdc_raw: None,
                    after_usdc_raw: None,
                    usdc_delta_raw: None,
                    before_ctoken_amount: None,
                    after_ctoken_amount: None,
                    ctoken_delta_raw: None,
                    serialized_tx_bytes: None,
                    instruction_count: None,
                    ctoken_ata_create_included: None,
                    error: None,
                    error_reason: None,
                },
                _ => ChatExecuteOutcome {
                    status: self.outcome_status,
                    rule_id_hex: request.rule_id_hex,
                    canonical_rule_hash_hex: request.canonical_rule_hash_hex,
                    tx_signature: None,
                    solscan_url: None,
                    confirmation_slot: None,
                    used_save_display_apy_bps: None,
                    used_native_onchain_apr_bps: None,
                    used_threshold_bps: None,
                    before_usdc_raw: None,
                    after_usdc_raw: None,
                    usdc_delta_raw: None,
                    before_ctoken_amount: None,
                    after_ctoken_amount: None,
                    ctoken_delta_raw: None,
                    serialized_tx_bytes: None,
                    instruction_count: None,
                    ctoken_ata_create_included: None,
                    error: None,
                    error_reason: None,
                },
            }
        }
    }

    /// Dispatcher that PANICS if invoked. Used by tests that prove
    /// the watcher MUST NOT call the executor.
    #[derive(Debug)]
    struct UncallableDispatcher;
    #[async_trait]
    impl W5iExecutionDispatcher for UncallableDispatcher {
        async fn execute(&self, _request: ChatExecuteRequest) -> ChatExecuteOutcome {
            panic!(
                "W5i dispatcher MUST NOT be called for this scenario \
                 (condition false / completed / unpinned shape / save api error)"
            );
        }
    }

    // ── Env-gate tests ─────────────────────────────────────────────────

    #[test]
    fn config_not_fully_enabled_when_w5i_master_gate_off() {
        let cfg = Stage2W5iAutoExecuteConfig {
            w5i_master_gate_on: false,
            w5g_master_gate_on: true,
            w5g_env_approval_matches: true,
            w5g_cluster_mainnet_beta: true,
            w5g_keypair_path_present: true,
            rpc_url_present: true,
        };
        assert!(!cfg.fully_enabled());
        assert!(cfg.disabled_reason().contains("CLAW_STAGE2_W5I_AUTO_EXECUTION"));
    }

    #[test]
    fn config_not_fully_enabled_when_any_w5g_gate_missing() {
        for off in 0..5 {
            let mut cfg = Stage2W5iAutoExecuteConfig {
                w5i_master_gate_on: true,
                w5g_master_gate_on: true,
                w5g_env_approval_matches: true,
                w5g_cluster_mainnet_beta: true,
                w5g_keypair_path_present: true,
                rpc_url_present: true,
            };
            match off {
                0 => cfg.w5g_master_gate_on = false,
                1 => cfg.w5g_env_approval_matches = false,
                2 => cfg.w5g_cluster_mainnet_beta = false,
                3 => cfg.w5g_keypair_path_present = false,
                4 => cfg.rpc_url_present = false,
                _ => unreachable!(),
            }
            assert!(!cfg.fully_enabled(), "off={off} should disable");
        }
    }

    #[test]
    fn config_fully_enabled_when_all_gates_set() {
        let cfg = Stage2W5iAutoExecuteConfig {
            w5i_master_gate_on: true,
            w5g_master_gate_on: true,
            w5g_env_approval_matches: true,
            w5g_cluster_mainnet_beta: true,
            w5g_keypair_path_present: true,
            rpc_url_present: true,
        };
        assert!(cfg.fully_enabled());
        assert_eq!(cfg.disabled_reason(), "all gates satisfied");
    }

    // ── Tick: condition false → no executor call ──────────────────────

    #[tokio::test]
    async fn condition_false_does_not_execute() {
        // Threshold=400 bps, current=312 bps → condition false → no
        // dispatch. Use the panic dispatcher to prove no call happens.
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 400).await;
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            Arc::new(UncallableDispatcher),
        );
        let report = watcher.tick().await.unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.skipped_condition_false, 1);
        assert_eq!(report.dispatched, 0);
        // Intent must STAY in budget_reserved.
        let stored = repo.get("a".repeat(32).as_str()).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::BudgetReserved);
    }

    // ── Tick: condition true → exactly one dispatch + completed ──────

    #[tokio::test]
    async fn condition_true_leases_once_and_calls_dispatcher_once() {
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 100).await;
        let dispatcher = Arc::new(StubDispatcher::new(
            ChatExecuteStatus::Completed,
            repo.clone(),
        ));
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            dispatcher.clone(),
        );
        let report = watcher.tick().await.unwrap();
        assert_eq!(report.dispatched, 1);
        assert_eq!(report.completed, 1);
        assert_eq!(dispatcher.calls(), 1);
        let stored = repo.get("a".repeat(32).as_str()).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::Completed);
        assert!(stored.execution_signature.is_some());
    }

    // ── Tick: double tick cannot double execute ──────────────────────

    #[tokio::test]
    async fn double_tick_cannot_double_execute_same_intent() {
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 100).await;
        let dispatcher = Arc::new(StubDispatcher::new(
            ChatExecuteStatus::Completed,
            repo.clone(),
        ));
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            dispatcher.clone(),
        );
        // First tick: executes.
        let _ = watcher.tick().await.unwrap();
        assert_eq!(dispatcher.calls(), 1);
        // Second tick: intent is completed, list_budget_reserved
        // returns []; no second dispatch.
        let r2 = watcher.tick().await.unwrap();
        assert_eq!(r2.scanned, 0);
        assert_eq!(r2.dispatched, 0);
        assert_eq!(dispatcher.calls(), 1, "executor must be called at most once");
    }

    // ── Tick: completed intent is skipped ─────────────────────────────

    #[tokio::test]
    async fn completed_intent_is_skipped() {
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 100).await;
        // Manually advance to completed via a real lease + mark.
        let now = chrono::Utc::now().timestamp_millis();
        repo.lease_execution_if_budget_reserved("a".repeat(32).as_str(), now)
            .await
            .unwrap();
        repo.mark_completed_if_executing("a".repeat(32).as_str(), "PriorTxSig")
            .await
            .unwrap();
        // Watcher must NOT see this intent (list_budget_reserved
        // filters by status).
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            Arc::new(UncallableDispatcher),
        );
        let report = watcher.tick().await.unwrap();
        assert_eq!(report.scanned, 0);
    }

    // ── Tick: executing intent is skipped (CAS race) ──────────────────

    #[tokio::test]
    async fn executing_intent_is_skipped_by_watcher() {
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 100).await;
        // Simulate a parallel manual W5g approval winning the lease.
        let now = chrono::Utc::now().timestamp_millis();
        repo.lease_execution_if_budget_reserved("a".repeat(32).as_str(), now)
            .await
            .unwrap();
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            Arc::new(UncallableDispatcher),
        );
        let report = watcher.tick().await.unwrap();
        assert_eq!(report.scanned, 0);
    }

    // ── Tick: w5i and manual w5g race; only one executes ─────────────

    #[tokio::test]
    async fn w5i_and_manual_w5g_race_only_one_executes() {
        // Both paths share the same `intent_repo`. When the watcher
        // ticks and a manual approval command arrives in parallel,
        // they BOTH attempt `lease_execution_if_budget_reserved`. The
        // DB grants the lease to exactly one; the other gets 0 rows
        // and returns PrechecksFailed. We use the StubDispatcher's
        // built-in CAS attempt to simulate that race.
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 100).await;
        let dispatcher = Arc::new(StubDispatcher::new(
            ChatExecuteStatus::Completed,
            repo.clone(),
        ));
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            dispatcher.clone(),
        );
        // Watcher dispatches one call. The dispatcher's CAS wins.
        let _ = watcher.tick().await.unwrap();
        assert_eq!(dispatcher.calls(), 1);
        // Now simulate the manual W5g approval command racing in.
        // It would also dispatch through the same executor, calling
        // the same CAS. The intent is now `completed`, so the CAS
        // fails — represented here by a second direct dispatcher
        // call to the same Arc.
        let outcome = dispatcher
            .execute(ChatExecuteRequest {
                rule_id_hex: "a".repeat(32),
                canonical_rule_hash_hex: "00".repeat(32),
                approval_phrase: W5G_REQUIRED_APPROVAL_PHRASE.to_string(),
            })
            .await;
        // The manual path's CAS finds the intent in `completed`, so
        // lease returns 0 → PrechecksFailed / rule_not_executable.
        assert_eq!(outcome.status, ChatExecuteStatus::PrechecksFailed);
        assert_eq!(
            outcome.error,
            Some(ChatExecuteErrorCode::RuleNotExecutable)
        );
        // And the intent stays completed — no second tx.
        let stored = repo.get("a".repeat(32).as_str()).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::Completed);
    }

    // ── Tick: broadcasted_timeout does NOT retry automatically ───────

    #[tokio::test]
    async fn broadcasted_timeout_does_not_retry_automatically() {
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 100).await;
        let dispatcher = Arc::new(StubDispatcher::new(
            ChatExecuteStatus::BroadcastedTimeout,
            repo.clone(),
        ));
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            dispatcher.clone(),
        );
        // First tick: dispatched once, status went to executing (CAS
        // lease won by stub) then stays executing because the
        // dispatcher returned BroadcastedTimeout WITHOUT completing.
        let _ = watcher.tick().await.unwrap();
        assert_eq!(dispatcher.calls(), 1);
        let stored = repo.get("a".repeat(32).as_str()).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::Executing);
        // Second tick: list_budget_reserved returns [] (intent is
        // executing, not budget_reserved). Dispatcher is NOT called
        // again.
        let r2 = watcher.tick().await.unwrap();
        assert_eq!(r2.scanned, 0);
        assert_eq!(dispatcher.calls(), 1);
    }

    // ── Tick: non-pinned shape is silently skipped ───────────────────

    #[tokio::test]
    async fn non_pinned_amount_is_skipped() {
        // Phase 5c-lite: the watcher's amount check is a BAND check
        // (PHASE5C_MIN_AMOUNT_RAW..=PHASE5C_MAX_AMOUNT_RAW) rather
        // than strict equality with the pre-Phase-5c pinned constant.
        // We use an amount clearly OUTSIDE that band (5 USDC = 5M raw)
        // to prove the watcher still skips non-recognised shapes.
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut wrong = pinned_intent(&"a".repeat(32), 100, now_ms);
        wrong.amount_raw = 5_000_000; // above the Phase 5c-lite band
        repo.insert(&wrong).await.unwrap();
        repo.mark_funding_submitted_if_required(&wrong.intent_id, "Sig")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&wrong.intent_id, "Sig", 100)
            .await
            .unwrap();
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(StubSaveApy { bps: 312 }),
            Arc::new(UncallableDispatcher),
        );
        let report = watcher.tick().await.unwrap();
        assert_eq!(report.skipped_not_pinned_shape, 1);
        assert_eq!(report.dispatched, 0);
    }

    // ── Resiliency: save apy error → no panic, no execute ───────────

    #[tokio::test]
    async fn save_api_error_does_not_execute_and_returns_typed_err() {
        let (_db, repo) = fixture_repo_with_budget_reserved("a".repeat(32).as_str(), 100).await;
        let watcher = Stage2W5iAutoExecuteWatcher::new(
            repo.clone(),
            Arc::new(FailingSaveApy),
            Arc::new(UncallableDispatcher),
        );
        let err = watcher.tick().await.unwrap_err();
        assert!(matches!(err, TickError::SaveApyFailed(_)));
        // Intent must STAY in budget_reserved — the watcher must NOT
        // touch state on transient Save APY failure.
        let stored = repo.get("a".repeat(32).as_str()).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::BudgetReserved);
    }

    // ── Source-guard: no blocking-thread sleep call ────────────────

    #[test]
    fn source_guard_no_std_thread_sleep() {
        const SRC: &str = include_str!("stage2_w5i_auto_execute.rs");
        // Needle assembled at runtime so this guard test does NOT
        // contain the joined literal.
        let needle = format!("{}{}", "std::thread::", "sleep");
        assert!(
            !SRC.contains(&needle),
            "stage2_w5i_auto_execute.rs must not contain `{needle}` — \
             the watcher must use tokio::time::sleep / interval to \
             avoid blocking the Tokio runtime thread"
        );
    }

    #[test]
    fn source_guard_no_send_or_keypair_load() {
        const SRC: &str = include_str!("stage2_w5i_auto_execute.rs");
        let needles = [
            format!("{}{}", "send", "Transaction("),
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "read_keypair_", "file"),
        ];
        for n in &needles {
            assert!(
                !SRC.contains(n.as_str()),
                "stage2_w5i_auto_execute.rs must not contain `{n}` — \
                 broadcasting / keypair loading belongs to the \
                 existing W5g LiveStage2ChatExecuteSender, not the watcher"
            );
        }
    }
}

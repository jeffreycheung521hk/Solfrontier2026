//! Stage 2 Executor Glue — W4 / E1-lite — Solend demo executor.
//!
//! Sits one stage downstream of the W2 watcher + W3 condition
//! evaluator. The watcher transitions `active → condition_met`; the
//! executor transitions `condition_met → executing → completed | failed`.
//!
//! # Scope (W4-lite)
//!
//! - Select `condition_met` rules with a Solend `ActionSpec`.
//! - Atomically lease them via the state-store CAS guard
//!   ([`Stage2WatchRuleRepository::mark_executing_if_condition_met`])
//!   AND a same-process in-flight `HashSet`.
//! - Build a strongly typed [`Stage2ExecuteActionRequest`] from the
//!   rule + the bound [`DemoSolendExecutionFixture`]
//!   (`MAINNET_BETA_DEMO_USDC_TUPLE`).
//! - Send the request to an injected [`Stage2ExecutionClient`].
//! - On success → [`Stage2WatchRuleRepository::mark_completed`].
//! - On failure → [`Stage2WatchRuleRepository::mark_failed_if_not_terminal`].
//! - One-shot v1: no retries. A failed execution is terminal.
//!
//! # Hard scope boundaries
//!
//! - **No live RPC.** The default execution client is
//!   [`MockExecutionClient`]; production senders attach later behind
//!   the same [`Stage2ExecutionClient`] seam.
//! - **No `Transaction` / `VersionedTransaction` construction.** The
//!   executor produces a strongly typed Rust request struct that
//!   captures everything a downstream sender would need (account
//!   pubkeys, nonce, canonical hash, demo oracle tuple), but it does
//!   NOT call any Solana builder, fetch a blockhash, or sign.
//! - **No oracle policy change.** The demo fixture is the
//!   P5c-pinned `MAINNET_BETA_DEMO_USDC_TUPLE`; rules with a
//!   different reserve fail closed (`DemoFixtureMismatch`).
//! - **No Jupiter routing.** Jupiter `ActionSpec` is rejected via
//!   the executor's `action_type_allowlist` (default: Solend only).
//! - **No `clawsol-authority` crate dependency.** Demo tuple
//!   pubkeys are re-pinned here as base58 strings; a parity test
//!   round-trips them so a divergence from the program-side const
//!   surfaces loudly. Pulling in the BPF crate would change the
//!   gateway's build graph for substrate-only work.
//!
//! # No-retry contract
//!
//! Both the lease (CAS) and the in-flight guard prevent a second
//! attempt while one is in flight. Once the client returns — success
//! OR failure — the executor moves the rule to a terminal status. A
//! crashed daemon that holds the lease orphans the row in `executing`;
//! recovery is out of scope for W4-lite and lands in W5/operator
//! tooling.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use claw_state_store::stage2_watch_rules::{
    Stage2WatchRuleRepository, StoredWatchRule, WatchRuleStatus,
};
use claw_state_store::StoreError;
use claw_types::canonical_intent::PubkeyBytes;
use claw_types::stage2_watch_rule::{
    ActionSpec, WatchRuleActionType, WithdrawMode, STAGE2_WATCH_RULE_SCHEMA_VERSION,
};

use crate::stage2_watcher::{Stage2Clock, Stage2TickContext, SystemClock};

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors raised by the executor or the client it dispatches to.
///
/// W4-lite collapses all client errors to a single terminal-failure
/// path (`mark_failed_if_not_terminal`), so the variant taxonomy
/// here is for caller-side classification only — no retry logic
/// reads it.
#[derive(Debug, thiserror::Error)]
pub enum Stage2ExecutorError {
    /// The state-store CAS guard refused the lease — either another
    /// actor already moved the row out of `condition_met`, or the row
    /// is in a terminal state. Caller MUST NOT mutate the row.
    #[error("lease lost for rule {rule_id_hex}")]
    LeaseLost { rule_id_hex: String },

    /// Rule's `action.action_type()` is not in the configured
    /// `action_type_allowlist` (W4-lite default: Solend only).
    #[error("unsupported action_type: {0:?}")]
    UnsupportedActionType(WatchRuleActionType),

    /// Rule's Solend action disagrees with the bound demo fixture
    /// (e.g. reserve_pubkey != `MAINNET_BETA_DEMO_USDC_TUPLE.reserve`).
    /// The demo executor fails closed; routing to a different fixture
    /// is a future slice.
    #[error("rule does not match demo fixture: {0}")]
    DemoFixtureMismatch(String),

    /// Rule has no remaining input headroom
    /// (`used_amount_raw >= max_input_amount_raw`). A one-shot v1 rule
    /// that already executed must not re-execute.
    #[error("input headroom exhausted: used={used} max={max}")]
    InputHeadroomExhausted { used: u64, max: u64 },

    /// The execution client returned an error.
    #[error("execution client error: {0}")]
    Client(#[from] Stage2ExecutionError),

    /// Infrastructure failure inside the executor itself (state-store
    /// I/O, serialisation, etc.). Not the rule's fault; not a client
    /// fault.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<StoreError> for Stage2ExecutorError {
    fn from(value: StoreError) -> Self {
        Self::Internal(format!("state-store: {value}"))
    }
}

// ── Execution client trait ──────────────────────────────────────────────────

/// Errors a [`Stage2ExecutionClient`] may return. Production senders
/// land behind this trait without changing the executor's call site.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum Stage2ExecutionError {
    /// Request shape was rejected (missing payload, action_type
    /// mismatch, zero amount). The executor caller produced a request
    /// the client can't dispatch.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Send / broadcast path failed before the network produced a
    /// signature. Production sender translates RPC errors here.
    #[error("send failed: {0}")]
    SendFailed(String),

    /// Send succeeded but the network did not confirm in time.
    #[error("confirmation failed: {0}")]
    ConfirmationFailed(String),

    /// Anything else inside the client.
    #[error("client internal: {0}")]
    Internal(String),
}

/// Pluggable execution client. The W4-lite default is
/// [`MockExecutionClient`]; production senders implement this trait.
///
/// # Contract
///
/// - `Ok(_)` — the action was dispatched AND confirmed by the client.
///   Executor moves the rule to `completed`.
/// - `Err(_)` — anything else. Executor moves the rule to `failed`.
///   W4-lite does NOT distinguish transient vs terminal at this seam;
///   one-shot v1 retires the rule either way.
#[async_trait]
pub trait Stage2ExecutionClient: Send + Sync + std::fmt::Debug {
    async fn send_and_confirm(
        &self,
        request: Stage2ExecuteActionRequest,
    ) -> Result<Stage2ExecutionReceipt, Stage2ExecutionError>;
}

// ── Strongly typed request / receipt ────────────────────────────────────────

/// Strongly typed off-chain projection of the `ExecuteAction` ix the
/// executor wants the client to dispatch.
///
/// Field naming mirrors the on-chain `AuthorityInstruction::ExecuteAction`
/// variant in
/// [`clawsol-authority::instruction`](../../../programs/clawsol-authority/src/instruction.rs),
/// so a future production sender can translate this struct into the
/// Borsh-serialised ix without re-resolving terminology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage2ExecuteActionRequest {
    pub rule_id: [u8; 16],
    pub canonical_rule_hash: [u8; 32],
    pub action_type: WatchRuleActionType,
    /// On-chain `u8` discriminator (`Stage2ActionType::to_u8`).
    pub action_type_byte: u8,
    pub schema_version: u8,
    /// Bounded by the rule's remaining headroom
    /// (`max_input_amount_raw - used_amount_raw`).
    pub input_amount_raw: u64,
    /// Replay-protection nonce. Stage 2 nonces are monotone per rule
    /// and start at 1.
    pub execution_nonce: u64,
    pub user: PubkeyBytes,
    pub executor: PubkeyBytes,
    pub delegated_wallet: PubkeyBytes,
    pub destination: PubkeyBytes,
    pub expires_at_slot: u64,
    /// `Some(_)` iff `action_type == SolendWithdrawAllDelegated`.
    pub solend: Option<Stage2SolendExecutePayload>,
}

/// Solend-specific account / oracle bundle, materialised from the
/// rule's `ActionSpec::SolendWithdrawAllDelegated` + the bound
/// [`DemoSolendExecutionFixture`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage2SolendExecutePayload {
    pub target_obligation: PubkeyBytes,
    pub reserve: PubkeyBytes,
    pub lending_market: PubkeyBytes,
    pub liquidity_mint: PubkeyBytes,
    pub ctoken_mint: PubkeyBytes,
    pub pyth_oracle: PubkeyBytes,
    pub switchboard_oracle: PubkeyBytes,
    /// Coerced to `None` for the demo tuple — `MAINNET_BETA_DEMO_USDC_TUPLE`
    /// has no extra oracle (P5c § extra_oracle semantics).
    pub extra_oracle: Option<PubkeyBytes>,
    pub withdraw_mode: WithdrawMode,
}

/// Confirmation receipt returned by a successful client dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage2ExecutionReceipt {
    pub rule_id: [u8; 16],
    pub execution_nonce: u64,
    /// Slot the client observed the tx confirmed at. The executor
    /// passes this through to `mark_completed(... slot)`.
    pub confirmation_slot: u64,
    /// Amount the client actually consumed. Production senders read
    /// this from a post-tx balance bracket; the mock just echoes back.
    pub used_amount_raw: u64,
    /// Production senders return the base58 tx signature here. The
    /// mock returns a synthetic sentinel.
    pub signature_sentinel: String,
}

// ── Mock execution client ───────────────────────────────────────────────────

/// Scripted outcome for [`MockExecutionClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockOutcome {
    Success {
        used_amount_raw: u64,
        confirmation_slot: u64,
        signature_sentinel: String,
    },
    Failure(Stage2ExecutionError),
}

/// Default execution client for W4-lite. Validates the request shape
/// then pops a pre-queued outcome.
#[derive(Debug, Default)]
pub struct MockExecutionClient {
    outcomes: Mutex<VecDeque<MockOutcome>>,
    received: Mutex<Vec<Stage2ExecuteActionRequest>>,
}

impl MockExecutionClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience: one queued success that echoes back the request's
    /// `input_amount_raw` as `used_amount_raw`, at slot 0, with a
    /// sentinel signature.
    pub fn with_success_default() -> Self {
        let m = Self::new();
        m.push_success_echo("mock-sig-default");
        m
    }

    pub fn push_success(
        &self,
        used_amount_raw: u64,
        confirmation_slot: u64,
        signature_sentinel: &str,
    ) {
        self.outcomes.lock().push_back(MockOutcome::Success {
            used_amount_raw,
            confirmation_slot,
            signature_sentinel: signature_sentinel.to_string(),
        });
    }

    /// Sentinel "echo" success — the receipt's `used_amount_raw` is
    /// filled at dispatch time from the request, so the test does not
    /// need to know the exact amount up front.
    pub fn push_success_echo(&self, signature_sentinel: &str) {
        self.outcomes.lock().push_back(MockOutcome::Success {
            used_amount_raw: u64::MAX, // sentinel — replaced below
            confirmation_slot: 0,
            signature_sentinel: signature_sentinel.to_string(),
        });
    }

    pub fn push_failure(&self, err: Stage2ExecutionError) {
        self.outcomes.lock().push_back(MockOutcome::Failure(err));
    }

    pub fn received(&self) -> Vec<Stage2ExecuteActionRequest> {
        self.received.lock().clone()
    }

    pub fn call_count(&self) -> usize {
        self.received.lock().len()
    }
}

#[async_trait]
impl Stage2ExecutionClient for MockExecutionClient {
    async fn send_and_confirm(
        &self,
        request: Stage2ExecuteActionRequest,
    ) -> Result<Stage2ExecutionReceipt, Stage2ExecutionError> {
        validate_request_shape(&request)?;

        let outcome = self.outcomes.lock().pop_front().ok_or_else(|| {
            Stage2ExecutionError::Internal(
                "MockExecutionClient: no outcomes queued".to_string(),
            )
        })?;

        self.received.lock().push(request.clone());

        match outcome {
            MockOutcome::Success {
                used_amount_raw,
                confirmation_slot,
                signature_sentinel,
            } => {
                // Echo sentinel — if a test pushed the sentinel value,
                // substitute the request's input amount.
                let used = if used_amount_raw == u64::MAX {
                    request.input_amount_raw
                } else {
                    used_amount_raw
                };
                Ok(Stage2ExecutionReceipt {
                    rule_id: request.rule_id,
                    execution_nonce: request.execution_nonce,
                    confirmation_slot,
                    used_amount_raw: used,
                    signature_sentinel,
                })
            }
            MockOutcome::Failure(e) => Err(e),
        }
    }
}

/// Structural validation. Runs inside [`MockExecutionClient`] and is
/// available as a free helper so a future production client can call
/// it before paying the RPC cost of a doomed send.
pub fn validate_request_shape(
    request: &Stage2ExecuteActionRequest,
) -> Result<(), Stage2ExecutionError> {
    if request.input_amount_raw == 0 {
        return Err(Stage2ExecutionError::InvalidRequest(
            "input_amount_raw must be > 0".to_string(),
        ));
    }
    if request.execution_nonce == 0 {
        return Err(Stage2ExecutionError::InvalidRequest(
            "execution_nonce must be > 0 (Stage 2 nonces start at 1)".to_string(),
        ));
    }
    if request.action_type.to_u8() != request.action_type_byte {
        return Err(Stage2ExecutionError::InvalidRequest(
            "action_type / action_type_byte mismatch".to_string(),
        ));
    }
    match request.action_type {
        WatchRuleActionType::SolendWithdrawAllDelegated => {
            if request.solend.is_none() {
                return Err(Stage2ExecutionError::InvalidRequest(
                    "Solend action_type requires solend payload".to_string(),
                ));
            }
        }
        WatchRuleActionType::JupiterBuySolWithUsdc => {
            return Err(Stage2ExecutionError::InvalidRequest(
                "W4-lite execution client does not handle Jupiter action_type"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

// ── Demo Solend fixture ─────────────────────────────────────────────────────

/// Off-chain mirror of `MAINNET_BETA_DEMO_USDC_TUPLE` from
/// `programs/clawsol-authority/src/solend_cpi_builder.rs`.
///
/// Re-pinned here as base58 strings so the gateway crate avoids a
/// build-graph dependency on the BPF crate. A parity test asserts
/// the round-trip stability of every field; a divergence from the
/// program-side const surfaces loudly the next time tests run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoSolendExecutionFixture {
    reserve: PubkeyBytes,
    lending_market: PubkeyBytes,
    liquidity_mint: PubkeyBytes,
    ctoken_mint: PubkeyBytes,
    pyth_oracle: PubkeyBytes,
    switchboard_oracle: PubkeyBytes,
    extra_oracle: Option<PubkeyBytes>,
}

/// Base58-pinned demo tuple. Source of truth:
/// `programs/clawsol-authority/src/solend_cpi_builder.rs::MAINNET_BETA_DEMO_USDC_TUPLE`.
pub const DEMO_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
pub const DEMO_LENDING_MARKET_BS58: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
pub const DEMO_LIQUIDITY_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const DEMO_CTOKEN_MINT_BS58: &str = "9n4nbM75f5Ui33ZbPYXn59EwSgE8CGsHtAeTH5YFeJ9E";
pub const DEMO_PYTH_ORACLE_BS58: &str = "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX";
pub const DEMO_SWITCHBOARD_ORACLE_BS58: &str = "nu11111111111111111111111111111111111111111";

impl DemoSolendExecutionFixture {
    /// The single mainnet-beta USDC tuple authorised by P5c.
    pub fn mainnet_beta_demo_usdc() -> Self {
        Self {
            reserve: pk_const(DEMO_RESERVE_BS58),
            lending_market: pk_const(DEMO_LENDING_MARKET_BS58),
            liquidity_mint: pk_const(DEMO_LIQUIDITY_MINT_BS58),
            ctoken_mint: pk_const(DEMO_CTOKEN_MINT_BS58),
            pyth_oracle: pk_const(DEMO_PYTH_ORACLE_BS58),
            switchboard_oracle: pk_const(DEMO_SWITCHBOARD_ORACLE_BS58),
            // The demo tuple has no extra oracle. P5c verifier coerces
            // `Option::None` ↔ `Pubkey::default()` for the equality
            // check; we pass `None` and let the production sender (or
            // the mock) translate when building the on-chain ix.
            extra_oracle: None,
        }
    }

    pub fn reserve(&self) -> &PubkeyBytes {
        &self.reserve
    }

    pub fn lending_market(&self) -> &PubkeyBytes {
        &self.lending_market
    }

    pub fn pyth_oracle(&self) -> &PubkeyBytes {
        &self.pyth_oracle
    }

    pub fn ctoken_mint(&self) -> &PubkeyBytes {
        &self.ctoken_mint
    }

    /// Build a Solend execute payload for a rule whose action matches
    /// this fixture. Returns `DemoFixtureMismatch` if the rule's
    /// reserve or lending market disagrees, and `UnsupportedActionType`
    /// if the action is not Solend.
    pub fn build_payload(
        &self,
        action: &ActionSpec,
    ) -> Result<Stage2SolendExecutePayload, Stage2ExecutorError> {
        let (target_obligation, reserve_pubkey, lending_market, withdraw_mode) =
            match action {
                ActionSpec::SolendWithdrawAllDelegated {
                    target_obligation,
                    reserve_pubkey,
                    lending_market,
                    destination_wallet: _,
                    withdraw_mode,
                } => (target_obligation, reserve_pubkey, lending_market, withdraw_mode),
                other => {
                    return Err(Stage2ExecutorError::UnsupportedActionType(
                        other.action_type(),
                    ));
                }
            };

        if reserve_pubkey != &self.reserve {
            return Err(Stage2ExecutorError::DemoFixtureMismatch(format!(
                "rule.reserve {} != demo.reserve {}",
                reserve_pubkey.to_base58(),
                self.reserve.to_base58(),
            )));
        }
        if lending_market != &self.lending_market {
            return Err(Stage2ExecutorError::DemoFixtureMismatch(format!(
                "rule.lending_market {} != demo.lending_market {}",
                lending_market.to_base58(),
                self.lending_market.to_base58(),
            )));
        }

        Ok(Stage2SolendExecutePayload {
            target_obligation: *target_obligation,
            reserve: self.reserve,
            lending_market: self.lending_market,
            liquidity_mint: self.liquidity_mint,
            ctoken_mint: self.ctoken_mint,
            pyth_oracle: self.pyth_oracle,
            switchboard_oracle: self.switchboard_oracle,
            extra_oracle: self.extra_oracle,
            withdraw_mode: *withdraw_mode,
        })
    }
}

fn pk_const(s: &str) -> PubkeyBytes {
    PubkeyBytes::from_base58(s).expect("demo fixture base58 must parse at init")
}

// ── Config + reports ────────────────────────────────────────────────────────

/// Executor configuration. Mirrors `Stage2WatcherConfig` style.
#[derive(Debug, Clone)]
pub struct Stage2ExecutorConfig {
    /// Maximum `condition_met` rules attempted per tick. Default 64;
    /// W4-lite is bounded so a backlog of ready rules can never produce
    /// an unbounded burst of mock dispatches.
    pub max_rules_per_tick: u32,

    /// Allowlist of `ActionSpec` variants this executor handles. The
    /// W4-lite default is `[SolendWithdrawAllDelegated]`. Jupiter rules
    /// are routed through a separate (future) executor.
    pub action_type_allowlist: Vec<WatchRuleActionType>,
}

impl Default for Stage2ExecutorConfig {
    fn default() -> Self {
        Self {
            max_rules_per_tick: 64,
            action_type_allowlist: vec![WatchRuleActionType::SolendWithdrawAllDelegated],
        }
    }
}

/// Per-rule outcome of a single executor pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage2ExecutorRuleResult {
    Completed {
        rule_id: [u8; 16],
        execution_nonce: u64,
        used_amount_raw: u64,
        confirmation_slot: u64,
        signature_sentinel: String,
    },
    Failed {
        rule_id: [u8; 16],
        error: String,
    },
    LeaseLost {
        rule_id: [u8; 16],
    },
    SkippedActionType {
        rule_id: [u8; 16],
        action_type: WatchRuleActionType,
    },
    DemoFixtureMismatch {
        rule_id: [u8; 16],
        detail: String,
    },
    InternalError {
        rule_id: [u8; 16],
        detail: String,
    },
}

/// Aggregated tick report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stage2ExecutorTickReport {
    pub rules_loaded: u32,
    pub rules_processed: u32,
    pub completed_count: u32,
    pub failed_count: u32,
    pub lease_lost_count: u32,
    pub skipped_action_type_count: u32,
    pub demo_mismatch_count: u32,
    pub internal_error_count: u32,
    pub per_rule: Vec<Stage2ExecutorRuleResult>,
}

// ── Executor ────────────────────────────────────────────────────────────────

/// Stage 2 executor over the W1 state-store + an injected execution
/// client + a bound demo Solend fixture.
#[derive(Debug)]
pub struct Stage2Executor {
    repo: Stage2WatchRuleRepository,
    client: Arc<dyn Stage2ExecutionClient>,
    demo_fixture: DemoSolendExecutionFixture,
    config: Stage2ExecutorConfig,
    clock: Arc<dyn Stage2Clock>,
    in_flight: Arc<Mutex<HashSet<[u8; 16]>>>,
    schema_version: u8,
}

impl Stage2Executor {
    /// Construct with defaults — bound to the mainnet-beta USDC demo
    /// tuple and the system clock.
    pub fn new(
        repo: Stage2WatchRuleRepository,
        client: Arc<dyn Stage2ExecutionClient>,
    ) -> Self {
        Self::with_components(
            repo,
            client,
            DemoSolendExecutionFixture::mainnet_beta_demo_usdc(),
            Arc::new(SystemClock),
            Stage2ExecutorConfig::default(),
        )
    }

    pub fn with_components(
        repo: Stage2WatchRuleRepository,
        client: Arc<dyn Stage2ExecutionClient>,
        demo_fixture: DemoSolendExecutionFixture,
        clock: Arc<dyn Stage2Clock>,
        config: Stage2ExecutorConfig,
    ) -> Self {
        Self {
            repo,
            client,
            demo_fixture,
            config,
            clock,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
        }
    }

    pub fn config(&self) -> &Stage2ExecutorConfig {
        &self.config
    }

    pub fn demo_fixture(&self) -> &DemoSolendExecutionFixture {
        &self.demo_fixture
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.lock().len()
    }

    /// Run one executor pass: load `condition_met` rules, lease them,
    /// dispatch through the client, write back terminal state.
    pub async fn execute_ready_rules_once(
        &self,
        ctx: Stage2TickContext,
    ) -> Stage2ExecutorTickReport {
        debug!(
            current_slot = ctx.current_slot,
            now_ms = ctx.now_ms,
            "stage2 executor tick start"
        );
        let mut report = Stage2ExecutorTickReport::default();

        let rules = match self
            .repo
            .list_pending_lifecycle_limit(self.config.max_rules_per_tick)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                report.internal_error_count += 1;
                report.per_rule.push(Stage2ExecutorRuleResult::InternalError {
                    rule_id: [0; 16],
                    detail: format!("list_pending_lifecycle_limit: {e}"),
                });
                return report;
            }
        };

        let ready: Vec<StoredWatchRule> = rules
            .into_iter()
            .filter(|r| r.status == WatchRuleStatus::ConditionMet)
            .collect();

        report.rules_loaded = ready.len() as u32;

        for stored in ready {
            let result = self.process_rule(stored, &ctx).await;
            self.tally_result(&result, &mut report);
            report.per_rule.push(result);
            report.rules_processed += 1;
        }

        info!(
            rules_loaded = report.rules_loaded,
            processed = report.rules_processed,
            completed = report.completed_count,
            failed = report.failed_count,
            lease_lost = report.lease_lost_count,
            skipped_action_type = report.skipped_action_type_count,
            demo_mismatch = report.demo_mismatch_count,
            internal = report.internal_error_count,
            "stage2 executor tick finished"
        );
        report
    }

    /// Targeted variant — process a single rule by id. Returns
    /// [`Stage2ExecutorRuleResult::LeaseLost`] if the rule is not in
    /// `condition_met` (e.g. already executing, completed, failed,
    /// expired, revoked, or still in `active`).
    pub async fn execute_rule_once(
        &self,
        rule_id: [u8; 16],
        ctx: Stage2TickContext,
    ) -> Stage2ExecutorRuleResult {
        let stored = match self.repo.get(&rule_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Stage2ExecutorRuleResult::InternalError {
                    rule_id,
                    detail: "rule not found".to_string(),
                };
            }
            Err(e) => {
                return Stage2ExecutorRuleResult::InternalError {
                    rule_id,
                    detail: format!("repo.get: {e}"),
                };
            }
        };
        if stored.status != WatchRuleStatus::ConditionMet {
            return Stage2ExecutorRuleResult::LeaseLost { rule_id };
        }
        self.process_rule(stored, &ctx).await
    }

    /// Build a strongly typed execute action request from a stored rule.
    /// Does not touch the state-store. Used internally by
    /// [`Self::process_rule`]; exposed publicly so callers can preview
    /// the wire shape (audit, dashboard, dry-run).
    pub fn build_execute_action_request(
        &self,
        stored: &StoredWatchRule,
        execution_nonce: u64,
    ) -> Result<Stage2ExecuteActionRequest, Stage2ExecutorError> {
        let rule = &stored.rule;
        let action_type = rule.action.action_type();
        if !self.config.action_type_allowlist.contains(&action_type) {
            return Err(Stage2ExecutorError::UnsupportedActionType(action_type));
        }

        // Headroom from the on-rule cap, not from the state-store
        // bookkeeping column, because the rule's own `used_amount_raw`
        // is what gets re-serialized into the canonical bytes the
        // PDA's `canonical_rule_hash` was bound against.
        let max = rule.max_input_amount_raw;
        let used = rule.used_amount_raw;
        if used >= max {
            return Err(Stage2ExecutorError::InputHeadroomExhausted { used, max });
        }
        let input_amount_raw = max - used;

        let solend = match &rule.action {
            ActionSpec::SolendWithdrawAllDelegated { .. } => {
                Some(self.demo_fixture.build_payload(&rule.action)?)
            }
            ActionSpec::JupiterBuySolWithUsdc { .. } => None,
        };

        Ok(Stage2ExecuteActionRequest {
            rule_id: rule.rule_id,
            canonical_rule_hash: stored.canonical_rule_hash,
            action_type,
            action_type_byte: action_type.to_u8(),
            schema_version: self.schema_version,
            input_amount_raw,
            execution_nonce,
            user: rule.user,
            executor: rule.executor,
            delegated_wallet: rule.delegated_wallet,
            destination: rule.destination,
            expires_at_slot: rule.expires_at_slot,
            solend,
        })
    }

    async fn process_rule(
        &self,
        stored: StoredWatchRule,
        _ctx: &Stage2TickContext,
    ) -> Stage2ExecutorRuleResult {
        let rule_id = stored.rule.rule_id;

        // 1. action_type allowlist guard.
        let action_type = stored.rule.action.action_type();
        if !self.config.action_type_allowlist.contains(&action_type) {
            debug!(
                rule_id = %hex_id(&rule_id),
                action_type = ?action_type,
                "skipped: action_type not in allowlist"
            );
            return Stage2ExecutorRuleResult::SkippedActionType {
                rule_id,
                action_type,
            };
        }

        // 2. Same-process in-flight guard. Cheap; protects within one
        //    daemon instance against a concurrent tick frame picking up
        //    the same `condition_met` row before the CAS commits.
        let _guard = match try_acquire_in_flight(&self.in_flight, rule_id) {
            Some(g) => g,
            None => {
                debug!(
                    rule_id = %hex_id(&rule_id),
                    "skipped: already in-flight in this process"
                );
                return Stage2ExecutorRuleResult::LeaseLost { rule_id };
            }
        };

        // 3. Cross-process CAS lease. The next nonce is monotone per
        //    rule (state-store column `execution_nonce`, starts at 0).
        let next_nonce = stored.execution_nonce.saturating_add(1);
        match self
            .repo
            .mark_executing_if_condition_met(&rule_id, next_nonce)
            .await
        {
            Ok(0) => {
                debug!(
                    rule_id = %hex_id(&rule_id),
                    "lease lost: rule not in condition_met (race or already executed)"
                );
                return Stage2ExecutorRuleResult::LeaseLost { rule_id };
            }
            Ok(_) => {}
            Err(e) => {
                return Stage2ExecutorRuleResult::InternalError {
                    rule_id,
                    detail: format!("mark_executing_if_condition_met: {e}"),
                };
            }
        }

        // 4. Build the strongly typed request. Errors here are
        //    terminal — the rule has been leased, we cannot leave it
        //    in `executing` indefinitely.
        let request = match self.build_execute_action_request(&stored, next_nonce) {
            Ok(r) => r,
            Err(Stage2ExecutorError::DemoFixtureMismatch(detail)) => {
                self.mark_failed_best_effort(&rule_id, &detail).await;
                return Stage2ExecutorRuleResult::DemoFixtureMismatch {
                    rule_id,
                    detail,
                };
            }
            Err(Stage2ExecutorError::UnsupportedActionType(at)) => {
                let detail = format!("unsupported action_type: {at:?}");
                self.mark_failed_best_effort(&rule_id, &detail).await;
                return Stage2ExecutorRuleResult::SkippedActionType {
                    rule_id,
                    action_type: at,
                };
            }
            Err(Stage2ExecutorError::InputHeadroomExhausted { used, max }) => {
                let detail = format!("input headroom exhausted: used={used} max={max}");
                self.mark_failed_best_effort(&rule_id, &detail).await;
                return Stage2ExecutorRuleResult::Failed {
                    rule_id,
                    error: detail,
                };
            }
            Err(other) => {
                let detail = other.to_string();
                self.mark_failed_best_effort(&rule_id, &detail).await;
                return Stage2ExecutorRuleResult::Failed {
                    rule_id,
                    error: detail,
                };
            }
        };

        // 5. Dispatch.
        let now_ms_per_rule = self.clock.now_ms();
        debug!(
            rule_id = %hex_id(&rule_id),
            execution_nonce = next_nonce,
            input_amount_raw = request.input_amount_raw,
            now_ms = now_ms_per_rule,
            "stage2 executor dispatching to execution client"
        );

        let receipt_result = self.client.send_and_confirm(request).await;

        // 6. Terminal write-back.
        match receipt_result {
            Ok(receipt) => {
                if let Err(e) = self
                    .repo
                    .mark_completed(
                        &rule_id,
                        receipt.used_amount_raw,
                        receipt.confirmation_slot,
                    )
                    .await
                {
                    return Stage2ExecutorRuleResult::InternalError {
                        rule_id,
                        detail: format!("mark_completed: {e}"),
                    };
                }
                info!(
                    rule_id = %hex_id(&rule_id),
                    execution_nonce = receipt.execution_nonce,
                    confirmation_slot = receipt.confirmation_slot,
                    used_amount_raw = receipt.used_amount_raw,
                    "stage2 executor: rule completed"
                );
                Stage2ExecutorRuleResult::Completed {
                    rule_id,
                    execution_nonce: receipt.execution_nonce,
                    used_amount_raw: receipt.used_amount_raw,
                    confirmation_slot: receipt.confirmation_slot,
                    signature_sentinel: receipt.signature_sentinel,
                }
            }
            Err(e) => {
                let detail = e.to_string();
                self.mark_failed_best_effort(&rule_id, &detail).await;
                warn!(
                    rule_id = %hex_id(&rule_id),
                    error = %detail,
                    "stage2 executor: rule failed; no retry (one-shot v1)"
                );
                Stage2ExecutorRuleResult::Failed {
                    rule_id,
                    error: detail,
                }
            }
        }
    }

    async fn mark_failed_best_effort(&self, rule_id: &[u8; 16], detail: &str) {
        // Use the non-CAS `mark_failed` so a row in `executing` (post
        // lease) is moved to `failed`. `mark_failed_if_not_terminal`'s
        // WHERE only matches active/condition_met, which would leave
        // an executing row stuck.
        if let Err(e) = self.repo.mark_failed(rule_id, detail).await {
            warn!(
                rule_id = %hex_id(rule_id),
                error = %e,
                "mark_failed after executor terminal error"
            );
        }
    }

    fn tally_result(
        &self,
        result: &Stage2ExecutorRuleResult,
        report: &mut Stage2ExecutorTickReport,
    ) {
        match result {
            Stage2ExecutorRuleResult::Completed { .. } => report.completed_count += 1,
            Stage2ExecutorRuleResult::Failed { .. } => report.failed_count += 1,
            Stage2ExecutorRuleResult::LeaseLost { .. } => report.lease_lost_count += 1,
            Stage2ExecutorRuleResult::SkippedActionType { .. } => {
                report.skipped_action_type_count += 1
            }
            Stage2ExecutorRuleResult::DemoFixtureMismatch { .. } => {
                report.demo_mismatch_count += 1
            }
            Stage2ExecutorRuleResult::InternalError { .. } => {
                report.internal_error_count += 1
            }
        }
    }
}

// ── In-flight guard (same-process race protection) ──────────────────────────

struct InFlightGuard {
    set: Arc<Mutex<HashSet<[u8; 16]>>>,
    rule_id: [u8; 16],
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set.lock().remove(&self.rule_id);
    }
}

fn try_acquire_in_flight(
    set: &Arc<Mutex<HashSet<[u8; 16]>>>,
    rule_id: [u8; 16],
) -> Option<InFlightGuard> {
    let mut guard = set.lock();
    if guard.contains(&rule_id) {
        None
    } else {
        guard.insert(rule_id);
        Some(InFlightGuard {
            set: set.clone(),
            rule_id,
        })
    }
}

fn hex_id(rule_id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in rule_id {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use claw_state_store::db::Database;
    use claw_types::stage2_watch_rule::{
        Comparison, Condition, ConditionLogic, JupiterApiVersion, RateKind,
        VerificationLevel, WatchRule, WithdrawMode, BoundMode,
    };

    const DEMO_USER: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WSOL_MINT_BS58: &str = "So11111111111111111111111111111111111111112";

    /// MockClock for tests — pinned `now_ms`.
    #[derive(Debug)]
    struct PinnedClock(i64);

    impl Stage2Clock for PinnedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    async fn test_repo() -> (Database, Stage2WatchRuleRepository) {
        let db = Database::open_in_memory()
            .await
            .expect("in-memory DB opens");
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        (db, repo)
    }

    fn pk_b(b: u8) -> PubkeyBytes {
        PubkeyBytes::new([b; 32])
    }

    fn pk_str(s: &str) -> PubkeyBytes {
        PubkeyBytes::from_base58(s).expect("test base58 pubkey parses")
    }

    /// A Solend rule whose action targets the demo tuple's reserve +
    /// lending market — i.e. the "happy path" rule the executor should
    /// successfully dispatch.
    fn demo_matching_solend_rule(rule_id: [u8; 16]) -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id,
            user: pk_str(DEMO_USER),
            executor: pk_b(0xEE),
            delegated_wallet: pk_b(0xBB),
            created_at_slot: 415_500_000,
            expires_at_slot: 415_700_000,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![Condition::SolendReserveSupplyRate {
                reserve_pubkey: pk_str(DEMO_RESERVE_BS58),
                lending_market: pk_str(DEMO_LENDING_MARKET_BS58),
                solend_program_id: pk_str(SOLEND_PROGRAM_ID_BS58),
                comparison: Comparison::Lt,
                threshold_bps: 1_000,
                rate_kind: RateKind::Apr,
                formula_version: 1,
                max_reserve_staleness_slots: 16,
                required_refresh_same_tx: true,
            }],
            action: ActionSpec::SolendWithdrawAllDelegated {
                target_obligation: pk_b(0x10),
                reserve_pubkey: pk_str(DEMO_RESERVE_BS58),
                lending_market: pk_str(DEMO_LENDING_MARKET_BS58),
                destination_wallet: pk_str(DEMO_USER),
                withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
            },
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk_str(DEMO_USER),
            slippage_bps: 0,
        }
    }

    /// A Solend rule whose reserve disagrees with the demo tuple —
    /// must fail the `DemoFixtureMismatch` gate.
    fn demo_mismatching_solend_rule(rule_id: [u8; 16]) -> WatchRule {
        let mut r = demo_matching_solend_rule(rule_id);
        // Replace reserve_pubkey on both the action and the condition
        // so the rule is internally consistent (mismatch is between
        // rule and fixture, not within the rule).
        let bogus_reserve = pk_b(0x42);
        let bogus_lm = pk_str(DEMO_LENDING_MARKET_BS58);
        r.action = ActionSpec::SolendWithdrawAllDelegated {
            target_obligation: pk_b(0x10),
            reserve_pubkey: bogus_reserve,
            lending_market: bogus_lm,
            destination_wallet: pk_str(DEMO_USER),
            withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
        };
        r
    }

    /// A Jupiter rule — must hit the `action_type_allowlist` guard.
    fn jupiter_rule(rule_id: [u8; 16]) -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id,
            user: pk_str(DEMO_USER),
            executor: pk_b(0xEE),
            delegated_wallet: pk_b(0xBB),
            created_at_slot: 415_500_000,
            expires_at_slot: 415_700_000,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![Condition::PythPrice {
                feed_id: [0xAA; 32],
                price_update_account: pk_b(0x99),
                comparison: Comparison::Gt,
                threshold_mantissa: 9_000,
                threshold_exponent: -2,
                max_age_seconds: 30,
                max_confidence_bps: 50,
                verification_level_required: VerificationLevel::Full,
                bound_mode: BoundMode::AdverseLowerForGt,
            }],
            action: ActionSpec::JupiterBuySolWithUsdc {
                input_mint: pk_str(USDC_MINT_BS58),
                output_mint: pk_str(WSOL_MINT_BS58),
                input_amount_raw: 1_000_000,
                min_output_amount_raw: None,
                jupiter_api_version: JupiterApiVersion::V2,
                max_accounts_hint: 48,
                require_pre_post_bracket: true,
            },
            max_input_amount_raw: 1_000_000,
            used_amount_raw: 0,
            destination: pk_str(DEMO_USER),
            slippage_bps: 50,
        }
    }

    fn ctx_at(slot: u64, now_ms: i64) -> Stage2TickContext {
        Stage2TickContext::new(slot, now_ms / 1000, now_ms)
    }

    // ── Demo fixture parity ─────────────────────────────────────────────

    #[test]
    fn demo_fixture_pinned_pubkeys_roundtrip_to_themselves() {
        let f = DemoSolendExecutionFixture::mainnet_beta_demo_usdc();
        assert_eq!(f.reserve().to_base58(), DEMO_RESERVE_BS58);
        assert_eq!(f.lending_market().to_base58(), DEMO_LENDING_MARKET_BS58);
        assert_eq!(f.pyth_oracle().to_base58(), DEMO_PYTH_ORACLE_BS58);
        assert_eq!(f.ctoken_mint().to_base58(), DEMO_CTOKEN_MINT_BS58);
        // Extra fields not surfaced via getter — assert by constructing
        // the fixture and projecting the private fields via `build_payload`
        // on a known-good action.
        let action = ActionSpec::SolendWithdrawAllDelegated {
            target_obligation: pk_b(0x10),
            reserve_pubkey: pk_str(DEMO_RESERVE_BS58),
            lending_market: pk_str(DEMO_LENDING_MARKET_BS58),
            destination_wallet: pk_str(DEMO_USER),
            withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
        };
        let payload = f.build_payload(&action).unwrap();
        assert_eq!(payload.liquidity_mint.to_base58(), DEMO_LIQUIDITY_MINT_BS58);
        assert_eq!(
            payload.switchboard_oracle.to_base58(),
            DEMO_SWITCHBOARD_ORACLE_BS58
        );
        assert!(payload.extra_oracle.is_none());
    }

    #[test]
    fn demo_fixture_rejects_reserve_mismatch() {
        let f = DemoSolendExecutionFixture::mainnet_beta_demo_usdc();
        let action = ActionSpec::SolendWithdrawAllDelegated {
            target_obligation: pk_b(0x10),
            reserve_pubkey: pk_b(0x42), // bogus
            lending_market: pk_str(DEMO_LENDING_MARKET_BS58),
            destination_wallet: pk_str(DEMO_USER),
            withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
        };
        let err = f.build_payload(&action).unwrap_err();
        assert!(
            matches!(err, Stage2ExecutorError::DemoFixtureMismatch(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn demo_fixture_rejects_lending_market_mismatch() {
        let f = DemoSolendExecutionFixture::mainnet_beta_demo_usdc();
        let action = ActionSpec::SolendWithdrawAllDelegated {
            target_obligation: pk_b(0x10),
            reserve_pubkey: pk_str(DEMO_RESERVE_BS58),
            lending_market: pk_b(0x77), // bogus
            destination_wallet: pk_str(DEMO_USER),
            withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
        };
        let err = f.build_payload(&action).unwrap_err();
        assert!(
            matches!(err, Stage2ExecutorError::DemoFixtureMismatch(_)),
            "got {err:?}"
        );
    }

    // ── Request shape validation ────────────────────────────────────────

    #[test]
    fn request_shape_rejects_zero_amount() {
        let req = Stage2ExecuteActionRequest {
            rule_id: [0xD0; 16],
            canonical_rule_hash: [0; 32],
            action_type: WatchRuleActionType::SolendWithdrawAllDelegated,
            action_type_byte: 1,
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            input_amount_raw: 0, // poison
            execution_nonce: 1,
            user: pk_str(DEMO_USER),
            executor: pk_b(0xEE),
            delegated_wallet: pk_b(0xBB),
            destination: pk_str(DEMO_USER),
            expires_at_slot: 1,
            solend: Some(dummy_solend_payload()),
        };
        let err = validate_request_shape(&req).unwrap_err();
        assert_eq!(
            err,
            Stage2ExecutionError::InvalidRequest("input_amount_raw must be > 0".to_string()),
        );
    }

    #[test]
    fn request_shape_rejects_zero_nonce() {
        let mut req = good_solend_request();
        req.execution_nonce = 0;
        let err = validate_request_shape(&req).unwrap_err();
        assert!(matches!(err, Stage2ExecutionError::InvalidRequest(_)));
    }

    #[test]
    fn request_shape_rejects_solend_without_payload() {
        let mut req = good_solend_request();
        req.solend = None;
        let err = validate_request_shape(&req).unwrap_err();
        assert!(matches!(err, Stage2ExecutionError::InvalidRequest(_)));
    }

    #[test]
    fn request_shape_rejects_jupiter() {
        let mut req = good_solend_request();
        req.action_type = WatchRuleActionType::JupiterBuySolWithUsdc;
        req.action_type_byte = WatchRuleActionType::JupiterBuySolWithUsdc.to_u8();
        req.solend = None;
        let err = validate_request_shape(&req).unwrap_err();
        assert!(matches!(err, Stage2ExecutionError::InvalidRequest(_)));
    }

    #[test]
    fn request_shape_rejects_action_type_byte_mismatch() {
        let mut req = good_solend_request();
        req.action_type_byte = 2; // disagrees with action_type
        let err = validate_request_shape(&req).unwrap_err();
        assert!(matches!(err, Stage2ExecutionError::InvalidRequest(_)));
    }

    fn good_solend_request() -> Stage2ExecuteActionRequest {
        Stage2ExecuteActionRequest {
            rule_id: [0xD0; 16],
            canonical_rule_hash: [0; 32],
            action_type: WatchRuleActionType::SolendWithdrawAllDelegated,
            action_type_byte: 1,
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            input_amount_raw: 5_000_000,
            execution_nonce: 1,
            user: pk_str(DEMO_USER),
            executor: pk_b(0xEE),
            delegated_wallet: pk_b(0xBB),
            destination: pk_str(DEMO_USER),
            expires_at_slot: 415_700_000,
            solend: Some(dummy_solend_payload()),
        }
    }

    fn dummy_solend_payload() -> Stage2SolendExecutePayload {
        Stage2SolendExecutePayload {
            target_obligation: pk_b(0x10),
            reserve: pk_str(DEMO_RESERVE_BS58),
            lending_market: pk_str(DEMO_LENDING_MARKET_BS58),
            liquidity_mint: pk_str(DEMO_LIQUIDITY_MINT_BS58),
            ctoken_mint: pk_str(DEMO_CTOKEN_MINT_BS58),
            pyth_oracle: pk_str(DEMO_PYTH_ORACLE_BS58),
            switchboard_oracle: pk_str(DEMO_SWITCHBOARD_ORACLE_BS58),
            extra_oracle: None,
            withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
        }
    }

    // ── Executor: happy path ────────────────────────────────────────────

    fn executor_with_mock(
        repo: Stage2WatchRuleRepository,
        mock: Arc<MockExecutionClient>,
    ) -> Stage2Executor {
        Stage2Executor::with_components(
            repo,
            mock as Arc<dyn Stage2ExecutionClient>,
            DemoSolendExecutionFixture::mainnet_beta_demo_usdc(),
            Arc::new(PinnedClock(1_700_000_000_000)),
            Stage2ExecutorConfig::default(),
        )
    }

    #[tokio::test]
    async fn happy_path_condition_met_to_completed() {
        let (_db, repo) = test_repo().await;
        let rule = demo_matching_solend_rule([0xA1; 16]);
        repo.insert(&rule).await.unwrap();
        repo.mark_condition_met(&rule.rule_id).await.unwrap();

        let mock = Arc::new(MockExecutionClient::new());
        mock.push_success(rule.max_input_amount_raw, 415_600_000, "mock-sig-A1");
        let executor = executor_with_mock(repo.clone(), mock.clone());

        let result = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;

        match result {
            Stage2ExecutorRuleResult::Completed {
                rule_id,
                execution_nonce,
                used_amount_raw,
                confirmation_slot,
                signature_sentinel,
            } => {
                assert_eq!(rule_id, rule.rule_id);
                assert_eq!(execution_nonce, 1);
                assert_eq!(used_amount_raw, rule.max_input_amount_raw);
                assert_eq!(confirmation_slot, 415_600_000);
                assert_eq!(signature_sentinel, "mock-sig-A1");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // State-store post-conditions.
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Completed);
        assert!(loaded.completed);
        assert_eq!(loaded.execution_nonce, 1);
        assert_eq!(loaded.last_checked_slot, Some(415_600_000));

        // Mock received exactly one request and it was shape-valid.
        assert_eq!(mock.call_count(), 1);
        let recv = &mock.received()[0];
        assert_eq!(recv.rule_id, rule.rule_id);
        assert_eq!(recv.execution_nonce, 1);
        assert_eq!(recv.action_type, WatchRuleActionType::SolendWithdrawAllDelegated);
        assert_eq!(recv.action_type_byte, 1);
        assert_eq!(recv.input_amount_raw, rule.max_input_amount_raw);
        let s = recv.solend.as_ref().expect("solend payload present");
        assert_eq!(s.reserve.to_base58(), DEMO_RESERVE_BS58);
        assert_eq!(s.lending_market.to_base58(), DEMO_LENDING_MARKET_BS58);
        assert_eq!(s.pyth_oracle.to_base58(), DEMO_PYTH_ORACLE_BS58);
        // Executor in-flight set must be empty after dispatch.
        assert_eq!(executor.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn client_failure_marks_rule_failed_no_retry() {
        let (_db, repo) = test_repo().await;
        let rule = demo_matching_solend_rule([0xA2; 16]);
        repo.insert(&rule).await.unwrap();
        repo.mark_condition_met(&rule.rule_id).await.unwrap();

        let mock = Arc::new(MockExecutionClient::new());
        mock.push_failure(Stage2ExecutionError::ConfirmationFailed(
            "node returned not finalised in time".to_string(),
        ));
        let executor = executor_with_mock(repo.clone(), mock.clone());

        let result = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        assert!(matches!(result, Stage2ExecutorRuleResult::Failed { .. }));

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
        assert!(loaded.last_error.is_some());
        // Failed != completed.
        assert!(!loaded.completed);
        // Execution nonce was leased — must persist (replay protection).
        assert_eq!(loaded.execution_nonce, 1);

        // Re-running must be a no-op (no retry).
        let mock2 = Arc::new(MockExecutionClient::with_success_default());
        let executor2 = executor_with_mock(repo.clone(), mock2.clone());
        let second = executor2
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        assert!(
            matches!(second, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "second pass on a failed rule must observe LeaseLost, got {second:?}"
        );
        assert_eq!(mock2.call_count(), 0, "no second client call on failed rule");
    }

    #[tokio::test]
    async fn lease_guard_refuses_active_row() {
        let (_db, repo) = test_repo().await;
        let rule = demo_matching_solend_rule([0xA3; 16]);
        repo.insert(&rule).await.unwrap();
        // Skip mark_condition_met — row stays active.

        let mock = Arc::new(MockExecutionClient::new());
        let executor = executor_with_mock(repo.clone(), mock.clone());
        let result = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        assert!(matches!(result, Stage2ExecutorRuleResult::LeaseLost { .. }));

        // No mock call, rule unchanged.
        assert_eq!(mock.call_count(), 0);
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
    }

    #[tokio::test]
    async fn lease_guard_refuses_already_executing_row() {
        let (_db, repo) = test_repo().await;
        let rule = demo_matching_solend_rule([0xA4; 16]);
        repo.insert(&rule).await.unwrap();
        repo.mark_condition_met(&rule.rule_id).await.unwrap();
        // Another actor already took the lease.
        repo.mark_executing(&rule.rule_id, 1).await.unwrap();

        let mock = Arc::new(MockExecutionClient::with_success_default());
        let executor = executor_with_mock(repo.clone(), mock.clone());

        let result = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        assert!(matches!(result, Stage2ExecutorRuleResult::LeaseLost { .. }));
        assert_eq!(mock.call_count(), 0);

        // No state mutation beyond what the prior actor did.
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Executing);
        assert_eq!(loaded.execution_nonce, 1);
    }

    #[tokio::test]
    async fn demo_fixture_mismatch_fails_terminal_without_client_call() {
        let (_db, repo) = test_repo().await;
        let rule = demo_mismatching_solend_rule([0xA5; 16]);
        repo.insert(&rule).await.unwrap();
        repo.mark_condition_met(&rule.rule_id).await.unwrap();

        let mock = Arc::new(MockExecutionClient::with_success_default());
        let executor = executor_with_mock(repo.clone(), mock.clone());

        let result = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        assert!(matches!(
            result,
            Stage2ExecutorRuleResult::DemoFixtureMismatch { .. }
        ));
        // No client call.
        assert_eq!(mock.call_count(), 0);

        // Rule is terminal-failed.
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
        assert!(loaded.last_error.is_some());
    }

    #[tokio::test]
    async fn jupiter_rule_is_skipped_via_action_type_allowlist() {
        let (_db, repo) = test_repo().await;
        let rule = jupiter_rule([0xA6; 16]);
        repo.insert(&rule).await.unwrap();
        repo.mark_condition_met(&rule.rule_id).await.unwrap();

        let mock = Arc::new(MockExecutionClient::with_success_default());
        let executor = executor_with_mock(repo.clone(), mock.clone());

        let result = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        assert!(matches!(
            result,
            Stage2ExecutorRuleResult::SkippedActionType { .. }
        ));
        assert_eq!(mock.call_count(), 0);

        // The action_type guard fires BEFORE the CAS lease, so the row
        // stays in `condition_met` for a future Jupiter executor slice
        // to pick up. This is intentional.
        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::ConditionMet);
    }

    #[tokio::test]
    async fn batch_tick_processes_only_condition_met_rules() {
        let (_db, repo) = test_repo().await;

        let active_rule = demo_matching_solend_rule([0xB1; 16]);
        let cm_rule = demo_matching_solend_rule([0xB2; 16]);
        let executing_rule = demo_matching_solend_rule([0xB3; 16]);
        repo.insert(&active_rule).await.unwrap();
        repo.insert(&cm_rule).await.unwrap();
        repo.insert(&executing_rule).await.unwrap();
        repo.mark_condition_met(&cm_rule.rule_id).await.unwrap();
        repo.mark_condition_met(&executing_rule.rule_id).await.unwrap();
        repo.mark_executing(&executing_rule.rule_id, 1).await.unwrap();

        let mock = Arc::new(MockExecutionClient::new());
        mock.push_success(cm_rule.max_input_amount_raw, 415_600_000, "mock-sig-B2");
        let executor = executor_with_mock(repo.clone(), mock.clone());

        let report = executor
            .execute_ready_rules_once(ctx_at(415_550_000, 1_700_000_000_000))
            .await;

        assert_eq!(report.rules_loaded, 1);
        assert_eq!(report.rules_processed, 1);
        assert_eq!(report.completed_count, 1);
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.lease_lost_count, 0);
        assert_eq!(mock.call_count(), 1);
        // Only the cm_rule made it through.
        let recv = &mock.received()[0];
        assert_eq!(recv.rule_id, cm_rule.rule_id);

        // Active rule unchanged.
        assert_eq!(
            repo.get(&active_rule.rule_id).await.unwrap().unwrap().status,
            WatchRuleStatus::Active,
        );
        // Executing rule unchanged.
        assert_eq!(
            repo.get(&executing_rule.rule_id).await.unwrap().unwrap().status,
            WatchRuleStatus::Executing,
        );
        // Completed rule transitioned.
        assert_eq!(
            repo.get(&cm_rule.rule_id).await.unwrap().unwrap().status,
            WatchRuleStatus::Completed,
        );
    }

    #[tokio::test]
    async fn build_request_carries_canonical_rule_hash_from_stored_row() {
        let (_db, repo) = test_repo().await;
        let rule = demo_matching_solend_rule([0xC1; 16]);
        repo.insert(&rule).await.unwrap();
        let stored = repo.get(&rule.rule_id).await.unwrap().unwrap();

        let mock = Arc::new(MockExecutionClient::new());
        let executor = executor_with_mock(repo.clone(), mock.clone());

        let request = executor
            .build_execute_action_request(&stored, 1)
            .expect("build_execute_action_request succeeds for demo-matching rule");
        // Hash is sourced from the stored row, not recomputed.
        assert_eq!(request.canonical_rule_hash, stored.canonical_rule_hash);
        assert_eq!(request.rule_id, rule.rule_id);
        assert_eq!(request.user, rule.user);
        assert_eq!(request.executor, rule.executor);
        assert_eq!(request.delegated_wallet, rule.delegated_wallet);
        assert_eq!(request.destination, rule.destination);
        assert_eq!(request.expires_at_slot, rule.expires_at_slot);
        assert_eq!(request.input_amount_raw, rule.max_input_amount_raw);
        assert_eq!(request.execution_nonce, 1);
        assert_eq!(
            request.action_type,
            WatchRuleActionType::SolendWithdrawAllDelegated
        );
        assert_eq!(request.action_type_byte, 1);
    }

    #[tokio::test]
    async fn execute_rule_once_returns_internal_error_for_unknown_rule() {
        let (_db, repo) = test_repo().await;
        let mock = Arc::new(MockExecutionClient::with_success_default());
        let executor = executor_with_mock(repo.clone(), mock.clone());
        let unknown = [0xFF; 16];
        let result = executor
            .execute_rule_once(unknown, ctx_at(0, 0))
            .await;
        assert!(matches!(
            result,
            Stage2ExecutorRuleResult::InternalError { .. }
        ));
    }

    #[tokio::test]
    async fn in_flight_count_returns_to_zero_after_dispatch() {
        let (_db, repo) = test_repo().await;
        let rule = demo_matching_solend_rule([0xD1; 16]);
        repo.insert(&rule).await.unwrap();
        repo.mark_condition_met(&rule.rule_id).await.unwrap();

        let mock = Arc::new(MockExecutionClient::with_success_default());
        let executor = executor_with_mock(repo.clone(), mock.clone());

        assert_eq!(executor.in_flight_count(), 0);
        let _ = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        // Guard drop after dispatch.
        assert_eq!(executor.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn input_headroom_exhaustion_fails_terminal() {
        let (_db, repo) = test_repo().await;
        // Build a rule where used == max so no headroom exists.
        let mut rule = demo_matching_solend_rule([0xE1; 16]);
        rule.used_amount_raw = rule.max_input_amount_raw;
        repo.insert(&rule).await.unwrap();
        repo.mark_condition_met(&rule.rule_id).await.unwrap();

        let mock = Arc::new(MockExecutionClient::with_success_default());
        let executor = executor_with_mock(repo.clone(), mock.clone());
        let result = executor
            .execute_rule_once(rule.rule_id, ctx_at(415_550_000, 1_700_000_000_000))
            .await;
        assert!(matches!(result, Stage2ExecutorRuleResult::Failed { .. }));
        assert_eq!(mock.call_count(), 0);

        let loaded = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
    }
}

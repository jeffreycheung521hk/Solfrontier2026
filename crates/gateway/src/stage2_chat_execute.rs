//! Stage 2 W5g — chat-card controlled-wallet Solend deposit execution.
//!
//! This module is the **production** counterpart to the W5c test
//! harness (`crates/gateway/tests/common/w5c_deposit_support.rs`) and
//! exists because Rust forbids production code in `src/` from
//! importing from `tests/`. The W5c harness keeps its test-only
//! shape; this module re-implements the production-safe subset
//! needed for the W5g `/chat` execution route. (Future cleanup may
//! invert the dependency.)
//!
//! # W5g semantics
//!
//! After a W5f chat command produces a `ready_to_execute` card, an
//! operator explicitly approves live execution by POSTing to
//! `/sessions/:id/stage2/w5g/execute` with the persisted `rule_id`,
//! its `canonical_rule_hash`, and the EXACT approval phrase. This
//! orchestrator then:
//!
//!  1. Validates env-level approval gates (master gate + env approval
//!     phrase + keypair path + cluster + RPC URL).
//!  2. Validates the request approval phrase matches.
//!  3. Looks up the persisted `WatchRule` and confirms its identity
//!     (`canonical_rule_hash`) + status (`condition_met` / executable).
//!  4. Re-fetches **Save display APY** + **native B-O1 APR** + the
//!     **controlled-wallet USDC balance** at the same slot, and
//!     refuses if any precondition has moved against execution.
//!  5. Delegates the actual build + sign + send + poll to a typed
//!     [`Stage2ChatExecuteSender`] trait so unit tests can inject a
//!     deterministic mock.
//!  6. On `Finalized` from the sender, marks the rule completed in
//!     the durable repo and returns the full execution DTO with USDC
//!     and cToken deltas.
//!  7. On `BroadcastedTimeout`, returns a typed timeout result with
//!     the transaction signature, but **does NOT mark the rule
//!     completed**. The card stays non-terminal so the operator can
//!     re-check on Solscan.
//!
//! # Hard guards (W5g + addendum)
//!
//!  - Serialized transaction byte length is **checked against 1232
//!    bytes** before broadcast. Oversize → typed error, no send.
//!  - `getSignatureStatuses` polling uses **bounded exponential
//!    backoff** (2s → 4s → 8s, capped) with a total deadline; not
//!    fixed-interval. Tolerates a small bounded number of transient
//!    polling errors before timing out.
//!  - `sendTransaction` is invoked with `skipPreflight=false`,
//!    `maxRetries=0`, against the **standard** Helius RPC. **No
//!    Helius Sender** path, **no `confirmTransaction`** path.
//!  - The transaction is signed by the controlled-wallet keypair
//!    loaded from `CLAW_STAGE2_DELEGATED_KEYPAIR_PATH`. The user's
//!    main wallet is NEVER a signer; **no Phantom popup**, ever.
//!
//! # What this module is NOT
//!
//!  - Not a `clawsol-authority` `ExecuteAction`. The Solend program
//!    is invoked directly by the controlled-wallet keypair; the
//!    on-chain ClawSol authority is not in the loop.
//!  - Not an `AuthorizationRecord` PDA execution. No on-chain
//!    user-delegated authorization is consulted.
//!  - Not a Jupiter swap. No external pricing or routing.
//!  - Not a first-class production `SolendDeposit` `ActionSpec`. The
//!    rule's `ActionSpec` is still the W5e carrier
//!    (`SolendWithdrawAllDelegated`); the deposit happens because the
//!    orchestrator hard-codes the W5g amount and target obligation.
//!
//! # No-overclaim
//!
//! Proves: *operator-approved direct-controlled-wallet Solend deposit
//! against a persisted W5e/W5f conditional order, with explicit
//! env-gated approval phrase and live re-checks immediately before
//! the build → size guard → send → bounded-backoff poll.*

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::{
    hash::Hash,
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};

use claw_state_store::stage2_watch_rules::{
    Stage2WatchRuleRepository, WatchRuleStatus,
};
use claw_types::stage2_watch_rule::{canonical_rule_hash, WatchRule};

use crate::integrations::solend::deposit::{
    build_deposit_reserve_liquidity_and_obligation_collateral_instruction,
    DepositInstructionInputs,
};
use crate::integrations::solend::raw::{
    decode_obligation, decode_reserve, SolendObligationRaw, SolendReserveRaw,
};
use crate::integrations::solend::refresh::{
    build_refresh_instructions, RefreshPlanInputs, ReserveRefreshInput,
};
use crate::lending::UnderlyingAmount;
use crate::stage2_demo_apr_bridge::{
    SaveDisplayApyFetcher, W5dAprFetcher, W5D_DEPOSIT_AMOUNT_RAW,
};
use crate::stage2_executor::{
    DEMO_CTOKEN_MINT_BS58, DEMO_LENDING_MARKET_BS58, DEMO_LIQUIDITY_MINT_BS58,
    DEMO_PYTH_ORACLE_BS58, DEMO_RESERVE_BS58,
};

// ── Pinned constants ─────────────────────────────────────────────────────

/// W5g hard-coded deposit amount in raw USDC base units (250 000 raw =
/// 0.25 USDC). The orchestrator refuses any other amount; this slice
/// is intentionally single-amount for safety.
pub const W5G_DEPOSIT_AMOUNT_RAW: u64 = W5D_DEPOSIT_AMOUNT_RAW;

/// Pinned controlled-wallet base58 (Slice 3C). The orchestrator
/// asserts the loaded keypair's public key equals this.
pub const W5G_CONTROLLED_WALLET_BS58: &str =
    "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";

/// Pinned target obligation base58 — same as the W5e rule binding.
pub const W5G_TARGET_OBLIGATION_BS58: &str =
    "BdFLjCcP9mCy557vNNGVbTUuvHxXsh8hc6jXzaPra1wN";

/// Solend program id (mainnet). Hex bytes are checked into the
/// existing stage2_executor; we re-pin here to keep this module
/// self-contained.
pub const W5G_SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

pub const W5G_SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const W5G_COMPUTE_BUDGET_PROGRAM_BS58: &str =
    "ComputeBudget111111111111111111111111111111";

/// ComputeBudget program instruction tags (from the public
/// `solana_compute_budget_program` IDL).
pub const W5G_COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT: u8 = 2;
pub const W5G_COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE: u8 = 3;

/// 400 000 compute units — generous ceiling for the worst-case shape
/// of the W5g tx (compute-budget × 2 + optional ATA create + Solend
/// RefreshReserve + DepositReserveLiquidityAndObligationCollateral).
/// Observed CU consumption on the W5c live run was ~88k.
pub const W5G_COMPUTE_UNIT_LIMIT: u32 = 400_000;

/// Priority-fee price in micro-lamports per CU. With the limit above,
/// the worst-case priority fee is `400_000 * 50_000 / 1_000_000 =
/// 20_000 lamports`.
pub const W5G_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 50_000;

/// Solana mainnet hard transaction-size limit (after signatures + body).
/// The pre-broadcast guard rejects any tx whose serialized byte
/// length exceeds this — `sendTransaction` would reject it anyway,
/// but bouncing here keeps the typed error path clean and avoids a
/// throwaway RPC call.
pub const W5G_SERIALIZED_TX_HARD_LIMIT_BYTES: usize = 1232;

/// USDC decimals (6). Used to validate the source ATA invariant.
pub const W5G_USDC_DECIMALS: u8 = 6;

/// Bounded backoff schedule (W5g addendum):
///   - First poll fires 2 000 ms after broadcast.
///   - Backoff doubles each iteration (2 000 → 4 000 → 8 000 → …),
///     capped at `W5G_POLL_BACKOFF_CAP_MS`.
///   - Overall deadline is `W5G_POLL_TOTAL_TIMEOUT_MS`.
///   - Up to `W5G_POLL_TRANSIENT_ERR_TOLERANCE` consecutive
///     transient polling errors are absorbed before bailing.
pub const W5G_POLL_INITIAL_BACKOFF_MS: u64 = 2_000;
pub const W5G_POLL_BACKOFF_CAP_MS: u64 = 8_000;
pub const W5G_POLL_TOTAL_TIMEOUT_MS: u64 = 120_000;
pub const W5G_POLL_TRANSIENT_ERR_TOLERANCE: u32 = 3;

/// Single-request HTTP timeout for an individual RPC call (not the
/// total polling deadline).
pub const W5G_RPC_REQUEST_TIMEOUT_MS: u64 = 15_000;

// ── Env-gate names + required values ─────────────────────────────────────

/// Master env gate; must be `"1"` for the executor to attempt any
/// live broadcast path. Anything else (including `"true"`, `"yes"`,
/// or unset) keeps the route fail-closed.
pub const W5G_ENV_MASTER_GATE: &str = "CLAW_STAGE2_LIVE_CHAT_EXECUTION";

/// Env-supplied operator-approval phrase. Must equal
/// [`W5G_REQUIRED_APPROVAL_PHRASE`] verbatim.
pub const W5G_ENV_APPROVAL_PHRASE_OWNER: &str =
    "CLAW_STAGE2_CHAT_EXECUTION_APPROVED";

/// The single literal approval phrase that must appear both in the
/// env var and in the HTTP request body.
pub const W5G_REQUIRED_APPROVAL_PHRASE: &str =
    "W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED";

/// Path to the controlled-wallet keypair JSON byte-array file.
pub const W5G_ENV_DELEGATED_KEYPAIR_PATH: &str =
    "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH";

/// Cluster the executor must be running against. Required value is
/// [`W5G_CLUSTER_MAINNET`]; any other value fails closed.
pub const W5G_ENV_CLUSTER: &str = "CLAW_STAGE2_CLUSTER";
pub const W5G_CLUSTER_MAINNET: &str = "mainnet-beta";

// ── Public DTOs ──────────────────────────────────────────────────────────

/// Inbound execution request. The API route DTO mirrors this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatExecuteRequest {
    pub rule_id_hex: String,
    pub canonical_rule_hash_hex: String,
    pub approval_phrase: String,
}

// ── W5g chat-command parser ──────────────────────────────────────────────
//
// The user-typed shape (built on the frontend by
// `buildW5gExecuteCommand`):
//
//   "Execute W5g conditional deposit <rule_id_hex> <canonical_rule_hash_hex>
//    with approval phrase W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED"
//
// is detected at the chat-route boundary (case-insensitive prefilter),
// parsed strictly into a [`ChatExecuteRequest`], and dispatched to
// [`Stage2ChatExecutor::execute`]. The orchestrator's typed result
// then flows back through the chat-route as a `W5gConditionalExecution`
// ChatResponse variant — so the operator never needs an out-of-band
// button.

/// Typed parse-failure for the W5g chat command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum W5gChatCommandParseError {
    MissingPrefix,
    MissingRuleIdHex,
    InvalidRuleIdHex { value: String },
    MissingCanonicalHashHex,
    InvalidCanonicalHashHex { value: String },
    MissingApprovalPhraseMarker,
    EmptyApprovalPhrase,
}

impl std::fmt::Display for W5gChatCommandParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            W5gChatCommandParseError::MissingPrefix => {
                write!(f, "missing 'Execute W5g conditional deposit' prefix")
            }
            W5gChatCommandParseError::MissingRuleIdHex => {
                write!(f, "missing 32-char rule_id hex after the prefix")
            }
            W5gChatCommandParseError::InvalidRuleIdHex { value } => write!(
                f,
                "rule_id hex must be 32 lowercase/uppercase hex chars; got {value:?}"
            ),
            W5gChatCommandParseError::MissingCanonicalHashHex => {
                write!(f, "missing 64-char canonical_rule_hash hex after rule_id")
            }
            W5gChatCommandParseError::InvalidCanonicalHashHex { value } => write!(
                f,
                "canonical_rule_hash hex must be 64 hex chars; got {value:?}"
            ),
            W5gChatCommandParseError::MissingApprovalPhraseMarker => {
                write!(f, "missing 'approval phrase' marker after canonical hash")
            }
            W5gChatCommandParseError::EmptyApprovalPhrase => {
                write!(f, "approval phrase is empty")
            }
        }
    }
}

/// Lightweight pre-filter the chat router uses to decide whether to
/// invoke the strict W5g parser at all. A message that matches this
/// filter is *worth* trying to parse; one that doesn't is left for
/// the W5d/W5f bridge or the LLM path.
pub fn looks_like_w5g_chat_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("execute w5g conditional deposit")
        && lower.contains("approval phrase")
}

/// Strict parser for the W5g chat approval command. Accepts the
/// exact grammar built on the frontend; whitespace-tolerant on the
/// separators between the prefix / rule_id / hash / marker, but
/// rejects any structural deviation (missing prefix, malformed hex,
/// missing marker, empty phrase).
pub fn parse_w5g_chat_command(
    text: &str,
) -> Result<ChatExecuteRequest, W5gChatCommandParseError> {
    let lower = text.to_ascii_lowercase();
    let prefix = "execute w5g conditional deposit";
    let prefix_idx = lower
        .find(prefix)
        .ok_or(W5gChatCommandParseError::MissingPrefix)?;
    // Slice the ORIGINAL `text` (preserving hex case) starting after
    // the prefix. ASCII-only fragments above mean byte offsets are
    // safe on the lowered string.
    let after_prefix = &text[prefix_idx + prefix.len()..];

    let mut iter = after_prefix.split_whitespace();
    let rule_id_token = iter
        .next()
        .ok_or(W5gChatCommandParseError::MissingRuleIdHex)?;
    if rule_id_token.len() != 32 || !rule_id_token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(W5gChatCommandParseError::InvalidRuleIdHex {
            value: rule_id_token.to_string(),
        });
    }
    let canonical_hash_token = iter
        .next()
        .ok_or(W5gChatCommandParseError::MissingCanonicalHashHex)?;
    if canonical_hash_token.len() != 64
        || !canonical_hash_token.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(W5gChatCommandParseError::InvalidCanonicalHashHex {
            value: canonical_hash_token.to_string(),
        });
    }

    // The remainder must contain the marker "approval phrase" (any
    // case) followed by the literal phrase. Search the ORIGINAL text
    // (case-insensitive) after the canonical hash token.
    let tail_idx_in_lower = {
        let tail_marker = "approval phrase";
        let scan_from = prefix_idx + prefix.len();
        lower[scan_from..]
            .find(tail_marker)
            .map(|i| scan_from + i + tail_marker.len())
            .ok_or(W5gChatCommandParseError::MissingApprovalPhraseMarker)?
    };
    let approval_phrase = text[tail_idx_in_lower..].trim().to_string();
    if approval_phrase.is_empty() {
        return Err(W5gChatCommandParseError::EmptyApprovalPhrase);
    }

    Ok(ChatExecuteRequest {
        rule_id_hex: rule_id_token.to_string(),
        canonical_rule_hash_hex: canonical_hash_token.to_string(),
        approval_phrase,
    })
}

/// High-level result of an execution attempt. The orchestrator
/// returns this; the API layer collapses it into a typed wire DTO
/// (`ChatExecuteResultDto`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatExecuteOutcome {
    pub status: ChatExecuteStatus,
    pub rule_id_hex: String,
    pub canonical_rule_hash_hex: String,
    pub tx_signature: Option<String>,
    pub solscan_url: Option<String>,
    pub confirmation_slot: Option<u64>,
    pub used_save_display_apy_bps: Option<u32>,
    pub used_native_onchain_apr_bps: Option<u32>,
    pub used_threshold_bps: Option<u32>,
    pub before_usdc_raw: Option<u64>,
    pub after_usdc_raw: Option<u64>,
    pub usdc_delta_raw: Option<i64>,
    pub before_ctoken_amount: Option<u64>,
    pub after_ctoken_amount: Option<u64>,
    pub ctoken_delta_raw: Option<i64>,
    pub serialized_tx_bytes: Option<usize>,
    pub instruction_count: Option<usize>,
    pub ctoken_ata_create_included: Option<bool>,
    pub error: Option<ChatExecuteErrorCode>,
    pub error_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatExecuteStatus {
    /// Execution completed — broadcast succeeded AND signature
    /// reached `finalized`. Rule marked completed in the durable repo.
    Completed,
    /// Broadcast succeeded (we have a tx signature) but the polling
    /// loop hit the total-deadline without observing `finalized`. The
    /// rule is NOT marked completed; the operator should re-check on
    /// Solscan or via the dashboard's repo view.
    BroadcastedTimeout,
    /// Refused before any RPC call — env gate / approval / rule
    /// lookup / canonical-hash / rule-status / re-check failure.
    PrechecksFailed,
    /// A live operation failed (tx build, oversize tx, broadcast
    /// rejection, on-chain failure). Distinguished from
    /// `PrechecksFailed` because it represents a live-system fault,
    /// not a config / business-rule refusal.
    ExecutionFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatExecuteErrorCode {
    MasterGateMissing,
    EnvApprovalMismatch,
    RequestApprovalMismatch,
    ClusterMismatch,
    KeypairPathMissing,
    KeypairLoadFailed,
    RpcUrlMissing,
    RuleNotFound,
    CanonicalHashMismatch,
    RuleNotExecutable,
    BudgetInsufficient,
    SaveApyBelowThreshold,
    MarketDataUnavailable,
    NativeAprFetchFailed,
    AmountMismatch,
    TxBuildFailed,
    TxSizeExceeded,
    BroadcastFailed,
    OnChainFailure,
    PollExhausted,
    PostTxFetchFailed,
    RepoUpdateFailed,
}

// ── Sender trait + send outcome ──────────────────────────────────────────

/// Inputs the orchestrator hands to the sender once all pre-checks
/// pass. The sender is responsible for actually building, signing,
/// broadcasting, and polling; the orchestrator is the boundary that
/// owns the decision logic.
#[derive(Debug, Clone)]
pub struct ChatExecuteSendRequest {
    pub controlled_wallet: Pubkey,
    pub amount_raw: u64,
    pub target_obligation: Pubkey,
    pub reserve_pubkey: Pubkey,
}

/// Outcome the sender returns. Note: the sender is the ONLY place
/// where the network actually runs; everything upstream is pure
/// orchestration.
#[derive(Debug, Clone)]
pub enum ChatExecuteSendOutcome {
    Finalized {
        tx_signature: String,
        confirmation_slot: u64,
        serialized_tx_bytes: usize,
        instruction_count: usize,
        ctoken_ata_create_included: bool,
        before_usdc_raw: u64,
        after_usdc_raw: u64,
        before_ctoken_amount: u64,
        after_ctoken_amount: u64,
    },
    BroadcastedTimeout {
        tx_signature: String,
        serialized_tx_bytes: usize,
        instruction_count: usize,
        ctoken_ata_create_included: bool,
        last_status: Option<String>,
    },
    TxBuildFailed {
        reason: String,
    },
    TxSizeExceeded {
        serialized_tx_bytes: usize,
        instruction_count: usize,
        ctoken_ata_create_included: bool,
    },
    BroadcastFailed {
        reason: String,
    },
    OnChainFailure {
        tx_signature: String,
        reason: String,
        serialized_tx_bytes: usize,
        instruction_count: usize,
        ctoken_ata_create_included: bool,
    },
}

/// The sender boundary the orchestrator depends on. Unit tests inject
/// a deterministic mock; the daemon wires [`LiveStage2ChatExecuteSender`].
#[async_trait]
pub trait Stage2ChatExecuteSender: Send + Sync + std::fmt::Debug {
    async fn build_sign_send_poll(
        &self,
        request: ChatExecuteSendRequest,
    ) -> ChatExecuteSendOutcome;
}

// ── Orchestrator ─────────────────────────────────────────────────────────

/// The configuration the daemon resolves at startup and hands to the
/// executor. Empty / unset values disable specific gates so the
/// orchestrator can return typed `PrechecksFailed` rather than panic.
#[derive(Debug, Clone)]
pub struct Stage2ChatExecuteConfig {
    pub master_gate_on: bool,
    pub env_approval_phrase: Option<String>,
    pub cluster: Option<String>,
    pub rpc_url_present: bool,
    pub keypair_path_present: bool,
}

impl Stage2ChatExecuteConfig {
    /// Build by reading the four env vars. The keypair file itself
    /// is loaded by [`LiveStage2ChatExecuteSender`] at sender-build
    /// time, not here — this is just a presence check.
    pub fn from_std_env() -> Self {
        let master_gate_on = std::env::var(W5G_ENV_MASTER_GATE)
            .map(|v| v == "1")
            .unwrap_or(false);
        let env_approval_phrase = std::env::var(W5G_ENV_APPROVAL_PHRASE_OWNER).ok();
        let cluster = std::env::var(W5G_ENV_CLUSTER).ok();
        let rpc_url_present = std::env::var("HELIUS_RPC_URL")
            .or_else(|_| std::env::var("CLAW_RPC_URL"))
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let keypair_path_present = std::env::var(W5G_ENV_DELEGATED_KEYPAIR_PATH)
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        Self {
            master_gate_on,
            env_approval_phrase,
            cluster,
            rpc_url_present,
            keypair_path_present,
        }
    }
}

/// W5g orchestrator. Pure logic + composition over four injected
/// dependencies. Production-wired with the live Save fetcher, the
/// live B-O1 native APR fetcher, the durable rule repo, and the live
/// sender; unit tests inject all four with deterministic mocks.
#[derive(Clone)]
pub struct Stage2ChatExecutor {
    sender: Arc<dyn Stage2ChatExecuteSender>,
    save_apy_fetcher: Arc<dyn SaveDisplayApyFetcher>,
    apr_fetcher: Arc<dyn W5dAprFetcher>,
    repo: Arc<Stage2WatchRuleRepository>,
    config: Stage2ChatExecuteConfig,
    /// W5h funding-intent repo. When wired, the executor:
    ///   - Refuses to execute if the rule has no W5h intent OR if
    ///     the intent isn't in `budget_reserved`.
    ///   - CAS-leases `budget_reserved → executing` BEFORE building
    ///     the tx. Refund cannot lease while we hold this lease.
    ///   - On `Finalized`, marks the intent `completed` and binds
    ///     the execution signature.
    ///   - On any failure past the lease, releases the lease back
    ///     to `budget_reserved` (so refund can still claim after
    ///     expiry).
    ///
    /// When `None`, the executor falls back to the pre-W5h behavior:
    /// every rule that survives the regular pre-checks proceeds to
    /// the sender. This keeps existing W5g tests (no W5h intent)
    /// working unchanged.
    w5h_intent_repo:
        Option<Arc<claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository>>,
}

impl std::fmt::Debug for Stage2ChatExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage2ChatExecutor")
            .field("config", &self.config)
            .finish()
    }
}

impl Stage2ChatExecutor {
    pub fn new(
        sender: Arc<dyn Stage2ChatExecuteSender>,
        save_apy_fetcher: Arc<dyn SaveDisplayApyFetcher>,
        apr_fetcher: Arc<dyn W5dAprFetcher>,
        repo: Arc<Stage2WatchRuleRepository>,
        config: Stage2ChatExecuteConfig,
    ) -> Self {
        Self {
            sender,
            save_apy_fetcher,
            apr_fetcher,
            repo,
            config,
            w5h_intent_repo: None,
        }
    }

    /// W5h — attach the funding-intent repo. When present, the
    /// executor gates `budget_reserved → executing` via a CAS lease
    /// and marks the intent `completed` after Finalized.
    pub fn with_w5h_intent_repo(
        mut self,
        repo: Arc<claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository>,
    ) -> Self {
        self.w5h_intent_repo = Some(repo);
        self
    }

    /// Single entry point. Routes the request through every gate and
    /// returns a typed outcome. Never panics; every failure produces
    /// a typed `ChatExecuteOutcome` with `status=PrechecksFailed` or
    /// `status=ExecutionFailed`.
    pub async fn execute(&self, request: ChatExecuteRequest) -> ChatExecuteOutcome {
        // 1. Master gate.
        if !self.config.master_gate_on {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::MasterGateMissing,
                format!("{W5G_ENV_MASTER_GATE} is not set to \"1\""),
            );
        }
        // 2. Env approval phrase.
        let env_phrase = match &self.config.env_approval_phrase {
            Some(p) => p,
            None => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::EnvApprovalMismatch,
                    format!("{W5G_ENV_APPROVAL_PHRASE_OWNER} is not set"),
                )
            }
        };
        if env_phrase != W5G_REQUIRED_APPROVAL_PHRASE {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::EnvApprovalMismatch,
                format!(
                    "{W5G_ENV_APPROVAL_PHRASE_OWNER} value does not match the required phrase"
                ),
            );
        }
        // 3. Request approval phrase.
        if request.approval_phrase != W5G_REQUIRED_APPROVAL_PHRASE {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::RequestApprovalMismatch,
                "request body approval_phrase does not match required phrase".to_string(),
            );
        }
        // 4. Cluster.
        let cluster = match &self.config.cluster {
            Some(c) => c,
            None => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::ClusterMismatch,
                    format!("{W5G_ENV_CLUSTER} is not set"),
                )
            }
        };
        if cluster != W5G_CLUSTER_MAINNET {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::ClusterMismatch,
                format!(
                    "{W5G_ENV_CLUSTER}={cluster:?}, expected {W5G_CLUSTER_MAINNET:?}"
                ),
            );
        }
        // 5. Keypair path + RPC presence (the live sender will fail
        //    again if the path is bad; this is a fast-fail).
        if !self.config.keypair_path_present {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::KeypairPathMissing,
                format!("{W5G_ENV_DELEGATED_KEYPAIR_PATH} is not set"),
            );
        }
        if !self.config.rpc_url_present {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::RpcUrlMissing,
                "neither HELIUS_RPC_URL nor CLAW_RPC_URL is set".to_string(),
            );
        }

        // 6. Rule lookup.
        let rule_id = match decode_rule_id_hex(&request.rule_id_hex) {
            Some(id) => id,
            None => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::RuleNotFound,
                    format!("rule_id_hex is not a 32-char hex string: {:?}", request.rule_id_hex),
                )
            }
        };
        let rule_lookup = self.repo.get(&rule_id).await;
        let stored = match rule_lookup {
            Ok(Some(r)) => r,
            Ok(None) => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::RuleNotFound,
                    format!("rule_id {} not found in repo", request.rule_id_hex),
                )
            }
            Err(e) => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::RuleNotFound,
                    format!("repo.get failed: {e}"),
                )
            }
        };
        let rule = stored.rule.clone();
        let rule_status = stored.status;

        // 7. Canonical hash match — recompute from the live rule
        //    bytes; the repo's stored hash is also available via
        //    `stored.canonical_rule_hash` and these two MUST agree
        //    by construction (the repo's IntegrityCheckFailed branch
        //    would have surfaced on `.get()` otherwise).
        let canonical_actual = hex_encode_32(&canonical_rule_hash(&rule));
        if !equals_ci(&canonical_actual, &request.canonical_rule_hash_hex) {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::CanonicalHashMismatch,
                format!(
                    "submitted canonical_rule_hash {} does not match persisted {}",
                    request.canonical_rule_hash_hex, canonical_actual
                ),
            );
        }

        // 8. Rule status executable.
        if !rule_status_is_executable(rule_status) {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::RuleNotExecutable,
                format!(
                    "rule status {rule_status:?} is not executable for W5g \
                     (must be active or condition_met)"
                ),
            );
        }

        // 9. Extract threshold + reserve / obligation pubkeys from the
        //    persisted rule.
        let threshold_bps = match extract_threshold_bps(&rule) {
            Some(t) => t,
            None => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::RuleNotExecutable,
                    "rule does not carry a SolendReserveSupplyRate condition".to_string(),
                )
            }
        };
        if rule.max_input_amount_raw != W5G_DEPOSIT_AMOUNT_RAW {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::AmountMismatch,
                format!(
                    "rule.max_input_amount_raw {} != {} (W5g is single-amount)",
                    rule.max_input_amount_raw, W5G_DEPOSIT_AMOUNT_RAW
                ),
            );
        }
        let controlled_wallet =
            Pubkey::new_from_array(rule.delegated_wallet.0);
        let reserve_pubkey = Pubkey::from_str(DEMO_RESERVE_BS58)
            .expect("DEMO_RESERVE_BS58 parses");
        let target_obligation = Pubkey::from_str(W5G_TARGET_OBLIGATION_BS58)
            .expect("W5G_TARGET_OBLIGATION_BS58 parses");

        // 10. Live re-check: Save display APY > threshold, native APR
        //     audit value, controlled-wallet USDC budget >= amount.
        let save = match self.save_apy_fetcher.fetch_main_pool_usdc().await {
            Ok(s) => s,
            Err(e) => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::MarketDataUnavailable,
                    format!("save APY fetch failed: {e}"),
                )
            }
        };
        if save.save_display_apy_bps <= threshold_bps {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::SaveApyBelowThreshold,
                format!(
                    "Save APY {} bps <= threshold {} bps at re-check time",
                    save.save_display_apy_bps, threshold_bps
                ),
            );
        }
        // Parse the demo command shape so the W5d evaluator can run
        // its budget + native-APR fetch.  We synthesise a parsed
        // command from the rule's threshold; the chat-input echo is
        // omitted (not relevant at execution time).
        let parsed = crate::stage2_demo_apr_bridge::DemoParsed {
            threshold_bps,
            threshold_pct_label: bps_to_pct_label(threshold_bps),
            amount_raw: W5G_DEPOSIT_AMOUNT_RAW,
        };
        let native = match self
            .apr_fetcher
            .evaluate("", &parsed)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::NativeAprFetchFailed,
                    format!("native APR fetch failed: {e}"),
                )
            }
        };
        if native.current_budget_raw < W5G_DEPOSIT_AMOUNT_RAW {
            return precheck_failure(
                &request,
                ChatExecuteErrorCode::BudgetInsufficient,
                format!(
                    "controlled wallet USDC balance {} raw < required {} raw",
                    native.current_budget_raw, W5G_DEPOSIT_AMOUNT_RAW
                ),
            );
        }

        // 11a. W5h funding-intent gate (race-safe lease).
        //
        // If the executor was constructed with a W5h intent repo,
        // require the intent to be present + in `budget_reserved` +
        // not-expired BEFORE any tx work. Acquire the
        // `budget_reserved → executing` lease via CAS; refund cannot
        // lease while we hold it. Lease failure → typed
        // PrechecksFailed.
        if let Some(intent_repo) = &self.w5h_intent_repo {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let leased = match intent_repo
                .lease_execution_if_budget_reserved(&request.rule_id_hex, now_ms)
                .await
            {
                Ok(n) => n == 1,
                Err(e) => {
                    return precheck_failure(
                        &request,
                        ChatExecuteErrorCode::RepoUpdateFailed,
                        format!("W5h intent_repo.lease_execution failed: {e}"),
                    );
                }
            };
            if !leased {
                return precheck_failure(
                    &request,
                    ChatExecuteErrorCode::RuleNotExecutable,
                    "W5h funding intent is not in `budget_reserved` (or already \
                     leased / refunding / completed / expired); execution lease \
                     could not be acquired".to_string(),
                );
            }
        }

        // 11b. Hand to the sender.
        let send_request = ChatExecuteSendRequest {
            controlled_wallet,
            amount_raw: W5G_DEPOSIT_AMOUNT_RAW,
            target_obligation,
            reserve_pubkey,
        };
        let send_outcome = self.sender.build_sign_send_poll(send_request).await;

        // 12. Build the outcome.
        let base_outcome = ChatExecuteOutcome {
            status: ChatExecuteStatus::ExecutionFailed,
            rule_id_hex: request.rule_id_hex.clone(),
            canonical_rule_hash_hex: canonical_actual.clone(),
            tx_signature: None,
            solscan_url: None,
            confirmation_slot: None,
            used_save_display_apy_bps: Some(save.save_display_apy_bps),
            used_native_onchain_apr_bps: Some(native.current_apr_bps),
            used_threshold_bps: Some(threshold_bps),
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
        };
        match send_outcome {
            ChatExecuteSendOutcome::Finalized {
                tx_signature,
                confirmation_slot,
                serialized_tx_bytes,
                instruction_count,
                ctoken_ata_create_included,
                before_usdc_raw,
                after_usdc_raw,
                before_ctoken_amount,
                after_ctoken_amount,
            } => {
                // Persist completion. Failure to update the repo is
                // surfaced but does NOT roll back the on-chain fact —
                // the user already has the signature; we report it
                // honestly. We use `mark_completed` (not a TOCTOU-safe
                // variant) because the orchestrator just verified the
                // rule was executable a few seconds ago and the rule
                // is owned end-to-end by this process; tx_signature
                // is stored in the W5g execution result DTO, not in
                // the rule row (the rules table doesn't carry a
                // signature column).
                let repo_marked = self
                    .repo
                    .mark_completed(
                        &rule_id,
                        W5G_DEPOSIT_AMOUNT_RAW,
                        confirmation_slot,
                    )
                    .await
                    .map(|n| n == 1)
                    .unwrap_or(false);
                // W5h — terminal-success transition on the funding
                // intent. Idempotent; if the executor isn't wired
                // with the intent repo, this is a no-op.
                if let Some(intent_repo) = &self.w5h_intent_repo {
                    let _ = intent_repo
                        .mark_completed_if_executing(
                            &request.rule_id_hex,
                            &tx_signature,
                        )
                        .await;
                }
                let mut o = base_outcome;
                o.status = ChatExecuteStatus::Completed;
                o.tx_signature = Some(tx_signature.clone());
                o.solscan_url = Some(format!("https://solscan.io/tx/{tx_signature}"));
                o.confirmation_slot = Some(confirmation_slot);
                o.serialized_tx_bytes = Some(serialized_tx_bytes);
                o.instruction_count = Some(instruction_count);
                o.ctoken_ata_create_included = Some(ctoken_ata_create_included);
                o.before_usdc_raw = Some(before_usdc_raw);
                o.after_usdc_raw = Some(after_usdc_raw);
                o.usdc_delta_raw = Some(signed_delta(before_usdc_raw, after_usdc_raw));
                o.before_ctoken_amount = Some(before_ctoken_amount);
                o.after_ctoken_amount = Some(after_ctoken_amount);
                o.ctoken_delta_raw = Some(signed_delta(before_ctoken_amount, after_ctoken_amount));
                if !repo_marked {
                    o.error = Some(ChatExecuteErrorCode::RepoUpdateFailed);
                    o.error_reason = Some(
                        "rule was finalized on-chain but the repo update returned 0 rows; \
                         the signature is real, the rule may already be in a non-active state"
                            .to_string(),
                    );
                }
                o
            }
            ChatExecuteSendOutcome::BroadcastedTimeout {
                tx_signature,
                serialized_tx_bytes,
                instruction_count,
                ctoken_ata_create_included,
                last_status,
            } => {
                let mut o = base_outcome;
                o.status = ChatExecuteStatus::BroadcastedTimeout;
                o.tx_signature = Some(tx_signature.clone());
                o.solscan_url = Some(format!("https://solscan.io/tx/{tx_signature}"));
                o.serialized_tx_bytes = Some(serialized_tx_bytes);
                o.instruction_count = Some(instruction_count);
                o.ctoken_ata_create_included = Some(ctoken_ata_create_included);
                o.error = Some(ChatExecuteErrorCode::PollExhausted);
                o.error_reason = Some(format!(
                    "polling deadline {} ms hit; last status {:?}",
                    W5G_POLL_TOTAL_TIMEOUT_MS, last_status
                ));
                o
            }
            ChatExecuteSendOutcome::TxBuildFailed { reason } => {
                // W5h — pre-broadcast failure: release the execution
                // lease so refund can claim after expiry. (No tx
                // signature, no on-chain side effect.)
                if let Some(intent_repo) = &self.w5h_intent_repo {
                    let _ = intent_repo
                        .release_execution_lease_to_budget_reserved(
                            &request.rule_id_hex,
                            &format!("tx build failed: {reason}"),
                        )
                        .await;
                }
                let mut o = base_outcome;
                o.status = ChatExecuteStatus::ExecutionFailed;
                o.error = Some(ChatExecuteErrorCode::TxBuildFailed);
                o.error_reason = Some(reason);
                o
            }
            ChatExecuteSendOutcome::TxSizeExceeded {
                serialized_tx_bytes,
                instruction_count,
                ctoken_ata_create_included,
            } => {
                // W5h — pre-broadcast size guard: release the lease.
                if let Some(intent_repo) = &self.w5h_intent_repo {
                    let _ = intent_repo
                        .release_execution_lease_to_budget_reserved(
                            &request.rule_id_hex,
                            &format!(
                                "tx size {serialized_tx_bytes} > 1232 limit"
                            ),
                        )
                        .await;
                }
                let mut o = base_outcome;
                o.status = ChatExecuteStatus::ExecutionFailed;
                o.serialized_tx_bytes = Some(serialized_tx_bytes);
                o.instruction_count = Some(instruction_count);
                o.ctoken_ata_create_included = Some(ctoken_ata_create_included);
                o.error = Some(ChatExecuteErrorCode::TxSizeExceeded);
                o.error_reason = Some(format!(
                    "serialized tx {serialized_tx_bytes} bytes > {W5G_SERIALIZED_TX_HARD_LIMIT_BYTES} hard limit"
                ));
                o
            }
            ChatExecuteSendOutcome::BroadcastFailed { reason } => {
                // W5h — sendTransaction rejected the tx before it
                // landed; no signature exists. Release the lease.
                if let Some(intent_repo) = &self.w5h_intent_repo {
                    let _ = intent_repo
                        .release_execution_lease_to_budget_reserved(
                            &request.rule_id_hex,
                            &format!("broadcast failed: {reason}"),
                        )
                        .await;
                }
                let mut o = base_outcome;
                o.status = ChatExecuteStatus::ExecutionFailed;
                o.error = Some(ChatExecuteErrorCode::BroadcastFailed);
                o.error_reason = Some(reason);
                o
            }
            ChatExecuteSendOutcome::OnChainFailure {
                tx_signature,
                reason,
                serialized_tx_bytes,
                instruction_count,
                ctoken_ata_create_included,
            } => {
                // W5h — tx finalized but failed on-chain. The
                // budget is still in the controlled wallet (USDC
                // wasn't transferred; the program returned an err).
                // Mark the intent FAILED (terminal) — neither
                // re-execute nor refund makes sense here without
                // operator review.
                if let Some(intent_repo) = &self.w5h_intent_repo {
                    let _ = intent_repo
                        .mark_failed(
                            &request.rule_id_hex,
                            &format!("on-chain failure: {reason}"),
                        )
                        .await;
                }
                let mut o = base_outcome;
                o.status = ChatExecuteStatus::ExecutionFailed;
                o.tx_signature = Some(tx_signature.clone());
                o.solscan_url = Some(format!("https://solscan.io/tx/{tx_signature}"));
                o.serialized_tx_bytes = Some(serialized_tx_bytes);
                o.instruction_count = Some(instruction_count);
                o.ctoken_ata_create_included = Some(ctoken_ata_create_included);
                o.error = Some(ChatExecuteErrorCode::OnChainFailure);
                o.error_reason = Some(reason);
                o
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn precheck_failure(
    request: &ChatExecuteRequest,
    code: ChatExecuteErrorCode,
    reason: String,
) -> ChatExecuteOutcome {
    ChatExecuteOutcome {
        status: ChatExecuteStatus::PrechecksFailed,
        rule_id_hex: request.rule_id_hex.clone(),
        canonical_rule_hash_hex: request.canonical_rule_hash_hex.clone(),
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
        error: Some(code),
        error_reason: Some(reason),
    }
}

fn decode_rule_id_hex(hex_str: &str) -> Option<[u8; 16]> {
    let s = hex_str.trim();
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        let byte_str = &s[i * 2..i * 2 + 2];
        out[i] = u8::from_str_radix(byte_str, 16).ok()?;
    }
    Some(out)
}

fn equals_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn rule_status_is_executable(status: WatchRuleStatus) -> bool {
    // For W5g a rule is executable if it's currently active (the
    // W5e/W5f path inserts at `Active` and never advances it without
    // an explicit watcher tick) or already in `ConditionMet` (a
    // future-watcher slice may flip it). It is NOT executable in
    // any of: Executing, Completed, Failed, Expired, Revoked.
    matches!(status, WatchRuleStatus::Active | WatchRuleStatus::ConditionMet)
}

fn hex_encode_32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn extract_threshold_bps(rule: &WatchRule) -> Option<u32> {
    use claw_types::stage2_watch_rule::Condition;
    for c in &rule.conditions {
        if let Condition::SolendReserveSupplyRate { threshold_bps, .. } = c {
            return Some(*threshold_bps);
        }
    }
    None
}

fn bps_to_pct_label(bps: u32) -> String {
    let whole = bps / 100;
    let frac = bps % 100;
    format!("{whole}.{frac:02}")
}

fn signed_delta(before: u64, after: u64) -> i64 {
    (after as i128 - before as i128) as i64
}

// ── Refusing sender (defense-in-depth) ───────────────────────────────────

/// A no-op sender that always returns `TxBuildFailed`. The daemon
/// wires this when the live env gate chain isn't fully satisfied
/// (master gate off, keypair path missing, keypair load failed,
/// etc.) so the orchestrator can ALWAYS be constructed and per-
/// request pre-checks run. By the time a request reaches this
/// sender's `build_sign_send_poll`, the orchestrator's pre-check
/// chain has already short-circuited with the appropriate typed
/// `PrechecksFailed` outcome — calling this sender is the
/// defense-in-depth fallback for a bug-in-orchestrator case.
#[derive(Debug)]
pub struct RefusingStage2ChatExecuteSender {
    reason: String,
}

impl RefusingStage2ChatExecuteSender {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl Stage2ChatExecuteSender for RefusingStage2ChatExecuteSender {
    async fn build_sign_send_poll(
        &self,
        _request: ChatExecuteSendRequest,
    ) -> ChatExecuteSendOutcome {
        ChatExecuteSendOutcome::TxBuildFailed {
            reason: format!(
                "refusing sender: {} — this code path should be unreachable; \
                 the orchestrator pre-check chain must fail-closed first",
                self.reason
            ),
        }
    }
}

// ── Live sender impl ─────────────────────────────────────────────────────

/// The production sender. Loads the controlled-wallet keypair from
/// disk, talks to standard Helius RPC, and applies the W5g hard
/// guards (size limit + bounded backoff).
#[derive(Debug)]
pub struct LiveStage2ChatExecuteSender {
    keypair: Keypair,
    rpc_url: String,
    http: reqwest::Client,
}

impl LiveStage2ChatExecuteSender {
    /// Build from env. `keypair_path` is loaded as a JSON byte-array
    /// file (Solana CLI keypair format).
    pub fn from_paths(
        keypair_path: &str,
        rpc_url: String,
    ) -> Result<Self, String> {
        let keypair = load_keypair_from_file(keypair_path)?;
        let expected_pk = Pubkey::from_str(W5G_CONTROLLED_WALLET_BS58)
            .expect("W5G_CONTROLLED_WALLET_BS58 parses");
        if keypair.pubkey() != expected_pk {
            return Err(format!(
                "loaded keypair pubkey {} does not equal pinned controlled wallet {}",
                keypair.pubkey(),
                expected_pk
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(W5G_RPC_REQUEST_TIMEOUT_MS))
            .build()
            .map_err(|e| format!("build reqwest: {e}"))?;
        Ok(Self {
            keypair,
            rpc_url,
            http,
        })
    }
}

#[async_trait]
impl Stage2ChatExecuteSender for LiveStage2ChatExecuteSender {
    async fn build_sign_send_poll(
        &self,
        request: ChatExecuteSendRequest,
    ) -> ChatExecuteSendOutcome {
        // ── Identity guard ───────────────────────────────────────────
        if self.keypair.pubkey() != request.controlled_wallet {
            return ChatExecuteSendOutcome::TxBuildFailed {
                reason: format!(
                    "keypair pubkey {} != requested controlled_wallet {}",
                    self.keypair.pubkey(),
                    request.controlled_wallet
                ),
            };
        }
        if request.amount_raw != W5G_DEPOSIT_AMOUNT_RAW {
            return ChatExecuteSendOutcome::TxBuildFailed {
                reason: format!(
                    "amount {} != W5g single-amount {}",
                    request.amount_raw, W5G_DEPOSIT_AMOUNT_RAW
                ),
            };
        }

        // ── Live reserve + obligation read ───────────────────────────
        let reserve_bytes =
            match rpc_get_account_data(&self.http, &self.rpc_url, &request.reserve_pubkey).await
            {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return ChatExecuteSendOutcome::TxBuildFailed {
                        reason: format!(
                            "reserve account {} does not exist",
                            request.reserve_pubkey
                        ),
                    }
                }
                Err(e) => {
                    return ChatExecuteSendOutcome::TxBuildFailed {
                        reason: format!("getAccountInfo(reserve): {e}"),
                    }
                }
            };
        let reserve = match decode_reserve(&reserve_bytes) {
            Ok(r) => r,
            Err(e) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!("decode_reserve: {e:?}"),
                }
            }
        };
        if let Err(e) = verify_main_pool_reserve(&reserve) {
            return ChatExecuteSendOutcome::TxBuildFailed { reason: e };
        }

        let obligation_bytes = match rpc_get_account_data(
            &self.http,
            &self.rpc_url,
            &request.target_obligation,
        )
        .await
        {
            Ok(Some(b)) => b,
            Ok(None) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!(
                        "obligation account {} does not exist",
                        request.target_obligation
                    ),
                }
            }
            Err(e) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!("getAccountInfo(obligation): {e}"),
                }
            }
        };
        let obligation = match decode_obligation(&obligation_bytes) {
            Ok(o) => o,
            Err(e) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!("decode_obligation: {e:?}"),
                }
            }
        };
        if let Err(e) =
            verify_main_pool_obligation(&obligation, &self.keypair.pubkey())
        {
            return ChatExecuteSendOutcome::TxBuildFailed { reason: e };
        }

        // ── ATAs + cToken-ATA existence ──────────────────────────────
        let usdc_mint = Pubkey::from_str(DEMO_LIQUIDITY_MINT_BS58).unwrap();
        let ctoken_mint = Pubkey::from_str(DEMO_CTOKEN_MINT_BS58).unwrap();
        let source_usdc_ata =
            get_associated_token_address(&self.keypair.pubkey(), &usdc_mint);
        let ctoken_ata =
            get_associated_token_address(&self.keypair.pubkey(), &ctoken_mint);
        let ctoken_ata_exists = match rpc_get_account_exists(
            &self.http,
            &self.rpc_url,
            &ctoken_ata,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!("getAccountInfo(cToken ATA): {e}"),
                }
            }
        };

        // ── Before-balance snapshots ─────────────────────────────────
        let before_usdc = match rpc_get_token_account_balance(
            &self.http,
            &self.rpc_url,
            &source_usdc_ata,
        )
        .await
        {
            Ok(Some(t)) => t.raw,
            Ok(None) => 0,
            Err(e) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!("before getTokenAccountBalance(usdc): {e}"),
                }
            }
        };
        if before_usdc < request.amount_raw {
            return ChatExecuteSendOutcome::TxBuildFailed {
                reason: format!(
                    "before USDC balance {before_usdc} raw < amount {} raw",
                    request.amount_raw
                ),
            };
        }
        // The before-cToken collateral is read from the OBLIGATION's
        // `deposits[]` entry for the pinned reserve — NOT the user's
        // cToken ATA. Solend's
        // `DepositReserveLiquidityAndObligationCollateral` instruction
        // mints cToken directly into the obligation's collateral
        // supply; the controlled wallet's cToken ATA stays at 0 by
        // design, so reading it here would always report a zero
        // delta even on a perfectly successful deposit (the bug we
        // surfaced during the W5g live-test window on 2026-05-12).
        //
        // We reuse the `obligation` already decoded above for the
        // invariant check — no extra RPC needed for the BEFORE read.
        let before_ctoken =
            obligation_pinned_reserve_collateral(&obligation, &request.reserve_pubkey);

        // ── Build instruction list ───────────────────────────────────
        let solend_program_id =
            Pubkey::from_str(W5G_SOLEND_PROGRAM_ID_BS58).unwrap();
        let spl_token = Pubkey::from_str(W5G_SPL_TOKEN_PROGRAM_BS58).unwrap();
        let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id,
            reserves: vec![ReserveRefreshInput {
                reserve_pubkey: request.reserve_pubkey,
                pyth_oracle: reserve.pyth_oracle,
                switchboard_oracle: reserve.switchboard_oracle,
            }],
            obligation: None,
        });
        let deposit_ix = match build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
            DepositInstructionInputs {
                solend_program_id,
                amount: UnderlyingAmount::new(request.amount_raw),
                source_liquidity: source_usdc_ata,
                user_collateral: ctoken_ata,
                reserve: request.reserve_pubkey,
                reserve_liquidity_supply: reserve.liquidity_supply,
                reserve_collateral_mint: reserve.collateral_mint,
                lending_market: reserve.lending_market,
                destination_deposit_collateral: reserve.collateral_supply,
                obligation: request.target_obligation,
                obligation_owner: self.keypair.pubkey(),
                pyth_oracle: reserve.pyth_oracle,
                switchboard_oracle: reserve.switchboard_oracle,
                user_transfer_authority: self.keypair.pubkey(),
            },
        ) {
            Ok(ix) => ix,
            Err(e) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!("build deposit ix: {e}"),
                }
            }
        };

        let mut ixs: Vec<Instruction> = Vec::with_capacity(5);
        ixs.push(compute_budget_set_unit_limit(W5G_COMPUTE_UNIT_LIMIT));
        ixs.push(compute_budget_set_unit_price(
            W5G_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        ));
        let ctoken_ata_create_included = !ctoken_ata_exists;
        if ctoken_ata_create_included {
            ixs.push(create_associated_token_account_idempotent(
                &self.keypair.pubkey(),
                &self.keypair.pubkey(),
                &ctoken_mint,
                &spl_token,
            ));
        }
        for ix in refresh_plan.instructions {
            ixs.push(ix);
        }
        ixs.push(deposit_ix);

        // ── Fresh blockhash → sign → size guard → broadcast ──────────
        let latest = match rpc_get_latest_blockhash(&self.http, &self.rpc_url).await {
            Ok(l) => l,
            Err(e) => {
                return ChatExecuteSendOutcome::TxBuildFailed {
                    reason: format!("getLatestBlockhash: {e}"),
                }
            }
        };
        let (tx_b64, serialized_tx_bytes) =
            match assemble_and_sign(&ixs, &self.keypair, latest.hash) {
                Ok(t) => t,
                Err(e) => {
                    return ChatExecuteSendOutcome::TxBuildFailed {
                        reason: format!("assemble/sign: {e}"),
                    }
                }
            };
        let instruction_count = ixs.len();
        if serialized_tx_bytes > W5G_SERIALIZED_TX_HARD_LIMIT_BYTES {
            return ChatExecuteSendOutcome::TxSizeExceeded {
                serialized_tx_bytes,
                instruction_count,
                ctoken_ata_create_included,
            };
        }

        // ── Broadcast (standard Helius, NOT Sender; skipPreflight=false,
        //    maxRetries=0). NO confirmTransaction.
        let signature =
            match rpc_send_transaction_base64(&self.http, &self.rpc_url, &tx_b64).await {
                Ok(s) => s,
                Err(e) => {
                    return ChatExecuteSendOutcome::BroadcastFailed {
                        reason: format!("sendTransaction: {e}"),
                    }
                }
            };

        // ── Bounded exponential backoff polling ──────────────────────
        let poll_outcome = poll_signature_with_backoff(
            &self.http,
            &self.rpc_url,
            &signature,
            PollSchedule {
                initial_backoff_ms: W5G_POLL_INITIAL_BACKOFF_MS,
                backoff_cap_ms: W5G_POLL_BACKOFF_CAP_MS,
                total_timeout_ms: W5G_POLL_TOTAL_TIMEOUT_MS,
                transient_err_tolerance: W5G_POLL_TRANSIENT_ERR_TOLERANCE,
            },
        )
        .await;

        match poll_outcome {
            PollOutcome::Finalized { slot } => {
                let after_usdc = rpc_get_token_account_balance(
                    &self.http,
                    &self.rpc_url,
                    &source_usdc_ata,
                )
                .await
                .ok()
                .flatten()
                .map(|t| t.raw)
                .unwrap_or(before_usdc);
                // After-cToken: re-fetch the obligation account, decode,
                // and sum the deposits[] entry for the pinned reserve.
                // This is the SAME computation as `before_ctoken` (just
                // post-finality). If any step fails we fall back to
                // `before_ctoken` so the delta reads 0 — that's still
                // honest (we couldn't read the after state) and the
                // operator sees the tx signature + Solscan to verify.
                let after_ctoken = match rpc_get_account_data(
                    &self.http,
                    &self.rpc_url,
                    &request.target_obligation,
                )
                .await
                {
                    Ok(Some(bytes)) => match decode_obligation(&bytes) {
                        Ok(after_obl) => obligation_pinned_reserve_collateral(
                            &after_obl,
                            &request.reserve_pubkey,
                        ),
                        Err(_) => before_ctoken,
                    },
                    _ => before_ctoken,
                };
                ChatExecuteSendOutcome::Finalized {
                    tx_signature: signature,
                    confirmation_slot: slot,
                    serialized_tx_bytes,
                    instruction_count,
                    ctoken_ata_create_included,
                    before_usdc_raw: before_usdc,
                    after_usdc_raw: after_usdc,
                    before_ctoken_amount: before_ctoken,
                    after_ctoken_amount: after_ctoken,
                }
            }
            PollOutcome::Timeout { last_status } => {
                ChatExecuteSendOutcome::BroadcastedTimeout {
                    tx_signature: signature,
                    serialized_tx_bytes,
                    instruction_count,
                    ctoken_ata_create_included,
                    last_status,
                }
            }
            PollOutcome::OnChainFailure { reason } => {
                ChatExecuteSendOutcome::OnChainFailure {
                    tx_signature: signature,
                    reason,
                    serialized_tx_bytes,
                    instruction_count,
                    ctoken_ata_create_included,
                }
            }
        }
    }
}

// ── Low-level helpers ────────────────────────────────────────────────────

fn load_keypair_from_file(path: &str) -> Result<Keypair, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read keypair file '{path}': {e}"))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw).map_err(|e| {
        format!("parse keypair file '{path}' as JSON byte array: {e}")
    })?;
    if bytes.len() != 64 {
        return Err(format!(
            "keypair file '{path}' contained {} bytes (expected 64)",
            bytes.len()
        ));
    }
    Keypair::from_bytes(&bytes).map_err(|e| format!("Keypair::from_bytes: {e}"))
}

fn compute_budget_set_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(W5G_COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str(W5G_COMPUTE_BUDGET_PROGRAM_BS58).unwrap(),
        accounts: vec![],
        data,
    }
}

fn compute_budget_set_unit_price(micro_lamports_per_cu: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(W5G_COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE);
    data.extend_from_slice(&micro_lamports_per_cu.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str(W5G_COMPUTE_BUDGET_PROGRAM_BS58).unwrap(),
        accounts: vec![],
        data,
    }
}

fn verify_main_pool_reserve(reserve: &SolendReserveRaw) -> Result<(), String> {
    let pin_lm = Pubkey::from_str(DEMO_LENDING_MARKET_BS58).unwrap();
    let pin_liq = Pubkey::from_str(DEMO_LIQUIDITY_MINT_BS58).unwrap();
    let pin_coll = Pubkey::from_str(DEMO_CTOKEN_MINT_BS58).unwrap();
    let pin_pyth = Pubkey::from_str(DEMO_PYTH_ORACLE_BS58).unwrap();
    if reserve.lending_market != pin_lm {
        return Err(format!(
            "reserve.lending_market {} != pinned {pin_lm}",
            reserve.lending_market
        ));
    }
    if reserve.liquidity_mint != pin_liq {
        return Err(format!(
            "reserve.liquidity_mint {} != pinned {pin_liq}",
            reserve.liquidity_mint
        ));
    }
    if reserve.collateral_mint != pin_coll {
        return Err(format!(
            "reserve.collateral_mint {} != pinned {pin_coll}",
            reserve.collateral_mint
        ));
    }
    if reserve.pyth_oracle != pin_pyth {
        return Err(format!(
            "reserve.pyth_oracle {} != pinned {pin_pyth}",
            reserve.pyth_oracle
        ));
    }
    if reserve.liquidity_mint_decimals != W5G_USDC_DECIMALS {
        return Err(format!(
            "reserve.liquidity_mint_decimals {} != {W5G_USDC_DECIMALS}",
            reserve.liquidity_mint_decimals
        ));
    }
    Ok(())
}

fn verify_main_pool_obligation(
    obligation: &SolendObligationRaw,
    controlled: &Pubkey,
) -> Result<(), String> {
    let pin_lm = Pubkey::from_str(DEMO_LENDING_MARKET_BS58).unwrap();
    if obligation.owner != *controlled {
        return Err(format!(
            "obligation.owner {} != controlled wallet {controlled}",
            obligation.owner
        ));
    }
    if obligation.lending_market != pin_lm {
        return Err(format!(
            "obligation.lending_market {} != pinned {pin_lm}",
            obligation.lending_market
        ));
    }
    Ok(())
}

/// Sum the cToken collateral in the obligation's `deposits[]` for
/// the pinned reserve. Pure; the obligation is already-decoded.
///
/// Solend's `DepositReserveLiquidityAndObligationCollateral` mints
/// cToken into the obligation's collateral slot for the reserve,
/// NOT into the user's cToken ATA. The W5g reporter therefore reads
/// from this function — the user's cToken ATA stays empty on a
/// deposit-and-collateralize tx, so reading it would always report
/// a zero delta even on a successful deposit.
///
/// Implementation: linear scan over `deposits` (Solend caps the
/// list at 8 entries, so the cost is trivially constant). Multiple
/// entries for the same reserve are summed for robustness even
/// though the program collapses them to one in normal operation.
pub fn obligation_pinned_reserve_collateral(
    obligation: &SolendObligationRaw,
    reserve_pubkey: &Pubkey,
) -> u64 {
    obligation
        .deposits
        .iter()
        .filter(|d| d.deposit_reserve == *reserve_pubkey)
        .map(|d| d.deposited_amount)
        .sum()
}

fn assemble_and_sign(
    ixs: &[Instruction],
    keypair: &Keypair,
    blockhash: Hash,
) -> Result<(String, usize), String> {
    let message = Message::new(ixs, Some(&keypair.pubkey()));
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[keypair], blockhash);
    let bytes = bincode::serialize(&tx).map_err(|e| format!("bincode: {e}"))?;
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((b64, bytes.len()))
}

#[derive(Debug)]
pub struct LatestBlockhash {
    pub hash: Hash,
    pub last_valid_block_height: u64,
}

#[derive(Debug)]
pub struct TokenAmount {
    pub raw: u64,
    pub decimals: u8,
}

#[derive(Debug, Clone)]
enum RpcErr {
    Transport(String),
    Status(u16),
    Body(String),
}
impl std::fmt::Display for RpcErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcErr::Transport(s) => write!(f, "transport: {s}"),
            RpcErr::Status(c) => write!(f, "http {c}"),
            RpcErr::Body(s) => write!(f, "body: {s}"),
        }
    }
}

async fn rpc_post(
    client: &reqwest::Client,
    url: &str,
    body: Value,
) -> Result<Value, RpcErr> {
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| RpcErr::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(RpcErr::Status(status));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| RpcErr::Body(e.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(RpcErr::Body(err.to_string()));
    }
    Ok(v)
}

async fn rpc_get_account_data(
    c: &reqwest::Client,
    url: &str,
    pk: &Pubkey,
) -> Result<Option<Vec<u8>>, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
            "params":[pk.to_string(), {"encoding":"base64","commitment":"confirmed"}]
        }),
    )
    .await?;
    let val = &v["result"]["value"];
    if val.is_null() {
        return Ok(None);
    }
    let data_arr = val
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| RpcErr::Body("missing data[]".into()))?;
    let b64 = data_arr
        .first()
        .and_then(|x| x.as_str())
        .ok_or_else(|| RpcErr::Body("missing data[0]".into()))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| RpcErr::Body(format!("base64: {e}")))?;
    Ok(Some(bytes))
}

async fn rpc_get_account_exists(
    c: &reqwest::Client,
    url: &str,
    pk: &Pubkey,
) -> Result<bool, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
            "params":[pk.to_string(), {"encoding":"base64","commitment":"confirmed"}]
        }),
    )
    .await?;
    Ok(!v["result"]["value"].is_null())
}

async fn rpc_get_latest_blockhash(
    c: &reqwest::Client,
    url: &str,
) -> Result<LatestBlockhash, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getLatestBlockhash",
            "params":[{"commitment":"finalized"}]
        }),
    )
    .await?;
    let bh_str = v["result"]["value"]["blockhash"]
        .as_str()
        .ok_or_else(|| RpcErr::Body("missing blockhash".into()))?;
    let lvbh = v["result"]["value"]["lastValidBlockHeight"]
        .as_u64()
        .ok_or_else(|| RpcErr::Body("missing lastValidBlockHeight".into()))?;
    let hash = Hash::from_str(bh_str).map_err(|e| RpcErr::Body(format!("hash: {e}")))?;
    Ok(LatestBlockhash {
        hash,
        last_valid_block_height: lvbh,
    })
}

async fn rpc_get_token_account_balance(
    c: &reqwest::Client,
    url: &str,
    ata: &Pubkey,
) -> Result<Option<TokenAmount>, RpcErr> {
    let r = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getTokenAccountBalance",
            "params":[ata.to_string(), {"commitment":"confirmed"}]
        }),
    )
    .await;
    match r {
        Ok(value) => {
            let val = &value["result"]["value"];
            let raw_str = val["amount"]
                .as_str()
                .ok_or_else(|| RpcErr::Body("missing amount".into()))?;
            let raw: u64 = raw_str
                .parse()
                .map_err(|e: std::num::ParseIntError| RpcErr::Body(format!("amount: {e}")))?;
            let decimals = val["decimals"].as_u64().unwrap_or(W5G_USDC_DECIMALS as u64) as u8;
            Ok(Some(TokenAmount { raw, decimals }))
        }
        Err(RpcErr::Body(s))
            if s.contains("could not find account")
                || s.contains("Invalid param")
                || s.contains("not found") =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

async fn rpc_send_transaction_base64(
    c: &reqwest::Client,
    url: &str,
    tx_b64: &str,
) -> Result<String, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"sendTransaction",
            "params":[tx_b64, {
                "encoding":"base64",
                "skipPreflight": false,
                "preflightCommitment":"confirmed",
                "maxRetries": 0
            }]
        }),
    )
    .await?;
    v["result"]
        .as_str()
        .ok_or_else(|| RpcErr::Body("missing signature in sendTransaction result".into()))
        .map(|s| s.to_string())
}

#[derive(Debug, Clone)]
enum SigStatus {
    NotFound,
    Pending { confirmation: String },
    Failed(String),
    Succeeded { slot: u64, confirmation: String },
}

async fn rpc_get_signature_status(
    c: &reqwest::Client,
    url: &str,
    signature: &str,
) -> Result<SigStatus, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
            "params":[[signature], {"searchTransactionHistory": true}]
        }),
    )
    .await?;
    let val = &v["result"]["value"][0];
    if val.is_null() {
        return Ok(SigStatus::NotFound);
    }
    if let Some(err) = val.get("err") {
        if !err.is_null() {
            return Ok(SigStatus::Failed(err.to_string()));
        }
    }
    let conf = val
        .get("confirmationStatus")
        .and_then(|s| s.as_str())
        .unwrap_or("processed")
        .to_string();
    if conf == "finalized" {
        let slot = val.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
        Ok(SigStatus::Succeeded {
            slot,
            confirmation: conf,
        })
    } else {
        Ok(SigStatus::Pending { confirmation: conf })
    }
}

/// Polling schedule the live sender uses. Exposed so tests can pass
/// a tighter schedule (microsecond-level) without flake.
#[derive(Debug, Clone, Copy)]
pub struct PollSchedule {
    pub initial_backoff_ms: u64,
    pub backoff_cap_ms: u64,
    pub total_timeout_ms: u64,
    pub transient_err_tolerance: u32,
}

#[derive(Debug, Clone)]
enum PollOutcome {
    Finalized { slot: u64 },
    Timeout { last_status: Option<String> },
    OnChainFailure { reason: String },
}

/// Bounded exponential backoff polling per the W5g addendum:
///
///   - First sleep = `initial_backoff_ms`
///   - Each subsequent sleep = `min(prev * 2, backoff_cap_ms)`
///   - Overall deadline = `total_timeout_ms` since the function was
///     entered
///   - Up to `transient_err_tolerance` consecutive transport / 429 /
///     transient errors are absorbed before bailing.
///
/// Returns `Finalized` once the signature reports `finalized`; never
/// returns on `confirmed` (we wait for finality so the post-tx
/// balance reads are deterministic).
async fn poll_signature_with_backoff(
    http: &reqwest::Client,
    url: &str,
    signature: &str,
    schedule: PollSchedule,
) -> PollOutcome {
    let deadline = std::time::Instant::now()
        + Duration::from_millis(schedule.total_timeout_ms);
    let mut backoff_ms = schedule.initial_backoff_ms;
    let mut last_status: Option<String> = None;
    let mut consecutive_errs: u32 = 0;
    loop {
        if std::time::Instant::now() >= deadline {
            return PollOutcome::Timeout { last_status };
        }
        let sleep_ms = backoff_ms.min(schedule.backoff_cap_ms);
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        if std::time::Instant::now() >= deadline {
            return PollOutcome::Timeout { last_status };
        }
        match rpc_get_signature_status(http, url, signature).await {
            Ok(SigStatus::NotFound) => {
                last_status = Some("not_found".to_string());
                consecutive_errs = 0;
            }
            Ok(SigStatus::Pending { confirmation }) => {
                last_status = Some(format!("pending({confirmation})"));
                consecutive_errs = 0;
            }
            Ok(SigStatus::Failed(err)) => {
                return PollOutcome::OnChainFailure {
                    reason: format!("tx failed on chain: {err}"),
                };
            }
            Ok(SigStatus::Succeeded { slot, confirmation }) => {
                last_status = Some(format!("succeeded({confirmation})"));
                if confirmation == "finalized" {
                    return PollOutcome::Finalized { slot };
                }
            }
            Err(e) => {
                consecutive_errs = consecutive_errs.saturating_add(1);
                last_status = Some(format!("transient_err({e})"));
                if consecutive_errs > schedule.transient_err_tolerance {
                    return PollOutcome::Timeout { last_status };
                }
            }
        }
        backoff_ms = (backoff_ms.saturating_mul(2)).min(schedule.backoff_cap_ms);
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage2_demo_apr_bridge::{
        controlled_wallet_addresses, DemoParsed, EvaluationError, SaveDisplayApyReading,
        W5dEvaluationResult,
    };
    use claw_state_store::db::Database;
    use claw_types::canonical_intent::PubkeyBytes;
    use claw_types::stage2_watch_rule::{
        ActionSpec, Comparison, Condition, ConditionLogic, RateKind, WithdrawMode,
        STAGE2_WATCH_RULE_SCHEMA_VERSION,
    };
    use std::sync::Mutex;

    // ── Helpers ────────────────────────────────────────────────────────

    const SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

    fn config_with_master_off() -> Stage2ChatExecuteConfig {
        Stage2ChatExecuteConfig {
            master_gate_on: false,
            env_approval_phrase: Some(W5G_REQUIRED_APPROVAL_PHRASE.to_string()),
            cluster: Some(W5G_CLUSTER_MAINNET.to_string()),
            rpc_url_present: true,
            keypair_path_present: true,
        }
    }

    fn config_all_on() -> Stage2ChatExecuteConfig {
        Stage2ChatExecuteConfig {
            master_gate_on: true,
            env_approval_phrase: Some(W5G_REQUIRED_APPROVAL_PHRASE.to_string()),
            cluster: Some(W5G_CLUSTER_MAINNET.to_string()),
            rpc_url_present: true,
            keypair_path_present: true,
        }
    }

    fn fixture_rule(threshold_bps: u32, amount_raw: u64) -> WatchRule {
        let pk_str = |s: &str| PubkeyBytes::from_base58(s).expect("base58 parses");
        let controlled = pk_str(W5G_CONTROLLED_WALLET_BS58);
        let reserve = pk_str(DEMO_RESERVE_BS58);
        let lending_market = pk_str(DEMO_LENDING_MARKET_BS58);
        let solend_program_id = pk_str(SOLEND_PROGRAM_ID_BS58);
        let target_obligation = pk_str(W5G_TARGET_OBLIGATION_BS58);
        let rule_id = {
            let mut id = [0u8; 16];
            id[0..4].copy_from_slice(&threshold_bps.to_le_bytes());
            id[4..12].copy_from_slice(&amount_raw.to_le_bytes());
            id[12..16].copy_from_slice(&controlled.0[0..4]);
            id
        };
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id,
            user: controlled,
            executor: controlled,
            delegated_wallet: controlled,
            created_at_slot: 419_000_000,
            expires_at_slot: 419_500_000,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![Condition::SolendReserveSupplyRate {
                reserve_pubkey: reserve,
                lending_market,
                solend_program_id,
                comparison: Comparison::Gt,
                threshold_bps,
                rate_kind: RateKind::Apr,
                formula_version: 1,
                max_reserve_staleness_slots: 16,
                required_refresh_same_tx: true,
            }],
            action: ActionSpec::SolendWithdrawAllDelegated {
                target_obligation,
                reserve_pubkey: reserve,
                lending_market,
                destination_wallet: controlled,
                withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
            },
            max_input_amount_raw: amount_raw,
            used_amount_raw: 0,
            destination: controlled,
            slippage_bps: 0,
        }
    }

    async fn test_repo() -> (Database, Arc<Stage2WatchRuleRepository>) {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        let repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));
        (db, repo)
    }

    fn good_request_for(rule: &WatchRule) -> ChatExecuteRequest {
        let mut hex = String::with_capacity(32);
        for b in rule.rule_id {
            use std::fmt::Write as _;
            let _ = write!(hex, "{:02x}", b);
        }
        let hash = hex_encode_32(&canonical_rule_hash(rule));
        ChatExecuteRequest {
            rule_id_hex: hex,
            canonical_rule_hash_hex: hash,
            approval_phrase: W5G_REQUIRED_APPROVAL_PHRASE.to_string(),
        }
    }

    // ── Local mocks (not re-exported from stage2_demo_apr_bridge to keep
    //    test-only structs from leaking into the production surface) ────

    #[derive(Debug, Clone)]
    struct StubSaveFetcher {
        outcome: Result<SaveDisplayApyReading, EvaluationError>,
    }
    impl StubSaveFetcher {
        fn apy_bps(bps: u32) -> Self {
            Self {
                outcome: Ok(SaveDisplayApyReading {
                    save_display_apy_bps: bps,
                    raw_supply_interest_str: format!("{:.2}", bps as f64 / 100.0),
                    reserve_pubkey: DEMO_RESERVE_BS58.to_string(),
                    lending_market: DEMO_LENDING_MARKET_BS58.to_string(),
                    liquidity_mint: DEMO_LIQUIDITY_MINT_BS58.to_string(),
                    collateral_mint: DEMO_CTOKEN_MINT_BS58.to_string(),
                    rewards_present: false,
                }),
            }
        }
        fn unavailable() -> Self {
            Self {
                outcome: Err(EvaluationError::MarketDataUnavailable {
                    reason: "transport: stub".to_string(),
                }),
            }
        }
    }
    #[async_trait]
    impl SaveDisplayApyFetcher for StubSaveFetcher {
        async fn fetch_main_pool_usdc(
            &self,
        ) -> Result<SaveDisplayApyReading, EvaluationError> {
            self.outcome.clone()
        }
    }

    #[derive(Debug, Clone)]
    struct StubAprFetcher {
        native_apr_bps: u32,
        budget_raw: u64,
    }
    #[async_trait]
    impl W5dAprFetcher for StubAprFetcher {
        async fn evaluate(
            &self,
            input_text: &str,
            parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            let (controlled_wallet, source_usdc_ata) = controlled_wallet_addresses();
            Ok(crate::stage2_demo_apr_bridge::compose_w5e_result(
                input_text,
                parsed,
                self.native_apr_bps,
                self.budget_raw,
                419_000_001,
                controlled_wallet,
                source_usdc_ata,
            ))
        }
    }

    #[derive(Debug, Clone)]
    enum Programmed {
        Finalized,
        BroadcastedTimeout,
        TxSizeExceeded,
        BroadcastFailed,
        OnChainFailure,
        TxBuildFailed,
    }
    #[derive(Debug)]
    struct StubSender {
        programmed: Programmed,
        called_with: Mutex<Option<ChatExecuteSendRequest>>,
    }
    impl StubSender {
        fn programmed(p: Programmed) -> Self {
            Self {
                programmed: p,
                called_with: Mutex::new(None),
            }
        }
    }
    #[async_trait]
    impl Stage2ChatExecuteSender for StubSender {
        async fn build_sign_send_poll(
            &self,
            request: ChatExecuteSendRequest,
        ) -> ChatExecuteSendOutcome {
            *self.called_with.lock().unwrap() = Some(request);
            match self.programmed {
                Programmed::Finalized => ChatExecuteSendOutcome::Finalized {
                    tx_signature: "SiG4mock4test".to_string(),
                    confirmation_slot: 419_048_388,
                    serialized_tx_bytes: 920,
                    instruction_count: 5,
                    ctoken_ata_create_included: true,
                    before_usdc_raw: 403_487,
                    after_usdc_raw: 153_487,
                    before_ctoken_amount: 0,
                    after_ctoken_amount: 192_876,
                },
                Programmed::BroadcastedTimeout => ChatExecuteSendOutcome::BroadcastedTimeout {
                    tx_signature: "SiG4timeout".to_string(),
                    serialized_tx_bytes: 920,
                    instruction_count: 5,
                    ctoken_ata_create_included: true,
                    last_status: Some("pending(processed)".to_string()),
                },
                Programmed::TxSizeExceeded => ChatExecuteSendOutcome::TxSizeExceeded {
                    serialized_tx_bytes: 1500,
                    instruction_count: 8,
                    ctoken_ata_create_included: true,
                },
                Programmed::BroadcastFailed => ChatExecuteSendOutcome::BroadcastFailed {
                    reason: "stub: 429 Too Many Requests".to_string(),
                },
                Programmed::OnChainFailure => ChatExecuteSendOutcome::OnChainFailure {
                    tx_signature: "SiGonchainfail".to_string(),
                    reason: "InsufficientFunds".to_string(),
                    serialized_tx_bytes: 920,
                    instruction_count: 5,
                    ctoken_ata_create_included: false,
                },
                Programmed::TxBuildFailed => ChatExecuteSendOutcome::TxBuildFailed {
                    reason: "stub: reserve account missing".to_string(),
                },
            }
        }
    }

    async fn build_executor(
        save: StubSaveFetcher,
        apr: StubAprFetcher,
        sender: StubSender,
        cfg: Stage2ChatExecuteConfig,
    ) -> (Database, Stage2ChatExecutor) {
        let (db, repo) = test_repo().await;
        let exec = Stage2ChatExecutor::new(
            Arc::new(sender),
            Arc::new(save),
            Arc::new(apr),
            repo,
            cfg,
        );
        (db, exec)
    }

    // ── Pure / unit tests ──────────────────────────────────────────────

    #[test]
    fn rule_id_hex_decode_round_trips() {
        assert_eq!(
            decode_rule_id_hex("00112233445566778899aabbccddeeff"),
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                0xcc, 0xdd, 0xee, 0xff,
            ])
        );
        assert_eq!(
            decode_rule_id_hex("00112233445566778899AABBCCDDEEFF"),
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                0xcc, 0xdd, 0xee, 0xff,
            ])
        );
        assert_eq!(decode_rule_id_hex("not-hex"), None);
        assert_eq!(decode_rule_id_hex(""), None);
        assert_eq!(decode_rule_id_hex("aabbccdd"), None);
    }

    #[test]
    fn compute_budget_ix_bytes_pinned() {
        let ix = compute_budget_set_unit_limit(400_000);
        assert_eq!(ix.data[0], W5G_COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT);
        let n =
            u32::from_le_bytes([ix.data[1], ix.data[2], ix.data[3], ix.data[4]]);
        assert_eq!(n, 400_000);

        let ix = compute_budget_set_unit_price(50_000);
        assert_eq!(ix.data[0], W5G_COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE);
        let n = u64::from_le_bytes([
            ix.data[1], ix.data[2], ix.data[3], ix.data[4],
            ix.data[5], ix.data[6], ix.data[7], ix.data[8],
        ]);
        assert_eq!(n, 50_000);
    }

    #[test]
    fn signed_delta_handles_zero_growth_shrink() {
        assert_eq!(signed_delta(100, 200), 100);
        assert_eq!(signed_delta(100, 50), -50);
        assert_eq!(signed_delta(0, 0), 0);
        // 0.25 USDC ≈ 250_000 raw is the only realistic input range
        // for this slice; verify the helper handles that scale
        // cleanly without panic.
        assert_eq!(signed_delta(403_487, 153_487), -250_000);
        assert_eq!(signed_delta(0, 192_876), 192_876);
    }

    // ── W5g reporter regression (2026-05-12) ──────────────────────────
    //
    // The cToken delta MUST be measured from the obligation's
    // `deposits[]` entry for the pinned reserve, NOT from the user's
    // cToken ATA. Solend's deposit-and-collateralize ix mints cToken
    // directly into the obligation's collateral supply, so the user's
    // cToken ATA stays at 0 even on a successful deposit. The bug we
    // hit on the W5g live send (tx ftXu…oCz, slot 419139121) was:
    // ATA-derived delta said `0` while the obligation actually grew
    // by +192 822 cToken.

    fn mk_obligation_with_collateral(
        pinned_reserve_amount: u64,
    ) -> crate::integrations::solend::raw::SolendObligationRaw {
        use crate::integrations::solend::raw::{
            SolendObligationCollateralRaw, SolendObligationRaw,
        };
        let pinned =
            Pubkey::from_str(DEMO_RESERVE_BS58).expect("pinned reserve parses");
        let other = Pubkey::from_str("So11111111111111111111111111111111111111112")
            .expect("placeholder pubkey parses");
        let lending_market = Pubkey::from_str(DEMO_LENDING_MARKET_BS58)
            .expect("lending market parses");
        let owner = Pubkey::from_str(W5G_CONTROLLED_WALLET_BS58)
            .expect("controlled wallet parses");
        SolendObligationRaw {
            version: 1,
            last_update_slot: 419_139_121,
            last_update_stale: false,
            lending_market,
            owner,
            // Two collateral entries: a noise entry for a different
            // reserve, plus the pinned one. The helper must sum only
            // the pinned entry.
            deposits: vec![
                SolendObligationCollateralRaw {
                    deposit_reserve: other,
                    deposited_amount: 12_345_678,
                },
                SolendObligationCollateralRaw {
                    deposit_reserve: pinned,
                    deposited_amount: pinned_reserve_amount,
                },
            ],
            borrows: Vec::new(),
            borrowed_value_upper_bound_wads: 0,
            borrowing_isolated_asset: false,
            super_unhealthy_borrow_value_wads: 0,
            unweighted_borrowed_value_wads: 0,
            closeable: false,
        }
    }

    #[test]
    fn obligation_pinned_reserve_collateral_sums_only_pinned_entries() {
        let pinned =
            Pubkey::from_str(DEMO_RESERVE_BS58).expect("pinned reserve parses");

        // Zero collateral for the pinned reserve.
        let before = mk_obligation_with_collateral(0);
        assert_eq!(
            obligation_pinned_reserve_collateral(&before, &pinned),
            0,
            "pre-deposit obligation must report 0 collateral for the pinned reserve"
        );

        // After a deposit of 192 822 cToken (real value observed in
        // the W5g live-send tx).
        let after = mk_obligation_with_collateral(192_822);
        assert_eq!(
            obligation_pinned_reserve_collateral(&after, &pinned),
            192_822,
            "post-deposit obligation must report the deposit amount as collateral"
        );

        // Delta computation matches the W5g reporter's intent.
        let delta = signed_delta(
            obligation_pinned_reserve_collateral(&before, &pinned),
            obligation_pinned_reserve_collateral(&after, &pinned),
        );
        assert_eq!(delta, 192_822);
    }

    /// W5g reporter regression: when two unrelated collateral entries
    /// exist alongside the pinned one, the helper ignores the noise.
    /// This proves the fix for the W5g live-send symptom — the user's
    /// cToken ATA had two unrelated balances elsewhere in the system
    /// (a different reserve's cToken supply, the controlled wallet's
    /// empty ATA), and only the obligation's pinned-reserve entry
    /// must be summed.
    #[test]
    fn obligation_pinned_reserve_collateral_ignores_other_reserves() {
        use crate::integrations::solend::raw::{
            SolendObligationCollateralRaw, SolendObligationRaw,
        };
        let pinned =
            Pubkey::from_str(DEMO_RESERVE_BS58).expect("pinned reserve parses");
        let other_a = Pubkey::from_str("11111111111111111111111111111111").unwrap();
        let other_b = Pubkey::from_str("So11111111111111111111111111111111111111112")
            .expect("other pubkey parses");
        let lending_market = Pubkey::from_str(DEMO_LENDING_MARKET_BS58).unwrap();
        let owner = Pubkey::from_str(W5G_CONTROLLED_WALLET_BS58).unwrap();
        let obl = SolendObligationRaw {
            version: 1,
            last_update_slot: 419_139_121,
            last_update_stale: false,
            lending_market,
            owner,
            deposits: vec![
                SolendObligationCollateralRaw {
                    deposit_reserve: other_a,
                    deposited_amount: 999_999,
                },
                SolendObligationCollateralRaw {
                    deposit_reserve: pinned,
                    deposited_amount: 192_822,
                },
                SolendObligationCollateralRaw {
                    deposit_reserve: other_b,
                    deposited_amount: 1_234_567,
                },
            ],
            borrows: Vec::new(),
            borrowed_value_upper_bound_wads: 0,
            borrowing_isolated_asset: false,
            super_unhealthy_borrow_value_wads: 0,
            unweighted_borrowed_value_wads: 0,
            closeable: false,
        };
        assert_eq!(
            obligation_pinned_reserve_collateral(&obl, &pinned),
            192_822,
            "noise from other reserves must not contaminate the pinned-reserve sum"
        );
    }

    /// Reporter regression — direct delta of two obligations with a
    /// deposit between them. Pinned at 0 → 192_822 should produce a
    /// `ctoken_delta_raw` of +192_822 (the W5g live-send actual).
    #[test]
    fn obligation_collateral_delta_matches_w5g_live_send_actual() {
        let pinned =
            Pubkey::from_str(DEMO_RESERVE_BS58).expect("pinned reserve parses");
        let before = mk_obligation_with_collateral(0);
        let after = mk_obligation_with_collateral(192_822);
        let before_c = obligation_pinned_reserve_collateral(&before, &pinned);
        let after_c = obligation_pinned_reserve_collateral(&after, &pinned);
        assert_eq!(before_c, 0);
        assert_eq!(after_c, 192_822);
        assert_eq!(signed_delta(before_c, after_c), 192_822_i64);
    }

    /// W5g — orchestrator built with a `RefusingStage2ChatExecuteSender`
    /// + `master_gate_on=false` returns typed `PrechecksFailed` for
    /// any well-formed request, WITHOUT calling the sender. This is
    /// the gates-off chat-route behavior the daemon ships in
    /// production.
    #[tokio::test]
    async fn refusing_sender_path_returns_prechecks_failed_when_gates_off() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let mut cfg = config_all_on();
        cfg.master_gate_on = false; // simulate the daemon-startup case
        let exec = Stage2ChatExecutor::new(
            Arc::new(RefusingStage2ChatExecuteSender::new("test: master gate off")),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            cfg,
        );
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::MasterGateMissing));
        assert!(o.tx_signature.is_none(), "no broadcast on gates-off path");
        // Rule must stay Active — never marked completed.
        let stored = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(stored.status, WatchRuleStatus::Active);
    }

    /// REGRESSION (W5g live-send blocker, 2026-05-12): end-to-end
    /// trace from bridge re-type to orchestrator approval.
    ///
    ///   1. First chat call inserts rule A at slot S1 → canonical
    ///      hash H1.
    ///   2. Second chat call at slot S2 hits the UNIQUE collision
    ///      in `repo.insert`. Post-fix, the response echoes H1
    ///      (the PERSISTED hash), not the freshly-built H2.
    ///   3. Build a `ChatExecuteRequest` from the second call's
    ///      response (rule_id + H1 + literal phrase).
    ///   4. Hand the request to the orchestrator with a Finalized
    ///      mock sender. Pre-fix this would fail with
    ///      `CanonicalHashMismatch`; post-fix it must reach the
    ///      sender and return `Completed`.
    #[tokio::test]
    async fn w5g_approval_after_collision_passes_canonical_precheck() {
        use crate::stage2_demo_apr_bridge::{
            compose_w5e_result, controlled_wallet_addresses, handle_demo_command_v2,
            DemoParsed, W5dEvaluationResult,
        };

        // Local mock with programmable last_checked_slot (the bridge's
        // own MockW5dAprFetcher lives in its `cfg(test)` module and
        // isn't visible across modules).
        #[derive(Debug, Clone)]
        struct SlotControlledAprFetcher {
            native_apr_bps: u32,
            budget_raw: u64,
            slot: u64,
        }
        #[async_trait]
        impl W5dAprFetcher for SlotControlledAprFetcher {
            async fn evaluate(
                &self,
                input_text: &str,
                parsed: &DemoParsed,
            ) -> Result<W5dEvaluationResult, EvaluationError> {
                let (controlled_wallet, source_usdc_ata) =
                    controlled_wallet_addresses();
                Ok(compose_w5e_result(
                    input_text,
                    parsed,
                    self.native_apr_bps,
                    self.budget_raw,
                    self.slot,
                    controlled_wallet,
                    source_usdc_ata,
                ))
            }
        }

        let (_db, repo) = test_repo().await;
        let input = "If Solend Main Pool USDC deposit APR is above 1%, \
                     deposit 0.25 USDC from my bounded executor wallet into Solend.";

        // Step 1: insert rule at slot S1 via the bridge.
        let fetcher1 = SlotControlledAprFetcher {
            native_apr_bps: 163,
            budget_raw: 500_000,
            slot: 100_000_000,
        };
        let r1 = handle_demo_command_v2(&fetcher1, Some(&repo), input)
            .await
            .unwrap();
        let h1 = r1.canonical_rule_hash_hex.clone().unwrap();
        let id1 = r1.rule_id_hex.clone().unwrap();

        // Step 2: re-type at slot S2. Response MUST echo H1.
        let fetcher2 = SlotControlledAprFetcher {
            native_apr_bps: 163,
            budget_raw: 500_000,
            slot: 200_000_000,
        };
        let r2 = handle_demo_command_v2(&fetcher2, Some(&repo), input)
            .await
            .unwrap();
        assert_eq!(r2.canonical_rule_hash_hex.as_deref(), Some(h1.as_str()));
        assert_eq!(r2.rule_id_hex.as_deref(), Some(id1.as_str()));

        // Step 3: build the W5g approval request from r2's identity.
        let req = ChatExecuteRequest {
            rule_id_hex: id1.clone(),
            canonical_rule_hash_hex: h1.clone(),
            approval_phrase: W5G_REQUIRED_APPROVAL_PHRASE.to_string(),
        };

        // Step 4: dispatch through the orchestrator with all gates
        // on + mocked Finalized sender. We use Save APY 210 + threshold
        // 100 (from the bridge command above) so the condition is met.
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher {
                native_apr_bps: 165,
                budget_raw: 500_000,
            }),
            repo.clone(),
            config_all_on(),
        );
        let o = exec.execute(req).await;

        // Critical assert: canonical-hash precheck must pass when the
        // request carries the hash returned by the bridge after a
        // UNIQUE-collision fallback. Pre-fix this was
        // `error=CanonicalHashMismatch`; post-fix the orchestrator
        // reaches the sender and reports Finalized.
        assert_ne!(
            o.error,
            Some(ChatExecuteErrorCode::CanonicalHashMismatch),
            "BUG REGRESSION: canonical-hash precheck must accept the \
             hash returned by handle_demo_command_v2 even after a \
             UNIQUE-collision fallback"
        );
        assert_eq!(o.status, ChatExecuteStatus::Completed);
        assert_eq!(o.tx_signature.as_deref(), Some("SiG4mock4test"));
    }

    // ── W5h × W5g integration: race-safe execution gate ────────────────

    async fn insert_w5h_intent(
        repo: &claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository,
        rule_id_hex: &str,
        canonical_hash_hex: &str,
        expires_at_ms: i64,
    ) {
        use claw_state_store::stage2_w5h_funding::NewW5hFundingIntent;
        let now_ms = chrono::Utc::now().timestamp_millis();
        repo.insert(&NewW5hFundingIntent {
            intent_id: rule_id_hex.to_string(),
            rule_id_hex: rule_id_hex.to_string(),
            canonical_rule_hash_hex: canonical_hash_hex.to_string(),
            user_wallet: "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string(),
            user_usdc_ata: "TestUserUsdcAta1111111111111111111111111111".to_string(),
            controlled_wallet: W5G_CONTROLLED_WALLET_BS58.to_string(),
            controlled_usdc_ata: "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3"
                .to_string(),
            amount_raw: W5G_DEPOSIT_AMOUNT_RAW,
            threshold_bps: 100,
            save_display_apy_bps_at_creation: 210,
            native_onchain_apr_bps_at_creation: 165,
            created_at_ms: now_ms,
            expires_at_ms,
        })
        .await
        .unwrap();
    }

    /// Happy path: rule + W5h intent in `budget_reserved` → execute
    /// reaches the sender, returns `Completed`, marks the intent
    /// completed in the repo.
    #[tokio::test]
    async fn w5h_executor_with_budget_reserved_intent_completes() {
        use claw_state_store::stage2_w5h_funding::{
            Stage2W5hFundingIntentRepository, W5hIntentStatus,
        };
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        // Build a W5h intent for the same rule_id.
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            repo.pool().clone(),
        ));
        let req = good_request_for(&rule);
        let canonical = req.canonical_rule_hash_hex.clone();
        let now_ms = chrono::Utc::now().timestamp_millis();
        insert_w5h_intent(
            &intent_repo,
            &req.rule_id_hex,
            &canonical,
            now_ms + 180_000,
        )
        .await;
        // Advance intent to budget_reserved.
        intent_repo
            .mark_funding_submitted_if_required(&req.rule_id_hex, "FundingSig1")
            .await
            .unwrap();
        intent_repo
            .mark_budget_reserved_if_submitted(&req.rule_id_hex, "FundingSig1", 1)
            .await
            .unwrap();

        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        )
        .with_w5h_intent_repo(intent_repo.clone());
        let o = exec.execute(req.clone()).await;
        assert_eq!(o.status, ChatExecuteStatus::Completed);
        // W5h intent must now be Completed.
        let stored = intent_repo.get(&req.rule_id_hex).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::Completed);
        assert_eq!(stored.execution_signature.as_deref(), Some("SiG4mock4test"));
    }

    /// Refuse path: rule exists but W5h intent is still in
    /// `funding_required` (user hasn't paid yet) → execute is
    /// blocked with `RuleNotExecutable`, no broadcast.
    #[tokio::test]
    async fn w5h_executor_with_funding_required_intent_refuses() {
        use claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository;
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            repo.pool().clone(),
        ));
        let req = good_request_for(&rule);
        let now_ms = chrono::Utc::now().timestamp_millis();
        insert_w5h_intent(
            &intent_repo,
            &req.rule_id_hex,
            &req.canonical_rule_hash_hex,
            now_ms + 180_000,
        )
        .await;
        // Leave intent in funding_required.
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        )
        .with_w5h_intent_repo(intent_repo.clone());
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::RuleNotExecutable));
        assert!(o.tx_signature.is_none());
    }

    /// Refuse path: W5h intent has expired (now > expires_at_ms) →
    /// execute lease fails; refund window is open.
    #[tokio::test]
    async fn w5h_executor_refuses_expired_intent() {
        use claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository;
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            repo.pool().clone(),
        ));
        let req = good_request_for(&rule);
        // Expiry in the past.
        insert_w5h_intent(
            &intent_repo,
            &req.rule_id_hex,
            &req.canonical_rule_hash_hex,
            1, // expired long ago
        )
        .await;
        intent_repo
            .mark_funding_submitted_if_required(&req.rule_id_hex, "FundingSig1")
            .await
            .unwrap();
        intent_repo
            .mark_budget_reserved_if_submitted(&req.rule_id_hex, "FundingSig1", 1)
            .await
            .unwrap();
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        )
        .with_w5h_intent_repo(intent_repo);
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::RuleNotExecutable));
    }

    /// Race: refund won the lease first → execute is refused, no
    /// broadcast call.
    #[tokio::test]
    async fn w5h_executor_refuses_when_refund_already_leased() {
        use claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository;
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            repo.pool().clone(),
        ));
        let req = good_request_for(&rule);
        let now_ms = chrono::Utc::now().timestamp_millis();
        insert_w5h_intent(
            &intent_repo,
            &req.rule_id_hex,
            &req.canonical_rule_hash_hex,
            now_ms - 1, // already expired
        )
        .await;
        intent_repo
            .mark_funding_submitted_if_required(&req.rule_id_hex, "FundingSig1")
            .await
            .unwrap();
        intent_repo
            .mark_budget_reserved_if_submitted(&req.rule_id_hex, "FundingSig1", 1)
            .await
            .unwrap();
        // Refund acquires the lease first.
        let n = intent_repo
            .lease_refund_if_expired_or_past(&req.rule_id_hex, now_ms)
            .await
            .unwrap();
        assert_eq!(n, 1);
        // Execute attempt — refused.
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        )
        .with_w5h_intent_repo(intent_repo);
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::RuleNotExecutable));
    }

    /// Sender returns `TxBuildFailed` after the lease was acquired
    /// → W5h intent is released back to `budget_reserved`, NOT left
    /// stuck in `executing` (otherwise a future refund couldn't
    /// claim the budget).
    #[tokio::test]
    async fn w5h_executor_releases_lease_on_tx_build_failed() {
        use claw_state_store::stage2_w5h_funding::{
            Stage2W5hFundingIntentRepository, W5hIntentStatus,
        };
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            repo.pool().clone(),
        ));
        let req = good_request_for(&rule);
        let now_ms = chrono::Utc::now().timestamp_millis();
        insert_w5h_intent(
            &intent_repo,
            &req.rule_id_hex,
            &req.canonical_rule_hash_hex,
            now_ms + 180_000,
        )
        .await;
        intent_repo
            .mark_funding_submitted_if_required(&req.rule_id_hex, "FundingSig1")
            .await
            .unwrap();
        intent_repo
            .mark_budget_reserved_if_submitted(&req.rule_id_hex, "FundingSig1", 1)
            .await
            .unwrap();
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::TxBuildFailed)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        )
        .with_w5h_intent_repo(intent_repo.clone());
        let o = exec.execute(req.clone()).await;
        assert_eq!(o.status, ChatExecuteStatus::ExecutionFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::TxBuildFailed));
        let stored = intent_repo.get(&req.rule_id_hex).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::BudgetReserved);
        assert!(stored.last_error.is_some());
    }

    #[test]
    fn rule_status_is_executable_table() {
        assert!(rule_status_is_executable(WatchRuleStatus::Active));
        assert!(rule_status_is_executable(WatchRuleStatus::ConditionMet));
        assert!(!rule_status_is_executable(WatchRuleStatus::Executing));
        assert!(!rule_status_is_executable(WatchRuleStatus::Completed));
        assert!(!rule_status_is_executable(WatchRuleStatus::Failed));
        assert!(!rule_status_is_executable(WatchRuleStatus::Expired));
        assert!(!rule_status_is_executable(WatchRuleStatus::Revoked));
    }

    // ── W5g chat-command parser ────────────────────────────────────────

    /// Canonical happy-path: the exact string the frontend's
    /// `buildW5gExecuteCommand` produces parses cleanly into a
    /// `ChatExecuteRequest`.
    #[test]
    fn w5g_chat_parser_accepts_canonical_command() {
        let cmd = "Execute W5g conditional deposit \
                   f401000090d00300000000009a62dace \
                   26068ac3efbf438407ed607901ea24cb28f67e6a6f6064fd48b879341576931d \
                   with approval phrase W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED";
        let r = parse_w5g_chat_command(cmd).unwrap();
        assert_eq!(r.rule_id_hex, "f401000090d00300000000009a62dace");
        assert_eq!(
            r.canonical_rule_hash_hex,
            "26068ac3efbf438407ed607901ea24cb28f67e6a6f6064fd48b879341576931d"
        );
        assert_eq!(r.approval_phrase, W5G_REQUIRED_APPROVAL_PHRASE);
    }

    /// Detector + parser are case-insensitive on the marker words
    /// but preserve case on the hex tokens (rule_id_hex is stored
    /// case-insensitively elsewhere via `decode_rule_id_hex`).
    #[test]
    fn w5g_chat_parser_case_insensitive_markers() {
        let cmd = "EXECUTE W5G CONDITIONAL DEPOSIT \
                   F401000090D00300000000009A62DACE \
                   26068AC3EFBF438407ED607901EA24CB28F67E6A6F6064FD48B879341576931D \
                   WITH APPROVAL PHRASE W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED";
        let r = parse_w5g_chat_command(cmd).unwrap();
        assert_eq!(r.rule_id_hex, "F401000090D00300000000009A62DACE");
        assert!(looks_like_w5g_chat_command(cmd));
    }

    #[test]
    fn w5g_detector_rejects_w5d_and_arbitrary_text() {
        assert!(!looks_like_w5g_chat_command(
            "If Solend Main Pool USDC deposit APR is above 1%, ..."
        ));
        assert!(!looks_like_w5g_chat_command("show my balances"));
        // The "approval phrase" marker is required. A message that
        // has the prefix but no marker stays in the LLM fall-through
        // path (intentionally permissive — the parser then rejects).
        assert!(!looks_like_w5g_chat_command(
            "execute w5g conditional deposit and please send it"
        ));
        // Marker without the prefix also stays in fall-through.
        assert!(!looks_like_w5g_chat_command(
            "tell me the approval phrase for this rule"
        ));
    }

    #[test]
    fn w5g_chat_parser_rejects_short_rule_id_hex() {
        let cmd = "Execute W5g conditional deposit \
                   deadbeef \
                   26068ac3efbf438407ed607901ea24cb28f67e6a6f6064fd48b879341576931d \
                   with approval phrase W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED";
        match parse_w5g_chat_command(cmd).unwrap_err() {
            W5gChatCommandParseError::InvalidRuleIdHex { .. } => {}
            other => panic!("expected InvalidRuleIdHex, got {other:?}"),
        }
    }

    #[test]
    fn w5g_chat_parser_rejects_non_hex_canonical() {
        let cmd = "Execute W5g conditional deposit \
                   f401000090d00300000000009a62dace \
                   this-is-not-hex-at-all-this-is-not-hex-at-all-this-is-not-hex-at \
                   with approval phrase W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED";
        match parse_w5g_chat_command(cmd).unwrap_err() {
            W5gChatCommandParseError::InvalidCanonicalHashHex { .. } => {}
            other => panic!("expected InvalidCanonicalHashHex, got {other:?}"),
        }
    }

    #[test]
    fn w5g_chat_parser_rejects_missing_approval_marker() {
        let cmd = "Execute W5g conditional deposit \
                   f401000090d00300000000009a62dace \
                   26068ac3efbf438407ed607901ea24cb28f67e6a6f6064fd48b879341576931d \
                   and please send it";
        match parse_w5g_chat_command(cmd).unwrap_err() {
            W5gChatCommandParseError::MissingApprovalPhraseMarker => {}
            other => panic!("expected MissingApprovalPhraseMarker, got {other:?}"),
        }
    }

    #[test]
    fn w5g_chat_parser_passes_through_wrong_approval_phrase_to_orchestrator() {
        // The PARSER does not check the phrase value; the
        // orchestrator does (and surfaces RequestApprovalMismatch).
        // This means a typo-ed phrase still parses but is rejected
        // downstream — exactly the typed error the operator needs.
        let cmd = "Execute W5g conditional deposit \
                   f401000090d00300000000009a62dace \
                   26068ac3efbf438407ed607901ea24cb28f67e6a6f6064fd48b879341576931d \
                   with approval phrase YOLO";
        let r = parse_w5g_chat_command(cmd).unwrap();
        assert_eq!(r.approval_phrase, "YOLO");
    }

    // ── Orchestrator: gate refusals ────────────────────────────────────

    #[tokio::test]
    async fn rejects_when_master_gate_off() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let req = good_request_for(&rule);
        let (_db, exec) = build_executor(
            StubSaveFetcher::apy_bps(210),
            StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 },
            StubSender::programmed(Programmed::Finalized),
            config_with_master_off(),
        )
        .await;
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::MasterGateMissing));
        assert!(o.tx_signature.is_none());
    }

    #[tokio::test]
    async fn rejects_when_env_phrase_missing() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let req = good_request_for(&rule);
        let mut cfg = config_all_on();
        cfg.env_approval_phrase = None;
        let (_db, exec) = build_executor(
            StubSaveFetcher::apy_bps(210),
            StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 },
            StubSender::programmed(Programmed::Finalized),
            cfg,
        )
        .await;
        assert_eq!(
            exec.execute(req).await.error,
            Some(ChatExecuteErrorCode::EnvApprovalMismatch)
        );
    }

    #[tokio::test]
    async fn rejects_when_env_phrase_wrong() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let req = good_request_for(&rule);
        let mut cfg = config_all_on();
        cfg.env_approval_phrase = Some("WRONG PHRASE".to_string());
        let (_db, exec) = build_executor(
            StubSaveFetcher::apy_bps(210),
            StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 },
            StubSender::programmed(Programmed::Finalized),
            cfg,
        )
        .await;
        assert_eq!(
            exec.execute(req).await.error,
            Some(ChatExecuteErrorCode::EnvApprovalMismatch)
        );
    }

    #[tokio::test]
    async fn rejects_when_request_phrase_wrong() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let mut req = good_request_for(&rule);
        req.approval_phrase = "not the right phrase".to_string();
        let (_db, exec) = build_executor(
            StubSaveFetcher::apy_bps(210),
            StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 },
            StubSender::programmed(Programmed::Finalized),
            config_all_on(),
        )
        .await;
        let o = exec.execute(req).await;
        assert_eq!(o.error, Some(ChatExecuteErrorCode::RequestApprovalMismatch));
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
    }

    #[tokio::test]
    async fn rejects_when_cluster_not_mainnet() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let req = good_request_for(&rule);
        let mut cfg = config_all_on();
        cfg.cluster = Some("devnet".to_string());
        let (_db, exec) = build_executor(
            StubSaveFetcher::apy_bps(210),
            StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 },
            StubSender::programmed(Programmed::Finalized),
            cfg,
        )
        .await;
        assert_eq!(
            exec.execute(req).await.error,
            Some(ChatExecuteErrorCode::ClusterMismatch)
        );
    }

    #[tokio::test]
    async fn rejects_when_rule_not_found_in_repo() {
        // Note: this test does NOT insert the rule, so the repo
        // returns Ok(None) and we surface RuleNotFound.
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let req = good_request_for(&rule);
        let (_db, exec) = build_executor(
            StubSaveFetcher::apy_bps(210),
            StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 },
            StubSender::programmed(Programmed::Finalized),
            config_all_on(),
        )
        .await;
        let o = exec.execute(req).await;
        assert_eq!(o.error, Some(ChatExecuteErrorCode::RuleNotFound));
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
    }

    #[tokio::test]
    async fn rejects_when_canonical_hash_mismatch() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let mut req = good_request_for(&rule);
        req.canonical_rule_hash_hex =
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo,
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.error, Some(ChatExecuteErrorCode::CanonicalHashMismatch));
    }

    #[tokio::test]
    async fn rejects_when_save_apy_below_threshold() {
        let rule = fixture_rule(250, W5G_DEPOSIT_AMOUNT_RAW); // threshold 2.50 %
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            // Save APY 2.10 % — below the 2.50 % threshold.
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo,
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.error, Some(ChatExecuteErrorCode::SaveApyBelowThreshold));
        assert_eq!(o.status, ChatExecuteStatus::PrechecksFailed);
        assert!(o.tx_signature.is_none());
    }

    #[tokio::test]
    async fn rejects_when_save_api_unavailable_no_silent_native_fallback() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::unavailable()),
            // Even if native says go (210 > 180), Save unavailable → fail closed.
            Arc::new(StubAprFetcher { native_apr_bps: 210, budget_raw: 500_000 }),
            repo,
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.error, Some(ChatExecuteErrorCode::MarketDataUnavailable));
        assert!(o.tx_signature.is_none(), "no signature on fail-closed path");
    }

    #[tokio::test]
    async fn rejects_when_budget_insufficient() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            // Budget below 250_000 raw.
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 100_000 }),
            repo,
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.error, Some(ChatExecuteErrorCode::BudgetInsufficient));
    }

    #[tokio::test]
    async fn rejects_when_rule_already_completed() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        // Flip status to Completed.
        repo.mark_completed(&rule.rule_id, W5G_DEPOSIT_AMOUNT_RAW, 419_999_999)
            .await
            .unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo,
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.error, Some(ChatExecuteErrorCode::RuleNotExecutable));
    }

    // ── Orchestrator: sender outcomes ──────────────────────────────────

    #[tokio::test]
    async fn finalized_outcome_marks_rule_completed_and_carries_deltas() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::Finalized)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::Completed);
        assert_eq!(o.tx_signature.as_deref(), Some("SiG4mock4test"));
        assert_eq!(
            o.solscan_url.as_deref(),
            Some("https://solscan.io/tx/SiG4mock4test"),
        );
        assert_eq!(o.confirmation_slot, Some(419_048_388));
        assert_eq!(o.usdc_delta_raw, Some(-250_000));
        assert_eq!(o.ctoken_delta_raw, Some(192_876));
        assert_eq!(o.serialized_tx_bytes, Some(920));
        assert_eq!(o.instruction_count, Some(5));
        assert_eq!(o.ctoken_ata_create_included, Some(true));
        assert_eq!(o.used_save_display_apy_bps, Some(210));
        assert_eq!(o.used_native_onchain_apr_bps, Some(165));
        assert_eq!(o.used_threshold_bps, Some(180));
        assert!(o.error.is_none());
        // Rule should now be Completed in the repo.
        let stored = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(stored.status, WatchRuleStatus::Completed);
    }

    #[tokio::test]
    async fn broadcasted_timeout_does_not_mark_completed_but_returns_signature() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::BroadcastedTimeout)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::BroadcastedTimeout);
        assert_eq!(o.tx_signature.as_deref(), Some("SiG4timeout"));
        assert!(o.solscan_url.is_some());
        assert_eq!(o.serialized_tx_bytes, Some(920));
        // Rule must NOT be marked completed.
        let stored = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(stored.status, WatchRuleStatus::Active);
    }

    #[tokio::test]
    async fn tx_size_exceeded_aborts_before_marking_completed() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::TxSizeExceeded)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::ExecutionFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::TxSizeExceeded));
        assert!(o.tx_signature.is_none(), "no broadcast → no signature");
        assert_eq!(o.serialized_tx_bytes, Some(1500));
        // Rule must NOT be marked completed.
        let stored = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(stored.status, WatchRuleStatus::Active);
    }

    #[tokio::test]
    async fn broadcast_failed_surfaces_typed_code_and_does_not_mark_completed() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::BroadcastFailed)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::ExecutionFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::BroadcastFailed));
        assert!(o.tx_signature.is_none());
        let stored = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(stored.status, WatchRuleStatus::Active);
    }

    #[tokio::test]
    async fn on_chain_failure_carries_signature_but_no_completed() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::OnChainFailure)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::ExecutionFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::OnChainFailure));
        assert_eq!(o.tx_signature.as_deref(), Some("SiGonchainfail"));
        let stored = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(stored.status, WatchRuleStatus::Active);
    }

    #[tokio::test]
    async fn tx_build_failed_short_circuits_before_signature() {
        let rule = fixture_rule(180, W5G_DEPOSIT_AMOUNT_RAW);
        let (_db, repo) = test_repo().await;
        repo.insert(&rule).await.unwrap();
        let req = good_request_for(&rule);
        let exec = Stage2ChatExecutor::new(
            Arc::new(StubSender::programmed(Programmed::TxBuildFailed)),
            Arc::new(StubSaveFetcher::apy_bps(210)),
            Arc::new(StubAprFetcher { native_apr_bps: 165, budget_raw: 500_000 }),
            repo.clone(),
            config_all_on(),
        );
        let o = exec.execute(req).await;
        assert_eq!(o.status, ChatExecuteStatus::ExecutionFailed);
        assert_eq!(o.error, Some(ChatExecuteErrorCode::TxBuildFailed));
        assert!(o.tx_signature.is_none());
    }

    // ── Source guards ──────────────────────────────────────────────────

    // Source-scan guards. All needles are runtime-assembled from
    // unjoined fragments so the test source itself does NOT contain
    // the joined literal — otherwise the scan would always trip on
    // its own needle list. (See `routes/chat.rs::source_guard_tests`
    // for the same convention.)

    /// The W5g production module must NOT issue an actual JSON-RPC
    /// confirmation call site of the forbidden form. The brief
    /// explicitly requires bounded-backoff polling of signature
    /// statuses instead. Needle and doc-comment are fragmented so
    /// the test does not match itself.
    #[test]
    fn source_does_not_issue_the_forbidden_rpc_method() {
        const SRC: &str = include_str!("stage2_chat_execute.rs");
        // Runtime-built — never appears literally in the source.
        let bad = format!(
            "{}{}{}",
            "\"method\":\"",
            "conf",
            "irmTransaction\"",
        );
        assert!(
            !SRC.contains(bad.as_str()),
            "production paths must NOT issue `{bad}`"
        );
        // And positively assert the polling method we DO use.
        let good = format!(
            "{}{}",
            "\"method\":\"getSig",
            "natureStatuses\"",
        );
        assert!(
            SRC.contains(good.as_str()),
            "stage2_chat_execute.rs must use `{good}` for confirmation polling"
        );
    }

    /// `sendTransaction` MUST be invoked with `skipPreflight=false`.
    /// We check both that no `true` form appears AND that the
    /// `false` form is present.
    #[test]
    fn source_does_not_set_skip_preflight_true() {
        const SRC: &str = include_str!("stage2_chat_execute.rs");
        let bad = format!(
            "{}{}{}",
            "\"skip",
            "Preflight\": ",
            "true",
        );
        assert!(
            !SRC.contains(bad.as_str()),
            "stage2_chat_execute.rs must NOT contain `{bad}` \
             — W5g requires skipPreflight=false"
        );
        let good = format!(
            "{}{}{}",
            "\"skip",
            "Preflight\": ",
            "false",
        );
        assert!(
            SRC.contains(good.as_str()),
            "stage2_chat_execute.rs must contain `{good}` — required for W5g safety"
        );
    }

    /// The W5g production module must NOT call the dedicated
    /// fast-broadcast endpoint. Standard JSON-RPC only. All needles
    /// are runtime-assembled so the test does not match its own
    /// source.
    #[test]
    fn source_must_use_only_standard_rpc_endpoint() {
        const SRC: &str = include_str!("stage2_chat_execute.rs");
        let needles = [
            format!("{}{}", "Helius", "Sender"),     // type name
            format!("{}{}", "helius", "_sender"),    // module / fn name
            format!("{}{}", "sender.helius.", "xyz"),// hypothetical host
            format!("{}{}{}", "/sender/", "v1/", "tx"),
        ];
        for n in &needles {
            assert!(
                !SRC.contains(n.as_str()),
                "stage2_chat_execute.rs must NOT contain `{n}` — \
                 W5g requires standard RPC only"
            );
        }
    }

    /// `maxRetries=0` is required (no automatic resubmission). One
    /// is the only correct value. Runtime-assembled.
    #[test]
    fn source_sets_max_retries_zero() {
        const SRC: &str = include_str!("stage2_chat_execute.rs");
        let good = format!("{}{}", "\"maxRetries\": ", "0");
        assert!(
            SRC.contains(good.as_str()),
            "stage2_chat_execute.rs must contain `{good}` — W5g forbids automatic retries"
        );
    }
}

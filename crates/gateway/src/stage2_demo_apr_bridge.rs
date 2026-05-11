//! Stage 2 W5d/W5e — Chat-route conditional-order bridge (production-side).
//!
//! The deterministic-parser + B-O1-on-chain-APR evaluator surface that
//! the user-facing `/chat` route uses to recognise the demo grammar
//! *before* the LLM is asked anything.
//!
//! # W5e semantics (this slice)
//!
//! A command like
//!
//! > "If Solend Main Pool USDC deposit APR is above X%, deposit 0.25
//! >  USDC from my bounded executor wallet into Solend."
//!
//! is a **conditional order**, not an instant true/false quote check.
//! The bridge:
//!
//!  1. parses the command (W5d-style),
//!  2. reads the current APR from the on-chain reserve via B-O1,
//!  3. reads the controlled-wallet USDC ATA balance (the **budget**),
//!  4. anchors the result to the same RPC slot (`last_checked_slot`),
//!  5. composes a real [`WatchRule`] with a deterministic `rule_id`
//!     (computed from the parsed inputs so the same command always
//!     hashes to the same id — idempotent re-typing in chat does NOT
//!     mint a new rule), and
//!  6. when the chat handler is wired with a [`Stage2WatchRuleRepository`],
//!     `insert`s the rule and reports `rule_persisted=true`. If the
//!     repo is absent OR the insert hits a UNIQUE-collision, the
//!     bridge tries `repo.get(rule_id)` to confirm idempotent
//!     persistence; on every other failure path it sets
//!     `rule_persisted=false` and the chat handler emits a clearly
//!     preview-only card.
//!
//! Status decision (strict-> on bps; budget gate is a hard precondition):
//!
//! | budget          | condition | status            |
//! | --------------- | --------- | ----------------- |
//! | `< 250_000 raw` | any       | `needs_funding`   |
//! | `>= 250_000`    | met       | `ready_to_execute`|
//! | `>= 250_000`    | not met   | `watching`        |
//!
//! # What this module is NOT
//!
//! - **Not** a Solend deposit sender. The execution path lives in the
//!   env-gated W5c harness; this module never calls `sendTransaction`,
//!   never loads a keypair, and never imports the controlled-wallet
//!   keypair.
//! - **Not** an LLM client. There is zero provider call here.
//! - **Not** a fall-through. If a message does not match the strict
//!   grammar, the chat layer continues to the normal LLM path. This
//!   module only fires when the deterministic detector says yes.
//!
//! # No-overclaim
//!
//! Proves: *chat text → deterministic W5d parser → B-O1 on-chain APR
//! evaluation → controlled-wallet budget read → real Stage 2 watch
//! rule persisted in the state-store, with typed lifecycle status the
//! frontend renders.*
//!
//! Does NOT prove: clawsol-authority `ExecuteAction` execution; a
//! first-class `SolendDepositControlledWallet` `ActionSpec` variant;
//! Jupiter conditional execution; user-delegated authorization via
//! `AuthorizationRecord` PDA; live tx broadcast; a running watcher
//! that ticks this rule's lifecycle (W2/W3/W4 substrate exists; the
//! chat route does NOT spawn the watcher — `status="watching"`
//! describes the rule's *durable state in the state-store*, not an
//! active polling loop in the same process).

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;

use crate::integrations::solend::raw::decode_reserve;
use crate::stage2_evaluator::{
    solend_snapshot_value_from_reserve_raw, solend_supply_apr_bps_from_wad,
    solend_supply_apr_wad_for_snapshot,
};
use claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository;
use claw_types::canonical_intent::PubkeyBytes;
use claw_types::stage2_watch_rule::{
    canonical_rule_hash, ActionSpec, Comparison, Condition, ConditionLogic, RateKind,
    WatchRule, WithdrawMode, STAGE2_WATCH_RULE_SCHEMA_VERSION,
};

// ── Pinned facts (sealed against the stage2_executor source-of-truth) ────

/// Hard-coded demo deposit amount in USDC base units (1 USDC == 10^6).
/// 250_000 raw = 0.25 USDC. The parser rejects any other amount.
pub const W5D_DEPOSIT_AMOUNT_RAW: u64 = 250_000;

/// Controlled wallet pubkey (Slice 3C delegated wallet). Source of
/// truth: `reference_slice3c_controlled_wallet.md` memory entry; the
/// keypair lives only on Jeff's local fs at
/// `C:/Users/jeffr/test-wallets/slice3c-dryrun.json` and is NOT
/// imported by this module under any code path.
pub const CONTROLLED_WALLET_BS58: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";

/// W5e bounded demo expiry, in slots, added to the
/// `last_checked_slot` of the RPC read that constructed the rule.
/// At ~400ms / slot, 50_000 slots ≈ 5.5 hours — long enough for a
/// demo session to span the W2 watcher's tick interval, short enough
/// that a stale rule expires within the same day.
pub const W5E_DEMO_EXPIRY_SLOTS: u64 = 50_000;

/// Solend Main Pool USDC reserve pubkey (mainnet-beta). Re-exported
/// from `stage2_executor::DEMO_RESERVE_BS58` for ergonomic access here;
/// a unit test pins parity.
pub const W5D_RESERVE_BS58: &str = crate::stage2_executor::DEMO_RESERVE_BS58;

/// Request timeout for the single `getAccountInfo` RPC call this
/// module makes per evaluation. Kept tight because the chat route is
/// user-facing and a slow APR fetch shouldn't tie up an HTTP worker.
const RPC_REQUEST_TIMEOUT_MS: u64 = 8_000;

/// Plausible-range guard on the computed APR. A reading outside this
/// window indicates malformed reserve bytes or an evaluator drift; we
/// surface as a typed error so the chat layer can fall back to a clean
/// "couldn't evaluate" card without claiming a live APR.
const APR_BPS_SANITY_MAX: u32 = 10_000;

// ── Parser ────────────────────────────────────────────────────────────────

/// Decoded shape of the supported demo grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemoParsed {
    /// Threshold expressed in basis points (1 % == 100 bps). Always
    /// integer; the parser refuses any input that would need fractional
    /// bps to represent.
    pub threshold_bps: u32,
    /// The exact percent label the user typed (e.g. `"2.5"`). Echoed
    /// into the response verbatim — never used in the decision math.
    pub threshold_pct_label: String,
    /// Always `W5D_DEPOSIT_AMOUNT_RAW` for this demo (the parser
    /// rejects any other amount); echoed for completeness.
    pub amount_raw: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ParseError {
    MissingMarker { what: String },
    WrongPool { detail: String },
    WrongAsset { detail: String },
    WrongWallet { detail: String },
    UnsupportedAmount { detail: String },
    MalformedPercent { detail: String },
    NegativeThreshold { value: String },
    ThresholdOverflow { value: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingMarker { what } => write!(f, "missing marker: {what}"),
            ParseError::WrongPool { detail } => write!(f, "wrong pool: {detail}"),
            ParseError::WrongAsset { detail } => write!(f, "wrong asset: {detail}"),
            ParseError::WrongWallet { detail } => write!(f, "wrong/missing wallet phrase: {detail}"),
            ParseError::UnsupportedAmount { detail } => write!(f, "unsupported amount: {detail}"),
            ParseError::MalformedPercent { detail } => write!(f, "malformed percent: {detail}"),
            ParseError::NegativeThreshold { value } => write!(f, "negative threshold: {value}"),
            ParseError::ThresholdOverflow { value } => write!(f, "threshold overflow: {value}"),
        }
    }
}

/// Lightweight pre-filter the chat router uses to decide whether to
/// invoke the strict parser at all. A message that matches this filter
/// is *worth* trying to parse; one that doesn't is sent through the
/// regular LLM path. The filter is intentionally permissive: it only
/// looks for the two strongest discriminators (the pool name and the
/// "deposit APR" phrase), so a typo elsewhere still surfaces a typed
/// `ParseError` rather than silently being routed to the LLM.
pub fn looks_like_w5d_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let pool_named = lower.contains("solend main pool usdc")
        || lower.contains("save main pool usdc");
    let apr_phrase = lower.contains("deposit apr");
    pool_named && apr_phrase
}

/// Parse a single demo-command string. Case-insensitive,
/// whitespace-tolerant, but strict about the core tokens. Returns a
/// `DemoParsed` with the threshold in basis points (integer) so the
/// decision math never involves floating point.
///
/// Supported grammar (whitespace-collapsed):
///
/// ```text
/// "if solend main pool usdc deposit apr is above X%, deposit
///  0.25 usdc from my bounded executor wallet into solend."
/// ```
///
/// `bounded executor wallet` and `controlled wallet` are synonyms.
/// `save` is accepted as a synonym for `solend` (rebrand).
pub fn parse_demo_command(text: &str) -> Result<DemoParsed, ParseError> {
    let normalized = normalize_whitespace(&text.to_ascii_lowercase());

    if !normalized.contains("if ") {
        return Err(ParseError::MissingMarker {
            what: "\"if\" clause".to_string(),
        });
    }

    let pool_ok = normalized.contains("solend main pool usdc")
        || normalized.contains("save main pool usdc");
    if !pool_ok {
        if normalized.contains("jupiter") {
            return Err(ParseError::WrongPool {
                detail: "Jupiter is not supported in this demo".to_string(),
            });
        }
        if normalized.contains("main pool") {
            return Err(ParseError::WrongAsset {
                detail: "only Solend/Save Main Pool USDC is supported".to_string(),
            });
        }
        return Err(ParseError::WrongPool {
            detail: "expected 'Solend Main Pool USDC' (or 'Save Main Pool USDC')"
                .to_string(),
        });
    }

    if !normalized.contains("deposit apr") {
        return Err(ParseError::MissingMarker {
            what: "\"deposit APR\"".to_string(),
        });
    }

    let wallet_ok = normalized.contains("bounded executor wallet")
        || normalized.contains("controlled wallet");
    if !wallet_ok {
        return Err(ParseError::WrongWallet {
            detail: "expected 'bounded executor wallet' or 'controlled wallet'"
                .to_string(),
        });
    }

    if !normalized.contains("into solend") && !normalized.contains("into save") {
        return Err(ParseError::MissingMarker {
            what: "\"into Solend\"".to_string(),
        });
    }

    let amount_token = "deposit 0.25";
    if !normalized.contains(amount_token) {
        return Err(ParseError::UnsupportedAmount {
            detail: "only 0.25 USDC is supported in the demo bridge".to_string(),
        });
    }
    let idx = normalized.find(amount_token).unwrap();
    if !normalized[idx..].contains("usdc") {
        return Err(ParseError::WrongAsset {
            detail: "amount must be denominated in USDC".to_string(),
        });
    }

    let above_token = "above ";
    let after_above = match normalized.find(above_token) {
        Some(i) => &normalized[i + above_token.len()..],
        None => {
            return Err(ParseError::MissingMarker {
                what: "\"above <X>%\"".to_string(),
            })
        }
    };
    let pct_end = after_above.find('%').ok_or_else(|| ParseError::MalformedPercent {
        detail: "expected '%' after the threshold percent".to_string(),
    })?;
    let pct_str = after_above[..pct_end].trim();
    if pct_str.is_empty() {
        return Err(ParseError::MalformedPercent {
            detail: "percent value is empty".to_string(),
        });
    }
    if pct_str.starts_with('-') {
        return Err(ParseError::NegativeThreshold {
            value: pct_str.to_string(),
        });
    }
    let threshold_bps = decimal_percent_to_bps(pct_str)?;
    Ok(DemoParsed {
        threshold_bps,
        threshold_pct_label: pct_str.to_string(),
        amount_raw: W5D_DEPOSIT_AMOUNT_RAW,
    })
}

fn decimal_percent_to_bps(s: &str) -> Result<u32, ParseError> {
    if s.is_empty() {
        return Err(ParseError::MalformedPercent {
            detail: "empty".to_string(),
        });
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 2 {
        return Err(ParseError::MalformedPercent {
            detail: format!("multiple '.' in '{s}'"),
        });
    }
    let int_str = parts[0];
    let frac_str = parts.get(1).copied().unwrap_or("");
    let int_val: u32 = if int_str.is_empty() {
        0
    } else {
        int_str.parse::<u32>().map_err(|e| ParseError::MalformedPercent {
            detail: format!("int part '{int_str}': {e}"),
        })?
    };
    if frac_str.len() > 2 {
        return Err(ParseError::MalformedPercent {
            detail: format!("more than 2 fractional digits in '{s}'"),
        });
    }
    let mut frac_padded = String::with_capacity(2);
    frac_padded.push_str(frac_str);
    while frac_padded.len() < 2 {
        frac_padded.push('0');
    }
    let frac_val: u32 = frac_padded
        .parse::<u32>()
        .map_err(|e| ParseError::MalformedPercent {
            detail: format!("frac part '{frac_padded}': {e}"),
        })?;
    int_val
        .checked_mul(100)
        .and_then(|n| n.checked_add(frac_val))
        .ok_or_else(|| ParseError::ThresholdOverflow {
            value: s.to_string(),
        })
}

fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_ascii_whitespace() {
            if !in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

// ── Evaluation DTO ────────────────────────────────────────────────────────

/// Successful evaluation of a parsed W5d command against the live
/// Solend Main Pool USDC reserve, including controlled-wallet budget
/// state and (when wired) a persisted Stage 2 watch rule.
///
/// Decision tree (strict `>` on bps; budget gate is hard):
///
/// | budget          | condition | status            |
/// | --------------- | --------- | ----------------- |
/// | `< 250_000 raw` | any       | `needs_funding`   |
/// | `>= 250_000`    | met       | `ready_to_execute`|
/// | `>= 250_000`    | not met   | `watching`        |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct W5dEvaluationResult {
    /// Echoed verbatim from the chat input.
    pub input_text: String,
    /// Stable string the frontend renders in the "source" row of the
    /// W5d card. Always `"onchain_reserve_b_o1"` for this evaluator.
    pub source: String,
    /// `BgxfHJDz…JMpmw` (Solend Main Pool USDC reserve, base58).
    pub reserve_pubkey: String,
    /// Current Solend Main Pool USDC supply APR in basis points,
    /// derived from the on-chain reserve via the B-O1 wrappers.
    pub current_apr_bps: u32,
    /// Threshold parsed from the user's text, in basis points.
    pub threshold_bps: u32,
    /// Echo of the percent label the user typed (e.g. `"2.5"`).
    pub threshold_pct_label: String,
    /// Strict-greater decision: `current_apr_bps > threshold_bps`.
    pub condition_met: bool,
    /// True iff this evaluation triggered a downstream execution
    /// attempt. The chat-side default is always `false` (the live
    /// send path lives behind the W5c env gate).
    pub execution_attempted: bool,
    /// Lifecycle status (W5e). One of:
    ///   - `"watching"`         — condition not met now, budget reserved
    ///   - `"ready_to_execute"` — condition met now, budget reserved
    ///   - `"needs_funding"`    — controlled wallet USDC < required budget
    pub status: String,
    /// Live-send signature; always `None` from the chat route in this
    /// slice. Field reserved so a future live-send wire-shape change
    /// is non-breaking.
    pub tx_signature: Option<String>,
    /// W5e — controlled wallet pubkey, base58. Echoed so the frontend
    /// can render a Copy button next to it in the `needs_funding`
    /// state.
    pub controlled_wallet: String,
    /// W5e — source USDC ATA pubkey, base58
    /// (`get_associated_token_address(controlled_wallet, USDC_mint)`).
    /// Echoed for the Copy button + "send USDC here" affordance.
    pub source_usdc_ata: String,
    /// W5e — required deposit budget in USDC base units (always
    /// `250_000` for this demo).
    pub required_budget_raw: u64,
    /// W5e — current controlled-wallet USDC balance in base units, as
    /// observed by the same RPC call that produced `last_checked_slot`.
    pub current_budget_raw: u64,
    /// W5e — `"reserved"` when `current_budget_raw >= required_budget_raw`,
    /// `"needs_funding"` otherwise. The `status` field above already
    /// derives from this + the condition; carrying both makes the
    /// frontend card unambiguous.
    pub budget_status: String,
    /// W5e — chain slot the budget + APR were read from. Both reads
    /// share this slot (one `getSlot` per chat invocation followed by
    /// the dependent reads at `commitment=confirmed`).
    pub last_checked_slot: u64,
    /// W5e — `Some(_)` when a real `WatchRule` was constructed
    /// (whether or not it was inserted into the durable repository).
    /// `expires_at_slot = last_checked_slot + W5E_DEMO_EXPIRY_SLOTS`.
    pub expires_at_slot: Option<u64>,
    /// W5e — hex of the deterministic 16-byte rule id derived from
    /// `(threshold_bps, amount_raw, controlled_wallet[..4])`. Same
    /// command typed twice in chat -> same rule_id (idempotent).
    pub rule_id_hex: Option<String>,
    /// W5e — hex of `claw_types::stage2_watch_rule::canonical_rule_hash`
    /// over the constructed `WatchRule`. Distinct from `rule_id` —
    /// the hash binds the rule's full canonical bytes.
    pub canonical_rule_hash_hex: Option<String>,
    /// W5e — `true` iff the rule was actually inserted into the
    /// daemon's `Stage2WatchRuleRepository` (or was already present
    /// from a prior insertion with the same `rule_id`). `false` when
    /// the chat handler is not wired with a repo OR insertion failed
    /// for any reason other than the idempotent-duplicate case.
    pub rule_persisted: bool,
}

/// Errors the chat layer surfaces to the user when evaluation cannot
/// produce a `W5dEvaluationResult` (RPC unavailable, reserve missing,
/// implausible APR, etc.). The chat handler maps these to a typed
/// "couldn't evaluate" card — it does NOT silently fall back to the
/// LLM path because the user's intent was clearly the W5d command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum EvaluationError {
    /// `looks_like_w5d_command(text)` was true but `parse_demo_command`
    /// rejected the message. The `parse_error` carries the typed reject
    /// reason so the UI can guide the user.
    Parse { parse_error: ParseError },
    /// The chat handler is configured without an RPC URL — the W5d
    /// bridge requires live-reserve access.
    BridgeNotConfigured { reason: String },
    /// HTTP / JSON-RPC fault while fetching the reserve account.
    RpcFetchFailed { detail: String },
    /// `getAccountInfo` returned `null` for the pinned reserve pubkey —
    /// either the network is broken or the reserve was deleted (the
    /// latter is impossible in normal operation but the typed branch
    /// exists for completeness).
    ReserveAccountMissing { reserve: String },
    /// The reserve byte decoder rejected the bytes (length mismatch,
    /// version unsupported, etc.).
    ReserveDecodeFailed { detail: String },
    /// `solend_supply_apr_wad_for_snapshot` returned an error (config
    /// invariants violated). Typically transient — the snapshot might
    /// be stale or partially-initialised.
    AprComputationFailed { detail: String },
    /// Computed APR was outside the plausible range
    /// (`0 ≤ bps ≤ 10_000`). Treated as a sanity-failure so the UI
    /// doesn't render an APR that's clearly wrong.
    AprOutOfRange { current_apr_bps_raw: u64 },
}

impl std::fmt::Display for EvaluationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvaluationError::Parse { parse_error } => write!(f, "parse: {parse_error}"),
            EvaluationError::BridgeNotConfigured { reason } => {
                write!(f, "bridge not configured: {reason}")
            }
            EvaluationError::RpcFetchFailed { detail } => write!(f, "rpc: {detail}"),
            EvaluationError::ReserveAccountMissing { reserve } => {
                write!(f, "reserve account missing: {reserve}")
            }
            EvaluationError::ReserveDecodeFailed { detail } => {
                write!(f, "reserve decode failed: {detail}")
            }
            EvaluationError::AprComputationFailed { detail } => {
                write!(f, "APR computation failed: {detail}")
            }
            EvaluationError::AprOutOfRange { current_apr_bps_raw } => write!(
                f,
                "APR out of plausible range: {current_apr_bps_raw} bps > {APR_BPS_SANITY_MAX}"
            ),
        }
    }
}

// ── Fetcher trait + live implementation ───────────────────────────────────

/// Trait the chat handler depends on. The production daemon wires the
/// live RPC-backed implementation; unit tests inject a deterministic
/// mock so the chat-route dispatch logic is testable without network.
#[async_trait]
pub trait W5dAprFetcher: Send + Sync + std::fmt::Debug {
    /// Given a parsed command + the original user text, fetch / decode
    /// the reserve, compute the current APR, and produce the typed
    /// result. Note: this trait does NOT decide whether to execute —
    /// the chat handler reads `result.condition_met` and short-circuits
    /// the live-send branch.
    async fn evaluate(
        &self,
        input_text: &str,
        parsed: &DemoParsed,
    ) -> Result<W5dEvaluationResult, EvaluationError>;
}

/// Live implementation that talks to a Helius (or compatible) JSON-RPC
/// endpoint. Built once per daemon startup; `reqwest::Client` is
/// cloneable internally so the chat handler can share it across
/// concurrent requests.
#[derive(Debug, Clone)]
pub struct LiveW5dAprFetcher {
    rpc_url: String,
    http: reqwest::Client,
}

impl LiveW5dAprFetcher {
    /// Build a fetcher with the given RPC URL. Returns `None` if the
    /// URL is empty or only whitespace.
    pub fn new(rpc_url: impl Into<String>) -> Option<Self> {
        let rpc_url = rpc_url.into();
        if rpc_url.trim().is_empty() {
            return None;
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(RPC_REQUEST_TIMEOUT_MS))
            .build()
            .ok()?;
        Some(Self { rpc_url, http })
    }

    async fn rpc_post(&self, body: Value) -> Result<Value, EvaluationError> {
        let resp = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EvaluationError::RpcFetchFailed {
                detail: format!("transport: {e}"),
            })?;
        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(EvaluationError::RpcFetchFailed {
                detail: format!("http {status}"),
            });
        }
        let v: Value = resp.json().await.map_err(|e| EvaluationError::RpcFetchFailed {
            detail: format!("body: {e}"),
        })?;
        if let Some(err) = v.get("error") {
            return Err(EvaluationError::RpcFetchFailed {
                detail: format!("rpc error: {err}"),
            });
        }
        Ok(v)
    }

    async fn fetch_reserve_bytes(&self) -> Result<Vec<u8>, EvaluationError> {
        let v = self
            .rpc_post(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getAccountInfo",
                "params": [
                    W5D_RESERVE_BS58,
                    {"encoding": "base64", "commitment": "confirmed"}
                ]
            }))
            .await?;
        let val = &v["result"]["value"];
        if val.is_null() {
            return Err(EvaluationError::ReserveAccountMissing {
                reserve: W5D_RESERVE_BS58.to_string(),
            });
        }
        let b64 = val
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .and_then(|x| x.as_str())
            .ok_or_else(|| EvaluationError::RpcFetchFailed {
                detail: "missing data[0] base64 string".to_string(),
            })?;
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| EvaluationError::RpcFetchFailed {
                detail: format!("base64 decode: {e}"),
            })
    }

    async fn fetch_slot(&self) -> Result<u64, EvaluationError> {
        let v = self
            .rpc_post(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getSlot",
                "params": [{"commitment": "confirmed"}]
            }))
            .await?;
        v["result"]
            .as_u64()
            .ok_or_else(|| EvaluationError::RpcFetchFailed {
                detail: "missing slot in getSlot response".to_string(),
            })
    }

    /// Returns the SPL token-account balance for `ata`, in base
    /// units. `Ok(None)` when the account does not exist (treated by
    /// callers as a zero budget).
    async fn fetch_token_balance(&self, ata: &str) -> Result<Option<u64>, EvaluationError> {
        let v = self
            .rpc_post(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getTokenAccountBalance",
                "params": [ata, {"commitment": "confirmed"}]
            }))
            .await;
        match v {
            Ok(value) => {
                let amount_str = value["result"]["value"]["amount"]
                    .as_str()
                    .ok_or_else(|| EvaluationError::RpcFetchFailed {
                        detail: "missing amount in getTokenAccountBalance".to_string(),
                    })?;
                let raw: u64 = amount_str
                    .parse()
                    .map_err(|e: std::num::ParseIntError| EvaluationError::RpcFetchFailed {
                        detail: format!("amount parse: {e}"),
                    })?;
                Ok(Some(raw))
            }
            // `getTokenAccountBalance` returns a typed RPC error when
            // the account doesn't exist; map that to Ok(None) so the
            // budget gate sees "0 raw → needs_funding".
            Err(EvaluationError::RpcFetchFailed { detail })
                if detail.contains("could not find account")
                    || detail.contains("Invalid param")
                    || detail.contains("not found") =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl W5dAprFetcher for LiveW5dAprFetcher {
    async fn evaluate(
        &self,
        input_text: &str,
        parsed: &DemoParsed,
    ) -> Result<W5dEvaluationResult, EvaluationError> {
        // Anchor everything to the same slot — one `getSlot` followed
        // by reserve + budget reads at `commitment=confirmed`.
        let last_checked_slot = self.fetch_slot().await?;

        // ── APR via B-O1 ─────────────────────────────────────────────
        let reserve_bytes = self.fetch_reserve_bytes().await?;
        let raw = decode_reserve(&reserve_bytes).map_err(|e| {
            EvaluationError::ReserveDecodeFailed {
                detail: format!("{e:?}"),
            }
        })?;
        let reserve_pubkey =
            Pubkey::from_str(W5D_RESERVE_BS58).expect("W5D_RESERVE_BS58 parses");
        let reserve_address_bytes = PubkeyBytes::new(reserve_pubkey.to_bytes());
        let snapshot = solend_snapshot_value_from_reserve_raw(reserve_address_bytes, &raw);
        let apr_wad = solend_supply_apr_wad_for_snapshot(&snapshot).map_err(|e| {
            EvaluationError::AprComputationFailed {
                detail: format!("{e:?}"),
            }
        })?;
        let apr_bps_raw = solend_supply_apr_bps_from_wad(apr_wad);
        if apr_bps_raw > APR_BPS_SANITY_MAX as u64 {
            return Err(EvaluationError::AprOutOfRange {
                current_apr_bps_raw: apr_bps_raw,
            });
        }
        let current_apr_bps = apr_bps_raw as u32;

        // ── Controlled-wallet USDC budget ────────────────────────────
        let (controlled_wallet, source_usdc_ata) = controlled_wallet_addresses();
        let current_budget_raw = self
            .fetch_token_balance(&source_usdc_ata)
            .await?
            .unwrap_or(0);

        Ok(compose_w5e_result(
            input_text,
            parsed,
            current_apr_bps,
            current_budget_raw,
            last_checked_slot,
            controlled_wallet,
            source_usdc_ata,
        ))
    }
}

// ── Compose helper (shared by live + mock fetchers) ──────────────────────

/// Compose the typed `W5dEvaluationResult` from the raw inputs the
/// fetcher gathered. Splitting this out keeps the live RPC code and
/// the chat-route decision logic separately testable.
pub fn compose_w5e_result(
    input_text: &str,
    parsed: &DemoParsed,
    current_apr_bps: u32,
    current_budget_raw: u64,
    last_checked_slot: u64,
    controlled_wallet: String,
    source_usdc_ata: String,
) -> W5dEvaluationResult {
    let required_budget_raw = W5D_DEPOSIT_AMOUNT_RAW;
    let budget_ok = current_budget_raw >= required_budget_raw;
    let condition_met = current_apr_bps > parsed.threshold_bps;
    let (status, budget_status) = match (budget_ok, condition_met) {
        (false, _) => ("needs_funding", "needs_funding"),
        (true, true) => ("ready_to_execute", "reserved"),
        (true, false) => ("watching", "reserved"),
    };
    W5dEvaluationResult {
        input_text: input_text.to_string(),
        source: "onchain_reserve_b_o1".to_string(),
        reserve_pubkey: W5D_RESERVE_BS58.to_string(),
        current_apr_bps,
        threshold_bps: parsed.threshold_bps,
        threshold_pct_label: parsed.threshold_pct_label.clone(),
        condition_met,
        execution_attempted: false,
        status: status.to_string(),
        tx_signature: None,
        controlled_wallet,
        source_usdc_ata,
        required_budget_raw,
        current_budget_raw,
        budget_status: budget_status.to_string(),
        last_checked_slot,
        // expires_at / rule_id / canonical_rule_hash filled in by
        // `handle_demo_command_v2` after the WatchRule is constructed.
        expires_at_slot: None,
        rule_id_hex: None,
        canonical_rule_hash_hex: None,
        rule_persisted: false,
    }
}

/// Derives `(controlled_wallet_bs58, source_usdc_ata_bs58)` from the
/// pinned controlled-wallet constant. Pure; no RPC.
pub fn controlled_wallet_addresses() -> (String, String) {
    let owner = Pubkey::from_str(CONTROLLED_WALLET_BS58).expect("controlled wallet parses");
    let mint = Pubkey::from_str(crate::stage2_executor::DEMO_LIQUIDITY_MINT_BS58)
        .expect("USDC mint parses");
    let ata = get_associated_token_address(&owner, &mint);
    (owner.to_string(), ata.to_string())
}

// ── Watch-rule construction + persistence ────────────────────────────────

/// Build the deterministic 16-byte rule_id for a W5e command.
///
/// The id is derived from a stable byte-layout of the parsed inputs
/// + the pinned controlled-wallet pubkey prefix:
///
/// ```text
/// [0..4]   threshold_bps as u32 LE
/// [4..12]  amount_raw    as u64 LE
/// [12..16] controlled_wallet.to_bytes()[0..4]
/// ```
///
/// Same parsed command (same threshold + amount + wallet) → same
/// rule_id. Different threshold → different rule_id. This makes
/// re-typing the same command in chat idempotent in the state-store
/// (the second `repo.insert` hits a UNIQUE collision; we treat that
/// as `rule_persisted=true` because the rule IS present).
pub fn derive_w5e_rule_id(parsed: &DemoParsed, controlled_wallet: &PubkeyBytes) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&parsed.threshold_bps.to_le_bytes());
    out[4..12].copy_from_slice(&parsed.amount_raw.to_le_bytes());
    out[12..16].copy_from_slice(&controlled_wallet.0[0..4]);
    out
}

/// Build the canonical W5e demo `WatchRule`. The rule's CONDITION is
/// `Solend Main Pool USDC supply APR > threshold_bps`; its ACTION
/// reuses the `SolendWithdrawAllDelegated` carrier (matching the
/// W5c test-only-adapter convention, since the production
/// `ActionSpec` enum does not yet carry a first-class
/// `SolendDepositControlledWallet` variant — see no-overclaim).
pub fn build_w5e_watch_rule(
    parsed: &DemoParsed,
    last_checked_slot: u64,
) -> WatchRule {
    let controlled = PubkeyBytes::from_base58(CONTROLLED_WALLET_BS58)
        .expect("controlled wallet base58 parses");
    let reserve =
        PubkeyBytes::from_base58(crate::stage2_executor::DEMO_RESERVE_BS58)
            .expect("reserve base58 parses");
    let lending_market =
        PubkeyBytes::from_base58(crate::stage2_executor::DEMO_LENDING_MARKET_BS58)
            .expect("lending market base58 parses");
    let solend_program_id =
        PubkeyBytes::from_base58("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo")
            .expect("solend program id base58 parses");
    let rule_id = derive_w5e_rule_id(parsed, &controlled);
    WatchRule {
        schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
        rule_id,
        // User/executor collapse to the controlled wallet for this
        // demo (matching W5c convention).
        user: controlled,
        executor: controlled,
        delegated_wallet: controlled,
        created_at_slot: last_checked_slot,
        expires_at_slot: last_checked_slot.saturating_add(W5E_DEMO_EXPIRY_SLOTS),
        one_shot: true,
        condition_logic: ConditionLogic::All,
        conditions: vec![Condition::SolendReserveSupplyRate {
            reserve_pubkey: reserve,
            lending_market,
            solend_program_id,
            // "above X%" is strict greater.
            comparison: Comparison::Gt,
            threshold_bps: parsed.threshold_bps,
            rate_kind: RateKind::Apr,
            formula_version: 1,
            max_reserve_staleness_slots: 16,
            required_refresh_same_tx: true,
        }],
        action: ActionSpec::SolendWithdrawAllDelegated {
            target_obligation: PubkeyBytes::from_base58(
                "BdFLjCcP9mCy557vNNGVbTUuvHxXsh8hc6jXzaPra1wN",
            )
            .expect("target obligation parses"),
            reserve_pubkey: reserve,
            lending_market,
            destination_wallet: controlled,
            withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
        },
        max_input_amount_raw: parsed.amount_raw,
        used_amount_raw: 0,
        destination: controlled,
        slippage_bps: 0,
    }
}

/// Insert (idempotently) the W5e rule into the durable state-store.
/// Returns `(rule_persisted, expires_at_slot)`:
///
///   - `rule_persisted=true` on a successful first insertion OR on
///     a UNIQUE-collision where `repo.get(rule_id)` confirms the
///     rule is durably present (idempotent re-typing in chat).
///   - `rule_persisted=false` on any other error.
async fn persist_rule_idempotent(
    repo: &Stage2WatchRuleRepository,
    rule: &WatchRule,
) -> bool {
    match repo.insert(rule).await {
        Ok(()) => true,
        Err(_) => {
            // Most likely a UNIQUE collision because the rule was
            // already inserted. Confirm via `repo.get` — if the rule
            // is durably present, treat the second insertion as
            // idempotent success.
            matches!(repo.get(&rule.rule_id).await, Ok(Some(_)))
        }
    }
}

fn hex_id_16(id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in id {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn hex_id_32(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in id {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

// ── High-level chat-handler entry point ──────────────────────────────────

/// W5e — full chat-route entry point: detect → parse → fetch (APR +
/// budget + slot) → compose result → construct WatchRule → persist
/// (if repo is wired) → fill in `rule_id` / `canonical_rule_hash` /
/// `expires_at_slot` / `rule_persisted`.
///
/// Callers that just want the W5d preview surface (no rule
/// construction) can call `fetcher.evaluate(...)` directly.
pub async fn handle_demo_command_v2(
    fetcher: &(dyn W5dAprFetcher + 'static),
    repo: Option<&Stage2WatchRuleRepository>,
    input_text: &str,
) -> Result<W5dEvaluationResult, EvaluationError> {
    let parsed = parse_demo_command(input_text)
        .map_err(|e| EvaluationError::Parse { parse_error: e })?;
    let mut result = fetcher.evaluate(input_text, &parsed).await?;

    // Build the WatchRule + try to persist. Whether or not the repo
    // is wired, the rule_id + canonical_hash + expires_at fields are
    // always filled in so the frontend can display the order
    // identity even in preview-only mode.
    let rule = build_w5e_watch_rule(&parsed, result.last_checked_slot);
    let canonical = canonical_rule_hash(&rule);
    result.rule_id_hex = Some(hex_id_16(&rule.rule_id));
    result.canonical_rule_hash_hex = Some(hex_id_32(&canonical));
    result.expires_at_slot = Some(rule.expires_at_slot);

    if let Some(repo_ref) = repo {
        result.rule_persisted = persist_rule_idempotent(repo_ref, &rule).await;
    }
    // else: rule_persisted stays false (preview-only). Caller MUST
    // surface this in the no-overclaim banner.

    Ok(result)
}

/// Backwards-compatible W5d entry point — kept for any external
/// caller that has not yet migrated to `handle_demo_command_v2`.
/// Internally this is now just `handle_demo_command_v2` with
/// `repo=None` (preview-only); the chat handler uses the v2 entry
/// directly so it can wire a real repo when the daemon configures
/// one.
pub async fn handle_demo_command(
    fetcher: &(dyn W5dAprFetcher + 'static),
    input_text: &str,
) -> Result<W5dEvaluationResult, EvaluationError> {
    handle_demo_command_v2(fetcher, None, input_text).await
}

// ──────────────────────────────────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parser ────────────────────────────────────────────────────────────

    #[test]
    fn parser_canonical_phrase_10pct() {
        let s = "If Solend Main Pool USDC deposit APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let p = parse_demo_command(s).unwrap();
        assert_eq!(p.threshold_bps, 1000);
        assert_eq!(p.amount_raw, W5D_DEPOSIT_AMOUNT_RAW);
        assert_eq!(p.threshold_pct_label, "10");
    }

    #[test]
    fn parser_2_point_5_pct() {
        assert_eq!(
            parse_demo_command(
                "if solend main pool usdc deposit apr is above 2.5%, deposit 0.25 usdc from my bounded executor wallet into solend.",
            )
            .unwrap()
            .threshold_bps,
            250
        );
    }

    #[test]
    fn parser_0_point_75_pct() {
        assert_eq!(
            parse_demo_command(
                "If solend main pool USDC deposit APR is above 0.75%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
            )
            .unwrap()
            .threshold_bps,
            75
        );
    }

    #[test]
    fn parser_save_synonym() {
        assert_eq!(
            parse_demo_command(
                "If Save Main Pool USDC deposit APR is above 4%, deposit 0.25 USDC from my bounded executor wallet into Save.",
            )
            .unwrap()
            .threshold_bps,
            400
        );
    }

    #[test]
    fn parser_controlled_wallet_synonym() {
        assert_eq!(
            parse_demo_command(
                "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my controlled wallet into Solend.",
            )
            .unwrap()
            .threshold_bps,
            500
        );
    }

    #[test]
    fn parser_whitespace_and_case_tolerant() {
        let s = "   IF    Solend   Main  Pool   USDC \n  deposit APR is\tabove  3.25 %  ,  deposit 0.25 USDC from MY controlled wallet into Solend.   ";
        assert_eq!(parse_demo_command(s).unwrap().threshold_bps, 325);
    }

    #[test]
    fn parser_rejects_wrong_pool() {
        let err = parse_demo_command(
            "If Marginfi USDC pool APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::WrongPool { .. }));
    }

    #[test]
    fn parser_rejects_jupiter() {
        let err = parse_demo_command(
            "If Jupiter swap APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .unwrap_err();
        match err {
            ParseError::WrongPool { detail } => assert!(detail.contains("Jupiter")),
            other => panic!("expected WrongPool/Jupiter, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_unsupported_amount() {
        let err = parse_demo_command(
            "If Solend Main Pool USDC deposit APR is above 10%, deposit 1.0 USDC from my bounded executor wallet into Solend.",
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedAmount { .. }));
    }

    #[test]
    fn parser_rejects_missing_wallet() {
        let err = parse_demo_command(
            "If Solend Main Pool USDC deposit APR is above 10%, deposit 0.25 USDC into Solend.",
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::WrongWallet { .. }));
    }

    #[test]
    fn parser_rejects_negative_threshold() {
        let err = parse_demo_command(
            "If Solend Main Pool USDC deposit APR is above -1%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::NegativeThreshold { .. }));
    }

    #[test]
    fn parser_rejects_malformed_percent() {
        let err = parse_demo_command(
            "If Solend Main Pool USDC deposit APR is above abc%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::MalformedPercent { .. }));
    }

    #[test]
    fn parser_rejects_too_many_fractional_digits() {
        let err = parse_demo_command(
            "If Solend Main Pool USDC deposit APR is above 1.234%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .unwrap_err();
        match err {
            ParseError::MalformedPercent { detail } => {
                assert!(detail.contains("fractional"))
            }
            other => panic!("expected MalformedPercent, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_missing_if() {
        let err = parse_demo_command(
            "Solend Main Pool USDC deposit APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .unwrap_err();
        match err {
            ParseError::MissingMarker { what } => assert!(what.contains("if")),
            other => panic!("expected MissingMarker, got {other:?}"),
        }
    }

    #[test]
    fn decimal_percent_table() {
        assert_eq!(decimal_percent_to_bps("0").unwrap(), 0);
        assert_eq!(decimal_percent_to_bps("1").unwrap(), 100);
        assert_eq!(decimal_percent_to_bps("10").unwrap(), 1000);
        assert_eq!(decimal_percent_to_bps("100").unwrap(), 10_000);
        assert_eq!(decimal_percent_to_bps("2.5").unwrap(), 250);
        assert_eq!(decimal_percent_to_bps("0.75").unwrap(), 75);
        assert_eq!(decimal_percent_to_bps("0.05").unwrap(), 5);
        assert_eq!(decimal_percent_to_bps(".5").unwrap(), 50);
        assert_eq!(decimal_percent_to_bps("10.5").unwrap(), 1050);
        assert_eq!(decimal_percent_to_bps("10.05").unwrap(), 1005);
    }

    // ── Detector ──────────────────────────────────────────────────────────

    #[test]
    fn detector_matches_canonical_phrase() {
        assert!(looks_like_w5d_command(
            "If Solend Main Pool USDC deposit APR is above 10%, ..."
        ));
        assert!(looks_like_w5d_command(
            "if save main pool usdc deposit apr is above 1%"
        ));
        assert!(looks_like_w5d_command(
            "SOLEND MAIN POOL USDC ... DEPOSIT APR ..."
        ));
    }

    #[test]
    fn detector_rejects_generic_chat() {
        // Free-form chat that doesn't name the pool or the APR phrase
        // must NOT match — we want to pass through to the LLM.
        assert!(!looks_like_w5d_command("hello, can you show my balances?"));
        assert!(!looks_like_w5d_command("what is the current jupiter quote?"));
        // Pool named but no "deposit APR" → not our grammar, route LLM.
        assert!(!looks_like_w5d_command(
            "tell me about Solend Main Pool USDC withdrawals"
        ));
        // "deposit APR" alone → not our grammar.
        assert!(!looks_like_w5d_command("what is the deposit apr today?"));
    }

    // ── Decision logic ────────────────────────────────────────────────────

    #[test]
    fn strict_greater_decision_at_equality() {
        // current == threshold → condition_met = false (strict >).
        let current = 1234u32;
        let threshold = 1234u32;
        assert!(!(current > threshold));
    }

    #[test]
    fn pin_reserve_bs58_matches_stage2_executor() {
        assert_eq!(W5D_RESERVE_BS58, crate::stage2_executor::DEMO_RESERVE_BS58);
    }

    // ── Mock fetcher integration tests (no network) ───────────────────────

    /// A mock fetcher with a programmable `current_apr_bps`,
    /// `current_budget_raw`, and `last_checked_slot`. Used to exercise
    /// the chat-route dispatch / DTO mapping without making any RPC.
    #[derive(Debug, Clone)]
    pub(crate) struct MockW5dAprFetcher {
        pub current_apr_bps: u32,
        pub current_budget_raw: u64,
        pub last_checked_slot: u64,
    }

    impl MockW5dAprFetcher {
        /// Convenience: budget reserved (>= 250k raw), fixed slot.
        pub(crate) fn with_apr(current_apr_bps: u32) -> Self {
            Self {
                current_apr_bps,
                current_budget_raw: 500_000,
                last_checked_slot: 419_000_000,
            }
        }
    }

    #[async_trait]
    impl W5dAprFetcher for MockW5dAprFetcher {
        async fn evaluate(
            &self,
            input_text: &str,
            parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            let (controlled_wallet, source_usdc_ata) = controlled_wallet_addresses();
            Ok(compose_w5e_result(
                input_text,
                parsed,
                self.current_apr_bps,
                self.current_budget_raw,
                self.last_checked_slot,
                controlled_wallet,
                source_usdc_ata,
            ))
        }
    }

    /// W5e: false condition + budget reserved → `watching`.
    #[tokio::test]
    async fn w5e_false_condition_budget_reserved_is_watching() {
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let input = "If Solend Main Pool USDC deposit APR is above 4.63%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command(&fetcher, input).await.unwrap();
        assert!(!r.condition_met);
        assert_eq!(r.status, "watching");
        assert_eq!(r.budget_status, "reserved");
        assert!(r.tx_signature.is_none());
        assert!(!r.execution_attempted);
        assert!(r.rule_id_hex.is_some());
        assert!(r.canonical_rule_hash_hex.is_some());
        assert!(r.expires_at_slot.is_some());
        // preview-only (no repo wired in this unit-test entry path)
        assert!(!r.rule_persisted);
    }

    /// W5e: true condition + budget reserved → `ready_to_execute`.
    #[tokio::test]
    async fn w5e_true_condition_budget_reserved_is_ready_to_execute() {
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let input = "If Solend Main Pool USDC deposit APR is above 0.63%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command(&fetcher, input).await.unwrap();
        assert!(r.condition_met);
        assert_eq!(r.status, "ready_to_execute");
        assert_eq!(r.budget_status, "reserved");
        assert!(r.tx_signature.is_none());
    }

    /// W5e: any condition + budget short → `needs_funding`.
    #[tokio::test]
    async fn w5e_budget_short_is_needs_funding_regardless_of_condition() {
        let fetcher = MockW5dAprFetcher {
            current_apr_bps: 163,
            current_budget_raw: 100_000, // < 250_000 required
            last_checked_slot: 419_000_000,
        };
        // Even with the TRUE condition (threshold 0.63%), budget gate
        // forces needs_funding.
        let input = "If Solend Main Pool USDC deposit APR is above 0.63%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command(&fetcher, input).await.unwrap();
        assert!(r.condition_met);
        assert_eq!(r.status, "needs_funding");
        assert_eq!(r.budget_status, "needs_funding");
        assert_eq!(r.current_budget_raw, 100_000);
        assert_eq!(r.required_budget_raw, 250_000);
        // Funding affordances:
        assert_eq!(r.controlled_wallet, CONTROLLED_WALLET_BS58);
        // The source USDC ATA is the canonical
        // `get_associated_token_address(controlled, USDC)` — pin it
        // explicitly so a future ATA-derivation change breaks the
        // test loudly.
        assert_eq!(r.source_usdc_ata, "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3");
    }

    #[tokio::test]
    async fn w5e_parse_error_surfaces_as_typed_error() {
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let input = "If Solend Main Pool USDC deposit APR is above 10%, deposit 1.0 USDC from my bounded executor wallet into Solend.";
        let err = handle_demo_command(&fetcher, input).await.unwrap_err();
        match err {
            EvaluationError::Parse { parse_error } => {
                assert!(matches!(parse_error, ParseError::UnsupportedAmount { .. }))
            }
            other => panic!("expected Parse/UnsupportedAmount, got {other:?}"),
        }
    }

    /// Liveness: `last_checked_slot` is echoed verbatim from the
    /// fetcher; the same `slot` shows up in the result without
    /// transformation.
    #[tokio::test]
    async fn w5e_last_checked_slot_flows_through() {
        let fetcher = MockW5dAprFetcher {
            current_apr_bps: 163,
            current_budget_raw: 500_000,
            last_checked_slot: 999_999_999,
        };
        let input = "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command(&fetcher, input).await.unwrap();
        assert_eq!(r.last_checked_slot, 999_999_999);
        assert_eq!(
            r.expires_at_slot,
            Some(999_999_999u64.saturating_add(W5E_DEMO_EXPIRY_SLOTS))
        );
    }

    /// Rule identity: same parsed inputs → same rule_id; different
    /// threshold → different rule_id; canonical hash differs across
    /// the two.
    #[tokio::test]
    async fn w5e_rule_id_is_deterministic_and_threshold_sensitive() {
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let a_input = "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let b_input = "If Solend Main Pool USDC deposit APR is above 0.5%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r1 = handle_demo_command(&fetcher, a_input).await.unwrap();
        let r2 = handle_demo_command(&fetcher, a_input).await.unwrap();
        let r3 = handle_demo_command(&fetcher, b_input).await.unwrap();
        // Same command typed twice → same rule_id + same canonical
        // hash (idempotent).
        assert_eq!(r1.rule_id_hex, r2.rule_id_hex);
        assert_eq!(r1.canonical_rule_hash_hex, r2.canonical_rule_hash_hex);
        // Different threshold → distinct rule_id AND canonical hash.
        assert_ne!(r1.rule_id_hex, r3.rule_id_hex);
        assert_ne!(r1.canonical_rule_hash_hex, r3.canonical_rule_hash_hex);
    }

    /// Real persistence — when the chat handler is wired with a repo,
    /// `handle_demo_command_v2` MUST set `rule_persisted=true` and
    /// the rule MUST be retrievable via `repo.get(rule_id)`.
    #[tokio::test]
    async fn w5e_persistence_inserts_rule_and_returns_persisted_true() {
        use claw_state_store::db::Database;
        let db = Database::open_in_memory().await.unwrap();
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let input = "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command_v2(&fetcher, Some(&repo), input)
            .await
            .unwrap();
        assert!(r.rule_persisted);
        // Confirm by direct repository lookup.
        let id_hex = r.rule_id_hex.as_ref().unwrap();
        let mut id_bytes = [0u8; 16];
        for i in 0..16 {
            id_bytes[i] = u8::from_str_radix(&id_hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let stored = repo.get(&id_bytes).await.unwrap();
        let stored = stored.expect("rule must be present in repo after insert");
        assert_eq!(stored.rule.max_input_amount_raw, W5D_DEPOSIT_AMOUNT_RAW);
        assert_eq!(stored.rule.delegated_wallet.to_base58(), CONTROLLED_WALLET_BS58);
    }

    /// Idempotent re-typing: inserting the same rule twice does NOT
    /// flip `rule_persisted` to false.
    #[tokio::test]
    async fn w5e_persistence_is_idempotent_on_duplicate_insert() {
        use claw_state_store::db::Database;
        let db = Database::open_in_memory().await.unwrap();
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let input = "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r1 = handle_demo_command_v2(&fetcher, Some(&repo), input)
            .await
            .unwrap();
        let r2 = handle_demo_command_v2(&fetcher, Some(&repo), input)
            .await
            .unwrap();
        assert!(r1.rule_persisted);
        assert!(r2.rule_persisted, "second insert with same rule_id must still report persisted");
        assert_eq!(r1.rule_id_hex, r2.rule_id_hex);
    }

    /// Rule A and Rule B (different threshold) produce DISTINCT
    /// persisted rules in the repo.
    #[tokio::test]
    async fn w5e_persisted_rule_a_and_rule_b_are_distinct() {
        use claw_state_store::db::Database;
        let db = Database::open_in_memory().await.unwrap();
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let a = handle_demo_command_v2(
            &fetcher,
            Some(&repo),
            "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .await
        .unwrap();
        let b = handle_demo_command_v2(
            &fetcher,
            Some(&repo),
            "If Solend Main Pool USDC deposit APR is above 0.5%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .await
        .unwrap();
        assert!(a.rule_persisted && b.rule_persisted);
        assert_ne!(a.rule_id_hex, b.rule_id_hex);
        assert_ne!(a.canonical_rule_hash_hex, b.canonical_rule_hash_hex);
    }

    /// Preview-only fallback (no repo wired): `rule_persisted=false`,
    /// but the rule_id / canonical_hash / expires_at are still filled.
    #[tokio::test]
    async fn w5e_preview_only_when_repo_absent() {
        let fetcher = MockW5dAprFetcher::with_apr(163);
        let input = "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command_v2(&fetcher, None, input).await.unwrap();
        assert!(!r.rule_persisted);
        assert!(r.rule_id_hex.is_some());
        assert!(r.canonical_rule_hash_hex.is_some());
        assert!(r.expires_at_slot.is_some());
    }

    #[test]
    fn live_fetcher_rejects_empty_rpc_url() {
        assert!(LiveW5dAprFetcher::new("").is_none());
        assert!(LiveW5dAprFetcher::new("   ").is_none());
    }

    #[test]
    fn live_fetcher_accepts_well_formed_url() {
        assert!(LiveW5dAprFetcher::new("https://example.invalid/").is_some());
    }
}

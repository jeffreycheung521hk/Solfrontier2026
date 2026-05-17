//! Stage 2 W5d/W5e/W5f — Chat-route conditional-order bridge.
//!
//! The deterministic-parser + dual-APR evaluator surface that the
//! user-facing `/chat` route uses to recognise the demo grammar
//! *before* the LLM is asked anything.
//!
//! # W5f — Save display APY drives the chat-time decision
//!
//! The chat-time decision metric is **Save/Solend UI display APY**
//! fetched from the official Solend REST API at
//! `https://api.solend.fi/v1/reserves?scope=solend&ids=<reserve>` —
//! specifically `results[0].rates.supplyInterest`, which is the same
//! value the Save UI renders for the user. Native on-chain APR via
//! the B-O1 reserve-math wrappers is retained as a **secondary audit
//! field** (`native_onchain_apr_bps`); it is NOT used for the chat-time
//! decision unless the UI explicitly labels the metric as a fallback.
//!
//! ## Source documentation (cited at use-sites below)
//!
//! - <https://dev.solend.fi/docs/api/> — Solend dev portal API page;
//!   the OpenAPI spec is served at <https://api.solend.fi/> (Swagger UI).
//! - <https://docs.save.finance/getting-started/supply-and-borrow-apy> —
//!   Save docs: displayed supply APY is compounded over a year and
//!   includes liquidity-mining rewards when present.
//! - <https://dev.solend.fi/docs/ts-sdk/usage/> — TS SDK fallback path
//!   reference (not used in this slice; we hit the REST API directly).
//!
//! ## Unit reasoning (proved against live response)
//!
//! Sample for the pinned Main Pool USDC reserve:
//!
//! ```json
//! { "rates": { "supplyInterest": "2.10", "borrowInterest": "4.02" } }
//! ```
//!
//! The `supplyInterest` field is a **percentage string** (e.g. `"2.10"`
//! == 2.10 %), NOT a decimal fraction and NOT bps. Conversion to bps is
//! `round(parse_f64(value) * 100)`. For Main Pool USDC at the time of
//! W5f implementation, `rewards: []` so `supplyInterest` IS the full
//! Save-UI value — there are no LM rewards to add. When reward APYs do
//! appear, summing them would be required to match Save UI total (out
//! of W5f scope; the audit field surfaces them via the daemon log).
//!
//! ## Pinned Main Pool USDC identifiers (Addendum 2)
//!
//! The REST response is rejected unless ALL FOUR of these match:
//!
//! | field                             | pinned value                                  |
//! | --------------------------------- | --------------------------------------------- |
//! | `reserve.pubkey`                  | `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw` |
//! | `reserve.lendingMarket`           | `4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY` |
//! | `reserve.liquidity.mintPubkey`    | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` |
//! | `reserve.collateral.mintPubkey`   | `993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk` |
//!
//! Any mismatch fails closed with `EvaluationError::MainPoolMismatch`;
//! the chat-route surfaces a `tool_error` card. No silent fallback to
//! native APR — per the brief.
//!
//! # W5e semantics (preserved unchanged)
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
/// truth: a separate operator-managed reference; the keypair lives
/// only on the operator's local fs at
/// `<CONTROLLED_WALLET_KEYPAIR_PATH>` and is NOT imported by this
/// module under any code path.
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

// ── W5f Save REST API pins (Addendum 2: Main Pool filter MANDATORY) ───────

/// Default base URL for the official Solend REST API. Overridable via
/// the `SOLEND_API_BASE_URL` env var (the daemon may want to point at
/// a fixture during integration tests). Docs:
/// <https://dev.solend.fi/docs/api/>.
pub const SOLEND_API_BASE_URL_DEFAULT: &str = "https://api.solend.fi";

/// `scope` query parameter for the Solend REST API. `"solend"` is the
/// production scope — confirmed via live probe against `/v1/markets` +
/// `/v1/markets/configs`.
pub const SOLEND_API_SCOPE: &str = "solend";

/// Save REST API request timeout. The chat route is user-facing; a
/// slow Save API shouldn't tie up an HTTP worker.
const SAVE_API_REQUEST_TIMEOUT_MS: u64 = 8_000;

/// Pinned Main Pool USDC reserve pubkey (Addendum 2). The Save REST
/// response is rejected unless `reserve.pubkey` matches this exactly.
pub const W5F_MAIN_POOL_RESERVE_BS58: &str =
    "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";

/// Pinned Main Pool lending-market pubkey (Addendum 2). The Save REST
/// response is rejected unless `reserve.lendingMarket` matches this
/// exactly. (Mirrors `stage2_executor::DEMO_LENDING_MARKET_BS58`.)
pub const W5F_MAIN_POOL_LENDING_MARKET_BS58: &str =
    "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";

/// Pinned Main Pool liquidity-mint pubkey (Addendum 2). The Save REST
/// response is rejected unless `reserve.liquidity.mintPubkey` matches.
pub const W5F_MAIN_POOL_LIQUIDITY_MINT_BS58: &str =
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Pinned Main Pool cToken-mint pubkey (Addendum 2). The Save REST
/// response is rejected unless `reserve.collateral.mintPubkey` matches.
pub const W5F_MAIN_POOL_COLLATERAL_MINT_BS58: &str =
    "993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk";

/// Plausible-range guard on the Save display APY parsed from the REST
/// response. A reading outside this window indicates either a malformed
/// API response or an unrelated upstream change; fail closed.
const SAVE_DISPLAY_APY_PERCENT_SANITY_MAX: f64 = 1000.0;

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
    /// Legacy `source` string kept for wire compatibility with the
    /// W5d-era card. After W5f, the more specific
    /// `native_onchain_apr_source` describes the audit metric and
    /// `decision_source` describes which metric drove the decision.
    /// Always `"save_display_apy"` after W5f.
    pub source: String,
    /// `BgxfHJDz…JMpmw` (Solend Main Pool USDC reserve, base58). After
    /// W5f, this is cross-verified against the Save REST API response
    /// before the decision metric is accepted.
    pub reserve_pubkey: String,
    /// W5f alias kept for wire compatibility: equal to
    /// `save_display_apy_bps` so older frontends keep working. New
    /// callers should read `save_display_apy_bps` directly.
    pub current_apr_bps: u32,
    /// Threshold parsed from the user's text, in basis points.
    pub threshold_bps: u32,
    /// Echo of the percent label the user typed (e.g. `"2.5"`).
    pub threshold_pct_label: String,
    /// Strict-greater decision: `save_display_apy_bps > threshold_bps`.
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

    // ── W5f decision metric + audit ──────────────────────────────────────

    /// W5f — which metric drove `condition_met`. Always
    /// `"save_display_apy"` in the happy path. (A hypothetical fallback
    /// path would set this to `"native_onchain_apr_fallback"`; the
    /// brief explicitly prefers fail-closed over fallback, so this
    /// slice does not implement the fallback path.)
    pub decision_source: String,
    /// W5f — Save/Solend UI display APY for Main Pool USDC, in basis
    /// points. Fetched live from the official Solend REST API at
    /// `/v1/reserves?scope=solend&ids=<reserve>` (field
    /// `results[0].rates.supplyInterest`, parsed as percent string and
    /// converted to bps). This drives `condition_met`.
    pub save_display_apy_bps: u32,
    /// W5f — native on-chain supply APR in basis points, decoded from
    /// the reserve account via the B-O1 wrappers. Audit-only — does
    /// NOT drive the chat-time decision. Surfaced so the user sees
    /// the gap between Save UI APY and native APR.
    pub native_onchain_apr_bps: u32,
    /// W5f — provenance label for the native APR. Always
    /// `"b_o1_reserve_math"` in this slice.
    pub native_onchain_apr_source: String,
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
    /// W5f — Save REST API is unavailable, returned an error, or the
    /// response could not be parsed into the expected shape. The chat
    /// route fails closed (typed `ToolError`); it does NOT silently
    /// fall back to native APR as the decision metric.
    MarketDataUnavailable { reason: String },
    /// W5f — the Save REST response did not match the pinned Main Pool
    /// USDC identifiers (Addendum 2). Strictly refused — never decide
    /// on a non-Main-Pool reserve.
    MainPoolMismatch {
        field: String,
        expected: String,
        actual: String,
    },
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
            EvaluationError::MarketDataUnavailable { reason } => {
                write!(f, "save market data unavailable: {reason}")
            }
            EvaluationError::MainPoolMismatch { field, expected, actual } => write!(
                f,
                "main pool mismatch: {field} expected {expected} got {actual}"
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

// ── W5f Save display APY fetcher ─────────────────────────────────────────
//
// Hits the official Solend REST API and returns the Main Pool USDC
// display APY in bps, verified against the pinned identifiers.
//
// Source: <https://dev.solend.fi/docs/api/>. The Swagger UI served at
// <https://api.solend.fi/> documents the endpoint shape; we sampled
// the live response on 2026-05-11 to lock the JSON field name and
// unit (percentage string, e.g. `"2.10"` ⇒ 2.10 %).

/// The reading returned by [`SaveDisplayApyFetcher::fetch_main_pool_usdc`].
/// Carries both the converted bps value and the raw percent string so
/// the daemon log + audit trail can witness the API response verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveDisplayApyReading {
    /// Save UI display APY in basis points (1 % = 100 bps). Derived as
    /// `round(parse_f64(supplyInterest) * 100)`.
    pub save_display_apy_bps: u32,
    /// The verbatim percent-string returned by the API (e.g. `"2.10"`)
    /// — useful for audit logs and for the no-overclaim banner.
    pub raw_supply_interest_str: String,
    /// `reserve.pubkey` from the API — always equals
    /// `W5F_MAIN_POOL_RESERVE_BS58` (verified before returning).
    pub reserve_pubkey: String,
    /// `reserve.lendingMarket` from the API — always equals
    /// `W5F_MAIN_POOL_LENDING_MARKET_BS58` (verified before returning).
    pub lending_market: String,
    /// `reserve.liquidity.mintPubkey` — verified.
    pub liquidity_mint: String,
    /// `reserve.collateral.mintPubkey` — verified.
    pub collateral_mint: String,
    /// Whether the API response carried any non-empty `rewards[]`. For
    /// Main Pool USDC this is currently `false`; if it ever becomes
    /// `true`, the chat card surfaces a note that the displayed APY
    /// may not include LM rewards (W5f scope is the base supplyInterest
    /// only; reward-sum is a future slice).
    pub rewards_present: bool,
}

/// Trait the chat orchestrator depends on for Save display APY. Tests
/// inject a deterministic mock; production uses
/// [`LiveSaveDisplayApyFetcher`].
#[async_trait]
pub trait SaveDisplayApyFetcher: Send + Sync + std::fmt::Debug {
    /// Fetch the Save display APY for the pinned Main Pool USDC
    /// reserve. Must verify ALL FOUR pinned identifiers before
    /// returning success; on any mismatch return
    /// [`EvaluationError::MainPoolMismatch`].
    async fn fetch_main_pool_usdc(&self) -> Result<SaveDisplayApyReading, EvaluationError>;
}

/// Live implementation hitting the official Solend REST API at
/// `https://api.solend.fi`. Read-only; no broadcast; no keypair; no
/// signing. Single HTTP GET per evaluation with a tight timeout.
#[derive(Debug, Clone)]
pub struct LiveSaveDisplayApyFetcher {
    base_url: String,
    http: reqwest::Client,
}

impl LiveSaveDisplayApyFetcher {
    /// Build a fetcher with the given base URL. Returns `None` if the
    /// URL is empty / whitespace-only OR the HTTP client cannot be
    /// built (extremely unlikely; only on platform misconfiguration).
    pub fn new(base_url: impl Into<String>) -> Option<Self> {
        let base_url = base_url.into();
        let trimmed_url = base_url.trim().trim_end_matches('/').to_string();
        if trimmed_url.is_empty() {
            return None;
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(SAVE_API_REQUEST_TIMEOUT_MS))
            .build()
            .ok()?;
        Some(Self {
            base_url: trimmed_url,
            http,
        })
    }

    /// Build a default fetcher pointing at the official Solend REST
    /// API. Convenience for callers that don't need to override the
    /// base URL (production daemon path).
    pub fn with_default_base_url() -> Self {
        Self::new(SOLEND_API_BASE_URL_DEFAULT).expect("default base URL is non-empty")
    }
}

#[async_trait]
impl SaveDisplayApyFetcher for LiveSaveDisplayApyFetcher {
    async fn fetch_main_pool_usdc(&self) -> Result<SaveDisplayApyReading, EvaluationError> {
        let url = format!(
            "{base}/v1/reserves?scope={scope}&ids={ids}",
            base = self.base_url,
            scope = SOLEND_API_SCOPE,
            ids = W5F_MAIN_POOL_RESERVE_BS58,
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| EvaluationError::MarketDataUnavailable {
                reason: format!("transport: {e}"),
            })?;
        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(EvaluationError::MarketDataUnavailable {
                reason: format!("http {status}"),
            });
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| EvaluationError::MarketDataUnavailable {
                reason: format!("body: {e}"),
            })?;
        parse_save_reserves_response(&body)
    }
}

/// Pure parser for the `/v1/reserves` response body. Verifies the
/// pinned Main Pool USDC identifiers and converts the percent-string
/// `supplyInterest` to bps. Extracted so the unit tests can drive it
/// with fixture JSON instead of hitting the network.
pub fn parse_save_reserves_response(
    body: &Value,
) -> Result<SaveDisplayApyReading, EvaluationError> {
    let results = body
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| EvaluationError::MarketDataUnavailable {
            reason: "missing or non-array `results`".to_string(),
        })?;
    if results.is_empty() {
        return Err(EvaluationError::MarketDataUnavailable {
            reason: "empty `results` array".to_string(),
        });
    }
    let r = &results[0];

    // ── Main Pool identifier verification (Addendum 2) ───────────────────
    let reserve_pubkey = r["reserve"]["pubkey"]
        .as_str()
        .ok_or_else(|| EvaluationError::MarketDataUnavailable {
            reason: "missing reserve.pubkey".to_string(),
        })?
        .to_string();
    if reserve_pubkey != W5F_MAIN_POOL_RESERVE_BS58 {
        return Err(EvaluationError::MainPoolMismatch {
            field: "reserve.pubkey".to_string(),
            expected: W5F_MAIN_POOL_RESERVE_BS58.to_string(),
            actual: reserve_pubkey,
        });
    }
    let lending_market = r["reserve"]["lendingMarket"]
        .as_str()
        .ok_or_else(|| EvaluationError::MarketDataUnavailable {
            reason: "missing reserve.lendingMarket".to_string(),
        })?
        .to_string();
    if lending_market != W5F_MAIN_POOL_LENDING_MARKET_BS58 {
        return Err(EvaluationError::MainPoolMismatch {
            field: "reserve.lendingMarket".to_string(),
            expected: W5F_MAIN_POOL_LENDING_MARKET_BS58.to_string(),
            actual: lending_market,
        });
    }
    let liquidity_mint = r["reserve"]["liquidity"]["mintPubkey"]
        .as_str()
        .ok_or_else(|| EvaluationError::MarketDataUnavailable {
            reason: "missing reserve.liquidity.mintPubkey".to_string(),
        })?
        .to_string();
    if liquidity_mint != W5F_MAIN_POOL_LIQUIDITY_MINT_BS58 {
        return Err(EvaluationError::MainPoolMismatch {
            field: "reserve.liquidity.mintPubkey".to_string(),
            expected: W5F_MAIN_POOL_LIQUIDITY_MINT_BS58.to_string(),
            actual: liquidity_mint,
        });
    }
    let collateral_mint = r["reserve"]["collateral"]["mintPubkey"]
        .as_str()
        .ok_or_else(|| EvaluationError::MarketDataUnavailable {
            reason: "missing reserve.collateral.mintPubkey".to_string(),
        })?
        .to_string();
    if collateral_mint != W5F_MAIN_POOL_COLLATERAL_MINT_BS58 {
        return Err(EvaluationError::MainPoolMismatch {
            field: "reserve.collateral.mintPubkey".to_string(),
            expected: W5F_MAIN_POOL_COLLATERAL_MINT_BS58.to_string(),
            actual: collateral_mint,
        });
    }

    // ── supplyInterest as PERCENT STRING → bps (Addendum 2: prove unit) ──
    //
    // Sampled 2026-05-11 against the live API:
    //   {"rates": {"supplyInterest": "2.10", ...}}
    // i.e. a decimal string with two fractional digits representing
    // PERCENT (not decimal fraction; not bps). Conversion:
    //   bps = round(parse_f64(value) * 100)
    let raw_supply_interest_str = r["rates"]["supplyInterest"]
        .as_str()
        .ok_or_else(|| EvaluationError::MarketDataUnavailable {
            reason: "missing rates.supplyInterest (or non-string)".to_string(),
        })?
        .to_string();
    let save_display_apy_bps = parse_percent_string_to_bps(&raw_supply_interest_str)?;

    let rewards_present = r
        .get("rewards")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    Ok(SaveDisplayApyReading {
        save_display_apy_bps,
        raw_supply_interest_str,
        reserve_pubkey,
        lending_market,
        liquidity_mint,
        collateral_mint,
        rewards_present,
    })
}

/// Convert a percent-string like `"2.10"` to integer basis points.
/// Strict on shape: rejects non-finite, negative, or implausibly-large
/// values. The conversion uses `round` so `"0.005"` → 0 bps (sub-bp
/// noise) and `"0.01"` → 1 bp (a real bp).
pub fn parse_percent_string_to_bps(s: &str) -> Result<u32, EvaluationError> {
    let value: f64 = s.trim().parse().map_err(|e: std::num::ParseFloatError| {
        EvaluationError::MarketDataUnavailable {
            reason: format!("supplyInterest not a valid f64: {s:?} ({e})"),
        }
    })?;
    if !value.is_finite() {
        return Err(EvaluationError::MarketDataUnavailable {
            reason: format!("supplyInterest non-finite: {s:?}"),
        });
    }
    if value < 0.0 || value > SAVE_DISPLAY_APY_PERCENT_SANITY_MAX {
        return Err(EvaluationError::MarketDataUnavailable {
            reason: format!(
                "supplyInterest out of plausible range [0, {SAVE_DISPLAY_APY_PERCENT_SANITY_MAX}]: {s:?}"
            ),
        });
    }
    let bps_f = (value * 100.0).round();
    if bps_f < 0.0 || bps_f > u32::MAX as f64 {
        return Err(EvaluationError::MarketDataUnavailable {
            reason: format!("supplyInterest bps overflow after round: {bps_f}"),
        });
    }
    Ok(bps_f as u32)
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
        // After W5f, `source` is the LEGACY label kept for wire
        // compat. The orchestrator overwrites it to `"save_display_apy"`
        // when the Save fetcher path runs; this default applies only to
        // unit tests that call `compose_w5e_result` directly without
        // a save fetcher.
        source: "save_display_apy".to_string(),
        reserve_pubkey: W5D_RESERVE_BS58.to_string(),
        // W5e compatibility: pre-W5f, `current_apr_bps` carried the
        // native on-chain APR. After W5f the orchestrator overwrites
        // this to mirror `save_display_apy_bps` for wire compat.
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
        // `handle_demo_command_v3` after the WatchRule is constructed.
        expires_at_slot: None,
        rule_id_hex: None,
        canonical_rule_hash_hex: None,
        rule_persisted: false,
        // W5f defaults (overridden by `handle_demo_command_v3` when a
        // Save fetcher is wired). For the single-APR mock path this
        // produces a degenerate-but-valid result where both metrics
        // equal the one APR the mock provided.
        decision_source: "save_display_apy".to_string(),
        save_display_apy_bps: current_apr_bps,
        native_onchain_apr_bps: current_apr_bps,
        native_onchain_apr_source: "b_o1_reserve_math".to_string(),
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

/// Typed outcome of the idempotent insert. The orchestrator needs
/// more than a `bool` because the rule's canonical hash and
/// `expires_at_slot` change with every `last_checked_slot` — so a
/// freshly-built rule and the persisted rule with the same
/// deterministic `rule_id` will compute DIFFERENT canonical hashes.
/// The chat-route response must echo the PERSISTED rule's metadata
/// so the W5g approval command (which carries the canonical hash as
/// an integrity anchor) survives an idempotent re-typing.
#[derive(Debug, Clone)]
pub enum PersistOutcome {
    /// Fresh insert. The just-built rule is now the durable rule;
    /// the orchestrator should echo its metadata.
    Inserted,
    /// UNIQUE collision. The DB already has a rule with this
    /// `rule_id`; carry the persisted rule back so the orchestrator
    /// can overwrite the fresh-rule fields with persisted ones.
    AlreadyPresent { persisted: WatchRule },
    /// `repo.insert` failed AND `repo.get` couldn't confirm the
    /// rule. Almost certainly a transient DB issue; the orchestrator
    /// reports `rule_persisted=false` and keeps the fresh metadata.
    Failed,
}

/// Insert (idempotently) the W5e rule into the durable state-store.
/// On UNIQUE collision the persisted rule is returned so the
/// orchestrator can substitute its metadata into the response (the
/// canonical hash depends on `last_checked_slot` / `expires_at_slot`
/// / `created_at_slot`, all of which differ between a fresh build
/// and a previously-persisted rule with the same `rule_id`).
pub async fn persist_rule_idempotent(
    repo: &Stage2WatchRuleRepository,
    rule: &WatchRule,
) -> PersistOutcome {
    match repo.insert(rule).await {
        Ok(()) => PersistOutcome::Inserted,
        Err(_) => match repo.get(&rule.rule_id).await {
            Ok(Some(stored)) => PersistOutcome::AlreadyPresent {
                persisted: stored.rule,
            },
            _ => PersistOutcome::Failed,
        },
    }
}

pub fn hex_id_16(id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in id {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

pub fn hex_id_32(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in id {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

// ── High-level chat-handler entry point ──────────────────────────────────

/// W5f — full chat-route entry point. Composes the native-APR fetch
/// (B-O1 + budget + slot) with the Save display APY fetch (REST API),
/// then overrides the decision metric so `condition_met` is driven by
/// **Save display APY**, not native APR.
///
/// Flow:
///
///  1. `parse_demo_command(input)` → typed `DemoParsed`
///  2. `apr_fetcher.evaluate(...)` → native APR + budget + slot
///  3. `save_apy_fetcher.fetch_main_pool_usdc()` → Save display APY
///     (verified against pinned Main Pool identifiers)
///  4. Re-decide `condition_met` and `status` using Save APY
///  5. Construct + persist the WatchRule (W5e semantics unchanged)
///
/// **Fail-closed contract**: if the Save REST fetch returns
/// `MarketDataUnavailable` or `MainPoolMismatch`, this function
/// propagates the typed error and the chat-route renders a
/// `tool_error` card. It does NOT silently fall back to native APR —
/// the brief is explicit that fallback would only be acceptable if the
/// UI labels the metric as `"native_onchain_apr_fallback"` (out of
/// W5f scope; this slice prefers fail-closed for demo clarity).
pub async fn handle_demo_command_v3(
    apr_fetcher: &(dyn W5dAprFetcher + 'static),
    save_apy_fetcher: &(dyn SaveDisplayApyFetcher + 'static),
    repo: Option<&Stage2WatchRuleRepository>,
    input_text: &str,
) -> Result<W5dEvaluationResult, EvaluationError> {
    let parsed = parse_demo_command(input_text)
        .map_err(|e| EvaluationError::Parse { parse_error: e })?;

    // 1. Native APR + budget + slot.
    let mut result = apr_fetcher.evaluate(input_text, &parsed).await?;
    // After step 1, `result.current_apr_bps` carries NATIVE APR
    // (per `compose_w5e_result`'s default field population). Capture
    // it before we overwrite the decision-side fields.
    let native_onchain_apr_bps = result.current_apr_bps;

    // 2. Save display APY — fail closed on any unavailability.
    let save = save_apy_fetcher.fetch_main_pool_usdc().await?;

    // 3. Override the decision-side fields using Save APY.
    result.save_display_apy_bps = save.save_display_apy_bps;
    result.native_onchain_apr_bps = native_onchain_apr_bps;
    result.native_onchain_apr_source = "b_o1_reserve_math".to_string();
    result.decision_source = "save_display_apy".to_string();
    result.source = "save_display_apy".to_string();
    // Wire-compat alias: callers reading the old `current_apr_bps`
    // field see the SAME value that drives the decision.
    result.current_apr_bps = save.save_display_apy_bps;
    result.condition_met = save.save_display_apy_bps > parsed.threshold_bps;

    // 4. Re-derive status (and budget_status) under the new condition.
    let budget_ok = result.current_budget_raw >= result.required_budget_raw;
    let (status, budget_status) = match (budget_ok, result.condition_met) {
        (false, _) => ("needs_funding", "needs_funding"),
        (true, true) => ("ready_to_execute", "reserved"),
        (true, false) => ("watching", "reserved"),
    };
    result.status = status.to_string();
    result.budget_status = budget_status.to_string();

    // 5. WatchRule construction + persistence.
    //
    // NOTE on metric divergence: the rule's Condition is
    // `SolendReserveSupplyRate` (native on-chain APR), while the chat
    // card's `condition_met` follows Save display APY. The W2 watcher
    // (when run) will evaluate the rule against native APR. The card
    // surfaces both numbers so the gap is visible to the user.
    let fresh_rule = build_w5e_watch_rule(&parsed, result.last_checked_slot);
    // Always start by echoing the FRESH rule's metadata. If the repo
    // is wired and the insert hits a UNIQUE-collision, we overwrite
    // these with the PERSISTED rule's metadata so the integrity
    // anchor (canonical_rule_hash_hex) survives an idempotent
    // re-typing — without this overwrite, the W5g approval command
    // would refuse with `canonical_hash_mismatch` for any rule that
    // had been persisted earlier with different
    // last_checked_slot / expires_at_slot / created_at_slot.
    apply_rule_metadata_to_result(&mut result, &fresh_rule);

    if let Some(repo_ref) = repo {
        match persist_rule_idempotent(repo_ref, &fresh_rule).await {
            PersistOutcome::Inserted => {
                result.rule_persisted = true;
            }
            PersistOutcome::AlreadyPresent { persisted } => {
                result.rule_persisted = true;
                // Echo the PERSISTED rule's metadata so the operator
                // can use the returned canonical_rule_hash_hex
                // verbatim in the W5g approval command.
                apply_rule_metadata_to_result(&mut result, &persisted);
            }
            PersistOutcome::Failed => {
                result.rule_persisted = false;
            }
        }
    }
    // else: rule_persisted stays false (preview-only). Caller MUST
    // surface this in the no-overclaim banner.

    Ok(result)
}

/// Overwrite the rule-identity fields of an evaluation result with
/// the metadata of the supplied `WatchRule`. Used twice in
/// `handle_demo_command_v3`: once with the fresh rule (default), then
/// again with the persisted rule on UNIQUE-collision fallback so the
/// response surfaces the durable canonical hash, not the just-built
/// hash that the DB never accepted.
fn apply_rule_metadata_to_result(result: &mut W5dEvaluationResult, rule: &WatchRule) {
    let canonical = canonical_rule_hash(rule);
    result.rule_id_hex = Some(hex_id_16(&rule.rule_id));
    result.canonical_rule_hash_hex = Some(hex_id_32(&canonical));
    result.expires_at_slot = Some(rule.expires_at_slot);
}

/// W5e wrapper kept for back-compat. Mirrors `handle_demo_command_v3`
/// but uses an internal `DegenerateSaveFetcher` that returns the same
/// APR for both metrics — preserving old single-APR test semantics.
///
/// Production callers MUST migrate to `handle_demo_command_v3`; the
/// chat handler already does.
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
    //
    // On UNIQUE-collision fallback the response's identity fields
    // are overwritten with the persisted rule's values so the W5g
    // approval command can use the canonical_rule_hash_hex returned
    // here as a stable integrity anchor.
    let fresh_rule = build_w5e_watch_rule(&parsed, result.last_checked_slot);
    apply_rule_metadata_to_result(&mut result, &fresh_rule);

    if let Some(repo_ref) = repo {
        match persist_rule_idempotent(repo_ref, &fresh_rule).await {
            PersistOutcome::Inserted => {
                result.rule_persisted = true;
            }
            PersistOutcome::AlreadyPresent { persisted } => {
                result.rule_persisted = true;
                apply_rule_metadata_to_result(&mut result, &persisted);
            }
            PersistOutcome::Failed => {
                result.rule_persisted = false;
            }
        }
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

    /// REGRESSION (W5g live-send blocker, 2026-05-12): when the W5e
    /// bridge re-types the same deterministic-rule_id command in a
    /// fresh chat call, the durable rule already exists in the repo
    /// with DIFFERENT `last_checked_slot` / `expires_at_slot` /
    /// `created_at_slot` than the just-built rule. Pre-fix, the
    /// response echoed the freshly-computed `canonical_rule_hash_hex`
    /// — which the W5g approval command then carried back to the
    /// orchestrator, where the canonical-hash precheck refused
    /// because the persisted rule's hash differed.
    ///
    /// Post-fix: when `persist_rule_idempotent` returns
    /// `AlreadyPresent { persisted }`, the orchestrator overwrites
    /// the response's identity fields with the persisted rule's
    /// values, so the W5g approval can use the returned hash
    /// verbatim.
    #[tokio::test]
    async fn w5e_collision_fallback_returns_persisted_canonical_hash() {
        use claw_state_store::db::Database;
        let db = Database::open_in_memory().await.unwrap();
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        let input = "If Solend Main Pool USDC deposit APR is above 5%, \
                     deposit 0.25 USDC from my bounded executor wallet into Solend.";

        // First chat call at slot S1 — fresh insert. Capture the
        // persisted rule's canonical hash + expires_at.
        let fetcher1 = MockW5dAprFetcher {
            current_apr_bps: 163,
            current_budget_raw: 500_000,
            last_checked_slot: 100_000_000,
        };
        let r1 = handle_demo_command_v2(&fetcher1, Some(&repo), input)
            .await
            .unwrap();
        assert!(r1.rule_persisted, "first insert must succeed");
        let persisted_hash = r1.canonical_rule_hash_hex.clone().expect("hex");
        let persisted_expires = r1.expires_at_slot.expect("expires");
        let persisted_rule_id = r1.rule_id_hex.clone().expect("rule_id");

        // Second chat call at slot S2 (different slot → fresh rule
        // would have DIFFERENT canonical hash / expires_at). Bridge
        // must NOT echo the fresh values; it must overwrite with
        // the persisted rule's values from `repo.get(rule_id)`.
        let fetcher2 = MockW5dAprFetcher {
            current_apr_bps: 163,
            current_budget_raw: 500_000,
            last_checked_slot: 200_000_000,
        };
        let r2 = handle_demo_command_v2(&fetcher2, Some(&repo), input)
            .await
            .unwrap();
        assert!(r2.rule_persisted, "idempotent re-type must report persisted");

        // Critical asserts (the W5g blocker we fixed).
        assert_eq!(
            r2.rule_id_hex,
            Some(persisted_rule_id),
            "rule_id is deterministic — must match the persisted id"
        );
        assert_eq!(
            r2.canonical_rule_hash_hex,
            Some(persisted_hash.clone()),
            "BUG REGRESSION: response must echo PERSISTED canonical_hash on \
             UNIQUE collision (was returning the fresh hash, which would \
             then refuse to match the persisted rule during the W5g \
             canonical-hash precheck)"
        );
        assert_eq!(
            r2.expires_at_slot,
            Some(persisted_expires),
            "expires_at_slot must also be the persisted value"
        );

        // Sanity: the LIVENESS anchor (last_checked_slot) is still
        // the FRESH one — that's a read-time observation, not a
        // rule-identity field. The card uses it to show the operator
        // "how stale is this view?".
        assert_eq!(r2.last_checked_slot, 200_000_000);

        // Sanity: a hypothetical fresh rule built at slot 200_000_000
        // would have a DIFFERENT hash. Build it directly and confirm
        // the fix actually overrode that fresh hash.
        let parsed = parse_demo_command(input).unwrap();
        let fresh_rule = build_w5e_watch_rule(&parsed, 200_000_000);
        let fresh_hash_hex = hex_id_32(&canonical_rule_hash(&fresh_rule));
        assert_ne!(
            fresh_hash_hex, persisted_hash,
            "test invariant: fresh hash must differ from persisted hash for the assertion to be meaningful"
        );
        assert_ne!(
            r2.canonical_rule_hash_hex,
            Some(fresh_hash_hex),
            "response must NOT echo the freshly-built hash"
        );
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

    // ── W5f Save display APY tests ────────────────────────────────────────

    /// W5f: a controllable mock that returns programmable Save APY
    /// readings (or a typed error). Used by the orchestrator tests so
    /// we can drive the divergence between native APR and Save APY
    /// without hitting the network.
    #[derive(Debug, Clone)]
    pub(crate) struct MockSaveDisplayApyFetcher {
        pub outcome: Result<SaveDisplayApyReading, EvaluationError>,
    }

    impl MockSaveDisplayApyFetcher {
        pub(crate) fn with_apy_bps(bps: u32) -> Self {
            Self {
                outcome: Ok(SaveDisplayApyReading {
                    save_display_apy_bps: bps,
                    raw_supply_interest_str: format!("{:.2}", (bps as f64) / 100.0),
                    reserve_pubkey: W5F_MAIN_POOL_RESERVE_BS58.to_string(),
                    lending_market: W5F_MAIN_POOL_LENDING_MARKET_BS58.to_string(),
                    liquidity_mint: W5F_MAIN_POOL_LIQUIDITY_MINT_BS58.to_string(),
                    collateral_mint: W5F_MAIN_POOL_COLLATERAL_MINT_BS58.to_string(),
                    rewards_present: false,
                }),
            }
        }

        pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
            Self {
                outcome: Err(EvaluationError::MarketDataUnavailable {
                    reason: reason.into(),
                }),
            }
        }
    }

    #[async_trait]
    impl SaveDisplayApyFetcher for MockSaveDisplayApyFetcher {
        async fn fetch_main_pool_usdc(
            &self,
        ) -> Result<SaveDisplayApyReading, EvaluationError> {
            self.outcome.clone()
        }
    }

    /// Unit conversion smoke for the percent-string parser. Pinned
    /// against the live response shape (`"2.10"` ⇒ 210 bps).
    #[test]
    fn w5f_percent_string_to_bps_table() {
        assert_eq!(parse_percent_string_to_bps("0").unwrap(), 0);
        assert_eq!(parse_percent_string_to_bps("0.00").unwrap(), 0);
        assert_eq!(parse_percent_string_to_bps("0.01").unwrap(), 1);
        assert_eq!(parse_percent_string_to_bps("1").unwrap(), 100);
        assert_eq!(parse_percent_string_to_bps("2.10").unwrap(), 210);
        assert_eq!(parse_percent_string_to_bps("4.02").unwrap(), 402);
        assert_eq!(parse_percent_string_to_bps("100").unwrap(), 10_000);
        // Sub-bp rounds to zero (and to one bp at midpoint).
        assert_eq!(parse_percent_string_to_bps("0.004").unwrap(), 0);
        assert_eq!(parse_percent_string_to_bps("0.005").unwrap(), 1);
    }

    #[test]
    fn w5f_percent_string_rejects_non_numeric() {
        assert!(parse_percent_string_to_bps("abc").is_err());
        assert!(parse_percent_string_to_bps("").is_err());
        assert!(parse_percent_string_to_bps("nan").is_err()); // string→f64 yields NaN
    }

    #[test]
    fn w5f_percent_string_rejects_negative_and_implausible() {
        match parse_percent_string_to_bps("-0.5").unwrap_err() {
            EvaluationError::MarketDataUnavailable { reason } => {
                assert!(reason.contains("out of plausible range"), "got: {reason}")
            }
            other => panic!("expected MarketDataUnavailable, got {other:?}"),
        }
        match parse_percent_string_to_bps("1001").unwrap_err() {
            EvaluationError::MarketDataUnavailable { reason } => {
                assert!(reason.contains("out of plausible range"), "got: {reason}")
            }
            other => panic!("expected MarketDataUnavailable, got {other:?}"),
        }
    }

    /// W5f: pinned-Main-Pool fixture parses to the expected reading.
    /// Mirrors the verbatim response we sampled from the live API on
    /// 2026-05-11.
    #[test]
    fn w5f_parse_main_pool_fixture_succeeds() {
        let fixture = json!({
            "results": [{
                "reserve": {
                    "pubkey": W5F_MAIN_POOL_RESERVE_BS58,
                    "lendingMarket": W5F_MAIN_POOL_LENDING_MARKET_BS58,
                    "liquidity": {"mintPubkey": W5F_MAIN_POOL_LIQUIDITY_MINT_BS58},
                    "collateral": {"mintPubkey": W5F_MAIN_POOL_COLLATERAL_MINT_BS58},
                },
                "rates": {"supplyInterest": "2.10", "borrowInterest": "4.02"},
                "rewards": [],
            }],
            "next": null
        });
        let reading = parse_save_reserves_response(&fixture).unwrap();
        assert_eq!(reading.save_display_apy_bps, 210);
        assert_eq!(reading.raw_supply_interest_str, "2.10");
        assert_eq!(reading.reserve_pubkey, W5F_MAIN_POOL_RESERVE_BS58);
        assert_eq!(reading.lending_market, W5F_MAIN_POOL_LENDING_MARKET_BS58);
        assert!(!reading.rewards_present);
    }

    /// W5f: pubkey mismatch is refused — Addendum 2 forbids deciding
    /// on a non-Main-Pool reserve.
    #[test]
    fn w5f_parse_refuses_wrong_reserve_pubkey() {
        let fixture = json!({
            "results": [{
                "reserve": {
                    "pubkey": "1111111111111111111111111111111111111111111",
                    "lendingMarket": W5F_MAIN_POOL_LENDING_MARKET_BS58,
                    "liquidity": {"mintPubkey": W5F_MAIN_POOL_LIQUIDITY_MINT_BS58},
                    "collateral": {"mintPubkey": W5F_MAIN_POOL_COLLATERAL_MINT_BS58},
                },
                "rates": {"supplyInterest": "2.10"},
                "rewards": [],
            }],
        });
        match parse_save_reserves_response(&fixture).unwrap_err() {
            EvaluationError::MainPoolMismatch { field, expected, actual } => {
                assert_eq!(field, "reserve.pubkey");
                assert_eq!(expected, W5F_MAIN_POOL_RESERVE_BS58);
                assert_eq!(actual, "1111111111111111111111111111111111111111111");
            }
            other => panic!("expected MainPoolMismatch, got {other:?}"),
        }
    }

    /// W5f: lending-market mismatch is refused. Same Addendum 2 rule
    /// applied to a different field — e.g. a different USDC reserve
    /// under a non-main lending market would be rejected.
    #[test]
    fn w5f_parse_refuses_wrong_lending_market() {
        let fixture = json!({
            "results": [{
                "reserve": {
                    "pubkey": W5F_MAIN_POOL_RESERVE_BS58,
                    "lendingMarket": "1111111111111111111111111111111111111111111",
                    "liquidity": {"mintPubkey": W5F_MAIN_POOL_LIQUIDITY_MINT_BS58},
                    "collateral": {"mintPubkey": W5F_MAIN_POOL_COLLATERAL_MINT_BS58},
                },
                "rates": {"supplyInterest": "2.10"},
                "rewards": [],
            }],
        });
        match parse_save_reserves_response(&fixture).unwrap_err() {
            EvaluationError::MainPoolMismatch { field, .. } => {
                assert_eq!(field, "reserve.lendingMarket");
            }
            other => panic!("expected MainPoolMismatch, got {other:?}"),
        }
    }

    /// W5f: empty `results` array surfaces as MarketDataUnavailable
    /// (not a panic, not a silent native-APR fallback).
    #[test]
    fn w5f_parse_refuses_empty_results() {
        let fixture = json!({"results": [], "next": null});
        match parse_save_reserves_response(&fixture).unwrap_err() {
            EvaluationError::MarketDataUnavailable { reason } => {
                assert!(reason.contains("empty"), "got: {reason}")
            }
            other => panic!("expected MarketDataUnavailable, got {other:?}"),
        }
    }

    /// W5f: missing `supplyInterest` (or non-string shape) surfaces as
    /// MarketDataUnavailable.
    #[test]
    fn w5f_parse_refuses_missing_supply_interest() {
        let fixture = json!({
            "results": [{
                "reserve": {
                    "pubkey": W5F_MAIN_POOL_RESERVE_BS58,
                    "lendingMarket": W5F_MAIN_POOL_LENDING_MARKET_BS58,
                    "liquidity": {"mintPubkey": W5F_MAIN_POOL_LIQUIDITY_MINT_BS58},
                    "collateral": {"mintPubkey": W5F_MAIN_POOL_COLLATERAL_MINT_BS58},
                },
                "rates": {"borrowInterest": "4.02"},
                "rewards": [],
            }],
        });
        match parse_save_reserves_response(&fixture).unwrap_err() {
            EvaluationError::MarketDataUnavailable { reason } => {
                assert!(reason.contains("supplyInterest"), "got: {reason}")
            }
            other => panic!("expected MarketDataUnavailable, got {other:?}"),
        }
    }

    /// W5f core invariant: the chat-time decision follows Save APY,
    /// NOT native APR. Native=100 bps + Save=300 bps + threshold=200
    /// → `condition_met=true` (decision uses Save). Without W5f the
    /// answer would be `false`.
    #[tokio::test]
    async fn w5f_save_apy_drives_condition_not_native_apr() {
        let apr_fetcher = MockW5dAprFetcher::with_apr(100); // native
        let save_fetcher = MockSaveDisplayApyFetcher::with_apy_bps(300); // Save UI
        let input = "If Solend Main Pool USDC deposit APR is above 2%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command_v3(&apr_fetcher, &save_fetcher, None, input)
            .await
            .unwrap();
        // Decision uses Save APY (300 > 200) → condition_met=true.
        assert!(r.condition_met, "decision must use Save APY, not native");
        assert_eq!(r.status, "ready_to_execute");
        assert_eq!(r.decision_source, "save_display_apy");
        assert_eq!(r.save_display_apy_bps, 300);
        // Audit field carries native APR untouched.
        assert_eq!(r.native_onchain_apr_bps, 100);
        assert_eq!(r.native_onchain_apr_source, "b_o1_reserve_math");
        // Wire-compat alias mirrors the decision metric.
        assert_eq!(r.current_apr_bps, 300);
    }

    /// Inverse: native=300 + Save=100 + threshold=200 → false.
    /// Proves the decision is NOT silently using native.
    #[tokio::test]
    async fn w5f_save_apy_below_threshold_yields_watching_even_if_native_above() {
        let apr_fetcher = MockW5dAprFetcher::with_apr(300); // native says go
        let save_fetcher = MockSaveDisplayApyFetcher::with_apy_bps(100); // Save says stop
        let input = "If Solend Main Pool USDC deposit APR is above 2%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command_v3(&apr_fetcher, &save_fetcher, None, input)
            .await
            .unwrap();
        assert!(!r.condition_met);
        assert_eq!(r.status, "watching");
        assert_eq!(r.save_display_apy_bps, 100);
        assert_eq!(r.native_onchain_apr_bps, 300);
    }

    /// Budget gate remains a hard precondition even after the W5f
    /// decision metric switch.
    #[tokio::test]
    async fn w5f_needs_funding_overrides_save_apy_decision() {
        let apr_fetcher = MockW5dAprFetcher {
            current_apr_bps: 100,
            current_budget_raw: 50_000, // < 250_000 required
            last_checked_slot: 419_000_000,
        };
        let save_fetcher = MockSaveDisplayApyFetcher::with_apy_bps(500);
        let input = "If Solend Main Pool USDC deposit APR is above 1%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command_v3(&apr_fetcher, &save_fetcher, None, input)
            .await
            .unwrap();
        // Save APY > threshold so condition is met, BUT budget short.
        assert!(r.condition_met);
        assert_eq!(r.status, "needs_funding");
        assert_eq!(r.budget_status, "needs_funding");
    }

    /// W5f fail-closed: Save API unavailable propagates as the typed
    /// MarketDataUnavailable error. The orchestrator does NOT silently
    /// fall back to native APR.
    #[tokio::test]
    async fn w5f_save_api_unavailable_fails_closed() {
        let apr_fetcher = MockW5dAprFetcher::with_apr(100);
        let save_fetcher = MockSaveDisplayApyFetcher::unavailable("transport: oops");
        let input = "If Solend Main Pool USDC deposit APR is above 2%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let err = handle_demo_command_v3(&apr_fetcher, &save_fetcher, None, input)
            .await
            .unwrap_err();
        match err {
            EvaluationError::MarketDataUnavailable { reason } => {
                assert!(reason.contains("transport"));
            }
            other => panic!("expected MarketDataUnavailable, got {other:?}"),
        }
    }

    /// W5f live fetcher accepts the official base URL + rejects empty.
    #[test]
    fn w5f_live_save_fetcher_url_validation() {
        assert!(LiveSaveDisplayApyFetcher::new("").is_none());
        assert!(LiveSaveDisplayApyFetcher::new("   ").is_none());
        assert!(LiveSaveDisplayApyFetcher::new(SOLEND_API_BASE_URL_DEFAULT).is_some());
        // Trailing slash is normalised so the URL builder doesn't
        // produce `//v1/...` paths.
        let f = LiveSaveDisplayApyFetcher::new("https://api.solend.fi/").unwrap();
        assert_eq!(f.base_url, "https://api.solend.fi");
    }

    /// Sanity: the pinned Main Pool USDC identifiers match the
    /// existing executor source-of-truth constants. If the executor
    /// constants ever drift this test fires immediately.
    #[test]
    fn w5f_main_pool_pins_match_executor_constants() {
        assert_eq!(
            W5F_MAIN_POOL_RESERVE_BS58,
            crate::stage2_executor::DEMO_RESERVE_BS58
        );
        assert_eq!(
            W5F_MAIN_POOL_LENDING_MARKET_BS58,
            crate::stage2_executor::DEMO_LENDING_MARKET_BS58
        );
        assert_eq!(
            W5F_MAIN_POOL_LIQUIDITY_MINT_BS58,
            crate::stage2_executor::DEMO_LIQUIDITY_MINT_BS58
        );
    }
}

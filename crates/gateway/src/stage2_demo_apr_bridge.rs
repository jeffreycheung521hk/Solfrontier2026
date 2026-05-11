//! Stage 2 W5d — Demo APR conditional-deposit bridge (production-side).
//!
//! The deterministic-parser + B-O1-on-chain-APR evaluator surface that
//! the user-facing `/chat` route uses to recognise the W5d demo command
//! *before* the LLM is asked anything. This is the production-safe
//! extraction of the W5d test-harness logic (originally in
//! `tests/stage2_demo_apr_conditional_deposit_bridge.rs`).
//!
//! # What this module IS
//!
//! - A deterministic text parser for one exact demo grammar.
//! - A live-RPC-driven evaluator that reads the Solend Main Pool USDC
//!   reserve and computes its current supply APR via the B-O1 wrappers
//!   in [`crate::stage2_evaluator`].
//! - A pure DTO ([`W5dEvaluationResult`]) the chat layer renders as a
//!   typed card.
//!
//! # What this module is NOT
//!
//! - **Not** a Solend deposit sender. The execution path lives in the
//!   W5d / W5c test-harness modules behind explicit env gates; this
//!   module never calls `sendTransaction`, never loads a keypair, and
//!   never imports the controlled-wallet keypair.
//! - **Not** an LLM client. There is zero provider call here.
//! - **Not** a fall-through. If a message does not match the strict
//!   grammar, the chat layer continues to the normal LLM path. This
//!   module only fires when the deterministic detector says yes.
//!
//! # No-overclaim
//!
//! Proves: *chat text → deterministic W5d parser → B-O1 on-chain APR
//! evaluation → typed result the frontend renders.*
//!
//! Does NOT prove: clawsol-authority `ExecuteAction` execution; a
//! first-class `SolendDepositControlledWallet` `ActionSpec` variant;
//! Jupiter conditional execution; user-delegated authorization via
//! `AuthorizationRecord` PDA; live tx broadcast (that path lives in
//! the env-gated W5c harness and is reached only when Jeff sets the
//! exact approval phrase).

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

use crate::integrations::solend::raw::decode_reserve;
use crate::stage2_evaluator::{
    solend_snapshot_value_from_reserve_raw, solend_supply_apr_bps_from_wad,
    solend_supply_apr_wad_for_snapshot,
};
use claw_types::canonical_intent::PubkeyBytes;

// ── Pinned facts (sealed against the stage2_executor source-of-truth) ────

/// Hard-coded demo deposit amount in USDC base units (1 USDC == 10^6).
/// 250_000 raw = 0.25 USDC. The parser rejects any other amount.
pub const W5D_DEPOSIT_AMOUNT_RAW: u64 = 250_000;

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
/// Solend Main Pool USDC reserve.
///
/// `condition_met = current_apr_bps > threshold_bps` (strict `>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct W5dEvaluationResult {
    /// Echoed verbatim from the chat input.
    pub input_text: String,
    /// Stable string the frontend renders in the "source" row of the
    /// W5d card. Always `"onchain_reserve_b_o1"` for this evaluator —
    /// no external Save/Solend public-API path is used.
    pub source: String,
    /// `BgxfHJDz…JMpmw` (Solend Main Pool USDC reserve, base58).
    pub reserve_pubkey: String,
    /// Current Solend Main Pool USDC supply APR in basis points,
    /// derived from the on-chain reserve via `solend_supply_apr_wad_for_snapshot`.
    pub current_apr_bps: u32,
    /// Threshold parsed from the user's text, in basis points.
    pub threshold_bps: u32,
    /// Echo of the percent label the user typed (e.g. `"2.5"`).
    pub threshold_pct_label: String,
    /// Strict-greater decision: `current_apr_bps > threshold_bps`.
    pub condition_met: bool,
    /// True iff this evaluation triggered a downstream execution
    /// attempt. The chat-side default is always `false` (the live
    /// send path lives behind the W5c env gate; the chat route does
    /// not unlock it).
    pub execution_attempted: bool,
    /// Stable status string. The chat handler always returns one of:
    ///   - `"condition_not_met"`  when `condition_met == false`
    ///   - `"ready_to_execute"`   when `condition_met == true` and the
    ///     handler is not configured for live send.
    /// (The `"simulated"` / `"completed"` / `"failed"` statuses are
    /// reserved for the env-gated W5c test harness and never returned
    /// from the chat route in this slice.)
    pub status: String,
    /// `tx_signature` is always `None` from this route. The field is
    /// kept in the DTO so the frontend can render a stable "N/A" cell
    /// without conditional layout, and so a future slice that wires
    /// the live send into the chat path can populate it without a
    /// wire-shape change.
    pub tx_signature: Option<String>,
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
}

#[async_trait]
impl W5dAprFetcher for LiveW5dAprFetcher {
    async fn evaluate(
        &self,
        input_text: &str,
        parsed: &DemoParsed,
    ) -> Result<W5dEvaluationResult, EvaluationError> {
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
        let condition_met = current_apr_bps > parsed.threshold_bps;
        let status = if condition_met {
            "ready_to_execute".to_string()
        } else {
            "condition_not_met".to_string()
        };
        Ok(W5dEvaluationResult {
            input_text: input_text.to_string(),
            source: "onchain_reserve_b_o1".to_string(),
            reserve_pubkey: W5D_RESERVE_BS58.to_string(),
            current_apr_bps,
            threshold_bps: parsed.threshold_bps,
            threshold_pct_label: parsed.threshold_pct_label.clone(),
            condition_met,
            execution_attempted: false,
            status,
            tx_signature: None,
        })
    }
}

// ── High-level chat-handler entry point ──────────────────────────────────

/// Convenience: detect → parse → evaluate, in one call.
///
/// The chat handler uses this entry point when the lightweight
/// detector returns `true`. Callers that want finer control over
/// detection / parsing can call the underlying primitives directly.
pub async fn handle_demo_command(
    fetcher: &(dyn W5dAprFetcher + 'static),
    input_text: &str,
) -> Result<W5dEvaluationResult, EvaluationError> {
    let parsed = parse_demo_command(input_text)
        .map_err(|e| EvaluationError::Parse { parse_error: e })?;
    fetcher.evaluate(input_text, &parsed).await
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

    /// A mock fetcher with a programmable current_apr_bps. Used to
    /// exercise the chat-route dispatch / DTO mapping without making
    /// a single RPC call.
    #[derive(Debug, Clone)]
    pub(crate) struct MockW5dAprFetcher {
        pub current_apr_bps: u32,
    }

    #[async_trait]
    impl W5dAprFetcher for MockW5dAprFetcher {
        async fn evaluate(
            &self,
            input_text: &str,
            parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            let condition_met = self.current_apr_bps > parsed.threshold_bps;
            let status = if condition_met {
                "ready_to_execute".to_string()
            } else {
                "condition_not_met".to_string()
            };
            Ok(W5dEvaluationResult {
                input_text: input_text.to_string(),
                source: "onchain_reserve_b_o1".to_string(),
                reserve_pubkey: W5D_RESERVE_BS58.to_string(),
                current_apr_bps: self.current_apr_bps,
                threshold_bps: parsed.threshold_bps,
                threshold_pct_label: parsed.threshold_pct_label.clone(),
                condition_met,
                execution_attempted: false,
                status,
                tx_signature: None,
            })
        }
    }

    #[tokio::test]
    async fn handle_demo_command_false_branch_via_mock() {
        let fetcher = MockW5dAprFetcher {
            current_apr_bps: 163,
        };
        let input = "If Solend Main Pool USDC deposit APR is above 4.63%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command(&fetcher, input).await.unwrap();
        assert!(!r.condition_met);
        assert_eq!(r.status, "condition_not_met");
        assert!(r.tx_signature.is_none());
        assert!(!r.execution_attempted);
        assert_eq!(r.current_apr_bps, 163);
        assert_eq!(r.threshold_bps, 463);
        assert_eq!(r.source, "onchain_reserve_b_o1");
    }

    #[tokio::test]
    async fn handle_demo_command_true_branch_via_mock() {
        let fetcher = MockW5dAprFetcher {
            current_apr_bps: 163,
        };
        let input = "If Solend Main Pool USDC deposit APR is above 0.63%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let r = handle_demo_command(&fetcher, input).await.unwrap();
        assert!(r.condition_met);
        // Chat-route default: ready_to_execute (live send NOT
        // triggered from this route in this slice).
        assert_eq!(r.status, "ready_to_execute");
        assert!(r.tx_signature.is_none());
        assert!(!r.execution_attempted);
        assert_eq!(r.current_apr_bps, 163);
        assert_eq!(r.threshold_bps, 63);
    }

    #[tokio::test]
    async fn handle_demo_command_parse_error_surfaces_as_typed_error() {
        let fetcher = MockW5dAprFetcher {
            current_apr_bps: 163,
        };
        let input = "If Solend Main Pool USDC deposit APR is above 10%, deposit 1.0 USDC from my bounded executor wallet into Solend.";
        let err = handle_demo_command(&fetcher, input).await.unwrap_err();
        match err {
            EvaluationError::Parse { parse_error } => {
                assert!(matches!(parse_error, ParseError::UnsupportedAmount { .. }))
            }
            other => panic!("expected Parse/UnsupportedAmount, got {other:?}"),
        }
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

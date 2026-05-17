//! Stage 2 W5d — Chat / demo APR conditional deposit bridge.
//!
//! Bridges a deterministic text-command parser to the W5c conditional
//! deposit path so the demo can show:
//!
//! ```text
//! User text:
//!   "If Solend Main Pool USDC deposit APR is above X%, deposit 0.25
//!    USDC from my bounded executor wallet into Solend."
//!
//! Flow:
//!   parse_demo_command(text) -> threshold_bps
//!     → fetch Solend Main Pool USDC reserve via Helius RPC
//!     → decode with B-O1 SolendReserveRaw rate-config fields
//!     → claw_gateway::stage2_evaluator::
//!         solend_supply_apr_wad_for_snapshot(...)
//!     → solend_supply_apr_bps_from_wad(...)
//!     → condition_met = current_apr_bps > threshold_bps  (strict >)
//!     → if false  : no execution; banner W5D RULE A RESULT
//!     → if true   : insert fixture rule, optionally invoke W5c live
//!                   client (gated behind W5c approval phrase); banner
//!                   W5D RULE B RESULT
//! ```
//!
//! # Architecture position
//!
//! This is a **chat/demo bridge**. The parser is deterministic
//! (not an LLM call), the APR source is the on-chain reserve via the
//! B-O1 decoder + W3 evaluator's exposed wrappers, and the live-send
//! path is W5c's verbatim direct-Solend deposit client (imported via
//! `common::w5c_deposit_support`). No `clawsol-authority`
//! `ExecuteAction`. No Jupiter. No W6 refund. No user main wallet
//! signer.
//!
//! # Demo scenario (Rule A vs Rule B)
//!
//! The brief's scenario uses the *current* APR as the baseline:
//!
//! - **Rule A** (expected `condition_met = false`):
//!   threshold = `current_apr_bps + 300`. APR can't be `>` itself+3%,
//!   so the rule never fires. The harness must therefore NOT load
//!   the keypair, build a tx, or call the W5c client.
//! - **Rule B** (expected `condition_met = true`):
//!   threshold = `current_apr_bps.saturating_sub(100)`. The actual
//!   APR exceeds it by ≈1% (unless the current APR is already <1%,
//!   in which case the saturating_sub clamps the threshold to 0 and
//!   the rule still fires whenever `current_apr_bps > 0`).
//!
//! # Rule identity / state-store collision guard
//!
//! Rule A and Rule B MUST have distinct `rule_id` AND distinct
//! `canonical_rule_hash` so they coexist in the in-memory state-store
//! without UNIQUE-constraint collisions. The fixture rule builder
//! takes `rule_id` as a parameter (W5d-prep refactor — see
//! `common::w5c_deposit_support::fixture_rule`); each rule's
//! `canonical_rule_hash` is automatically distinct because the hash
//! is computed over `canonical_rule_bytes(rule)` which includes
//! `rule.rule_id`.
//!
//! # Two-phase env gating
//!
//! - **Phase 0** (`CLAW_STAGE2_DEMO_APR_BRIDGE=1`):
//!   Live RPC fetch of the Solend Main Pool USDC reserve; compute
//!   current APR via B-O1; evaluate Rule A and Rule B locally; print
//!   their banners. Rule A halts at `condition_not_met`. Rule B runs
//!   through `Stage2Executor::execute_rule_once` *only* if the live
//!   gates below are also set — otherwise Rule B halts at
//!   `ready_to_execute` (durable state untouched past
//!   `condition_met`).
//!
//! - **Phase 2** (Rule B live send): requires
//!   `CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT=1` **and**
//!   `CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED="W5C LIVE CONDITIONAL
//!   DEPOSIT APPROVED"` (W5d piggy-backs on W5c's approval gate
//!   rather than minting a fresh W5d phrase).
//!
//! Default no-env behaviour: self-skip, no RPC, no keypair, no tx.

mod common;

use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::{pubkey::Pubkey, signature::Signer};

use claw_gateway::integrations::solend::raw::{decode_obligation, decode_reserve};
use claw_gateway::stage2_evaluator::{
    solend_snapshot_value_from_reserve_raw, solend_supply_apr_bps_from_wad,
    solend_supply_apr_wad_for_snapshot,
};
use claw_gateway::stage2_executor::{Stage2Executor, Stage2ExecutorRuleResult};
use claw_gateway::stage2_watcher::Stage2TickContext;

use claw_state_store::db::Database;
use claw_state_store::stage2_watch_rules::{Stage2WatchRuleRepository, WatchRuleStatus};

use claw_types::canonical_intent::PubkeyBytes;
use claw_types::stage2_watch_rule::{canonical_rule_bytes, canonical_rule_hash};

use common::w5c_deposit_support::*;

// ── W5d env var names ─────────────────────────────────────────────────────

const ENV_W5D_MASTER_GATE: &str = "CLAW_STAGE2_DEMO_APR_BRIDGE";
const ENV_W5C_LIVE_DEPOSIT_GATE: &str = "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT";
const ENV_RPC_PRIMARY: &str = "HELIUS_RPC_URL";
const ENV_RPC_SECONDARY: &str = "CLAW_RPC_URL";
const ENV_CLUSTER: &str = "CLAW_STAGE2_CLUSTER";
const ENV_KEYPAIR_PATH: &str = "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH";

// ── Distinct rule_ids ─────────────────────────────────────────────────────
//
// Same length (16 bytes) as the W5c FIXTURE_RULE_ID. Each contains the
// ASCII "W5D" + an "A" or "B" suffix for readability in hex dumps.
const W5D_RULE_A_ID: [u8; 16] = [
    0x57, 0x35, 0x44, 0x41, 0xA1, 0xA1, 0xA1, 0xA1, 0xA1, 0xA1, 0xA1, 0xA1,
    0xA1, 0xA1, 0xA1, 0xA1,
];
const W5D_RULE_B_ID: [u8; 16] = [
    0x57, 0x35, 0x44, 0x42, 0xB2, 0xB2, 0xB2, 0xB2, 0xB2, 0xB2, 0xB2, 0xB2,
    0xB2, 0xB2, 0xB2, 0xB2,
];

/// Source-scan list specific to this binary.
const FORBIDDEN_CALL_FORMS: &[&str] = &[
    "approve(",
    "setAuthority(",
    "set_authority(",
    "delegate(",
    "closeAccount(",
    "close_account(",
    "api.jup.ag/",
    "skipPreflight: true",
    "signTransaction(",
    "window.solana",
    "confirmTransaction(",
    "Keypair::from_base58_string",
    "Helius-Sender",
    "helius-sender",
];

// ──────────────────────────────────────────────────────────────────────────
// Demo-text parser (deterministic; no regex / no LLM dependency)
// ──────────────────────────────────────────────────────────────────────────

/// Decoded shape of the supported demo grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoParsed {
    pub threshold_bps: u32,
    /// The exact percent label the user typed (e.g. `"2.5"`). Echoed
    /// into the banner verbatim — never used in the decision math.
    pub threshold_pct_label: String,
    pub amount_raw: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    MissingMarker(&'static str),
    WrongPool(String),
    WrongAsset(String),
    WrongWallet(String),
    UnsupportedAmount(String),
    MalformedPercent(String),
    NegativeThreshold(String),
    ThresholdOverflow(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingMarker(m) => write!(f, "missing marker: {m}"),
            ParseError::WrongPool(m) => write!(f, "wrong pool: {m}"),
            ParseError::WrongAsset(m) => write!(f, "wrong asset: {m}"),
            ParseError::WrongWallet(m) => write!(f, "wrong/missing wallet phrase: {m}"),
            ParseError::UnsupportedAmount(m) => write!(f, "unsupported amount: {m}"),
            ParseError::MalformedPercent(m) => write!(f, "malformed percent: {m}"),
            ParseError::NegativeThreshold(m) => write!(f, "negative threshold: {m}"),
            ParseError::ThresholdOverflow(m) => write!(f, "threshold overflow: {m}"),
        }
    }
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
/// The literal `bounded executor wallet` and `controlled wallet`
/// are both accepted (they refer to the same wallet — Slice 3C's
/// `BPfDMm…hhs5L` keypair).
pub fn parse_demo_command(text: &str) -> Result<DemoParsed, ParseError> {
    let normalized = normalize_whitespace(&text.to_ascii_lowercase());

    // Required leading marker.
    if !normalized.contains("if ") {
        return Err(ParseError::MissingMarker("\"if\" clause"));
    }

    // Pool / asset markers — the brief is strict: only Solend Main
    // Pool USDC. "save main pool usdc" is accepted as a synonym since
    // Save is Solend's rebrand.
    let pool_ok = normalized.contains("solend main pool usdc")
        || normalized.contains("save main pool usdc");
    if !pool_ok {
        // Try to identify whether the user named a different pool /
        // asset so the error is actionable.
        if normalized.contains("jupiter") {
            return Err(ParseError::WrongPool(
                "Jupiter is not supported in this demo".to_string(),
            ));
        }
        if normalized.contains("main pool") {
            return Err(ParseError::WrongAsset(
                "only Solend/Save Main Pool USDC is supported".to_string(),
            ));
        }
        return Err(ParseError::WrongPool(
            "expected 'Solend Main Pool USDC' (or 'Save Main Pool USDC')"
                .to_string(),
        ));
    }

    if !normalized.contains("deposit apr") {
        return Err(ParseError::MissingMarker("\"deposit APR\""));
    }

    // Wallet phrase — accept either "bounded executor wallet" or
    // "controlled wallet".
    let wallet_ok = normalized.contains("bounded executor wallet")
        || normalized.contains("controlled wallet");
    if !wallet_ok {
        return Err(ParseError::WrongWallet(
            "expected 'bounded executor wallet' or 'controlled wallet'"
                .to_string(),
        ));
    }

    if !normalized.contains("into solend") && !normalized.contains("into save") {
        return Err(ParseError::MissingMarker("\"into Solend\""));
    }

    // Amount — strict 0.25 USDC for this demo. We accept any harmless
    // formatting (`0.25 usdc`, `0.25usdc`, `0.250000 usdc`) by
    // first looking for "0.25" then re-confirming USDC nearby.
    let amount_token = "deposit 0.25";
    if !normalized.contains(amount_token) {
        return Err(ParseError::UnsupportedAmount(
            "only 0.25 USDC is supported in the demo bridge".to_string(),
        ));
    }
    // Confirm "usdc" follows somewhere after the amount.
    let idx = normalized.find(amount_token).unwrap();
    if !normalized[idx..].contains("usdc") {
        return Err(ParseError::WrongAsset(
            "amount must be denominated in USDC".to_string(),
        ));
    }

    // Threshold — find the "above " token and the trailing "%".
    let above_token = "above ";
    let after_above = match normalized.find(above_token) {
        Some(i) => &normalized[i + above_token.len()..],
        None => return Err(ParseError::MissingMarker("\"above <X>%\"")),
    };
    let pct_end = after_above.find('%').ok_or_else(|| {
        ParseError::MalformedPercent(
            "expected '%' after the threshold percent".to_string(),
        )
    })?;
    let pct_str = after_above[..pct_end].trim();
    if pct_str.is_empty() {
        return Err(ParseError::MalformedPercent("percent value is empty".to_string()));
    }
    if pct_str.starts_with('-') {
        return Err(ParseError::NegativeThreshold(pct_str.to_string()));
    }
    let threshold_bps = decimal_percent_to_bps(pct_str)?;
    Ok(DemoParsed {
        threshold_bps,
        threshold_pct_label: pct_str.to_string(),
        amount_raw: 250_000,
    })
}

/// Convert a decimal-percent string (e.g. `"2.5"`, `"0.75"`, `"10"`,
/// `".5"`) into an integer basis-points value. 1 percent == 100 bps.
/// At most 2 fractional digits are read; an explicit error is raised
/// for non-digit characters or a malformed shape.
///
/// All arithmetic is u32-saturated. Caller's contract: the input has
/// already been stripped of the trailing `%` and any whitespace.
fn decimal_percent_to_bps(s: &str) -> Result<u32, ParseError> {
    if s.is_empty() {
        return Err(ParseError::MalformedPercent("empty".to_string()));
    }
    // Must consist of [0-9] and at most one '.'
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 2 {
        return Err(ParseError::MalformedPercent(format!(
            "multiple '.' in '{s}'"
        )));
    }
    let int_str = parts[0];
    let frac_str = parts.get(1).copied().unwrap_or("");
    // Allow ".5" — int_str may be empty if leading-dot, treat as 0.
    let int_val: u32 = if int_str.is_empty() {
        0
    } else {
        int_str.parse::<u32>().map_err(|e| {
            ParseError::MalformedPercent(format!("int part '{int_str}': {e}"))
        })?
    };
    if frac_str.len() > 2 {
        return Err(ParseError::MalformedPercent(format!(
            "more than 2 fractional digits in '{s}'"
        )));
    }
    // Pad the fractional digits to exactly 2 (so each percent's
    // hundredths place contributes 1 bps).
    let mut frac_padded = String::with_capacity(2);
    frac_padded.push_str(frac_str);
    while frac_padded.len() < 2 {
        frac_padded.push('0');
    }
    let frac_val: u32 = if frac_padded.is_empty() {
        0
    } else {
        frac_padded.parse::<u32>().map_err(|e| {
            ParseError::MalformedPercent(format!("frac part '{frac_padded}': {e}"))
        })?
    };
    int_val
        .checked_mul(100)
        .and_then(|n| n.checked_add(frac_val))
        .ok_or_else(|| ParseError::ThresholdOverflow(s.to_string()))
}

/// Collapse runs of ASCII whitespace into single spaces. Leaves
/// non-whitespace characters (digits, letters, punctuation) untouched.
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

// ──────────────────────────────────────────────────────────────────────────
// Env reader
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum EnvStatus {
    Skipped(String),
    Mismatch(String),
    Ready(Box<BridgeConfig>),
}

#[derive(Debug)]
struct BridgeConfig {
    rpc_url: String,
    cluster: Cluster,
    keypair_path: Option<String>,
    /// True iff both `CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT=1` AND the
    /// W5c approval phrase are set. When false, Rule B still
    /// evaluates `condition_met=true` but does NOT broadcast.
    live_send_authorised: bool,
    /// True iff the live deposit gate is set (regardless of approval
    /// phrase). When true, Rule B's branch may construct the W5c
    /// client + state-store and run a Phase-0 simulation. When false
    /// (only the W5d master gate is set), Rule B stops at
    /// `ready_to_execute` after the APR check.
    live_deposit_path_enabled: bool,
}

impl BridgeConfig {
    fn from_env_with<F>(getter: F) -> EnvStatus
    where
        F: Fn(&str) -> Option<String>,
    {
        if getter(ENV_W5D_MASTER_GATE).as_deref() != Some("1") {
            return EnvStatus::Skipped(format!(
                "{ENV_W5D_MASTER_GATE} not set to 1 — W5d demo APR bridge self-skipped"
            ));
        }
        let rpc_url = match getter(ENV_RPC_PRIMARY).or_else(|| getter(ENV_RPC_SECONDARY)) {
            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => {
                return EnvStatus::Skipped(format!(
                    "neither {ENV_RPC_PRIMARY} nor {ENV_RPC_SECONDARY} is set"
                ))
            }
        };
        let cluster_str = match getter(ENV_CLUSTER) {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => return EnvStatus::Skipped(format!("{ENV_CLUSTER} not set")),
        };
        let cluster = match Cluster::parse(&cluster_str) {
            Some(c) => c,
            None => {
                return EnvStatus::Mismatch(format!(
                    "{ENV_CLUSTER}='{cluster_str}' is not one of mainnet-beta | devnet | localnet"
                ))
            }
        };
        if cluster != Cluster::MainnetBeta {
            return EnvStatus::Mismatch(format!(
                "demo bridge target is mainnet-beta only; got cluster={cluster:?}"
            ));
        }
        let live_deposit_path_enabled =
            getter(ENV_W5C_LIVE_DEPOSIT_GATE).as_deref() == Some("1");
        let approval_phrase_set =
            getter(W5C_ENV_APPROVAL).as_deref() == Some(W5C_APPROVAL_PHRASE);
        let live_send_authorised = live_deposit_path_enabled && approval_phrase_set;
        let keypair_path = if live_deposit_path_enabled {
            // Live deposit path needs the keypair (for sim OR send).
            match getter(ENV_KEYPAIR_PATH) {
                Some(p) if !p.trim().is_empty() => Some(p.trim().to_string()),
                _ => {
                    return EnvStatus::Mismatch(format!(
                        "{ENV_W5C_LIVE_DEPOSIT_GATE}=1 requires {ENV_KEYPAIR_PATH} to point at a keypair"
                    ))
                }
            }
        } else {
            None
        };
        EnvStatus::Ready(Box::new(BridgeConfig {
            rpc_url,
            cluster,
            keypair_path,
            live_send_authorised,
            live_deposit_path_enabled,
        }))
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Banners (W5d-specific)
// ──────────────────────────────────────────────────────────────────────────

struct RuleABanner {
    input_text: String,
    reserve: Pubkey,
    rule_id: [u8; 16],
    canonical_rule_hash_hex: String,
    current_apr_bps: u32,
    threshold_bps: u32,
}

fn print_rule_a_banner(b: &RuleABanner) {
    eprintln!("\n============================================================");
    eprintln!("W5D CONDITIONAL DEPOSIT — RULE A");
    eprintln!("input: {}", b.input_text);
    eprintln!("source: onchain_reserve_b_o1");
    eprintln!("reserve: {}", b.reserve);
    eprintln!("rule_id: {}", hex_id(&b.rule_id));
    eprintln!("canonical_rule_hash: {}", b.canonical_rule_hash_hex);
    eprintln!("current_apr_bps: {}", b.current_apr_bps);
    eprintln!("threshold_bps: {}", b.threshold_bps);
    eprintln!("condition_met: false");
    eprintln!("execution_attempted: false");
    eprintln!("status: condition_not_met");
    eprintln!("tx_signature: N/A");
    eprintln!("============================================================");
}

struct RuleBBanner<'a> {
    input_text: String,
    reserve: Pubkey,
    rule_id: [u8; 16],
    canonical_rule_hash_hex: String,
    current_apr_bps: u32,
    threshold_bps: u32,
    /// `"ready_to_execute"` | `"simulated"` | `"completed"` | `"failed"`.
    w5c_status: &'a str,
    /// `"condition_met -> executing -> ..."` text.
    rule_transition: String,
    tx_signature: Option<String>,
}

fn print_rule_b_banner(b: &RuleBBanner) {
    eprintln!("\n============================================================");
    eprintln!("W5D CONDITIONAL DEPOSIT — RULE B");
    eprintln!("input: {}", b.input_text);
    eprintln!("source: onchain_reserve_b_o1");
    eprintln!("reserve: {}", b.reserve);
    eprintln!("rule_id: {}", hex_id(&b.rule_id));
    eprintln!("canonical_rule_hash: {}", b.canonical_rule_hash_hex);
    eprintln!("current_apr_bps: {}", b.current_apr_bps);
    eprintln!("threshold_bps: {}", b.threshold_bps);
    eprintln!("condition_met: true");
    eprintln!("execution_attempted: true");
    eprintln!("w5c_status: {}", b.w5c_status);
    eprintln!("rule_transition: {}", b.rule_transition);
    eprintln!(
        "tx_signature: {}",
        b.tx_signature.as_deref().unwrap_or("N/A")
    );
    eprintln!(
        "solscan: {}",
        b.tx_signature
            .as_deref()
            .map(|s| format!("https://solscan.io/tx/{s}"))
            .unwrap_or_else(|| "N/A".to_string())
    );
    eprintln!("============================================================");
}

fn canonical_rule_hash_hex(rule: &claw_types::stage2_watch_rule::WatchRule) -> String {
    let h = canonical_rule_hash(rule);
    let mut s = String::with_capacity(64);
    for b in h {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn format_demo_input(threshold_pct_label: &str) -> String {
    format!(
        "If Solend Main Pool USDC deposit APR is above {}%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        threshold_pct_label
    )
}

// ──────────────────────────────────────────────────────────────────────────
// Live test (env-gated)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage2_w5d_live_demo_apr_conditional_deposit_bridge() {
    let status = BridgeConfig::from_env_with(|k| env::var(k).ok());
    let cfg = match status {
        EnvStatus::Skipped(reason) => {
            eprintln!("[skip] W5d APR demo bridge: {reason}");
            return;
        }
        EnvStatus::Mismatch(msg) => panic!("STOP — W5d APR demo bridge: {msg}"),
        EnvStatus::Ready(c) => c,
    };

    eprintln!("──── W5d demo APR conditional deposit bridge ────");
    eprintln!("cluster                : {:?}", cfg.cluster);
    eprintln!("rpc                    : {}", redact_url(&cfg.rpc_url));
    eprintln!(
        "live deposit gate      : {}",
        if cfg.live_deposit_path_enabled { "ON" } else { "off (Phase-0 APR-only)" }
    );
    eprintln!(
        "live send authorised   : {}",
        if cfg.live_send_authorised { "YES (W5c approval phrase set)" } else { "no" }
    );

    // ── Fetch + decode reserve ──────────────────────────────────────────
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .expect("build reqwest client");
    let reserve_pubkey = Pubkey::from_str(claw_gateway::stage2_executor::DEMO_RESERVE_BS58)
        .expect("DEMO_RESERVE_BS58 parses");

    let reserve_bytes = retry(
        || rpc_get_account_data(&http, &cfg.rpc_url, &reserve_pubkey),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getAccountInfo(reserve): {e}"))
    .unwrap_or_else(|| panic!("STOP — reserve account missing"));
    let raw = decode_reserve(&reserve_bytes)
        .unwrap_or_else(|e| panic!("STOP — decode_reserve: {e}"));
    verify_reserve_p5c(&raw)
        .unwrap_or_else(|e| panic!("STOP — reserve P5c mismatch: {}", e.0));

    let reserve_address_bytes = PubkeyBytes::new(reserve_pubkey.to_bytes());
    let snapshot = solend_snapshot_value_from_reserve_raw(reserve_address_bytes, &raw);
    let apr_wad = solend_supply_apr_wad_for_snapshot(&snapshot)
        .unwrap_or_else(|e| panic!("STOP — supply_apr_wad: {e}"));
    let apr_bps_u64 = solend_supply_apr_bps_from_wad(apr_wad);
    if apr_bps_u64 > 10_000 {
        panic!("STOP — current_apr_bps={apr_bps_u64} > 10_000 (100%) implausibly high");
    }
    let current_apr_bps = apr_bps_u64 as u32;
    eprintln!(
        "✓ reserve decoded; current Solend Main Pool USDC supply APR = {} bps ({}.{:02}%)",
        current_apr_bps,
        current_apr_bps / 100,
        current_apr_bps % 100
    );

    // ── Rule A: condition is always false (current+3%) ──────────────────
    let rule_a_threshold_bps =
        current_apr_bps.checked_add(300).expect("threshold add overflow");
    let rule_a_text = format_demo_input(&format!(
        "{}.{:02}",
        rule_a_threshold_bps / 100,
        rule_a_threshold_bps % 100
    ));
    let _parsed_a = parse_demo_command(&rule_a_text)
        .unwrap_or_else(|e| panic!("STOP — Rule A parser self-check: {e}"));
    let rule_a = fixture_rule(
        W5D_RULE_A_ID,
        DEFAULT_DEPOSIT_AMOUNT_RAW,
        DEFAULT_TARGET_OBLIGATION_BS58,
        CONTROLLED_WALLET_BS58,
        CONTROLLED_WALLET_BS58,
        u64::MAX,
    );
    let rule_a_hash = canonical_rule_hash_hex(&rule_a);
    let condition_a_met = current_apr_bps > rule_a_threshold_bps;
    assert!(
        !condition_a_met,
        "Rule A condition must be unreachable (current+300 cannot be > current+300+1bps)"
    );
    print_rule_a_banner(&RuleABanner {
        input_text: rule_a_text,
        reserve: reserve_pubkey,
        rule_id: W5D_RULE_A_ID,
        canonical_rule_hash_hex: rule_a_hash.clone(),
        current_apr_bps,
        threshold_bps: rule_a_threshold_bps,
    });

    // ── Rule B: condition is always true (current-1%, saturating) ───────
    let rule_b_threshold_bps = current_apr_bps.saturating_sub(100);
    let rule_b_text = format_demo_input(&format!(
        "{}.{:02}",
        rule_b_threshold_bps / 100,
        rule_b_threshold_bps % 100
    ));
    let _parsed_b = parse_demo_command(&rule_b_text)
        .unwrap_or_else(|e| panic!("STOP — Rule B parser self-check: {e}"));
    let condition_b_met = current_apr_bps > rule_b_threshold_bps;
    if !condition_b_met {
        // This can happen only if current_apr_bps == 0, in which case
        // the demo cannot produce a true branch. Halt clearly.
        panic!(
            "STOP — Rule B's current_apr_bps={current_apr_bps} could not produce a true branch \
             after saturating threshold to {rule_b_threshold_bps}; demo requires a non-zero APR"
        );
    }

    // Build the Rule B fixture (distinct rule_id ensures distinct
    // canonical_rule_hash from Rule A — see the rule-identity guard
    // unit test).
    let chain_slot = retry(
        || rpc_get_slot(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getSlot: {e}"));
    let rule_b = fixture_rule(
        W5D_RULE_B_ID,
        DEFAULT_DEPOSIT_AMOUNT_RAW,
        DEFAULT_TARGET_OBLIGATION_BS58,
        CONTROLLED_WALLET_BS58,
        CONTROLLED_WALLET_BS58,
        chain_slot.saturating_add(50_000),
    );
    let rule_b_hash = canonical_rule_hash_hex(&rule_b);
    // Sanity: distinct rule_id => distinct hash.
    assert_ne!(
        rule_a_hash, rule_b_hash,
        "Rule A and Rule B must produce distinct canonical_rule_hash"
    );

    // ── If the live-deposit path is OFF, we stop here in
    //    ready_to_execute. Rule B has been evaluated true but the
    //    harness does not touch the state-store or load a keypair.
    if !cfg.live_deposit_path_enabled {
        print_rule_b_banner(&RuleBBanner {
            input_text: rule_b_text,
            reserve: reserve_pubkey,
            rule_id: W5D_RULE_B_ID,
            canonical_rule_hash_hex: rule_b_hash,
            current_apr_bps,
            threshold_bps: rule_b_threshold_bps,
            w5c_status: "ready_to_execute",
            rule_transition: "(state-store not touched — W5c live gate off)".to_string(),
            tx_signature: None,
        });
        return;
    }

    // ── Live-deposit path ON: load keypair + bring up state-store. ──────
    let keypair_path = cfg
        .keypair_path
        .as_deref()
        .expect("live_deposit_path_enabled implies keypair_path is set");
    let kp = load_keypair_from_file(keypair_path)
        .unwrap_or_else(|e| panic!("STOP — keypair load: {e}"));
    let controlled_pk = kp.pubkey();
    let expected_controlled = Pubkey::from_str(CONTROLLED_WALLET_BS58).unwrap();
    if controlled_pk != expected_controlled {
        panic!(
            "STOP — keypair pubkey mismatch: file={controlled_pk} expected={expected_controlled}"
        );
    }

    let db = Database::open_in_memory()
        .await
        .unwrap_or_else(|e| panic!("STOP — open_in_memory: {e}"));
    let repo = Stage2WatchRuleRepository::new(db.pool().clone());

    // Insert BOTH rules so the state-store collision-guard claim is
    // exercised end-to-end. Rule A is not promoted past `active`
    // (it failed its condition); Rule B is promoted to `condition_met`.
    repo.insert(&rule_a)
        .await
        .unwrap_or_else(|e| panic!("STOP — repo.insert(rule_a): {e}"));
    insert_and_mark_condition_met(&repo, &rule_b)
        .await
        .unwrap_or_else(|e| panic!("STOP — repo.insert + mark_condition_met (rule_b): {e}"));

    let target_obligation = Pubkey::from_str(DEFAULT_TARGET_OBLIGATION_BS58).unwrap();
    let client = LiveSolendDepositExecutionClient::new(kp, cfg.rpc_url.clone(), target_obligation);

    if !cfg.live_send_authorised {
        // Run a simulation-only path through the client (no executor
        // CAS, no broadcast). Rule B's durable state stays at
        // `condition_met`.
        let stored_b = repo
            .get(&rule_b.rule_id)
            .await
            .ok()
            .flatten()
            .expect("rule_b readable after insert");
        let dummy_executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(claw_gateway::stage2_executor::MockExecutionClient::with_success_default()),
        );
        let preview_request = dummy_executor
            .build_execute_action_request(&stored_b, stored_b.execution_nonce.saturating_add(1))
            .unwrap_or_else(|e| panic!("STOP — build_execute_action_request: {e}"));
        let (latest, sim, _plan) = client
            .simulate_only(&preview_request)
            .await
            .unwrap_or_else(|e| panic!("STOP — Rule B simulation: {e}"));
        eprintln!(
            "✓ Rule B simulation OK   blockhash={} CU={:?}",
            latest.hash, sim.units_consumed
        );
        if let Some(err) = &sim.err {
            panic!("STOP — Rule B simulation failed: {err}");
        }
        // Assert Rule B's durable status is still condition_met
        // (simulation must NOT advance the state machine).
        let after_b = repo
            .get(&rule_b.rule_id)
            .await
            .ok()
            .flatten()
            .expect("rule_b readable");
        assert_eq!(
            after_b.status,
            WatchRuleStatus::ConditionMet,
            "simulation-only path must not advance Rule B past condition_met"
        );
        print_rule_b_banner(&RuleBBanner {
            input_text: rule_b_text,
            reserve: reserve_pubkey,
            rule_id: W5D_RULE_B_ID,
            canonical_rule_hash_hex: rule_b_hash,
            current_apr_bps,
            threshold_bps: rule_b_threshold_bps,
            w5c_status: "simulated",
            rule_transition: "condition_met (not advanced; simulation-only)".to_string(),
            tx_signature: None,
        });
        return;
    }

    // ── Live send authorised. Invoke W5c live path via executor. ────────
    eprintln!("\n── Rule B Phase 2: live send via Stage2Executor (W5c path) ──");
    let executor = Stage2Executor::new(repo.clone(), Arc::new(client));
    let ctx = Stage2TickContext::new(
        chain_slot,
        chrono::Utc::now().timestamp(),
        chrono::Utc::now().timestamp_millis(),
    );
    let result = executor.execute_rule_once(rule_b.rule_id, ctx).await;

    let signature = match result {
        Stage2ExecutorRuleResult::Completed {
            execution_nonce,
            used_amount_raw,
            confirmation_slot,
            signature_sentinel,
            ..
        } => {
            eprintln!(
                "✓ executor Completed: execution_nonce={execution_nonce} slot={confirmation_slot} used={used_amount_raw}"
            );
            signature_sentinel
        }
        Stage2ExecutorRuleResult::Failed { error, .. } => {
            print_rule_b_banner(&RuleBBanner {
                input_text: rule_b_text,
                reserve: reserve_pubkey,
                rule_id: W5D_RULE_B_ID,
                canonical_rule_hash_hex: rule_b_hash,
                current_apr_bps,
                threshold_bps: rule_b_threshold_bps,
                w5c_status: "failed",
                rule_transition: "condition_met -> executing -> failed".to_string(),
                tx_signature: None,
            });
            panic!("STOP — executor Failed: {error}");
        }
        other => panic!("STOP — unexpected executor outcome: {other:?}"),
    };

    let after_b = repo
        .get(&rule_b.rule_id)
        .await
        .ok()
        .flatten()
        .expect("rule_b readable post-phase-2");
    assert_eq!(after_b.status, WatchRuleStatus::Completed);
    assert!(after_b.completed);

    // Cross-check SQL column (rule_json is frozen — see W5c's commit
    // 3d30a50 for the precedent).
    let used_in_sql: (i64,) = sqlx::query_as(
        "SELECT used_amount_raw FROM stage2_watch_rules WHERE rule_id = ?",
    )
    .bind(hex_id(&rule_b.rule_id))
    .fetch_one(db.pool())
    .await
    .unwrap_or_else(|e| panic!("STOP — SELECT used_amount_raw: {e}"));
    assert_eq!(used_in_sql.0 as u64, DEFAULT_DEPOSIT_AMOUNT_RAW);

    // Independent on-chain delta verification.
    let source_usdc_ata = spl_associated_token_account::get_associated_token_address(
        &expected_controlled,
        &Pubkey::from_str(claw_gateway::stage2_executor::DEMO_LIQUIDITY_MINT_BS58).unwrap(),
    );
    let _source_after = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &source_usdc_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — source USDC after: {e}"))
    .unwrap_or_else(|| panic!("STOP — source ATA vanished"));
    let obligation_bytes_after = retry(
        || rpc_get_account_data(&http, &cfg.rpc_url, &target_obligation),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — refetch obligation: {e}"))
    .unwrap_or_else(|| panic!("STOP — obligation vanished after tx"));
    let _obligation_after = decode_obligation(&obligation_bytes_after)
        .unwrap_or_else(|e| panic!("STOP — re-decode obligation: {e}"));
    // The W5c regression already pins the exact USDC/cToken delta
    // assertions per-tx; here we limit the post-tx check to "state
    // stayed sane" (source ATA + obligation account still readable)
    // so a future change to the W5c client's delta-assertion site
    // does not duplicate-fail through W5d.

    print_rule_b_banner(&RuleBBanner {
        input_text: rule_b_text,
        reserve: reserve_pubkey,
        rule_id: W5D_RULE_B_ID,
        canonical_rule_hash_hex: rule_b_hash,
        current_apr_bps,
        threshold_bps: rule_b_threshold_bps,
        w5c_status: "completed",
        rule_transition: "condition_met -> executing -> completed".to_string(),
        tx_signature: Some(signature.clone()),
    });

    eprintln!("\n──── W5d Rule B live deposit complete ────");
    eprintln!("signature : {signature}");
    eprintln!("Solscan   : https://solscan.io/tx/{signature}");
}

// ──────────────────────────────────────────────────────────────────────────
// Unit tests (always run; non-live; no network; no keypair)
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parser unit tests ─────────────────────────────────────────────────

    #[test]
    fn parser_accepts_canonical_phrase_10pct() {
        let s = "If Solend Main Pool USDC deposit APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let p = parse_demo_command(s).unwrap();
        assert_eq!(p.threshold_bps, 1000);
        assert_eq!(p.amount_raw, 250_000);
        assert_eq!(p.threshold_pct_label, "10");
    }

    #[test]
    fn parser_accepts_2_point_5_pct() {
        let s = "if solend main pool usdc deposit apr is above 2.5%, deposit 0.25 usdc from my bounded executor wallet into solend.";
        let p = parse_demo_command(s).unwrap();
        assert_eq!(p.threshold_bps, 250);
        assert_eq!(p.threshold_pct_label, "2.5");
    }

    #[test]
    fn parser_accepts_0_point_75_pct() {
        let s = "If solend main pool USDC deposit APR is above 0.75%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let p = parse_demo_command(s).unwrap();
        assert_eq!(p.threshold_bps, 75);
        assert_eq!(p.threshold_pct_label, "0.75");
    }

    #[test]
    fn parser_tolerates_extra_whitespace_and_mixed_case() {
        let s = "   IF    Solend   Main  Pool   USDC \n  deposit APR is\tabove  3.25 %  ,  deposit 0.25 USDC from MY controlled wallet into Solend.   ";
        // Note: the whitespace between number and '%' is harmless;
        // our parser greedily takes the chars up to '%'.
        let p = parse_demo_command(s).unwrap();
        assert_eq!(p.threshold_bps, 325);
    }

    #[test]
    fn parser_accepts_controlled_wallet_synonym() {
        let s = "If Solend Main Pool USDC deposit APR is above 5%, deposit 0.25 USDC from my controlled wallet into Solend.";
        let p = parse_demo_command(s).unwrap();
        assert_eq!(p.threshold_bps, 500);
    }

    #[test]
    fn parser_accepts_save_rebrand() {
        // Save = Solend rebrand.
        let s = "If Save Main Pool USDC deposit APR is above 4%, deposit 0.25 USDC from my bounded executor wallet into Save.";
        let p = parse_demo_command(s).unwrap();
        assert_eq!(p.threshold_bps, 400);
    }

    #[test]
    fn parser_rejects_wrong_pool() {
        let s = "If Marginfi USDC pool APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::WrongPool(_) => {}
            other => panic!("expected WrongPool, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_jupiter_phrase() {
        let s = "If Jupiter swap APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::WrongPool(m) => assert!(m.contains("Jupiter")),
            other => panic!("expected WrongPool/Jupiter, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_unsupported_amount() {
        let s = "If Solend Main Pool USDC deposit APR is above 10%, deposit 1.0 USDC from my bounded executor wallet into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::UnsupportedAmount(_) => {}
            other => panic!("expected UnsupportedAmount, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_missing_wallet_phrase() {
        let s = "If Solend Main Pool USDC deposit APR is above 10%, deposit 0.25 USDC into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::WrongWallet(_) => {}
            other => panic!("expected WrongWallet, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_negative_threshold() {
        let s = "If Solend Main Pool USDC deposit APR is above -1%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::NegativeThreshold(_) => {}
            other => panic!("expected NegativeThreshold, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_malformed_percent() {
        let s = "If Solend Main Pool USDC deposit APR is above abc%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::MalformedPercent(_) => {}
            other => panic!("expected MalformedPercent, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_more_than_two_fractional_digits() {
        let s = "If Solend Main Pool USDC deposit APR is above 1.234%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::MalformedPercent(m) => assert!(m.contains("fractional")),
            other => panic!("expected MalformedPercent, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_missing_if() {
        let s = "Solend Main Pool USDC deposit APR is above 10%, deposit 0.25 USDC from my bounded executor wallet into Solend.";
        let err = parse_demo_command(s).unwrap_err();
        match err {
            ParseError::MissingMarker(m) => assert!(m.contains("if")),
            other => panic!("expected MissingMarker, got {other:?}"),
        }
    }

    // ── decimal_percent_to_bps unit tests ─────────────────────────────────

    #[test]
    fn decimal_percent_unit_table() {
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

    // ── Strict-greater decision logic ─────────────────────────────────────

    #[test]
    fn condition_uses_strict_greater_not_greater_or_equal() {
        // current == threshold → condition_met = false
        let current = 1234u32;
        let threshold = 1234u32;
        assert!(!(current > threshold), "strict > must be false at equality");
    }

    #[test]
    fn scenario_thresholds_yield_expected_branches() {
        let current = 800u32; // 8.00% APR
        let rule_a_threshold = current + 300; // 11.00%
        let rule_b_threshold = current.saturating_sub(100); // 7.00%
        assert!(!(current > rule_a_threshold), "Rule A must be false");
        assert!(current > rule_b_threshold, "Rule B must be true");
    }

    #[test]
    fn saturating_sub_protects_zero_current_apr() {
        // current = 50 bps; threshold = current-100 saturates at 0.
        // current (50) > 0 → Rule B fires; current+300 = 350 → Rule A
        // does not.
        let current = 50u32;
        let rule_a_threshold = current + 300;
        let rule_b_threshold = current.saturating_sub(100);
        assert!(!(current > rule_a_threshold));
        assert!(current > rule_b_threshold);
        // current = 0 → Rule B saturates threshold to 0; 0 > 0 is
        // false → Rule B cannot fire. Surface explicitly: this is the
        // edge the live test panics on.
        let edge_current = 0u32;
        let edge_threshold = edge_current.saturating_sub(100);
        assert!(!(edge_current > edge_threshold));
    }

    // ── Rule identity guards ──────────────────────────────────────────────

    #[test]
    fn rule_a_and_rule_b_have_distinct_rule_ids() {
        assert_ne!(W5D_RULE_A_ID, W5D_RULE_B_ID);
    }

    #[test]
    fn rule_a_and_rule_b_have_distinct_canonical_hashes() {
        let rule_a = fixture_rule(
            W5D_RULE_A_ID,
            DEFAULT_DEPOSIT_AMOUNT_RAW,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            u64::MAX,
        );
        let rule_b = fixture_rule(
            W5D_RULE_B_ID,
            DEFAULT_DEPOSIT_AMOUNT_RAW,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            u64::MAX,
        );
        let hash_a = canonical_rule_hash_hex(&rule_a);
        let hash_b = canonical_rule_hash_hex(&rule_b);
        assert_ne!(hash_a, hash_b);
        // Distinct rule_ids also produce different rule body bytes.
        assert_ne!(canonical_rule_bytes(&rule_a), canonical_rule_bytes(&rule_b));
    }

    // ── Env-reader tests ──────────────────────────────────────────────────

    #[test]
    fn env_gate_skips_without_master_flag() {
        let status = BridgeConfig::from_env_with(|_| None);
        match status {
            EnvStatus::Skipped(m) => assert!(m.contains(ENV_W5D_MASTER_GATE)),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn env_cluster_must_be_mainnet_beta() {
        let status = BridgeConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_DEMO_APR_BRIDGE" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("devnet".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("mainnet-beta")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn env_live_deposit_path_requires_keypair() {
        let status = BridgeConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_DEMO_APR_BRIDGE" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("KEYPAIR_PATH")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn env_phase0_apr_only_does_not_need_keypair() {
        // Master gate only → APR-fetch + parse + decide; no keypair
        // needed; live_send_authorised stays false.
        let status = BridgeConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_DEMO_APR_BRIDGE" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert!(c.keypair_path.is_none());
                assert!(!c.live_deposit_path_enabled);
                assert!(!c.live_send_authorised);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn env_approval_phrase_alone_does_not_authorise_without_live_gate() {
        // Approval phrase set, but live deposit gate NOT set →
        // live_send_authorised stays false (must have both).
        let status = BridgeConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_DEMO_APR_BRIDGE" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED" => Some(W5C_APPROVAL_PHRASE.to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert!(!c.live_deposit_path_enabled);
                assert!(!c.live_send_authorised);
                assert!(c.keypair_path.is_none());
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn env_both_gates_and_exact_phrase_authorise() {
        let status = BridgeConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_DEMO_APR_BRIDGE" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED" => Some(W5C_APPROVAL_PHRASE.to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert!(c.live_deposit_path_enabled);
                assert!(c.live_send_authorised);
                assert_eq!(c.keypair_path.as_deref(), Some("/tmp/fake"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn env_lowercase_approval_phrase_does_not_authorise() {
        let status = BridgeConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_DEMO_APR_BRIDGE" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED" => {
                Some("w5c live conditional deposit approved".to_string())
            }
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert!(c.live_deposit_path_enabled);
                assert!(!c.live_send_authorised);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    // ── Source-scan invariant ─────────────────────────────────────────────

    #[test]
    fn no_send_path_default_invariant() {
        const SOURCE: &str =
            include_str!("stage2_demo_apr_conditional_deposit_bridge.rs");
        for forbidden in FORBIDDEN_CALL_FORMS {
            let count = SOURCE.matches(forbidden).count();
            assert!(
                count <= 1,
                "forbidden call-form `{forbidden}` appears {count} times in W5d harness; \
                 only the FORBIDDEN_CALL_FORMS allowlist entry is permitted"
            );
        }
    }
}

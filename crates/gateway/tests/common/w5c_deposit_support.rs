//! Shared W5c conditional-Solend-deposit test support.
//!
//! Everything here is **moved** from the W5c live harness file
//! (`tests/stage2_live_conditional_solend_deposit.rs`, commits 8bc05ff
//! + b1dc8d8 on origin/main). The items are lifted verbatim — no
//! behaviour change is intended. The only structural differences
//! from the W5c origin are:
//!
//!   1. `pub` visibility added so other test binaries (W5d demo
//!      bridge in particular) can import them.
//!   2. [`fixture_rule`] now takes `rule_id` as a parameter — the
//!      W5c origin hardcoded a single `FIXTURE_RULE_ID` constant,
//!      which is correct for a single-rule harness but breaks the
//!      W5d two-rule scenario (Rule A and Rule B need distinct
//!      `rule_id` / `canonical_rule_hash`).
//!   3. `#![allow(dead_code)]` at the parent `common/mod.rs` level
//!      because each test binary uses a different subset of these
//!      items.
//!
//! W5c-specific items (env-var names, source-scan FORBIDDEN_CALL_FORMS
//! list, banner shape, live test function) stay in the W5c file —
//! they're not generic-deposit infrastructure.
//!
//! W5d-specific items (text parser, two-rule banner shape, scenario
//! decision logic) live in the W5d file.
//!
//! # No Jupiter, no W6 refund, no clawsol-authority ExecuteAction
//!
//! This support module is exclusively for the direct-controlled-wallet
//! → Solend `DepositReserveLiquidityAndObligationCollateral` path. It
//! does not import or reference Jupiter, the controlled-wallet refund
//! TransferChecked path, or the on-chain authorization PDA program.

use std::fs;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
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

use claw_gateway::integrations::solend::deposit::{
    build_deposit_reserve_liquidity_and_obligation_collateral_instruction,
    DepositInstructionInputs,
};
use claw_gateway::integrations::solend::raw::{
    decode_obligation, decode_reserve, SolendObligationRaw, SolendReserveRaw,
};
use claw_gateway::integrations::solend::refresh::{
    build_refresh_instructions, RefreshPlanInputs, ReserveRefreshInput,
};
use claw_gateway::lending::UnderlyingAmount;
use claw_gateway::stage2_executor::{
    Stage2ExecuteActionRequest, Stage2ExecutionClient, Stage2ExecutionError,
    Stage2ExecutionReceipt, DEMO_CTOKEN_MINT_BS58, DEMO_LENDING_MARKET_BS58,
    DEMO_LIQUIDITY_MINT_BS58, DEMO_PYTH_ORACLE_BS58, DEMO_RESERVE_BS58,
};

use claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository;

use claw_types::canonical_intent::PubkeyBytes;
use claw_types::stage2_watch_rule::{
    ActionSpec, Comparison, Condition, ConditionLogic, RateKind, WatchRule, WithdrawMode,
    STAGE2_WATCH_RULE_SCHEMA_VERSION,
};

// ── Pinned facts (sealed against stage2_executor's source-of-truth) ──────

pub const CONTROLLED_WALLET_BS58: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";
pub const DEFAULT_TARGET_OBLIGATION_BS58: &str =
    "BdFLjCcP9mCy557vNNGVbTUuvHxXsh8hc6jXzaPra1wN";
pub const SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

pub const USDC_DECIMALS: u8 = 6;
/// 0.25 USDC; W5c default. W5d default also 0.25 USDC per the demo brief.
pub const DEFAULT_DEPOSIT_AMOUNT_RAW: u64 = 250_000;

pub const SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const COMPUTE_BUDGET_PROGRAM_BS58: &str = "ComputeBudget111111111111111111111111111111";

pub const COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT: u8 = 2;
pub const COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE: u8 = 3;

/// Compute-unit limit. RefreshReserve + Deposit (with 3 SPL Token
/// sub-CPIs) consumed ~88k CU on the W5c mainnet run; 400_000 is
/// a generous ceiling.
pub const COMPUTE_UNIT_LIMIT: u32 = 400_000;
/// Priority fee per CU in micro-lamports. 50_000 μ/CU keeps total
/// priority fee modest while still incentivising leader pickup:
/// 400_000 CU * 50_000 μ/CU / 1_000_000 = 20_000 lamports.
pub const COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 50_000;

// Retry / polling
pub const RPC_MAX_RETRY_ATTEMPTS: usize = 3;
pub const RPC_INITIAL_BACKOFF_MS: u64 = 500;
pub const REQUEST_TIMEOUT_MS: u64 = 15_000;
pub const STATUS_POLL_INTERVAL_MS: u64 = 2_000;
pub const STATUS_POLL_MAX_ATTEMPTS: usize = 60;

/// W5c approval-phrase env var name. W5d reads the same env var to
/// decide whether to invoke W5c's live broadcast path (i.e. it
/// piggy-backs on W5c's approval gate rather than minting a fresh
/// W5d phrase).
pub const W5C_ENV_APPROVAL: &str = "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED";
pub const W5C_APPROVAL_PHRASE: &str = "W5C LIVE CONDITIONAL DEPOSIT APPROVED";

// ── Cluster ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cluster {
    MainnetBeta,
    Devnet,
    Localnet,
}

impl Cluster {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mainnet-beta" | "mainnet" => Some(Cluster::MainnetBeta),
            "devnet" => Some(Cluster::Devnet),
            "localnet" => Some(Cluster::Localnet),
            _ => None,
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

pub fn redact_url(url: &str) -> String {
    let mut redacted = String::with_capacity(url.len());
    let mut in_query = false;
    let mut chars = url.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '?' {
            in_query = true;
            redacted.push(c);
            continue;
        }
        if in_query {
            let mut name = String::new();
            name.push(c);
            while let Some(&n) = chars.peek() {
                if n == '=' || n == '&' {
                    break;
                }
                name.push(n);
                chars.next();
            }
            redacted.push_str(&name);
            if let Some(&'=') = chars.peek() {
                redacted.push('=');
                chars.next();
                let lower = name.to_ascii_lowercase();
                let secret = lower.contains("api-key")
                    || lower.contains("api_key")
                    || lower.contains("token");
                while let Some(&n) = chars.peek() {
                    if n == '&' {
                        break;
                    }
                    chars.next();
                    if !secret {
                        redacted.push(n);
                    }
                }
                if secret {
                    redacted.push_str("***REDACTED***");
                }
            }
            continue;
        }
        redacted.push(c);
    }
    redacted
}

pub fn load_keypair_from_file(path: &str) -> Result<Keypair, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("read keypair file '{path}': {e}"))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .map_err(|e| format!("parse keypair file '{path}' as JSON byte array: {e}"))?;
    if bytes.len() != 64 {
        return Err(format!(
            "keypair file '{path}' contained {} bytes (expected 64)",
            bytes.len()
        ));
    }
    Keypair::from_bytes(&bytes).map_err(|e| format!("Keypair::from_bytes: {e}"))
}

pub async fn retry<F, Fut, T, E>(
    op: F,
    max_attempts: usize,
    initial_backoff: Duration,
) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut last: Option<E> = None;
    let mut attempt = 0;
    while attempt < max_attempts {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = Some(e);
                if attempt < max_attempts {
                    let backoff =
                        initial_backoff.saturating_mul(1u32 << (attempt - 1).min(8) as u32);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
    Err(last.expect("retry exited without recording an error"))
}

// ── RPC helpers ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum RpcErr {
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

pub async fn rpc_post(
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

pub async fn rpc_get_version(c: &reqwest::Client, url: &str) -> Result<String, RpcErr> {
    let v = rpc_post(c, url, json!({"jsonrpc":"2.0","id":1,"method":"getVersion"})).await?;
    v["result"]["solana-core"]
        .as_str()
        .ok_or_else(|| RpcErr::Body("missing solana-core".into()))
        .map(|s| s.to_string())
}

pub async fn rpc_get_slot(c: &reqwest::Client, url: &str) -> Result<u64, RpcErr> {
    let v = rpc_post(c, url, json!({"jsonrpc":"2.0","id":1,"method":"getSlot"})).await?;
    v["result"]
        .as_u64()
        .ok_or_else(|| RpcErr::Body("missing slot".into()))
}

#[derive(Debug)]
pub struct TokenAmount {
    pub raw: u64,
    pub decimals: u8,
}

pub async fn rpc_get_token_account_balance(
    c: &reqwest::Client,
    url: &str,
    ata: &Pubkey,
) -> Result<Option<TokenAmount>, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getTokenAccountBalance",
            "params":[ata.to_string(), {"commitment":"confirmed"}]
        }),
    )
    .await;
    match v {
        Ok(value) => {
            let val = &value["result"]["value"];
            let raw_str = val["amount"]
                .as_str()
                .ok_or_else(|| RpcErr::Body("missing amount".into()))?;
            let raw: u64 = raw_str
                .parse()
                .map_err(|e: std::num::ParseIntError| RpcErr::Body(format!("amount parse: {e}")))?;
            let decimals = val["decimals"].as_u64().unwrap_or(USDC_DECIMALS as u64) as u8;
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

#[derive(Debug)]
pub struct LatestBlockhash {
    pub hash: Hash,
    pub last_valid_block_height: u64,
}

pub async fn rpc_get_latest_blockhash(
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
    let hash = Hash::from_str(bh_str).map_err(|e| RpcErr::Body(format!("hash parse: {e}")))?;
    Ok(LatestBlockhash {
        hash,
        last_valid_block_height: lvbh,
    })
}

pub async fn rpc_get_account_exists(
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

pub async fn rpc_get_account_data(
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
        .ok_or_else(|| RpcErr::Body("missing data array".into()))?;
    let b64 = data_arr
        .first()
        .and_then(|x| x.as_str())
        .ok_or_else(|| RpcErr::Body("missing data[0] base64 string".into()))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| RpcErr::Body(format!("base64 decode: {e}")))?;
    Ok(Some(bytes))
}

#[derive(Debug, Clone)]
pub struct SimResult {
    pub err: Option<Value>,
    pub units_consumed: Option<u64>,
    pub logs: Vec<String>,
}

pub async fn rpc_simulate_transaction(
    c: &reqwest::Client,
    url: &str,
    tx_b64: &str,
) -> Result<SimResult, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"simulateTransaction",
            "params":[tx_b64, {
                "encoding":"base64",
                "commitment":"confirmed",
                "sigVerify": true,
                "replaceRecentBlockhash": false,
            }]
        }),
    )
    .await?;
    let val = &v["result"]["value"];
    let err = val
        .get("err")
        .and_then(|e| if e.is_null() { None } else { Some(e.clone()) });
    let units_consumed = val.get("unitsConsumed").and_then(|u| u.as_u64());
    let logs = val
        .get("logs")
        .and_then(|l| l.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Ok(SimResult {
        err,
        units_consumed,
        logs,
    })
}

pub async fn rpc_send_transaction_base64(
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
pub enum SigStatus {
    NotFound,
    Pending,
    Failed(Value),
    Succeeded { slot: u64, confirmation: String },
}

pub async fn rpc_get_signature_status(
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
            return Ok(SigStatus::Failed(err.clone()));
        }
    }
    let conf = val
        .get("confirmationStatus")
        .and_then(|s| s.as_str())
        .unwrap_or("processed")
        .to_string();
    if conf == "confirmed" || conf == "finalized" {
        let slot = val.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
        Ok(SigStatus::Succeeded {
            slot,
            confirmation: conf,
        })
    } else {
        Ok(SigStatus::Pending)
    }
}

// ── Compute-budget ix builders ────────────────────────────────────────────

pub fn compute_budget_set_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str(COMPUTE_BUDGET_PROGRAM_BS58).unwrap(),
        accounts: vec![],
        data,
    }
}

pub fn compute_budget_set_unit_price(micro_lamports_per_cu: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE);
    data.extend_from_slice(&micro_lamports_per_cu.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str(COMPUTE_BUDGET_PROGRAM_BS58).unwrap(),
        accounts: vec![],
        data,
    }
}

/// Priority-fee math (`compute_unit_limit * micro_lamports_per_cu /
/// 1_000_000 = priority_fee_lamports`). The deposit ix's actual CU
/// consumption is bounded by the limit; this is the worst case.
pub fn estimated_priority_fee_lamports(cu_limit: u32, price_micro_lamports: u64) -> u64 {
    (cu_limit as u64) * price_micro_lamports / 1_000_000
}

// ── Reserve / obligation invariant checks ─────────────────────────────────

#[derive(Debug)]
pub struct ReserveInvariantFail(pub String);

pub fn verify_reserve_p5c(decoded: &SolendReserveRaw) -> Result<(), ReserveInvariantFail> {
    let pin_lm = Pubkey::from_str(DEMO_LENDING_MARKET_BS58).unwrap();
    let pin_liq = Pubkey::from_str(DEMO_LIQUIDITY_MINT_BS58).unwrap();
    let pin_coll = Pubkey::from_str(DEMO_CTOKEN_MINT_BS58).unwrap();
    let pin_pyth = Pubkey::from_str(DEMO_PYTH_ORACLE_BS58).unwrap();
    if decoded.lending_market != pin_lm {
        return Err(ReserveInvariantFail(format!(
            "reserve.lending_market {} != P5c {pin_lm}",
            decoded.lending_market
        )));
    }
    if decoded.liquidity_mint != pin_liq {
        return Err(ReserveInvariantFail(format!(
            "reserve.liquidity_mint {} != P5c {pin_liq}",
            decoded.liquidity_mint
        )));
    }
    if decoded.collateral_mint != pin_coll {
        return Err(ReserveInvariantFail(format!(
            "reserve.collateral_mint {} != P5c {pin_coll}",
            decoded.collateral_mint
        )));
    }
    if decoded.pyth_oracle != pin_pyth {
        return Err(ReserveInvariantFail(format!(
            "reserve.pyth_oracle {} != P5c {pin_pyth}",
            decoded.pyth_oracle
        )));
    }
    if decoded.liquidity_mint_decimals != USDC_DECIMALS {
        return Err(ReserveInvariantFail(format!(
            "reserve.liquidity_mint_decimals {} != {USDC_DECIMALS}",
            decoded.liquidity_mint_decimals
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub struct ObligationInvariantFail(pub String);

pub fn verify_obligation_p5c(
    decoded: &SolendObligationRaw,
    controlled: &Pubkey,
) -> Result<(), ObligationInvariantFail> {
    let pin_lm = Pubkey::from_str(DEMO_LENDING_MARKET_BS58).unwrap();
    if decoded.owner != *controlled {
        return Err(ObligationInvariantFail(format!(
            "obligation.owner {} != controlled wallet {controlled}",
            decoded.owner
        )));
    }
    if decoded.lending_market != pin_lm {
        return Err(ObligationInvariantFail(format!(
            "obligation.lending_market {} != P5c {pin_lm}",
            decoded.lending_market
        )));
    }
    Ok(())
}

pub fn obligation_pinned_reserve_amount(decoded: &SolendObligationRaw) -> u64 {
    let pin_reserve = Pubkey::from_str(DEMO_RESERVE_BS58).unwrap();
    decoded
        .deposits
        .iter()
        .filter(|d| d.deposit_reserve == pin_reserve)
        .map(|d| d.deposited_amount)
        .sum()
}

// ── Live Solend deposit execution client ──────────────────────────────────

/// The first concrete `Stage2ExecutionClient` impl that does a live
/// mainnet send. Built once for W5c; re-used as-is by W5d via the
/// shared `common::w5c_deposit_support` import.
#[derive(Debug)]
pub struct LiveSolendDepositExecutionClient {
    keypair: Keypair,
    rpc_url: String,
    http: reqwest::Client,
    /// The obligation we'll deposit into. Forwarded into the deposit ix
    /// builder verbatim. We treat the executor's
    /// `Stage2ExecuteActionRequest.solend.target_obligation` as the
    /// authoritative source — this field is a sanity-cross-check.
    expected_target_obligation: Pubkey,
}

impl LiveSolendDepositExecutionClient {
    pub fn new(
        keypair: Keypair,
        rpc_url: String,
        expected_target_obligation: Pubkey,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .build()
            .expect("build reqwest client");
        Self {
            keypair,
            rpc_url,
            http,
            expected_target_obligation,
        }
    }

    /// Phase 0 path. Build + simulate the deposit transaction WITHOUT
    /// broadcasting. Returns the (live-blockhash, simulation) pair so
    /// the caller can display logs / CU consumption.
    pub async fn simulate_only(
        &self,
        request: &Stage2ExecuteActionRequest,
    ) -> Result<(LatestBlockhash, SimResult, BuiltTx), Stage2ExecutionError> {
        let plan = self.build_tx_plan(request).await?;
        let latest = retry(
            || rpc_get_latest_blockhash(&self.http, &self.rpc_url),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| Stage2ExecutionError::Internal(format!("getLatestBlockhash: {e}")))?;
        let tx_b64 = self.assemble_and_sign(&plan, &latest)?;
        let sim = retry(
            || rpc_simulate_transaction(&self.http, &self.rpc_url, &tx_b64),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| Stage2ExecutionError::Internal(format!("simulateTransaction: {e}")))?;
        Ok((latest, sim, plan))
    }

    pub async fn build_tx_plan(
        &self,
        request: &Stage2ExecuteActionRequest,
    ) -> Result<BuiltTx, Stage2ExecutionError> {
        // ── Wallet identity guard ────────────────────────────────────
        let payer_pubkey = self.keypair.pubkey();
        let delegated = Pubkey::new_from_array(request.delegated_wallet.0);
        if delegated != payer_pubkey {
            return Err(Stage2ExecutionError::InvalidRequest(format!(
                "delegated_wallet {delegated} != keypair pubkey {payer_pubkey}"
            )));
        }
        let expected_controlled = Pubkey::from_str(CONTROLLED_WALLET_BS58).unwrap();
        if payer_pubkey != expected_controlled {
            return Err(Stage2ExecutionError::InvalidRequest(format!(
                "keypair pubkey {payer_pubkey} != pinned controlled wallet {expected_controlled}"
            )));
        }

        let solend_payload = request
            .solend
            .as_ref()
            .ok_or_else(|| {
                Stage2ExecutionError::InvalidRequest(
                    "Stage2ExecuteActionRequest.solend missing".to_string(),
                )
            })?;
        let target_obligation =
            Pubkey::new_from_array(solend_payload.target_obligation.0);
        if target_obligation != self.expected_target_obligation {
            return Err(Stage2ExecutionError::InvalidRequest(format!(
                "solend.target_obligation {target_obligation} != expected {}",
                self.expected_target_obligation
            )));
        }

        // ── Live reserve decode (defence in depth) ───────────────────
        let solend_program_id = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap();
        let reserve_pubkey = Pubkey::new_from_array(solend_payload.reserve.0);
        let reserve_bytes = retry(
            || rpc_get_account_data(&self.http, &self.rpc_url, &reserve_pubkey),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| Stage2ExecutionError::Internal(format!("getAccountInfo(reserve): {e}")))?
        .ok_or_else(|| {
            Stage2ExecutionError::Internal(format!(
                "reserve account does not exist: {reserve_pubkey}"
            ))
        })?;
        let reserve = decode_reserve(&reserve_bytes)
            .map_err(|e| Stage2ExecutionError::Internal(format!("decode_reserve: {e}")))?;
        verify_reserve_p5c(&reserve)
            .map_err(|e| Stage2ExecutionError::InvalidRequest(format!("reserve P5c: {}", e.0)))?;

        // ── Live obligation decode + delegated-owner check ───────────
        let obligation_bytes = retry(
            || rpc_get_account_data(&self.http, &self.rpc_url, &target_obligation),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| {
            Stage2ExecutionError::Internal(format!("getAccountInfo(obligation): {e}"))
        })?
        .ok_or_else(|| {
            Stage2ExecutionError::Internal(format!(
                "obligation account does not exist: {target_obligation}"
            ))
        })?;
        let obligation = decode_obligation(&obligation_bytes)
            .map_err(|e| Stage2ExecutionError::Internal(format!("decode_obligation: {e}")))?;
        verify_obligation_p5c(&obligation, &payer_pubkey).map_err(|e| {
            Stage2ExecutionError::InvalidRequest(format!("obligation P5c: {}", e.0))
        })?;

        // ── Derive ATAs + cToken-ATA existence ───────────────────────
        let usdc_mint = Pubkey::from_str(DEMO_LIQUIDITY_MINT_BS58).unwrap();
        let ctoken_mint = Pubkey::from_str(DEMO_CTOKEN_MINT_BS58).unwrap();
        let source_usdc_ata = get_associated_token_address(&payer_pubkey, &usdc_mint);
        let ctoken_ata = get_associated_token_address(&payer_pubkey, &ctoken_mint);
        let ctoken_ata_exists = retry(
            || rpc_get_account_exists(&self.http, &self.rpc_url, &ctoken_ata),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| {
            Stage2ExecutionError::Internal(format!("getAccountInfo(cToken ATA): {e}"))
        })?;

        // ── Source-balance gate ──────────────────────────────────────
        let source = retry(
            || rpc_get_token_account_balance(&self.http, &self.rpc_url, &source_usdc_ata),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| {
            Stage2ExecutionError::Internal(format!("getTokenAccountBalance(source): {e}"))
        })?
        .ok_or_else(|| {
            Stage2ExecutionError::InvalidRequest(format!(
                "source USDC ATA does not exist: {source_usdc_ata}"
            ))
        })?;
        if source.decimals != USDC_DECIMALS {
            return Err(Stage2ExecutionError::InvalidRequest(format!(
                "source ATA decimals={} != {USDC_DECIMALS}",
                source.decimals
            )));
        }
        if source.raw < request.input_amount_raw {
            return Err(Stage2ExecutionError::InvalidRequest(format!(
                "source USDC balance {} raw < deposit amount {} raw — controlled wallet \
                 needs refunding first",
                source.raw, request.input_amount_raw
            )));
        }

        // ── Build instruction list ───────────────────────────────────
        let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap();

        let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id,
            reserves: vec![ReserveRefreshInput {
                reserve_pubkey,
                pyth_oracle: reserve.pyth_oracle,
                switchboard_oracle: reserve.switchboard_oracle,
            }],
            obligation: None,
        });

        let deposit_ix =
            build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
                DepositInstructionInputs {
                    solend_program_id,
                    amount: UnderlyingAmount::new(request.input_amount_raw),
                    source_liquidity: source_usdc_ata,
                    user_collateral: ctoken_ata,
                    reserve: reserve_pubkey,
                    reserve_liquidity_supply: reserve.liquidity_supply,
                    reserve_collateral_mint: reserve.collateral_mint,
                    lending_market: reserve.lending_market,
                    destination_deposit_collateral: reserve.collateral_supply,
                    obligation: target_obligation,
                    obligation_owner: payer_pubkey,
                    pyth_oracle: reserve.pyth_oracle,
                    switchboard_oracle: reserve.switchboard_oracle,
                    user_transfer_authority: payer_pubkey,
                },
            )
            .map_err(|e| Stage2ExecutionError::Internal(format!("build deposit ix: {e}")))?;

        let mut ixs: Vec<Instruction> = Vec::with_capacity(5);
        ixs.push(compute_budget_set_unit_limit(COMPUTE_UNIT_LIMIT));
        ixs.push(compute_budget_set_unit_price(
            COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        ));
        if !ctoken_ata_exists {
            ixs.push(create_associated_token_account_idempotent(
                &payer_pubkey,
                &payer_pubkey,
                &ctoken_mint,
                &spl_token,
            ));
        }
        for ix in refresh_plan.instructions {
            ixs.push(ix);
        }
        ixs.push(deposit_ix);

        Ok(BuiltTx {
            ixs,
            payer: payer_pubkey,
            source_usdc_ata,
            ctoken_ata,
            target_obligation,
            reserve_pubkey,
            ctoken_ata_existed_before: ctoken_ata_exists,
            source_usdc_before: source.raw,
            obligation_pinned_reserve_amount_before:
                obligation_pinned_reserve_amount(&obligation),
            obligation_deposits_len_before: obligation.deposits.len(),
        })
    }

    pub fn assemble_and_sign(
        &self,
        plan: &BuiltTx,
        latest: &LatestBlockhash,
    ) -> Result<String, Stage2ExecutionError> {
        let message = Message::new(&plan.ixs, Some(&plan.payer));
        let mut tx = Transaction::new_unsigned(message);
        tx.sign(&[&self.keypair], latest.hash);
        let tx_bytes = bincode::serialize(&tx)
            .map_err(|e| Stage2ExecutionError::Internal(format!("serialize tx: {e}")))?;
        use base64::Engine as _;
        Ok(base64::engine::general_purpose::STANDARD.encode(&tx_bytes))
    }

    pub async fn broadcast_and_confirm(
        &self,
        plan: &BuiltTx,
    ) -> Result<(String, u64, String), Stage2ExecutionError> {
        let fresh = retry(
            || rpc_get_latest_blockhash(&self.http, &self.rpc_url),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| Stage2ExecutionError::SendFailed(format!("fresh blockhash: {e}")))?;
        let tx_b64 = self.assemble_and_sign(plan, &fresh)?;
        let signature = retry(
            || rpc_send_transaction_base64(&self.http, &self.rpc_url, &tx_b64),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        .map_err(|e| Stage2ExecutionError::SendFailed(format!("sendTransaction: {e}")))?;

        let mut attempts = 0;
        let mut last_seen: Option<SigStatus> = None;
        while attempts < STATUS_POLL_MAX_ATTEMPTS {
            attempts += 1;
            tokio::time::sleep(Duration::from_millis(STATUS_POLL_INTERVAL_MS)).await;
            let st = match retry(
                || rpc_get_signature_status(&self.http, &self.rpc_url, &signature),
                RPC_MAX_RETRY_ATTEMPTS,
                Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
            )
            .await
            {
                Ok(s) => s,
                Err(_) => continue,
            };
            match st {
                SigStatus::NotFound | SigStatus::Pending => {
                    last_seen = Some(st);
                    continue;
                }
                SigStatus::Failed(err) => {
                    return Err(Stage2ExecutionError::ConfirmationFailed(format!(
                        "tx failed on chain: {err} (signature {signature})"
                    )));
                }
                SigStatus::Succeeded { slot, confirmation } => {
                    last_seen = Some(SigStatus::Succeeded {
                        slot,
                        confirmation: confirmation.clone(),
                    });
                    if confirmation == "finalized" {
                        return Ok((signature, slot, confirmation));
                    }
                }
            }
        }
        Err(Stage2ExecutionError::ConfirmationFailed(format!(
            "timeout after {STATUS_POLL_MAX_ATTEMPTS} attempts; last seen {last_seen:?}; signature {signature}"
        )))
    }
}

#[async_trait]
impl Stage2ExecutionClient for LiveSolendDepositExecutionClient {
    async fn send_and_confirm(
        &self,
        request: Stage2ExecuteActionRequest,
    ) -> Result<Stage2ExecutionReceipt, Stage2ExecutionError> {
        let plan = self.build_tx_plan(&request).await?;
        let (signature, slot, _confirmation) = self.broadcast_and_confirm(&plan).await?;
        Ok(Stage2ExecutionReceipt {
            rule_id: request.rule_id,
            execution_nonce: request.execution_nonce,
            confirmation_slot: slot,
            used_amount_raw: request.input_amount_raw,
            signature_sentinel: signature,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct BuiltTx {
    pub ixs: Vec<Instruction>,
    pub payer: Pubkey,
    pub source_usdc_ata: Pubkey,
    pub ctoken_ata: Pubkey,
    pub target_obligation: Pubkey,
    pub reserve_pubkey: Pubkey,
    pub ctoken_ata_existed_before: bool,
    pub source_usdc_before: u64,
    pub obligation_pinned_reserve_amount_before: u64,
    pub obligation_deposits_len_before: usize,
}

// ── Fixture rule + state-store helper ─────────────────────────────────────

/// Construct a deterministic fixture rule that:
/// - matches the demo P5c reserve + lending market (so the executor's
///   `DemoSolendExecutionFixture::build_payload` accepts it),
/// - has `delegated_wallet = controlled wallet`,
/// - has `max_input_amount_raw = amount_raw` (one-shot v1 → executor
///   sets `request.input_amount_raw = max - used = max`).
///
/// **W5d change vs W5c origin:** `rule_id` is now a parameter (the
/// W5c origin hardcoded a single `FIXTURE_RULE_ID`). W5d needs
/// distinct ids for Rule A and Rule B.
#[allow(clippy::too_many_arguments)]
pub fn fixture_rule(
    rule_id: [u8; 16],
    amount_raw: u64,
    target_obligation_bs58: &str,
    user_bs58: &str,
    executor_bs58: &str,
    expires_at_slot: u64,
) -> WatchRule {
    let pk = |s: &str| PubkeyBytes::from_base58(s).unwrap();
    let target_obligation = pk(target_obligation_bs58);
    let reserve = pk(DEMO_RESERVE_BS58);
    let lending_market = pk(DEMO_LENDING_MARKET_BS58);
    let solend_program_id = pk(SOLEND_PROGRAM_ID_BS58);
    let delegated = pk(CONTROLLED_WALLET_BS58);
    WatchRule {
        schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
        rule_id,
        user: pk(user_bs58),
        executor: pk(executor_bs58),
        delegated_wallet: delegated,
        created_at_slot: 415_500_000,
        expires_at_slot,
        one_shot: true,
        condition_logic: ConditionLogic::All,
        conditions: vec![Condition::SolendReserveSupplyRate {
            reserve_pubkey: reserve,
            lending_market,
            solend_program_id,
            comparison: Comparison::Lt,
            threshold_bps: 1_000,
            rate_kind: RateKind::Apr,
            formula_version: 1,
            max_reserve_staleness_slots: 16,
            required_refresh_same_tx: true,
        }],
        action: ActionSpec::SolendWithdrawAllDelegated {
            target_obligation,
            reserve_pubkey: reserve,
            lending_market,
            destination_wallet: delegated,
            withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
        },
        max_input_amount_raw: amount_raw,
        used_amount_raw: 0,
        destination: delegated,
        slippage_bps: 0,
    }
}

pub async fn insert_and_mark_condition_met(
    repo: &Stage2WatchRuleRepository,
    rule: &WatchRule,
) -> Result<(), String> {
    repo.insert(rule)
        .await
        .map_err(|e| format!("repo.insert: {e}"))?;
    let updated = repo
        .mark_condition_met_if_active(&rule.rule_id)
        .await
        .map_err(|e| format!("mark_condition_met_if_active: {e}"))?;
    if updated != 1 {
        return Err(format!(
            "mark_condition_met_if_active touched {updated} rows (expected 1)"
        ));
    }
    Ok(())
}

pub fn hex_id(rule_id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in rule_id {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

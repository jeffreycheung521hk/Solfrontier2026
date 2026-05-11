//! Stage 2 W5c — Live conditional Solend deposit harness.
//!
//! First real Stage 2 live conditional path: a `condition_met` rule in
//! the state-store → W4 [`Stage2Executor`] atomic CAS lease → test-local
//! [`LiveSolendDepositExecutionClient`] (implements the existing
//! `Stage2ExecutionClient` trait) → controlled wallet signs a direct
//! Solend deposit on mainnet → confirmation → rule transitions to
//! `completed` or `failed`.
//!
//! # Test-only adapter
//!
//! This slice **intentionally bypasses** `clawsol-authority`. There is
//! no `ExecuteAction`, no `AuthorizationRecord` PDA, and no on-chain
//! verifier. The controlled wallet is the direct signer of the Solend
//! ix list.
//!
//! The state-store `WatchRule` action field is
//! `ActionSpec::SolendWithdrawAllDelegated` because that variant exists
//! in [`claw_types::stage2_watch_rule::ActionSpec`]; adding a dedicated
//! `SolendDepositControlledWallet` variant would force a workspace-wide
//! schema-version bump and invalidate every pinned `canonical_rule_hash`
//! fixture. So instead, the [`LiveSolendDepositExecutionClient`]'s
//! `send_and_confirm` reinterprets the resulting
//! `Stage2ExecuteActionRequest`'s `target_obligation` + `reserve` +
//! `liquidity_mint` + `ctoken_mint` + oracle fields as a **deposit**
//! against that obligation. The W4 state machine, the CAS lease, and
//! the executor invocation are all real production code.
//!
//! The final report must call this out: the durable enum is unchanged,
//! and a future slice will land the actual deposit `ActionSpec` variant.
//!
//! # Architecture map
//!
//! ```text
//! [state-store: status=condition_met]
//!         │
//!         ▼   Stage2Executor::execute_rule_once
//! [process_rule]
//!  ├─ action_type_allowlist guard
//!  ├─ same-process in-flight guard
//!  ├─ CAS: mark_executing_if_condition_met(rule_id, nonce+1)
//!  │       └─ on lose: LeaseLost (no client call)
//!  ├─ build Stage2ExecuteActionRequest from rule + demo fixture
//!  ├─ client.send_and_confirm(request)
//!  │   └─ LiveSolendDepositExecutionClient:
//!  │        - verify request.delegated_wallet == controlled wallet keypair pubkey
//!  │        - fetch reserve account, decode via decode_reserve
//!  │        - verify reserve P5c tuple at send time (defence in depth)
//!  │        - build [CB, CB, CreateIdempotentATA?, RefreshReserve, DepositReserveLiquidityAndObligationCollateral]
//!  │        - fresh blockhash, sign with controlled wallet only
//!  │        - sendTransaction (skipPreflight=false maxRetries=0)
//!  │        - poll getSignatureStatuses (searchTransactionHistory=true)
//!  │        - return Stage2ExecutionReceipt
//!  ├─ on Ok:  mark_completed(rule_id, used_amount, slot)
//!  └─ on Err: mark_failed_best_effort(rule_id, detail)
//! ```
//!
//! # Two-phase env gating
//!
//! - **Phase 0** (`CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT=1`):
//!   Insert fixture rule → mark_condition_met_if_active → BUILD and
//!   SIMULATE the deposit tx (no CAS, no broadcast, durable state
//!   unchanged). Print `W5C LIVE CONDITIONAL DEPOSIT RESULT` banner
//!   with `leased_status: N/A` and `final_status: simulated`. STOP.
//!
//! - **Phase 2** (`CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED="W5C LIVE
//!   CONDITIONAL DEPOSIT APPROVED"`): require both env vars. Insert
//!   fixture rule → mark_condition_met_if_active → invoke
//!   `executor.execute_rule_once(rule_id, ctx)` → wait for finalized →
//!   reload rule and assert `status == completed` → print full banner
//!   with `leased_status: executing` and `final_status: completed`.
//!
//! # Required env
//!
//! | Variable | Purpose |
//! | --- | --- |
//! | `CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT=1`                | master Phase 0 gate |
//! | `HELIUS_RPC_URL` *or* `CLAW_RPC_URL`                    | Helius standard RPC |
//! | `CLAW_STAGE2_CLUSTER=mainnet-beta`                       | cluster sanity |
//! | `CLAW_STAGE2_DELEGATED_KEYPAIR_PATH=…`                   | controlled-wallet keypair JSON |
//! | `CLAW_STAGE2_CONDITIONAL_DEPOSIT_AMOUNT_RAW=250000`      | deposit amount raw (default 250,000 = 0.25 USDC) |
//! | `CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED=…`             | exact approval phrase for Phase 2 |
//!
//! # Sources of truth (Solend mainnet, deployed program)
//!
//! - `token-lending/sdk/src/instruction.rs`:
//!   `RefreshReserve` (tag 3), `InitObligation` (tag 6),
//!   `DepositReserveLiquidityAndObligationCollateral` (tag 14),
//!   `WithdrawObligationCollateralAndRedeemReserveCollateral` (tag 15)
//! - `token-lending/program/src/processor.rs`: `Clock::get()?` sysvar
//!   syscall in deposit handler — no Clock account in deposit/withdraw
//!   ix lists on mainnet branch.
//! - Repo's mainnet-verified builders:
//!   [`claw_gateway::integrations::solend::deposit`] and
//!   [`claw_gateway::integrations::solend::refresh`] — pinned by the
//!   May 2026 Phase 6I-K mainnet recovery audit.

use std::env;
use std::fs;
use std::str::FromStr;
use std::sync::Arc;
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
    MockExecutionClient, Stage2ExecuteActionRequest, Stage2ExecutionClient,
    Stage2ExecutionError, Stage2ExecutionReceipt, Stage2Executor, Stage2ExecutorRuleResult,
    DEMO_CTOKEN_MINT_BS58, DEMO_LENDING_MARKET_BS58, DEMO_LIQUIDITY_MINT_BS58,
    DEMO_PYTH_ORACLE_BS58, DEMO_RESERVE_BS58,
};
use claw_gateway::stage2_watcher::Stage2TickContext;

use claw_state_store::db::Database;
use claw_state_store::stage2_watch_rules::{
    Stage2WatchRuleRepository, WatchRuleStatus,
};

use claw_types::canonical_intent::PubkeyBytes;
use claw_types::stage2_watch_rule::{
    ActionSpec, Comparison, Condition, ConditionLogic, RateKind, WatchRule, WithdrawMode,
    STAGE2_WATCH_RULE_SCHEMA_VERSION,
};

// ── env var names ─────────────────────────────────────────────────────────

const ENV_GATE: &str = "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT";
const ENV_APPROVAL: &str = "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED";
const APPROVAL_PHRASE: &str = "W5C LIVE CONDITIONAL DEPOSIT APPROVED";

const ENV_RPC_PRIMARY: &str = "HELIUS_RPC_URL";
const ENV_RPC_SECONDARY: &str = "CLAW_RPC_URL";
const ENV_CLUSTER: &str = "CLAW_STAGE2_CLUSTER";
const ENV_KEYPAIR_PATH: &str = "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH";
const ENV_DEPOSIT_AMOUNT_RAW: &str = "CLAW_STAGE2_CONDITIONAL_DEPOSIT_AMOUNT_RAW";

// ── pinned facts (sealed against stage2_executor's source-of-truth) ──────

const CONTROLLED_WALLET_BS58: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";
const DEFAULT_TARGET_OBLIGATION_BS58: &str = "BdFLjCcP9mCy557vNNGVbTUuvHxXsh8hc6jXzaPra1wN";
const SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

const USDC_DECIMALS: u8 = 6;
/// 0.25 USDC; brief default for W5c.
const DEFAULT_DEPOSIT_AMOUNT_RAW: u64 = 250_000;

const SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const COMPUTE_BUDGET_PROGRAM_BS58: &str = "ComputeBudget111111111111111111111111111111";

const COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT: u8 = 2;
const COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE: u8 = 3;

/// Compute-unit limit. RefreshReserve + Deposit (with 3 SPL Token
/// sub-CPIs) consumed 87,803 CU on the W5b mainnet run; 400_000 is
/// a generous ceiling.
const COMPUTE_UNIT_LIMIT: u32 = 400_000;
/// Priority fee per CU in micro-lamports. 50_000 μ/CU keeps total
/// priority fee modest while still incentivising leader pickup:
/// 400_000 CU * 50_000 μ/CU / 1_000_000 = 20_000 lamports = 0.00002 SOL.
const COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 50_000;

// Retry / polling
const RPC_MAX_RETRY_ATTEMPTS: usize = 3;
const RPC_INITIAL_BACKOFF_MS: u64 = 500;
const REQUEST_TIMEOUT_MS: u64 = 15_000;
const STATUS_POLL_INTERVAL_MS: u64 = 2_000;
const STATUS_POLL_MAX_ATTEMPTS: usize = 60;

// Test-deterministic ids for the fixture rule.
const FIXTURE_RULE_ID: [u8; 16] = [
    0xC0, 0x5E, 0x05, 0x1C, 0xDE, 0x10, 0x5C, 0x57,
    0x05, 0xC0, 0x05, 0xC0, 0x05, 0xC0, 0x05, 0xC0,
];

// Source-scan list. Each forbidden token may appear at most once
// (the FORBIDDEN_CALL_FORMS allowlist entry itself counts as 1).
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

// ── cluster ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cluster {
    MainnetBeta,
    Devnet,
    Localnet,
}

impl Cluster {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "mainnet-beta" | "mainnet" => Some(Cluster::MainnetBeta),
            "devnet" => Some(Cluster::Devnet),
            "localnet" => Some(Cluster::Localnet),
            _ => None,
        }
    }
}

// ── env-reader ────────────────────────────────────────────────────────────

#[derive(Debug)]
enum EnvStatus {
    Skipped(String),
    Mismatch(String),
    Ready(Box<HarnessConfig>),
}

#[derive(Debug)]
struct HarnessConfig {
    rpc_url: String,
    cluster: Cluster,
    keypair_path: String,
    amount_raw: u64,
    approved: bool,
}

impl HarnessConfig {
    fn from_env_with<F>(getter: F) -> EnvStatus
    where
        F: Fn(&str) -> Option<String>,
    {
        if getter(ENV_GATE).as_deref() != Some("1") {
            return EnvStatus::Skipped(format!(
                "{ENV_GATE} not set to 1 — conditional deposit harness self-skipped"
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
                "conditional deposit target is mainnet-beta only; got cluster={cluster:?}"
            ));
        }
        let keypair_path = match getter(ENV_KEYPAIR_PATH) {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => {
                return EnvStatus::Skipped(format!(
                    "{ENV_KEYPAIR_PATH} not set — conditional deposit needs a keypair path"
                ))
            }
        };
        let amount_raw = match getter(ENV_DEPOSIT_AMOUNT_RAW) {
            Some(s) if !s.trim().is_empty() => match s.trim().parse::<u64>() {
                Ok(n) => n,
                Err(e) => {
                    return EnvStatus::Mismatch(format!(
                        "{ENV_DEPOSIT_AMOUNT_RAW}='{s}' is not a u64: {e}"
                    ))
                }
            },
            _ => DEFAULT_DEPOSIT_AMOUNT_RAW,
        };
        if amount_raw == 0 {
            return EnvStatus::Mismatch(format!("{ENV_DEPOSIT_AMOUNT_RAW} must be > 0"));
        }
        let approved = getter(ENV_APPROVAL).as_deref() == Some(APPROVAL_PHRASE);
        EnvStatus::Ready(Box::new(HarnessConfig {
            rpc_url,
            cluster,
            keypair_path,
            amount_raw,
            approved,
        }))
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

fn redact_url(url: &str) -> String {
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

fn load_keypair_from_file(path: &str) -> Result<Keypair, String> {
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

async fn retry<F, Fut, T, E>(
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

async fn rpc_post(client: &reqwest::Client, url: &str, body: Value) -> Result<Value, RpcErr> {
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

async fn rpc_get_version(c: &reqwest::Client, url: &str) -> Result<String, RpcErr> {
    let v = rpc_post(c, url, json!({"jsonrpc":"2.0","id":1,"method":"getVersion"})).await?;
    v["result"]["solana-core"]
        .as_str()
        .ok_or_else(|| RpcErr::Body("missing solana-core".into()))
        .map(|s| s.to_string())
}

async fn rpc_get_slot(c: &reqwest::Client, url: &str) -> Result<u64, RpcErr> {
    let v = rpc_post(c, url, json!({"jsonrpc":"2.0","id":1,"method":"getSlot"})).await?;
    v["result"]
        .as_u64()
        .ok_or_else(|| RpcErr::Body("missing slot".into()))
}

#[derive(Debug)]
struct TokenAmount {
    raw: u64,
    decimals: u8,
}

async fn rpc_get_token_account_balance(
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
struct LatestBlockhash {
    hash: Hash,
    last_valid_block_height: u64,
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
    let hash = Hash::from_str(bh_str).map_err(|e| RpcErr::Body(format!("hash parse: {e}")))?;
    Ok(LatestBlockhash {
        hash,
        last_valid_block_height: lvbh,
    })
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
struct SimResult {
    err: Option<Value>,
    units_consumed: Option<u64>,
    logs: Vec<String>,
}

async fn rpc_simulate_transaction(
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
    Pending,
    Failed(Value),
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

// ── compute-budget ix builders ────────────────────────────────────────────

fn compute_budget_set_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str(COMPUTE_BUDGET_PROGRAM_BS58).unwrap(),
        accounts: vec![],
        data,
    }
}

fn compute_budget_set_unit_price(micro_lamports_per_cu: u64) -> Instruction {
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
fn estimated_priority_fee_lamports(cu_limit: u32, price_micro_lamports: u64) -> u64 {
    (cu_limit as u64) * price_micro_lamports / 1_000_000
}

// ── reserve / obligation invariant checks ─────────────────────────────────

#[derive(Debug)]
struct ReserveInvariantFail(String);

fn verify_reserve_p5c(decoded: &SolendReserveRaw) -> Result<(), ReserveInvariantFail> {
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
struct ObligationInvariantFail(String);

fn verify_obligation_p5c(
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

fn obligation_pinned_reserve_amount(decoded: &SolendObligationRaw) -> u64 {
    let pin_reserve = Pubkey::from_str(DEMO_RESERVE_BS58).unwrap();
    decoded
        .deposits
        .iter()
        .filter(|d| d.deposit_reserve == pin_reserve)
        .map(|d| d.deposited_amount)
        .sum()
}

// ── Live Solend deposit execution client ──────────────────────────────────
//
// The first concrete `Stage2ExecutionClient` impl that does a live
// mainnet send. The mock client in `stage2_executor.rs` is for the
// pre-W5c unit-test surface only.

/// Construction-time inputs for the live client.
#[derive(Debug)]
struct LiveSolendDepositExecutionClient {
    keypair: Keypair,
    rpc_url: String,
    http: reqwest::Client,
    /// The obligation we'll deposit into. Forwarded into the deposit ix
    /// builder verbatim. We treat the executor's
    /// `Stage2ExecuteActionRequest.solend.target_obligation` as the
    /// authoritative source — this field is just a sanity-cross-check.
    expected_target_obligation: Pubkey,
}

impl LiveSolendDepositExecutionClient {
    fn new(
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
    async fn simulate_only(
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

    /// Build the deposit instruction plan from the executor's strongly
    /// typed request. This is the seam where the test-only adapter
    /// reinterprets a "withdraw" request as a deposit: the request's
    /// `solend.target_obligation` / `solend.reserve` / oracle pubkeys
    /// are exactly the inputs the deposit builder needs.
    async fn build_tx_plan(
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

    fn assemble_and_sign(
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

    /// Live broadcast helper. Caller has already simulated; this path
    /// re-fetches a fresh blockhash, re-signs, sends, and polls.
    async fn broadcast_and_confirm(
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
                    // "confirmed" — keep polling for finalized.
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

/// Output of [`LiveSolendDepositExecutionClient::build_tx_plan`].
/// Several fields aren't read after construction (their values flow
/// into the banner) — kept for the future "Display"-style banner
/// extension without forcing extra RPC calls.
#[allow(dead_code)]
#[derive(Debug)]
struct BuiltTx {
    ixs: Vec<Instruction>,
    payer: Pubkey,
    source_usdc_ata: Pubkey,
    ctoken_ata: Pubkey,
    target_obligation: Pubkey,
    reserve_pubkey: Pubkey,
    ctoken_ata_existed_before: bool,
    source_usdc_before: u64,
    obligation_pinned_reserve_amount_before: u64,
    obligation_deposits_len_before: usize,
}

// ── Test fixture rule builder ─────────────────────────────────────────────

/// Construct a deterministic-id fixture rule that:
/// - matches the demo P5c reserve + lending market (so the executor's
///   `DemoSolendExecutionFixture::build_payload` accepts it),
/// - has `delegated_wallet = controlled wallet`,
/// - has `max_input_amount_raw = amount_raw` (one-shot v1 → executor
///   sets `request.input_amount_raw = max - used = max`).
fn fixture_rule(
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
        rule_id: FIXTURE_RULE_ID,
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

async fn insert_and_mark_condition_met(
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

// ──────────────────────────────────────────────────────────────────────────
// Live test (env-gated)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage2_w5c_live_conditional_solend_deposit() {
    let status = HarnessConfig::from_env_with(|k| env::var(k).ok());
    let cfg = match status {
        EnvStatus::Skipped(reason) => {
            eprintln!("[skip] W5c conditional deposit: {reason}");
            return;
        }
        EnvStatus::Mismatch(msg) => panic!("STOP — W5c conditional deposit: {msg}"),
        EnvStatus::Ready(c) => c,
    };

    eprintln!("──── W5c live conditional Solend deposit harness ────");
    eprintln!("cluster           : {:?}", cfg.cluster);
    eprintln!("rpc               : {}", redact_url(&cfg.rpc_url));
    eprintln!(
        "keypair path      : {} (contents NOT printed)",
        cfg.keypair_path
    );
    eprintln!("target obligation : {DEFAULT_TARGET_OBLIGATION_BS58}");
    eprintln!("amount (raw)      : {}", cfg.amount_raw);
    eprintln!(
        "amount (UI)       : {}.{:06} USDC",
        cfg.amount_raw / 1_000_000,
        cfg.amount_raw % 1_000_000
    );
    eprintln!(
        "approved          : {}",
        if cfg.approved { "yes" } else { "no — Phase 0 only" }
    );

    // ── Keypair load ────────────────────────────────────────────────────
    let kp = load_keypair_from_file(&cfg.keypair_path)
        .unwrap_or_else(|e| panic!("STOP — keypair load failed: {e}"));
    let controlled_pk = kp.pubkey();
    let expected_controlled = Pubkey::from_str(CONTROLLED_WALLET_BS58).unwrap();
    if controlled_pk != expected_controlled {
        panic!(
            "STOP — keypair pubkey mismatch: file={controlled_pk} expected={expected_controlled}"
        );
    }
    eprintln!("\n✓ keypair loaded; pubkey matches pinned controlled wallet {controlled_pk}");

    // ── Bring up an in-memory state-store + insert fixture rule ─────────
    let db = Database::open_in_memory()
        .await
        .unwrap_or_else(|e| panic!("STOP — open_in_memory: {e}"));
    let repo = Stage2WatchRuleRepository::new(db.pool().clone());
    let target_obligation = Pubkey::from_str(DEFAULT_TARGET_OBLIGATION_BS58).unwrap();

    // ── Pull current slot (used to set rule.expires_at_slot) ────────────
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .expect("build reqwest client");
    let chain_slot = retry(
        || rpc_get_slot(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getSlot: {e}"));
    let ver = retry(
        || rpc_get_version(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getVersion: {e}"));
    eprintln!("✓ RPC OK            solana-core={ver} chain_slot={chain_slot}");

    let rule = fixture_rule(
        cfg.amount_raw,
        DEFAULT_TARGET_OBLIGATION_BS58,
        // user / executor — not signers; the fixture just needs valid
        // pubkeys. Use the controlled wallet for symmetry, since this
        // demo path collapses the user / executor / delegated identities
        // for substrate purposes.
        CONTROLLED_WALLET_BS58,
        CONTROLLED_WALLET_BS58,
        chain_slot.saturating_add(50_000), // expiry well ahead
    );

    insert_and_mark_condition_met(&repo, &rule)
        .await
        .unwrap_or_else(|e| panic!("STOP — fixture insert: {e}"));
    let stored = repo
        .get(&rule.rule_id)
        .await
        .ok()
        .flatten()
        .expect("rule readable post-insert");
    assert_eq!(stored.status, WatchRuleStatus::ConditionMet);
    eprintln!(
        "✓ fixture rule inserted; rule_id={} status={:?}",
        hex_id(&rule.rule_id),
        stored.status
    );

    // ── Construct live client ───────────────────────────────────────────
    let client = LiveSolendDepositExecutionClient::new(
        kp,
        cfg.rpc_url.clone(),
        target_obligation,
    );

    // The Stage2Executor builds a request from the rule. We build one
    // here too so Phase 0 can simulate via the client.
    let dummy_executor_for_request = Stage2Executor::new(
        repo.clone(),
        Arc::new(MockExecutionClient::with_success_default()),
    );
    let preview_request = dummy_executor_for_request
        .build_execute_action_request(&stored, stored.execution_nonce.saturating_add(1))
        .unwrap_or_else(|e| panic!("STOP — build_execute_action_request: {e}"));
    eprintln!(
        "✓ Stage2ExecuteActionRequest built  input_amount_raw={} delegated_wallet={}",
        preview_request.input_amount_raw,
        Pubkey::new_from_array(preview_request.delegated_wallet.0)
    );

    // ── Phase 0: simulate-only (durable state unchanged) ────────────────
    eprintln!("\n── Phase 0: simulation ──");
    let (latest, sim, plan) = client
        .simulate_only(&preview_request)
        .await
        .unwrap_or_else(|e| panic!("STOP — simulation failed: {e}"));
    eprintln!(
        "✓ blockhash         hash={} lvbh={}",
        latest.hash, latest.last_valid_block_height
    );
    eprintln!("simulated CU       : {:?}", sim.units_consumed);
    eprintln!("simulated err      : {:?}", sim.err);
    for l in &sim.logs {
        eprintln!("  log: {l}");
    }
    if let Some(err) = &sim.err {
        panic!("STOP — simulation failed: {err}");
    }
    eprintln!(
        "✓ simulation passed; source USDC before={} cToken-amount before={}",
        plan.source_usdc_before, plan.obligation_pinned_reserve_amount_before
    );

    // Phase 0 banner.
    let priority_fee_sim =
        estimated_priority_fee_lamports(COMPUTE_UNIT_LIMIT, COMPUTE_UNIT_PRICE_MICRO_LAMPORTS);
    eprintln!(
        "\n──────────────────────────────────────────────────────────────"
    );
    eprintln!("W5c conditional deposit preflight passed.");
    eprintln!(
        "Ready to deposit {}.{:06} USDC from controlled wallet {controlled_pk}",
        cfg.amount_raw / 1_000_000,
        cfg.amount_raw % 1_000_000
    );
    eprintln!("  via source USDC ATA   {}", plan.source_usdc_ata);
    eprintln!("  into obligation        {}", plan.target_obligation);
    eprintln!("  reserve                {}", plan.reserve_pubkey);
    eprintln!("  cToken minted to ATA   {}", plan.ctoken_ata);
    eprintln!("");
    eprintln!("Awaiting exact approval phrase: {APPROVAL_PHRASE}");
    eprintln!("To proceed to Phase 2 live send, re-run with env var:");
    eprintln!("  {ENV_APPROVAL}=\"{APPROVAL_PHRASE}\"");
    eprintln!(
        "──────────────────────────────────────────────────────────────"
    );

    if !cfg.approved {
        // Reload to confirm Phase 0 did NOT mutate durable state past
        // the condition_met transition.
        let after = repo
            .get(&rule.rule_id)
            .await
            .ok()
            .flatten()
            .expect("rule readable post-phase-0");
        assert_eq!(
            after.status,
            WatchRuleStatus::ConditionMet,
            "Phase 0 must not mutate rule past condition_met"
        );
        print_banner(BannerInputs {
            rule_id: rule.rule_id,
            previous_status: "condition_met",
            leased_status: "N/A (simulation-only did not mutate durable state)",
            final_status: "simulated",
            amount_raw: cfg.amount_raw,
            controlled_wallet: controlled_pk,
            source_usdc_ata: plan.source_usdc_ata,
            obligation: plan.target_obligation,
            before_usdc_raw: Some(plan.source_usdc_before),
            after_usdc_raw: None,
            before_ctoken_amount: Some(plan.obligation_pinned_reserve_amount_before),
            after_ctoken_amount: None,
            priority_fee_micro_lamports_per_cu: COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
            estimated_or_actual_cu: sim.units_consumed.unwrap_or(0),
            priority_fee_lamports: estimated_priority_fee_lamports(
                sim.units_consumed.map(|u| u as u32).unwrap_or(COMPUTE_UNIT_LIMIT),
                COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
            )
            .max(priority_fee_sim),
            tx_signature: None,
        });
        return;
    }

    // ── Phase 2: invoke W4 executor with the live client ────────────────
    eprintln!("\n── Phase 2: live send via Stage2Executor ──");
    let executor = Stage2Executor::new(repo.clone(), Arc::new(client));
    let ctx = Stage2TickContext::new(
        chain_slot,
        chrono::Utc::now().timestamp(),
        chrono::Utc::now().timestamp_millis(),
    );
    let result = executor.execute_rule_once(rule.rule_id, ctx).await;

    let (signature, slot, used) = match result {
        Stage2ExecutorRuleResult::Completed {
            execution_nonce,
            used_amount_raw,
            confirmation_slot,
            signature_sentinel,
            ..
        } => {
            eprintln!(
                "✓ executor Completed: execution_nonce={execution_nonce} \
                 slot={confirmation_slot} used={used_amount_raw}"
            );
            (signature_sentinel, confirmation_slot, used_amount_raw)
        }
        Stage2ExecutorRuleResult::Failed { error, .. } => {
            // Reload rule for banner before panicking.
            let after = repo
                .get(&rule.rule_id)
                .await
                .ok()
                .flatten()
                .map(|r| format!("{:?}", r.status))
                .unwrap_or_else(|| "<unreadable>".to_string());
            print_banner(BannerInputs {
                rule_id: rule.rule_id,
                previous_status: "condition_met",
                leased_status: "executing",
                final_status: "failed",
                amount_raw: cfg.amount_raw,
                controlled_wallet: controlled_pk,
                source_usdc_ata: plan.source_usdc_ata,
                obligation: plan.target_obligation,
                before_usdc_raw: Some(plan.source_usdc_before),
                after_usdc_raw: None,
                before_ctoken_amount: Some(plan.obligation_pinned_reserve_amount_before),
                after_ctoken_amount: None,
                priority_fee_micro_lamports_per_cu: COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
                estimated_or_actual_cu: sim.units_consumed.unwrap_or(COMPUTE_UNIT_LIMIT as u64),
                priority_fee_lamports: priority_fee_sim,
                tx_signature: None,
            });
            panic!(
                "STOP — executor Failed: {error}; rule reloaded status={after}"
            );
        }
        other => panic!("STOP — unexpected executor outcome: {other:?}"),
    };
    eprintln!("✓ signature  {signature}");
    eprintln!("  Solscan:   https://solscan.io/tx/{signature}");

    // Reload + assert state writeback.
    let after = repo
        .get(&rule.rule_id)
        .await
        .ok()
        .flatten()
        .expect("rule readable post-phase-2");
    assert_eq!(
        after.status,
        WatchRuleStatus::Completed,
        "executor must transition rule to completed on Ok receipt"
    );
    assert!(after.completed, "completed flag must be set");
    // The state-store `StoredWatchRule` exposes the inner rule
    // (post-execution); `used_amount_raw` lives on the WatchRule body.
    assert_eq!(after.rule.used_amount_raw, cfg.amount_raw);

    // Re-fetch USDC + obligation for hard delta assertions.
    let source_after = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &plan.source_usdc_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — source USDC after: {e}"))
    .unwrap_or_else(|| panic!("STOP — source ATA vanished"));
    let obligation_bytes_after = retry(
        || rpc_get_account_data(&http, &cfg.rpc_url, &plan.target_obligation),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — refetch obligation: {e}"))
    .unwrap_or_else(|| panic!("STOP — obligation vanished after tx"));
    let obligation_after = decode_obligation(&obligation_bytes_after)
        .unwrap_or_else(|e| panic!("STOP — re-decode obligation: {e}"));

    let after_ctoken = obligation_pinned_reserve_amount(&obligation_after);

    // Delta-based assertions (NEVER hardcoded absolute balances).
    let usdc_delta = plan
        .source_usdc_before
        .checked_sub(source_after.raw)
        .unwrap_or(0);
    assert_eq!(
        usdc_delta, cfg.amount_raw,
        "source USDC must decrease by exactly amount_raw"
    );
    assert!(
        after_ctoken > plan.obligation_pinned_reserve_amount_before,
        "obligation pinned-reserve cToken amount must strictly increase"
    );

    eprintln!(
        "\n── deltas ──\nUSDC: {} → {} (Δ -{})\ncToken: {} → {} (Δ +{})",
        plan.source_usdc_before,
        source_after.raw,
        usdc_delta,
        plan.obligation_pinned_reserve_amount_before,
        after_ctoken,
        after_ctoken - plan.obligation_pinned_reserve_amount_before
    );

    // Banner.
    let priority_fee_actual =
        estimated_priority_fee_lamports(COMPUTE_UNIT_LIMIT, COMPUTE_UNIT_PRICE_MICRO_LAMPORTS);
    print_banner(BannerInputs {
        rule_id: rule.rule_id,
        previous_status: "condition_met",
        leased_status: "executing",
        final_status: "completed",
        amount_raw: cfg.amount_raw,
        controlled_wallet: controlled_pk,
        source_usdc_ata: plan.source_usdc_ata,
        obligation: plan.target_obligation,
        before_usdc_raw: Some(plan.source_usdc_before),
        after_usdc_raw: Some(source_after.raw),
        before_ctoken_amount: Some(plan.obligation_pinned_reserve_amount_before),
        after_ctoken_amount: Some(after_ctoken),
        priority_fee_micro_lamports_per_cu: COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        estimated_or_actual_cu: sim.units_consumed.unwrap_or(COMPUTE_UNIT_LIMIT as u64),
        priority_fee_lamports: priority_fee_actual,
        tx_signature: Some(signature.clone()),
    });

    eprintln!("\n──── W5c conditional Solend deposit complete ────");
    eprintln!("signature : {signature}");
    eprintln!("slot      : {slot}");
    eprintln!("used      : {used}");
    eprintln!("Solscan   : https://solscan.io/tx/{signature}");
}

// ── Banner rendering ──────────────────────────────────────────────────────

struct BannerInputs {
    rule_id: [u8; 16],
    previous_status: &'static str,
    leased_status: &'static str,
    final_status: &'static str,
    amount_raw: u64,
    controlled_wallet: Pubkey,
    source_usdc_ata: Pubkey,
    obligation: Pubkey,
    before_usdc_raw: Option<u64>,
    after_usdc_raw: Option<u64>,
    before_ctoken_amount: Option<u64>,
    after_ctoken_amount: Option<u64>,
    priority_fee_micro_lamports_per_cu: u64,
    estimated_or_actual_cu: u64,
    priority_fee_lamports: u64,
    tx_signature: Option<String>,
}

fn print_banner(i: BannerInputs) {
    eprintln!("\n============================================================");
    eprintln!("W5C LIVE CONDITIONAL DEPOSIT RESULT");
    eprintln!("rule_id: {}", hex_id(&i.rule_id));
    eprintln!("previous_status: {}", i.previous_status);
    eprintln!("leased_status: {}", i.leased_status);
    eprintln!("final_status: {}", i.final_status);
    eprintln!("amount_raw: {}", i.amount_raw);
    eprintln!("controlled_wallet: {}", i.controlled_wallet);
    eprintln!("source_usdc_ata: {}", i.source_usdc_ata);
    eprintln!("obligation: {}", i.obligation);
    eprintln!("before_usdc_raw: {}", opt(i.before_usdc_raw));
    eprintln!("after_usdc_raw: {}", opt(i.after_usdc_raw));
    eprintln!(
        "usdc_delta_raw: {}",
        match (i.before_usdc_raw, i.after_usdc_raw) {
            (Some(b), Some(a)) => format!("-{}", b.saturating_sub(a)),
            _ => "N/A".to_string(),
        }
    );
    eprintln!("before_ctoken_amount: {}", opt(i.before_ctoken_amount));
    eprintln!("after_ctoken_amount: {}", opt(i.after_ctoken_amount));
    eprintln!(
        "ctoken_delta_raw: {}",
        match (i.before_ctoken_amount, i.after_ctoken_amount) {
            (Some(b), Some(a)) => format!("+{}", a.saturating_sub(b)),
            _ => "N/A".to_string(),
        }
    );
    eprintln!(
        "priority_fee_micro_lamports_per_cu: {}",
        i.priority_fee_micro_lamports_per_cu
    );
    eprintln!("estimated_or_actual_cu: {}", i.estimated_or_actual_cu);
    eprintln!("priority_fee_lamports: {}", i.priority_fee_lamports);
    let sig_field = i
        .tx_signature
        .as_deref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "N/A".to_string());
    let solscan_field = i
        .tx_signature
        .as_deref()
        .map(|s| format!("https://solscan.io/tx/{s}"))
        .unwrap_or_else(|| "N/A".to_string());
    eprintln!("tx_signature: {sig_field}");
    eprintln!("solscan: {solscan_field}");
    eprintln!("============================================================");
}

fn opt(x: Option<u64>) -> String {
    x.map(|n| n.to_string()).unwrap_or_else(|| "N/A".to_string())
}

fn hex_id(rule_id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in rule_id {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

// ──────────────────────────────────────────────────────────────────────────
// Unit tests (always run; non-live; no network; no keypair)
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use claw_gateway::stage2_executor::Stage2ExecutorRuleResult;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── env-reader tests ──────────────────────────────────────────────────

    #[test]
    fn env_gate_skips_without_master_flag() {
        let status = HarnessConfig::from_env_with(|_| None);
        match status {
            EnvStatus::Skipped(reason) => assert!(reason.contains(ENV_GATE)),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn cluster_must_be_mainnet_beta() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("devnet".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("mainnet-beta")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn missing_keypair_path_rejects() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Skipped(m) => assert!(m.contains(ENV_KEYPAIR_PATH)),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn deposit_amount_zero_rejected() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_AMOUNT_RAW" => Some("0".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("> 0")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn default_amount_is_quarter_usdc_when_unset() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert_eq!(c.amount_raw, DEFAULT_DEPOSIT_AMOUNT_RAW);
                assert_eq!(c.amount_raw, 250_000);
                assert!(!c.approved);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn approval_phrase_is_exact_match() {
        // lowercase variant must NOT approve
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED" => {
                Some("w5c live conditional deposit approved".to_string())
            }
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => assert!(!c.approved, "lowercase must not approve"),
            other => panic!("expected Ready, got {other:?}"),
        }
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED" => Some(APPROVAL_PHRASE.to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => assert!(c.approved, "exact phrase must approve"),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn redact_url_strips_api_key() {
        let r = redact_url("https://mainnet.helius-rpc.com/?api-key=abc123XYZ");
        assert!(r.contains("***REDACTED***"));
        assert!(!r.contains("abc123XYZ"));
    }

    #[test]
    fn priority_fee_math_matches_spec() {
        // 400_000 CU × 50_000 μ/CU / 1_000_000 = 20_000 lamports
        assert_eq!(
            estimated_priority_fee_lamports(400_000, 50_000),
            20_000
        );
        // 87_803 CU (W5b empirical) × 50_000 / 1_000_000 = 4_390 lamports
        assert_eq!(estimated_priority_fee_lamports(87_803, 50_000), 4_390);
    }

    #[test]
    fn pin_constants_match_stage2_executor() {
        // Sanity: the executor's source-of-truth re-exports match what
        // the harness uses for live-account verification.
        assert_eq!(
            DEMO_RESERVE_BS58,
            claw_gateway::stage2_executor::DEMO_RESERVE_BS58
        );
        assert_eq!(
            DEMO_LENDING_MARKET_BS58,
            claw_gateway::stage2_executor::DEMO_LENDING_MARKET_BS58
        );
        assert_eq!(
            DEMO_CTOKEN_MINT_BS58,
            claw_gateway::stage2_executor::DEMO_CTOKEN_MINT_BS58
        );
    }

    // ── State-machine tests using MockExecutionClient ─────────────────────
    //
    // These exercise the W4 path the live test relies on, with no
    // network calls. The live `LiveSolendDepositExecutionClient` is not
    // involved here — that's the *concrete* implementation; the mock
    // covers the *generic* behavior the executor relies on.

    async fn test_repo() -> (Database, Stage2WatchRuleRepository) {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        (db, repo)
    }

    fn ctx() -> Stage2TickContext {
        Stage2TickContext::new(
            415_500_000,
            chrono::Utc::now().timestamp(),
            chrono::Utc::now().timestamp_millis(),
        )
    }

    #[tokio::test]
    async fn cas_lease_prevents_double_execution() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule)
            .await
            .unwrap();

        let counter = Arc::new(AtomicUsize::new(0));

        #[derive(Debug)]
        struct CountingClient {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Stage2ExecutionClient for CountingClient {
            async fn send_and_confirm(
                &self,
                request: Stage2ExecuteActionRequest,
            ) -> Result<Stage2ExecutionReceipt, Stage2ExecutionError> {
                self.counter.fetch_add(1, Ordering::SeqCst);
                Ok(Stage2ExecutionReceipt {
                    rule_id: request.rule_id,
                    execution_nonce: request.execution_nonce,
                    confirmation_slot: 415_500_000,
                    used_amount_raw: request.input_amount_raw,
                    signature_sentinel: "mock-sig".into(),
                })
            }
        }

        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(CountingClient {
                counter: counter.clone(),
            }),
        );

        // First call: lease + dispatch.
        let r1 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(matches!(r1, Stage2ExecutorRuleResult::Completed { .. }));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Second call: rule is now `completed`, CAS should miss
        // (rule no longer in `condition_met`).
        let r2 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r2, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "expected LeaseLost on second call, got {r2:?}"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "client must not be called on a lost lease"
        );
    }

    #[tokio::test]
    async fn send_signature_without_confirmation_does_not_mark_completed() {
        // A failed client invocation must NOT mark completed. The
        // executor maps `Err(...)` → `mark_failed`, not `mark_completed`.
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule).await.unwrap();

        let mock = MockExecutionClient::new();
        mock.push_failure(Stage2ExecutionError::ConfirmationFailed(
            "confirmation timeout (signature 5xxxx never reached finalized)".into(),
        ));
        let executor = Stage2Executor::new(repo.clone(), Arc::new(mock));
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::Failed { .. }),
            "expected Failed, got {r:?}"
        );
        let after = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(after.status, WatchRuleStatus::Failed);
        assert!(!after.completed, "completed flag MUST stay false on failure");
    }

    #[tokio::test]
    async fn failure_marks_failed_and_no_retry() {
        // Two consecutive ticks with a failing client. First marks
        // failed; second observes terminal state and emits LeaseLost
        // (no retry).
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule).await.unwrap();

        let mock = MockExecutionClient::new();
        mock.push_failure(Stage2ExecutionError::SendFailed("RPC down".into()));
        // Second outcome is never reached if no-retry holds.
        mock.push_failure(Stage2ExecutionError::SendFailed("should not fire".into()));
        let executor = Stage2Executor::new(repo.clone(), Arc::new(mock));

        let r1 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(matches!(r1, Stage2ExecutorRuleResult::Failed { .. }));
        let r2 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(matches!(r2, Stage2ExecutorRuleResult::LeaseLost { .. }));
    }

    #[tokio::test]
    async fn revoked_rule_ignored() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        repo.insert(&rule).await.unwrap();
        repo.mark_revoked(&rule.rule_id).await.unwrap();
        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(MockExecutionClient::with_success_default()),
        );
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "revoked rule must not be executed; got {r:?}"
        );
    }

    #[tokio::test]
    async fn active_rule_not_in_condition_met_is_ignored() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        // Inserted but NOT marked condition_met — still `active`.
        repo.insert(&rule).await.unwrap();
        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(MockExecutionClient::with_success_default()),
        );
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "active rule must not be executed; got {r:?}"
        );
    }

    #[tokio::test]
    async fn completed_rule_is_ignored() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule).await.unwrap();
        // Externally mark completed.
        repo.mark_completed(&rule.rule_id, 250_000, 415_500_001)
            .await
            .unwrap();
        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(MockExecutionClient::with_success_default()),
        );
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "completed rule must not be executed; got {r:?}"
        );
    }

    #[tokio::test]
    async fn live_client_rejects_mismatching_delegated_wallet() {
        // The live client's identity guard: a request whose
        // `delegated_wallet` differs from the keypair's pubkey must
        // fail early — before any network call.
        let kp = Keypair::new();
        let other = Pubkey::new_unique();
        let client = LiveSolendDepositExecutionClient::new(
            kp,
            "https://example.invalid/".to_string(),
            Pubkey::from_str(DEFAULT_TARGET_OBLIGATION_BS58).unwrap(),
        );
        // Fabricate a request whose delegated_wallet is NOT the
        // keypair's pubkey.
        let bogus_request = Stage2ExecuteActionRequest {
            rule_id: [0; 16],
            canonical_rule_hash: [0; 32],
            action_type: claw_types::stage2_watch_rule::WatchRuleActionType::SolendWithdrawAllDelegated,
            action_type_byte: 1,
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            input_amount_raw: 250_000,
            execution_nonce: 1,
            user: PubkeyBytes::new([0; 32]),
            executor: PubkeyBytes::new([0; 32]),
            delegated_wallet: PubkeyBytes::new(other.to_bytes()),
            destination: PubkeyBytes::new([0; 32]),
            expires_at_slot: 1_000_000_000,
            solend: None,
        };
        let r = client.build_tx_plan(&bogus_request).await;
        match r {
            Err(Stage2ExecutionError::InvalidRequest(s)) => {
                assert!(
                    s.contains("delegated_wallet") || s.contains("keypair pubkey"),
                    "expected delegated_wallet mismatch, got {s}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    // ── Source-scan invariant ─────────────────────────────────────────────

    #[test]
    fn no_send_path_default_invariant() {
        const SOURCE: &str = include_str!("stage2_live_conditional_solend_deposit.rs");
        for forbidden in FORBIDDEN_CALL_FORMS {
            let count = SOURCE.matches(forbidden).count();
            assert!(
                count <= 1,
                "forbidden call-form `{forbidden}` appears {count} times in W5c harness; \
                 only the FORBIDDEN_CALL_FORMS allowlist entry is permitted"
            );
        }
    }

    // (`send_path_is_singular_and_gated` and `no_confirm_transaction_path`
    // were redundant with `no_send_path_default_invariant` and added
    // false-positive self-references to the very tokens they tried to
    // forbid. The forbidden-call-form table is the source of truth.)

    #[test]
    fn retry_policy_is_bounded() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let calls = Arc::new(AtomicUsize::new(0));
                let calls_ref = calls.clone();
                let r: Result<(), &'static str> = retry(
                    || {
                        let c = calls_ref.clone();
                        async move {
                            c.fetch_add(1, Ordering::SeqCst);
                            Err::<(), &'static str>("always fails")
                        }
                    },
                    3,
                    Duration::from_millis(1),
                )
                .await;
                assert!(r.is_err());
                assert_eq!(calls.load(Ordering::SeqCst), 3);
            });
    }
}

//! Stage 2 W5b — Solend deposit substrate harness for the controlled
//! wallet.
//!
//! This is the **deposit-side** companion to the W6 refund harness. It
//! moves USDC from the controlled wallet's USDC ATA into a Solend
//! obligation under that same wallet, by emitting:
//!
//! ```text
//! [CreateIdempotentATA(controlled cToken ATA) if missing,
//!  Solend RefreshReserve,
//!  Solend DepositReserveLiquidityAndObligationCollateral]
//! ```
//!
//! After confirmation, the target obligation's `deposits_len` is either
//! incremented (new deposit entry for the Main Pool USDC reserve) OR the
//! existing matching deposit entry's `deposited_amount` has increased.
//!
//! # What this harness deliberately is NOT
//!
//! - **Not a Stage 2 W5b withdraw.** The controlled wallet currently has
//!   no non-empty obligation; this harness creates that substrate.
//! - **Not a `clawsol-authority` ExecuteAction.** No Authorization PDA
//!   is created, no Stage 2 condition proof is built, no on-chain
//!   `ExecuteAction` Borsh ix is emitted. Direct Solend mainnet path.
//! - **Not a Jupiter path.** Zero touchpoints with `api.jup.ag` or any
//!   Jupiter integration.
//! - **Not signed by the user main wallet.** The user main wallet
//!   `C4QQjz…JGPNW` is never loaded or referenced as a signer.
//!
//! # Two-phase env gating
//!
//! **Phase 0** (`CLAW_STAGE2_LIVE_SOLEND_DEPOSIT=1`):
//!  - Load keypair, verify pubkey, derive ATAs.
//!  - Fetch + decode reserve, verify P5c tuple fields.
//!  - Fetch + decode obligation, verify owner + lending market.
//!  - Record before deposits_len + per-reserve before amount.
//!  - Build tx with fresh blockhash; simulate via Helius standard RPC.
//!  - STOP and print the approval prompt. No send.
//!
//! **Phase 2** (`CLAW_STAGE2_SOLEND_DEPOSIT_APPROVED="W5B LIVE SOLEND DEPOSIT APPROVED"`):
//!  - Only runs if BOTH `CLAW_STAGE2_LIVE_SOLEND_DEPOSIT=1` AND the
//!    approval phrase is set (exact, case-sensitive).
//!  - Re-fetches a fresh blockhash, re-signs, broadcasts with
//!    `skipPreflight=false maxRetries=0`, polls via
//!    `getSignatureStatuses` (`searchTransactionHistory=true`).
//!  - Re-decodes the obligation post-finalization and asserts the
//!    deposit landed by exact byte-level state delta.
//!
//! # Required env
//!
//! | Variable | Purpose |
//! | --- | --- |
//! | `CLAW_STAGE2_LIVE_SOLEND_DEPOSIT=1`            | master Phase 0 gate |
//! | `HELIUS_RPC_URL` *or* `CLAW_RPC_URL`           | Helius standard RPC (NOT Sender) |
//! | `CLAW_STAGE2_CLUSTER=mainnet-beta`             | cluster sanity |
//! | `CLAW_STAGE2_DELEGATED_KEYPAIR_PATH=…`         | path to controlled-wallet keypair JSON |
//! | `CLAW_STAGE2_TARGET_OBLIGATION=BdFLjCcP…ra1wN` | target Solend obligation (default: this pubkey) |
//! | `CLAW_STAGE2_SOLEND_DEPOSIT_AMOUNT_RAW=1000000`| raw USDC (default 1,000,000 = 1 USDC) |
//!
//! And, for Phase 2 only:
//!
//! | Variable | Required value |
//! | --- | --- |
//! | `CLAW_STAGE2_SOLEND_DEPOSIT_APPROVED` | `W5B LIVE SOLEND DEPOSIT APPROVED` (verbatim) |
//!
//! Without `CLAW_STAGE2_LIVE_SOLEND_DEPOSIT=1` the test self-skips with
//! zero network calls, zero keypair reads, zero side effects — safe
//! under default `cargo test`.
//!
//! # Run examples
//!
//! Phase 0 only (preflight + simulation, no send):
//!
//! ```powershell
//! $env:CLAW_STAGE2_LIVE_SOLEND_DEPOSIT="1"
//! $env:HELIUS_RPC_URL="https://mainnet.helius-rpc.com/?api-key=..."
//! $env:CLAW_STAGE2_CLUSTER="mainnet-beta"
//! $env:CLAW_STAGE2_DELEGATED_KEYPAIR_PATH="C:\Users\jeffr\test-wallets\slice3c-dryrun.json"
//! cargo test -p claw-gateway --test stage2_live_controlled_wallet_solend_deposit -- stage2_w5b_live_controlled_wallet_solend_deposit --nocapture --exact
//! ```
//!
//! Phase 2 (after Jeff explicitly approves):
//!
//! ```powershell
//! $env:CLAW_STAGE2_SOLEND_DEPOSIT_APPROVED="W5B LIVE SOLEND DEPOSIT APPROVED"
//! cargo test -p claw-gateway --test stage2_live_controlled_wallet_solend_deposit -- stage2_w5b_live_controlled_wallet_solend_deposit --nocapture --exact
//! ```

use std::env;
use std::fs;
use std::str::FromStr;
use std::time::Duration;

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

// ── env var names ─────────────────────────────────────────────────────────

const ENV_GATE: &str = "CLAW_STAGE2_LIVE_SOLEND_DEPOSIT";
const ENV_APPROVAL: &str = "CLAW_STAGE2_SOLEND_DEPOSIT_APPROVED";
const APPROVAL_PHRASE: &str = "W5B LIVE SOLEND DEPOSIT APPROVED";

const ENV_RPC_PRIMARY: &str = "HELIUS_RPC_URL";
const ENV_RPC_SECONDARY: &str = "CLAW_RPC_URL";
const ENV_CLUSTER: &str = "CLAW_STAGE2_CLUSTER";
const ENV_KEYPAIR_PATH: &str = "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH";
const ENV_TARGET_OBLIGATION: &str = "CLAW_STAGE2_TARGET_OBLIGATION";
const ENV_DEPOSIT_AMOUNT_RAW: &str = "CLAW_STAGE2_SOLEND_DEPOSIT_AMOUNT_RAW";

// ── pinned facts ──────────────────────────────────────────────────────────

/// Controlled wallet (Slice 3C delegated). Source of truth:
/// `reference_slice3c_controlled_wallet.md` memory + `data/phantom-mainnet.env`.
const CONTROLLED_WALLET_BS58: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";

/// Default target obligation if `CLAW_STAGE2_TARGET_OBLIGATION` is unset.
/// Discovered via `getProgramAccounts` Phase 0 of W5b — one of three
/// existing empty obligations owned by the controlled wallet under the
/// P5c-pinned lending market.
const DEFAULT_TARGET_OBLIGATION_BS58: &str = "BdFLjCcP9mCy557vNNGVbTUuvHxXsh8hc6jXzaPra1wN";

/// Solend mainnet program id. Same constant the deposit/refresh
/// builders use internally.
const SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

// P5c-pinned tuple (must match `claw_gateway::stage2_executor::DEMO_*_BS58`
// — see the unit test below that asserts byte-equality with the
// `stage2_executor` source-of-truth constants).
const PIN_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const PIN_LENDING_MARKET_BS58: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
const PIN_LIQUIDITY_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const PIN_CTOKEN_MINT_BS58: &str = "993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk";
const PIN_PYTH_ORACLE_BS58: &str = "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX";

const USDC_DECIMALS: u8 = 6;
const DEFAULT_DEPOSIT_AMOUNT_RAW: u64 = 1_000_000; // 1 USDC

// Well-known program ids
const SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const COMPUTE_BUDGET_PROGRAM_BS58: &str = "ComputeBudget111111111111111111111111111111";

// Compute Budget instruction tags (per
// solana-program/compute_budget/src/lib.rs).
const COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT: u8 = 2;
const COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE: u8 = 3;

// Compute budget — deposit reaches Solend CPI (refresh + deposit), so
// headroom is larger than refund's 30k.
const COMPUTE_UNIT_LIMIT: u32 = 400_000;
const COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 1_000_000; // 1 nano-lamport/CU

// Retry / polling
const RPC_MAX_RETRY_ATTEMPTS: usize = 3;
const RPC_INITIAL_BACKOFF_MS: u64 = 500;
const REQUEST_TIMEOUT_MS: u64 = 15_000;
const STATUS_POLL_INTERVAL_MS: u64 = 2_000;
const STATUS_POLL_MAX_ATTEMPTS: usize = 60; // 60 * 2s = 120s budget

// Source-scan list — any of these substrings appearing more than once
// (the FORBIDDEN_CALL_FORMS table entry being the one allowed copy)
// fails `no_send_path_default_invariant`. Mirrors the W6 refund harness
// posture: no Jupiter, no Phantom-frontend signing path, no SPL Token
// authority-change ixs, no preflight bypass.
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
    Ready(Box<DepositConfig>),
}

#[derive(Debug)]
struct DepositConfig {
    rpc_url: String,
    cluster: Cluster,
    keypair_path: String,
    target_obligation: Pubkey,
    amount_raw: u64,
    approved: bool,
}

impl DepositConfig {
    fn from_env_with<F>(getter: F) -> EnvStatus
    where
        F: Fn(&str) -> Option<String>,
    {
        if getter(ENV_GATE).as_deref() != Some("1") {
            return EnvStatus::Skipped(format!(
                "{} not set to 1 — deposit harness self-skipped",
                ENV_GATE
            ));
        }
        let rpc_url = match getter(ENV_RPC_PRIMARY).or_else(|| getter(ENV_RPC_SECONDARY)) {
            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => {
                return EnvStatus::Skipped(format!(
                    "neither {} nor {} is set",
                    ENV_RPC_PRIMARY, ENV_RPC_SECONDARY
                ))
            }
        };
        let cluster_str = match getter(ENV_CLUSTER) {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => return EnvStatus::Skipped(format!("{} not set", ENV_CLUSTER)),
        };
        let cluster = match Cluster::parse(&cluster_str) {
            Some(c) => c,
            None => {
                return EnvStatus::Mismatch(format!(
                    "{}='{}' is not one of mainnet-beta | devnet | localnet",
                    ENV_CLUSTER, cluster_str
                ))
            }
        };
        if cluster != Cluster::MainnetBeta {
            return EnvStatus::Mismatch(format!(
                "deposit target is mainnet-beta only; got cluster={:?}",
                cluster
            ));
        }
        let keypair_path = match getter(ENV_KEYPAIR_PATH) {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => {
                return EnvStatus::Skipped(format!(
                    "{} not set — deposit needs a keypair path",
                    ENV_KEYPAIR_PATH
                ))
            }
        };
        let target_obligation_str = getter(ENV_TARGET_OBLIGATION)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_TARGET_OBLIGATION_BS58.to_string());
        let target_obligation = match Pubkey::from_str(target_obligation_str.trim()) {
            Ok(pk) => pk,
            Err(e) => {
                return EnvStatus::Mismatch(format!(
                    "{}='{}' is not a valid base58 pubkey: {e}",
                    ENV_TARGET_OBLIGATION, target_obligation_str
                ))
            }
        };
        let amount_raw = match getter(ENV_DEPOSIT_AMOUNT_RAW) {
            Some(s) if !s.trim().is_empty() => match s.trim().parse::<u64>() {
                Ok(n) => n,
                Err(e) => {
                    return EnvStatus::Mismatch(format!(
                        "{}='{}' is not a u64: {e}",
                        ENV_DEPOSIT_AMOUNT_RAW, s
                    ))
                }
            },
            _ => DEFAULT_DEPOSIT_AMOUNT_RAW,
        };
        if amount_raw == 0 {
            return EnvStatus::Mismatch(format!("{} must be > 0", ENV_DEPOSIT_AMOUNT_RAW));
        }
        // Approval check — exact-match, case-sensitive, no whitespace
        // trimming.
        let approved = getter(ENV_APPROVAL).as_deref() == Some(APPROVAL_PHRASE);
        EnvStatus::Ready(Box::new(DepositConfig {
            rpc_url,
            cluster,
            keypair_path,
            target_obligation,
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

/// Load a keypair from a Solana CLI-style JSON keypair file (an array
/// of 64 bytes). NEVER prints the private bytes. Returns the keypair
/// or a stringified error.
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

// ── retry / bounded backoff ───────────────────────────────────────────────

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
                    let backoff = initial_backoff
                        .saturating_mul(1u32 << (attempt - 1).min(8) as u32);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
    Err(last.expect("retry exited without recording an error"))
}

// ── RPC helpers (raw JSON-RPC over reqwest; standard Helius, NOT Sender) ──

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
    v["result"].as_u64().ok_or_else(|| RpcErr::Body("missing slot".into()))
}

async fn rpc_get_balance(
    c: &reqwest::Client,
    url: &str,
    pk: &Pubkey,
) -> Result<u64, RpcErr> {
    let v = rpc_post(
        c,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getBalance",
            "params":[pk.to_string(), {"commitment":"confirmed"}]
        }),
    )
    .await?;
    v["result"]["value"]
        .as_u64()
        .ok_or_else(|| RpcErr::Body("missing value".into()))
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
            let raw: u64 = raw_str.parse().map_err(|e: std::num::ParseIntError| {
                RpcErr::Body(format!("amount parse: {e}"))
            })?;
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

/// Fetch the raw account bytes (base64-decoded) for an account. Returns
/// `None` if the account does not exist.
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

#[derive(Debug)]
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
    let err = val.get("err").and_then(|e| if e.is_null() { None } else { Some(e.clone()) });
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

#[derive(Debug)]
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

// ── compute-budget ix builders (hand-built; avoids extra crate dep) ───────

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

// ── reserve / obligation invariant checks ─────────────────────────────────

#[derive(Debug)]
struct ReserveInvariantFail(String);

/// Verify the decoded reserve matches the P5c pinned tuple exactly.
fn verify_reserve_p5c(decoded: &SolendReserveRaw) -> Result<(), ReserveInvariantFail> {
    let pin_lm = Pubkey::from_str(PIN_LENDING_MARKET_BS58).unwrap();
    let pin_liq = Pubkey::from_str(PIN_LIQUIDITY_MINT_BS58).unwrap();
    let pin_coll = Pubkey::from_str(PIN_CTOKEN_MINT_BS58).unwrap();
    let pin_pyth = Pubkey::from_str(PIN_PYTH_ORACLE_BS58).unwrap();
    if decoded.lending_market != pin_lm {
        return Err(ReserveInvariantFail(format!(
            "reserve.lending_market {} != P5c {}",
            decoded.lending_market, pin_lm
        )));
    }
    if decoded.liquidity_mint != pin_liq {
        return Err(ReserveInvariantFail(format!(
            "reserve.liquidity_mint {} != P5c {}",
            decoded.liquidity_mint, pin_liq
        )));
    }
    if decoded.collateral_mint != pin_coll {
        return Err(ReserveInvariantFail(format!(
            "reserve.collateral_mint {} != P5c {}",
            decoded.collateral_mint, pin_coll
        )));
    }
    if decoded.pyth_oracle != pin_pyth {
        return Err(ReserveInvariantFail(format!(
            "reserve.pyth_oracle {} != P5c {}",
            decoded.pyth_oracle, pin_pyth
        )));
    }
    if decoded.liquidity_mint_decimals != USDC_DECIMALS {
        return Err(ReserveInvariantFail(format!(
            "reserve.liquidity_mint_decimals {} != {}",
            decoded.liquidity_mint_decimals, USDC_DECIMALS
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct ObligationInvariantFail(String);

/// Verify the decoded obligation belongs to the controlled wallet under
/// the P5c lending market.
fn verify_obligation_p5c(
    decoded: &SolendObligationRaw,
    controlled: &Pubkey,
) -> Result<(), ObligationInvariantFail> {
    let pin_lm = Pubkey::from_str(PIN_LENDING_MARKET_BS58).unwrap();
    if decoded.owner != *controlled {
        return Err(ObligationInvariantFail(format!(
            "obligation.owner {} != controlled wallet {}",
            decoded.owner, controlled
        )));
    }
    if decoded.lending_market != pin_lm {
        return Err(ObligationInvariantFail(format!(
            "obligation.lending_market {} != P5c {}",
            decoded.lending_market, pin_lm
        )));
    }
    Ok(())
}

/// Sum the deposited cToken amount across all entries in the obligation
/// that reference the P5c-pinned reserve. `0` if no entry exists.
fn obligation_pinned_reserve_amount(decoded: &SolendObligationRaw) -> u64 {
    let pin_reserve = Pubkey::from_str(PIN_RESERVE_BS58).unwrap();
    decoded
        .deposits
        .iter()
        .filter(|d| d.deposit_reserve == pin_reserve)
        .map(|d| d.deposited_amount)
        .sum()
}

// ──────────────────────────────────────────────────────────────────────────
// Live test (env-gated)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage2_w5b_live_controlled_wallet_solend_deposit() {
    let status = DepositConfig::from_env_with(|k| env::var(k).ok());
    let cfg = match status {
        EnvStatus::Skipped(reason) => {
            eprintln!("[skip] W5b deposit: {reason}");
            return;
        }
        EnvStatus::Mismatch(msg) => panic!("STOP — W5b deposit: {msg}"),
        EnvStatus::Ready(c) => c,
    };

    eprintln!("──── W5b controlled-wallet Solend deposit harness ────");
    eprintln!("cluster           : {:?}", cfg.cluster);
    eprintln!("rpc               : {}", redact_url(&cfg.rpc_url));
    eprintln!("keypair path      : {} (contents NOT printed)", cfg.keypair_path);
    eprintln!("target obligation : {}", cfg.target_obligation);
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

    // ── Phase 0 step 1-2: load keypair, verify pubkey ────────────────────
    eprintln!("\n── Phase 0: preflight ──");
    let kp = load_keypair_from_file(&cfg.keypair_path)
        .unwrap_or_else(|e| panic!("STOP — keypair load failed: {e}"));
    let controlled_pk = kp.pubkey();
    let expected_controlled = Pubkey::from_str(CONTROLLED_WALLET_BS58).unwrap();
    if controlled_pk != expected_controlled {
        panic!(
            "STOP — keypair pubkey mismatch: file={} expected={}",
            controlled_pk, expected_controlled
        );
    }
    eprintln!(
        "✓ keypair loaded; pubkey matches pinned controlled wallet {}",
        controlled_pk
    );

    // ── Phase 0 step 3: derive ATAs + pubkey constants ───────────────────
    let solend_program_id = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap();
    let usdc_mint = Pubkey::from_str(PIN_LIQUIDITY_MINT_BS58).unwrap();
    let ctoken_mint = Pubkey::from_str(PIN_CTOKEN_MINT_BS58).unwrap();
    let pin_reserve = Pubkey::from_str(PIN_RESERVE_BS58).unwrap();
    let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap();

    let source_usdc_ata = get_associated_token_address(&controlled_pk, &usdc_mint);
    let controlled_ctoken_ata = get_associated_token_address(&controlled_pk, &ctoken_mint);
    eprintln!("source USDC ATA   : {}", source_usdc_ata);
    eprintln!("ctoken ATA        : {}", controlled_ctoken_ata);

    // ── RPC client ───────────────────────────────────────────────────────
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .expect("build reqwest client");

    // ── Phase 0 step 4: fetch chain meta + BEFORE balances ───────────────
    let ver = retry(
        || rpc_get_version(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getVersion: {e}"));
    let chain_slot = retry(
        || rpc_get_slot(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getSlot: {e}"));
    eprintln!("✓ RPC OK           solana-core={ver} slot={chain_slot}");

    let controlled_sol_before = retry(
        || rpc_get_balance(&http, &cfg.rpc_url, &controlled_pk),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — controlled SOL: {e}"));
    let source_usdc_before = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &source_usdc_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — source USDC: {e}"))
    .unwrap_or_else(|| panic!("STOP — source USDC ATA does not exist: {}", source_usdc_ata));

    eprintln!("\n── before balances ──");
    eprintln!(
        "controlled SOL    : {}.{:09} ({} lamports)",
        controlled_sol_before / 1_000_000_000,
        controlled_sol_before % 1_000_000_000,
        controlled_sol_before
    );
    eprintln!(
        "controlled USDC   : {}.{:06} ({} raw)",
        source_usdc_before.raw / 1_000_000,
        source_usdc_before.raw % 1_000_000,
        source_usdc_before.raw
    );

    // ── Phase 0 step 5: source balance gate ──────────────────────────────
    if source_usdc_before.decimals != USDC_DECIMALS {
        panic!(
            "STOP — source ATA decimals={} (expected {})",
            source_usdc_before.decimals, USDC_DECIMALS
        );
    }
    if source_usdc_before.raw < cfg.amount_raw {
        panic!(
            "STOP — controlled wallet needs refunding first: source USDC \
             balance ({} raw = {}.{:06} USDC) < deposit amount ({} raw)",
            source_usdc_before.raw,
            source_usdc_before.raw / 1_000_000,
            source_usdc_before.raw % 1_000_000,
            cfg.amount_raw,
        );
    }
    eprintln!(
        "✓ source USDC sufficient ({} >= {})",
        source_usdc_before.raw, cfg.amount_raw
    );

    // ── Phase 0 step 6: fetch + decode reserve, verify P5c tuple ─────────
    let reserve_bytes = retry(
        || rpc_get_account_data(&http, &cfg.rpc_url, &pin_reserve),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getAccountInfo(reserve): {e}"))
    .unwrap_or_else(|| panic!("STOP — reserve account does not exist: {}", pin_reserve));
    let reserve = decode_reserve(&reserve_bytes)
        .unwrap_or_else(|e| panic!("STOP — decode_reserve: {e}"));
    verify_reserve_p5c(&reserve)
        .unwrap_or_else(|e| panic!("STOP — reserve P5c mismatch: {}", e.0));
    eprintln!(
        "\n── reserve {} ──",
        pin_reserve
    );
    eprintln!("  liquidity_supply         : {}", reserve.liquidity_supply);
    eprintln!("  collateral_mint          : {}", reserve.collateral_mint);
    eprintln!("  collateral_supply (dest) : {}", reserve.collateral_supply);
    eprintln!("  pyth_oracle              : {}", reserve.pyth_oracle);
    eprintln!("  switchboard_oracle       : {}", reserve.switchboard_oracle);
    eprintln!(
        "  last_update_slot={} stale={}",
        reserve.last_update_slot, reserve.last_update_stale
    );
    eprintln!("✓ reserve P5c tuple verified");

    // ── Phase 0 step 7: fetch + decode obligation, verify ownership ──────
    let obligation_bytes = retry(
        || rpc_get_account_data(&http, &cfg.rpc_url, &cfg.target_obligation),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getAccountInfo(obligation): {e}"))
    .unwrap_or_else(|| {
        panic!(
            "STOP — target obligation account does not exist: {}. \
             InitObligation required for a fresh obligation; see init_obligation.rs.",
            cfg.target_obligation
        )
    });
    let obligation = decode_obligation(&obligation_bytes)
        .unwrap_or_else(|e| panic!("STOP — decode_obligation: {e}"));
    verify_obligation_p5c(&obligation, &controlled_pk)
        .unwrap_or_else(|e| panic!("STOP — obligation invariant: {}", e.0));
    let before_deposits_len = obligation.deposits.len();
    let before_pinned_reserve_amount = obligation_pinned_reserve_amount(&obligation);
    eprintln!(
        "\n── obligation {} ──",
        cfg.target_obligation
    );
    eprintln!("  owner                    : {}", obligation.owner);
    eprintln!("  lending_market           : {}", obligation.lending_market);
    eprintln!(
        "  last_update_slot={} stale={}",
        obligation.last_update_slot, obligation.last_update_stale
    );
    eprintln!(
        "  deposits_len before      : {} (pinned-reserve amount: {})",
        before_deposits_len, before_pinned_reserve_amount
    );
    eprintln!("✓ obligation owner + lending market verified");

    // ── Phase 0 step 8: cToken ATA existence + CreateIdempotent gating ──
    let ctoken_ata_exists = retry(
        || rpc_get_account_exists(&http, &cfg.rpc_url, &controlled_ctoken_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getAccountInfo(cToken ATA): {e}"));
    eprintln!(
        "cToken ATA exists  : {} ({})",
        ctoken_ata_exists,
        if ctoken_ata_exists {
            "no CreateIdempotent needed"
        } else {
            "will append CreateIdempotent"
        }
    );

    // ── Phase 0 step 9: blockhash + build tx ─────────────────────────────
    let latest_bh = retry(
        || rpc_get_latest_blockhash(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getLatestBlockhash: {e}"));
    eprintln!(
        "✓ blockhash        hash={} lvbh={}",
        latest_bh.hash, latest_bh.last_valid_block_height
    );

    // RefreshReserve plan — single reserve, no obligation refresh needed
    // for `DepositReserveLiquidityAndObligationCollateral` (per
    // `crates/gateway/src/integrations/solend/deposit.rs` module doc).
    let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
        solend_program_id,
        reserves: vec![ReserveRefreshInput {
            reserve_pubkey: pin_reserve,
            pyth_oracle: reserve.pyth_oracle,
            switchboard_oracle: reserve.switchboard_oracle,
        }],
        obligation: None,
    });
    assert_eq!(
        refresh_plan.instructions.len(),
        1,
        "expected exactly one RefreshReserve ix"
    );

    let deposit_ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
        DepositInstructionInputs {
            solend_program_id,
            amount: UnderlyingAmount::new(cfg.amount_raw),
            source_liquidity: source_usdc_ata,
            user_collateral: controlled_ctoken_ata,
            reserve: pin_reserve,
            reserve_liquidity_supply: reserve.liquidity_supply,
            reserve_collateral_mint: reserve.collateral_mint,
            lending_market: reserve.lending_market,
            destination_deposit_collateral: reserve.collateral_supply,
            obligation: cfg.target_obligation,
            obligation_owner: controlled_pk,
            pyth_oracle: reserve.pyth_oracle,
            switchboard_oracle: reserve.switchboard_oracle,
            user_transfer_authority: controlled_pk,
        },
    )
    .unwrap_or_else(|e| panic!("STOP — build deposit ix: {e}"));

    let mut ixs: Vec<Instruction> = Vec::with_capacity(5);
    ixs.push(compute_budget_set_unit_limit(COMPUTE_UNIT_LIMIT));
    ixs.push(compute_budget_set_unit_price(COMPUTE_UNIT_PRICE_MICRO_LAMPORTS));
    if !ctoken_ata_exists {
        ixs.push(create_associated_token_account_idempotent(
            &controlled_pk, // funding payer
            &controlled_pk, // ATA owner
            &ctoken_mint,
            &spl_token,
        ));
    }
    for ix in refresh_plan.instructions {
        ixs.push(ix);
    }
    ixs.push(deposit_ix);

    // Defence-in-depth: instruction count is bounded.
    assert!(
        ixs.len() <= 5,
        "instruction count exceeds (compute_limit, compute_price, optional create_ata, refresh_reserve, deposit)"
    );

    let message = Message::new(&ixs, Some(&controlled_pk));
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[&kp], latest_bh.hash);

    let tx_bytes = bincode::serialize(&tx).expect("serialize tx");
    use base64::Engine as _;
    let tx_b64 =
        base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

    // ── Phase 0 step 10: simulate ────────────────────────────────────────
    let sim = retry(
        || rpc_simulate_transaction(&http, &cfg.rpc_url, &tx_b64),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — simulateTransaction RPC: {e}"));

    eprintln!(
        "\n── simulation ──\n  err={:?}\n  units_consumed={:?}",
        sim.err, sim.units_consumed
    );
    for l in &sim.logs {
        eprintln!("  log: {l}");
    }
    if let Some(err) = &sim.err {
        panic!("STOP — simulation failed: {err}");
    }
    eprintln!("✓ simulation passed");

    // ── Phase 1: approval gate ───────────────────────────────────────────
    eprintln!(
        "\n──────────────────────────────────────────────────────────────"
    );
    eprintln!(
        "W5b deposit preflight passed. Ready to deposit exactly {}.{:06} USDC",
        cfg.amount_raw / 1_000_000,
        cfg.amount_raw % 1_000_000
    );
    eprintln!("  from controlled wallet  {}", controlled_pk);
    eprintln!("  via source USDC ATA     {}", source_usdc_ata);
    eprintln!("  into obligation         {}", cfg.target_obligation);
    eprintln!("  reserve                 {} (Solend Main Pool USDC)", pin_reserve);
    eprintln!("  cToken minted to ATA    {}", controlled_ctoken_ata);
    eprintln!("");
    eprintln!("Awaiting exact approval phrase: {APPROVAL_PHRASE}");
    eprintln!("To proceed to Phase 2 live send, re-run with env var:");
    eprintln!("  {ENV_APPROVAL}=\"{APPROVAL_PHRASE}\"");
    eprintln!(
        "──────────────────────────────────────────────────────────────"
    );

    if !cfg.approved {
        eprintln!("\n[Phase 2 skipped — approval phrase not present]");
        eprintln!("\n============================================================");
        eprintln!("W5B SOLEND DEPOSIT RESULT");
        eprintln!("controlled_wallet: {}", controlled_pk);
        eprintln!("obligation: {}", cfg.target_obligation);
        eprintln!("amount_raw: {}", cfg.amount_raw);
        eprintln!(
            "before_usdc: {}.{:06}",
            source_usdc_before.raw / 1_000_000,
            source_usdc_before.raw % 1_000_000
        );
        eprintln!("after_usdc: N/A");
        eprintln!("before_deposits_len: {}", before_deposits_len);
        eprintln!("after_deposits_len: N/A");
        eprintln!("tx_signature: N/A");
        eprintln!("status: simulated");
        eprintln!("============================================================");
        return;
    }

    // ── Phase 2: live send (only after approval) ─────────────────────────
    eprintln!("\n── Phase 2: live send (approval phrase verified) ──");

    let fresh_bh = retry(
        || rpc_get_latest_blockhash(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — fresh getLatestBlockhash: {e}"));
    eprintln!(
        "✓ fresh blockhash  hash={} lvbh={}",
        fresh_bh.hash, fresh_bh.last_valid_block_height
    );

    let message2 = Message::new(&ixs, Some(&controlled_pk));
    let mut tx2 = Transaction::new_unsigned(message2);
    tx2.sign(&[&kp], fresh_bh.hash);
    let tx2_bytes = bincode::serialize(&tx2).expect("serialize tx2");
    let tx2_b64 =
        base64::engine::general_purpose::STANDARD.encode(&tx2_bytes);

    let signature = retry(
        || rpc_send_transaction_base64(&http, &cfg.rpc_url, &tx2_b64),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — sendTransaction: {e}"));
    eprintln!("✓ sendTransaction submitted; signature={signature}");
    eprintln!("  Solscan: https://solscan.io/tx/{signature}");

    // Bounded confirmation poll.
    let mut attempts = 0;
    let mut terminal: Option<SigStatus> = None;
    while attempts < STATUS_POLL_MAX_ATTEMPTS {
        attempts += 1;
        tokio::time::sleep(Duration::from_millis(STATUS_POLL_INTERVAL_MS)).await;
        let st = match retry(
            || rpc_get_signature_status(&http, &cfg.rpc_url, &signature),
            RPC_MAX_RETRY_ATTEMPTS,
            Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  attempt {attempts}: RPC error: {e} (will retry)");
                continue;
            }
        };
        match st {
            SigStatus::NotFound => {
                eprintln!("  attempt {attempts}: not_found (still propagating)");
                continue;
            }
            SigStatus::Pending => {
                eprintln!("  attempt {attempts}: pending");
                continue;
            }
            SigStatus::Failed(err) => {
                eprintln!("✗ tx failed on chain: {err}");
                terminal = Some(SigStatus::Failed(err));
                break;
            }
            SigStatus::Succeeded { slot, confirmation } => {
                eprintln!("✓ attempt {attempts}: {confirmation} at slot {slot}");
                let is_finalized = confirmation == "finalized";
                terminal = Some(SigStatus::Succeeded { slot, confirmation });
                if is_finalized {
                    break;
                }
            }
        }
    }

    let terminal = terminal.unwrap_or_else(|| {
        panic!(
            "STOP — confirmation timeout after {} attempts (~{}s); signature={}",
            STATUS_POLL_MAX_ATTEMPTS,
            (STATUS_POLL_MAX_ATTEMPTS as u64 * STATUS_POLL_INTERVAL_MS) / 1000,
            signature
        )
    });
    let send_status = match &terminal {
        SigStatus::Failed(err) => {
            // Emit the result banner with status=failed before panicking,
            // so the operator always sees the full transition report.
            eprintln!("\n============================================================");
            eprintln!("W5B SOLEND DEPOSIT RESULT");
            eprintln!("controlled_wallet: {}", controlled_pk);
            eprintln!("obligation: {}", cfg.target_obligation);
            eprintln!("amount_raw: {}", cfg.amount_raw);
            eprintln!(
                "before_usdc: {}.{:06}",
                source_usdc_before.raw / 1_000_000,
                source_usdc_before.raw % 1_000_000
            );
            eprintln!("after_usdc: (not refetched — tx failed)");
            eprintln!("before_deposits_len: {}", before_deposits_len);
            eprintln!("after_deposits_len: (not refetched — tx failed)");
            eprintln!("tx_signature: {}", signature);
            eprintln!("status: failed");
            eprintln!("============================================================");
            panic!("STOP — tx failed: err={err} signature={signature}");
        }
        SigStatus::Succeeded { slot, confirmation } => {
            eprintln!(
                "\n✓ TERMINAL  confirmation={} slot={} signature={}",
                confirmation, slot, signature
            );
            confirmation.clone()
        }
        _ => panic!("STOP — unexpected terminal state"),
    };

    // ── Phase 2 step 8-9: re-fetch + assert deltas ───────────────────────
    let source_usdc_after = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &source_usdc_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — source USDC after: {e}"))
    .unwrap_or_else(|| panic!("STOP — source ATA vanished after tx"));
    let obligation_bytes_after = retry(
        || rpc_get_account_data(&http, &cfg.rpc_url, &cfg.target_obligation),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — refetch obligation: {e}"))
    .unwrap_or_else(|| panic!("STOP — obligation vanished after tx"));
    let obligation_after = decode_obligation(&obligation_bytes_after)
        .unwrap_or_else(|e| panic!("STOP — re-decode obligation: {e}"));

    let after_deposits_len = obligation_after.deposits.len();
    let after_pinned_reserve_amount = obligation_pinned_reserve_amount(&obligation_after);

    eprintln!("\n── after balances + obligation ──");
    eprintln!(
        "controlled USDC   : {}.{:06} (Δ -{})",
        source_usdc_after.raw / 1_000_000,
        source_usdc_after.raw % 1_000_000,
        source_usdc_before.raw - source_usdc_after.raw
    );
    eprintln!(
        "deposits_len      : {} → {}",
        before_deposits_len, after_deposits_len
    );
    eprintln!(
        "pinned-reserve cToken amount: {} → {} (Δ +{})",
        before_pinned_reserve_amount,
        after_pinned_reserve_amount,
        after_pinned_reserve_amount.saturating_sub(before_pinned_reserve_amount)
    );

    // Hard assertions: exact USDC delta, and the obligation has a deposit
    // entry for the pinned reserve with a strictly larger amount than
    // before (either a brand-new entry — `before == 0` — or an existing
    // entry that grew).
    assert_eq!(
        source_usdc_before.raw - source_usdc_after.raw,
        cfg.amount_raw,
        "source USDC decreased by exactly {} raw",
        cfg.amount_raw
    );
    assert!(
        after_pinned_reserve_amount > before_pinned_reserve_amount,
        "obligation pinned-reserve cToken amount did not increase: \
         before={before_pinned_reserve_amount} after={after_pinned_reserve_amount}"
    );

    // The terminal banner — required by the W5b addendum.
    eprintln!("\n============================================================");
    eprintln!("W5B SOLEND DEPOSIT RESULT");
    eprintln!("controlled_wallet: {}", controlled_pk);
    eprintln!("obligation: {}", cfg.target_obligation);
    eprintln!("amount_raw: {}", cfg.amount_raw);
    eprintln!(
        "before_usdc: {}.{:06}",
        source_usdc_before.raw / 1_000_000,
        source_usdc_before.raw % 1_000_000
    );
    eprintln!(
        "after_usdc: {}.{:06}",
        source_usdc_after.raw / 1_000_000,
        source_usdc_after.raw % 1_000_000
    );
    eprintln!("before_deposits_len: {}", before_deposits_len);
    eprintln!("after_deposits_len: {}", after_deposits_len);
    eprintln!("tx_signature: {}", signature);
    eprintln!("status: {}", send_status);
    eprintln!("============================================================");

    eprintln!("\n──── W5b Solend deposit complete ────");
    eprintln!("signature : {signature}");
    eprintln!("Solscan   : https://solscan.io/tx/{signature}");
}

// ──────────────────────────────────────────────────────────────────────────
// Unit tests (always run; non-live; no network; no keypair)
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_gate_skips_without_master_flag() {
        let status = DepositConfig::from_env_with(|_| None);
        match status {
            EnvStatus::Skipped(reason) => assert!(reason.contains(ENV_GATE)),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn cluster_must_be_mainnet_beta() {
        let status = DepositConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_SOLEND_DEPOSIT" => Some("1".to_string()),
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
    fn deposit_amount_zero_rejected() {
        let status = DepositConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_SOLEND_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_SOLEND_DEPOSIT_AMOUNT_RAW" => Some("0".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("> 0")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn default_amount_is_1_usdc_when_unset() {
        let status = DepositConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_SOLEND_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert_eq!(c.amount_raw, DEFAULT_DEPOSIT_AMOUNT_RAW);
                assert_eq!(c.amount_raw, 1_000_000);
                assert!(!c.approved);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn default_target_obligation_when_unset() {
        let status = DepositConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_SOLEND_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert_eq!(
                    c.target_obligation,
                    Pubkey::from_str(DEFAULT_TARGET_OBLIGATION_BS58).unwrap()
                );
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn approval_phrase_is_exact_match() {
        // Almost-but-not-the-right phrase
        let status = DepositConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_SOLEND_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_SOLEND_DEPOSIT_APPROVED" => {
                Some("w5b live solend deposit approved".to_string())
            }
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => assert!(!c.approved, "lowercase must not approve"),
            other => panic!("expected Ready, got {other:?}"),
        }
        // Exact phrase
        let status = DepositConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_SOLEND_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_SOLEND_DEPOSIT_APPROVED" => Some(APPROVAL_PHRASE.to_string()),
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
    fn compute_budget_ixs_shape() {
        let lim = compute_budget_set_unit_limit(400_000);
        assert_eq!(lim.data.len(), 5);
        assert_eq!(lim.data[0], COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT);
        assert_eq!(
            u32::from_le_bytes(lim.data[1..5].try_into().unwrap()),
            400_000
        );

        let pri = compute_budget_set_unit_price(1_000_000);
        assert_eq!(pri.data.len(), 9);
        assert_eq!(pri.data[0], COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE);
        assert_eq!(
            u64::from_le_bytes(pri.data[1..9].try_into().unwrap()),
            1_000_000
        );
    }

    /// Pinned-tuple parity: the in-file constants must byte-equal the
    /// `claw_gateway::stage2_executor::DEMO_*_BS58` constants (which are
    /// the workspace-wide source of truth) — divergence is a wire
    /// regression that this test catches before any RPC is touched.
    #[test]
    fn pin_constants_match_stage2_executor() {
        assert_eq!(PIN_RESERVE_BS58, claw_gateway::stage2_executor::DEMO_RESERVE_BS58);
        assert_eq!(
            PIN_LENDING_MARKET_BS58,
            claw_gateway::stage2_executor::DEMO_LENDING_MARKET_BS58
        );
        assert_eq!(
            PIN_LIQUIDITY_MINT_BS58,
            claw_gateway::stage2_executor::DEMO_LIQUIDITY_MINT_BS58
        );
        assert_eq!(
            PIN_CTOKEN_MINT_BS58,
            claw_gateway::stage2_executor::DEMO_CTOKEN_MINT_BS58
        );
        assert_eq!(
            PIN_PYTH_ORACLE_BS58,
            claw_gateway::stage2_executor::DEMO_PYTH_ORACLE_BS58
        );
    }

    /// Source-scan: no forbidden call-form appears more than once. The
    /// allowed appearance is the FORBIDDEN_CALL_FORMS allowlist entry.
    /// Catches accidental introduction of Jupiter / Phantom-frontend /
    /// SPL-authority-changing call sites in this harness.
    #[test]
    fn no_send_path_default_invariant() {
        const SOURCE: &str = include_str!("stage2_live_controlled_wallet_solend_deposit.rs");
        for forbidden in FORBIDDEN_CALL_FORMS {
            let count = SOURCE.matches(forbidden).count();
            assert!(
                count <= 1,
                "forbidden call-form `{forbidden}` appears {count} times in W5b deposit harness; \
                 only the FORBIDDEN_CALL_FORMS allowlist entry is permitted"
            );
        }
    }

    #[test]
    fn retry_policy_is_bounded() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let calls = std::sync::Arc::new(AtomicUsize::new(0));
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

    #[test]
    fn obligation_pinned_reserve_amount_sums_correctly() {
        use claw_gateway::integrations::solend::raw::SolendObligationCollateralRaw;
        let pin = Pubkey::from_str(PIN_RESERVE_BS58).unwrap();
        let other = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        let obl = SolendObligationRaw {
            version: 1,
            last_update_slot: 0,
            last_update_stale: false,
            lending_market: Pubkey::default(),
            owner: Pubkey::default(),
            deposits: vec![
                SolendObligationCollateralRaw {
                    deposit_reserve: pin,
                    deposited_amount: 42,
                },
                SolendObligationCollateralRaw {
                    deposit_reserve: other,
                    deposited_amount: 100,
                },
                SolendObligationCollateralRaw {
                    deposit_reserve: pin,
                    deposited_amount: 8,
                },
            ],
            borrows: vec![],
            borrowed_value_upper_bound_wads: 0,
            borrowing_isolated_asset: false,
            super_unhealthy_borrow_value_wads: 0,
            unweighted_borrowed_value_wads: 0,
            closeable: false,
        };
        assert_eq!(obligation_pinned_reserve_amount(&obl), 50);
    }
}

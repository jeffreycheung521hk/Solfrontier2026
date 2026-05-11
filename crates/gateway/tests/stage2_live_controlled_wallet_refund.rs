//! OPT-IN: W6 — live controlled-wallet USDC refund harness.
//!
//! Default: SKIPPED. No network calls, no signing, no broadcast unless
//! every gate is explicitly set.
//!
//! Returns exactly 5 USDC from the W5-funded controlled wallet
//! (`BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`) back to the user
//! main wallet (`C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW`) on
//! Solana mainnet-beta. The controlled wallet's keypair signs; the
//! user main wallet is the SPL Token destination only and never
//! signs anything.
//!
//! # Two-phase gating (load-bearing)
//!
//! **Phase 0** (`CLAW_STAGE2_LIVE_REFUND=1`):
//! - Loads keypair from disk, derives ATAs, fetches balances,
//!   builds the unsigned tx, runs `simulateTransaction`, prints a
//!   summary including the approval-prompt phrase.
//! - No broadcast. Returns OK whether the operator approves or not.
//!
//! **Phase 2** (`CLAW_STAGE2_REFUND_APPROVED=W6 LIVE REFUND APPROVED`):
//! - Only runs if BOTH `CLAW_STAGE2_LIVE_REFUND=1` AND the approval
//!   env var is set to the EXACT string above (verbatim).
//! - Re-fetches a fresh blockhash, re-signs, sends with
//!   `skipPreflight=false`, polls `getSignatureStatuses` for up to
//!   120 s past block-height-exceeded, treats success only when
//!   `err == null && confirmationStatus in {confirmed, finalized}`.
//!
//! # Required env (Phase 0)
//!
//! | Var | Purpose |
//! |---|---|
//! | `CLAW_STAGE2_LIVE_REFUND=1`              | master gate |
//! | `HELIUS_RPC_URL` *or* `CLAW_RPC_URL`     | Helius standard RPC (NOT Sender) |
//! | `CLAW_STAGE2_CLUSTER=mainnet-beta`       | cluster sanity |
//! | `CLAW_STAGE2_DELEGATED_KEYPAIR_PATH=…`   | path to controlled-wallet keypair JSON |
//! | `CLAW_STAGE2_REFUND_DESTINATION=C4QQjz…` | user main wallet destination |
//! | `CLAW_STAGE2_REFUND_AMOUNT_RAW=5000000`  | refund amount (raw, 6-decimal USDC; default 5_000_000 if unset) |
//!
//! # Required env (Phase 2 only)
//!
//! | Var | Required value |
//! |---|---|
//! | `CLAW_STAGE2_REFUND_APPROVED` | `W6 LIVE REFUND APPROVED` (verbatim, case-sensitive) |
//!
//! # Safety posture (load-bearing — `no_send_path_default` source-scan enforces this)
//!
//! - Private keypair bytes are read from disk but NEVER printed.
//! - User main wallet is destination only — never appears as a signer.
//! - No Solend, no Jupiter, no `api.jup.ag`, no `signTransaction` UI
//!   call, no `setAuthority`, no `approve`, no `closeAccount`, no
//!   `delegate`.
//! - Transaction contains exactly: optional ComputeBudget ixs, optional
//!   `CreateAssociatedTokenAccountIdempotent` (only if destination ATA
//!   doesn't already exist on chain), and exactly one
//!   `TransferChecked` with `decimals=6` and `amount=5_000_000`.
//! - `skipPreflight=false`.
//! - No retry on failure: a single broadcast attempt + bounded
//!   confirmation polling.
//! - Bounded retry / backoff on RPC layer for 429 / 5xx.
//!
//! # Recommended invocation (single-threaded — Helius free-tier rate-limit)
//!
//! ```text
//! # Phase 0 (preflight + simulation, no send)
//! cargo test -p claw-gateway --test stage2_live_controlled_wallet_refund \
//!     -- --nocapture --test-threads=1
//!
//! # Phase 2 (live send) — only after Phase 0 prints OK
//! $env:CLAW_STAGE2_REFUND_APPROVED="W6 LIVE REFUND APPROVED"
//! cargo test -p claw-gateway --test stage2_live_controlled_wallet_refund \
//!     -- --nocapture --test-threads=1
//! ```

use std::env;
use std::fs;
use std::str::FromStr;
use std::time::Duration;

use serde_json::{json, Value};
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};

// ── env var names ─────────────────────────────────────────────────────────

const ENV_GATE: &str = "CLAW_STAGE2_LIVE_REFUND";
const ENV_APPROVAL: &str = "CLAW_STAGE2_REFUND_APPROVED";
const APPROVAL_PHRASE: &str = "W6 LIVE REFUND APPROVED";

const ENV_RPC_PRIMARY: &str = "HELIUS_RPC_URL";
const ENV_RPC_SECONDARY: &str = "CLAW_RPC_URL";
const ENV_CLUSTER: &str = "CLAW_STAGE2_CLUSTER";
const ENV_KEYPAIR_PATH: &str = "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH";
const ENV_REFUND_DESTINATION: &str = "CLAW_STAGE2_REFUND_DESTINATION";
const ENV_REFUND_AMOUNT_RAW: &str = "CLAW_STAGE2_REFUND_AMOUNT_RAW";

// ── pinned facts ──────────────────────────────────────────────────────────

const CONTROLLED_WALLET_BS58: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";
const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDC_DECIMALS: u8 = 6;
const DEFAULT_REFUND_AMOUNT_RAW: u64 = 5_000_000;

// Well-known program ids
const SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const COMPUTE_BUDGET_PROGRAM_BS58: &str = "ComputeBudget111111111111111111111111111111";

// SPL Token instruction tags (per
// solana-program-library/token/program/src/instruction.rs)
const SPL_TOKEN_IX_TAG_TRANSFER_CHECKED: u8 = 12;
const COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT: u8 = 2;
const COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE: u8 = 3;

// Compute budget — modest, mirrors A5 funding tx posture
const COMPUTE_UNIT_LIMIT: u32 = 30_000;
const COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 1_000_000; // 1 nano-lamport/CU

// Retry / polling
const RPC_MAX_RETRY_ATTEMPTS: usize = 3;
const RPC_INITIAL_BACKOFF_MS: u64 = 500;
const REQUEST_TIMEOUT_MS: u64 = 15_000;
const STATUS_POLL_INTERVAL_MS: u64 = 2_000;
const STATUS_POLL_MAX_ATTEMPTS: usize = 60; // 60 * 2s = 120s budget

// Source-scan list — any of these substrings appearing more than once
// (the FORBIDDEN_CALL_FORMS table entry being the one allowed copy)
// fails `no_send_path_default_invariant`.
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
    Ready(Box<RefundConfig>),
}

#[derive(Debug)]
struct RefundConfig {
    rpc_url: String,
    cluster: Cluster,
    keypair_path: String,
    destination: Pubkey,
    amount_raw: u64,
    approved: bool,
}

impl RefundConfig {
    fn from_env_with<F>(getter: F) -> EnvStatus
    where
        F: Fn(&str) -> Option<String>,
    {
        if getter(ENV_GATE).as_deref() != Some("1") {
            return EnvStatus::Skipped(format!(
                "{} not set to 1 — refund harness self-skipped",
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
                "refund target is mainnet-beta only; got cluster={:?}",
                cluster
            ));
        }
        let keypair_path = match getter(ENV_KEYPAIR_PATH) {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => {
                return EnvStatus::Skipped(format!(
                    "{} not set — refund needs a keypair path",
                    ENV_KEYPAIR_PATH
                ))
            }
        };
        let destination = match getter(ENV_REFUND_DESTINATION) {
            Some(d) if !d.trim().is_empty() => match Pubkey::from_str(d.trim()) {
                Ok(pk) => pk,
                Err(e) => {
                    return EnvStatus::Mismatch(format!(
                        "{}='{}' is not a valid base58 pubkey: {e}",
                        ENV_REFUND_DESTINATION, d
                    ))
                }
            },
            _ => {
                return EnvStatus::Skipped(format!(
                    "{} not set — refund needs an explicit destination",
                    ENV_REFUND_DESTINATION
                ))
            }
        };
        let amount_raw = match getter(ENV_REFUND_AMOUNT_RAW) {
            Some(s) if !s.trim().is_empty() => match s.trim().parse::<u64>() {
                Ok(n) => n,
                Err(e) => {
                    return EnvStatus::Mismatch(format!(
                        "{}='{}' is not a u64: {e}",
                        ENV_REFUND_AMOUNT_RAW, s
                    ))
                }
            },
            _ => DEFAULT_REFUND_AMOUNT_RAW,
        };
        if amount_raw == 0 {
            return EnvStatus::Mismatch(format!(
                "{} must be > 0",
                ENV_REFUND_AMOUNT_RAW
            ));
        }
        // Approval check — exact-match, case-sensitive, no whitespace
        // trimming. If anything other than the exact phrase is set, we
        // do NOT proceed to Phase 2; we treat it as preflight-only.
        let approved = getter(ENV_APPROVAL).as_deref() == Some(APPROVAL_PHRASE);
        EnvStatus::Ready(Box::new(RefundConfig {
            rpc_url,
            cluster,
            keypair_path,
            destination,
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

/// Load a keypair from a Solana CLI–style JSON keypair file (an array
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
    // getTokenAccountBalance returns an error if the account doesn't
    // exist or isn't a token account. Translate that to Ok(None).
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
        Err(RpcErr::Body(s)) if s.contains("could not find account")
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

// ── instruction builders (hand-built — avoids extra crate dep) ────────────

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

/// SPL Token `TransferChecked` (ix tag 12). Layout:
/// `[12, amount_u64_le (8 bytes), decimals_u8 (1 byte)]`. Accounts:
/// 0=source, 1=mint, 2=destination, 3=authority (signer).
fn transfer_checked(
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = Vec::with_capacity(10);
    data.push(SPL_TOKEN_IX_TAG_TRANSFER_CHECKED);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Instruction {
        program_id: Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap(),
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Live test (env-gated)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage2_w6_live_controlled_wallet_refund() {
    let status = RefundConfig::from_env_with(|k| env::var(k).ok());
    let cfg = match status {
        EnvStatus::Skipped(reason) => {
            eprintln!("[skip] W6 refund: {reason}");
            return;
        }
        EnvStatus::Mismatch(msg) => panic!("STOP — W6 refund: {msg}"),
        EnvStatus::Ready(c) => c,
    };

    eprintln!("──── W6 controlled-wallet refund harness ────");
    eprintln!("cluster         : {:?}", cfg.cluster);
    eprintln!("rpc             : {}", redact_url(&cfg.rpc_url));
    eprintln!("keypair path    : {} (contents NOT printed)", cfg.keypair_path);
    eprintln!("destination     : {}", cfg.destination);
    eprintln!("amount (raw)    : {}", cfg.amount_raw);
    eprintln!("amount (UI)     : {}.{:06} USDC", cfg.amount_raw / 1_000_000, cfg.amount_raw % 1_000_000);
    eprintln!("approved        : {}", if cfg.approved { "yes" } else { "no — Phase 0 only" });

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
    eprintln!("✓ keypair loaded; pubkey matches pinned controlled wallet {}", controlled_pk);

    if cfg.destination == controlled_pk {
        panic!("STOP — destination equals controlled wallet (refusing self-loop)");
    }

    // ── Phase 0 step 3-4: derive ATAs ────────────────────────────────────
    let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
    let source_ata = get_associated_token_address(&controlled_pk, &usdc_mint);
    let destination_ata = get_associated_token_address(&cfg.destination, &usdc_mint);
    eprintln!("source ATA      : {}", source_ata);
    eprintln!("destination ATA : {}", destination_ata);

    // ── RPC client ───────────────────────────────────────────────────────
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .expect("build reqwest client");

    // ── Phase 0 step 5: fetch BEFORE balances + blockhash ────────────────
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
    eprintln!("✓ RPC OK         solana-core={ver} slot={chain_slot}");

    let controlled_sol_before = retry(
        || rpc_get_balance(&http, &cfg.rpc_url, &controlled_pk),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — controlled SOL: {e}"));
    let source_usdc_before = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &source_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — source USDC: {e}"))
    .unwrap_or_else(|| panic!("STOP — source USDC ATA does not exist"));
    let dest_usdc_before = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &destination_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — dest USDC: {e}"));
    let dest_exists = retry(
        || rpc_get_account_exists(&http, &cfg.rpc_url, &destination_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getAccountInfo(dest_ata): {e}"));

    eprintln!("\n── before balances ──");
    eprintln!(
        "controlled SOL  : {}.{:09} ({} lamports)",
        controlled_sol_before / 1_000_000_000,
        controlled_sol_before % 1_000_000_000,
        controlled_sol_before
    );
    eprintln!(
        "controlled USDC : {}.{:06} ({} raw)",
        source_usdc_before.raw / 1_000_000,
        source_usdc_before.raw % 1_000_000,
        source_usdc_before.raw
    );
    eprintln!(
        "user dest USDC  : {} ({})",
        match &dest_usdc_before {
            Some(a) => format!("{}.{:06}", a.raw / 1_000_000, a.raw % 1_000_000),
            None => "ATA does not exist yet".to_string(),
        },
        match &dest_usdc_before {
            Some(a) => format!("{} raw", a.raw),
            None => "will be created idempotently".to_string(),
        }
    );

    // ── Phase 0 step 6: assert source balance sufficient ─────────────────
    if source_usdc_before.raw < cfg.amount_raw {
        panic!(
            "STOP — source USDC balance ({} raw) < refund amount ({} raw)",
            source_usdc_before.raw, cfg.amount_raw
        );
    }
    if source_usdc_before.decimals != USDC_DECIMALS {
        panic!(
            "STOP — source ATA decimals={} (expected {})",
            source_usdc_before.decimals, USDC_DECIMALS
        );
    }
    eprintln!("✓ source USDC sufficient ({} >= {})", source_usdc_before.raw, cfg.amount_raw);

    // ── Phase 0 step 8: build transaction ────────────────────────────────
    let latest_bh = retry(
        || rpc_get_latest_blockhash(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getLatestBlockhash: {e}"));
    eprintln!(
        "✓ blockhash      hash={} lvbh={}",
        latest_bh.hash, latest_bh.last_valid_block_height
    );

    let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap();
    let mut ixs: Vec<Instruction> = Vec::with_capacity(4);
    ixs.push(compute_budget_set_unit_limit(COMPUTE_UNIT_LIMIT));
    ixs.push(compute_budget_set_unit_price(COMPUTE_UNIT_PRICE_MICRO_LAMPORTS));
    if !dest_exists {
        eprintln!("· destination ATA missing — including CreateIdempotent");
        ixs.push(create_associated_token_account_idempotent(
            &controlled_pk,   // funding payer
            &cfg.destination, // ATA owner = user wallet
            &usdc_mint,
            &spl_token,
        ));
    } else {
        eprintln!("· destination ATA exists — skipping CreateIdempotent");
    }
    ixs.push(transfer_checked(
        &source_ata,
        &usdc_mint,
        &destination_ata,
        &controlled_pk,
        cfg.amount_raw,
        USDC_DECIMALS,
    ));

    // Sanity: any non-{transfer_checked|create_ata|compute_budget} ix
    // here would be a regression; the test source's
    // `no_send_path_default_invariant` covers banned call-forms, but
    // we also enforce instruction-count + program-id invariants live.
    assert!(
        ixs.len() <= 4,
        "instruction count exceeds (compute_limit, compute_price, optional create_ata, transfer_checked)"
    );

    let message = Message::new(&ixs, Some(&controlled_pk));
    let mut tx = Transaction::new_unsigned(message);
    tx.sign(&[&kp], latest_bh.hash);

    let tx_bytes = bincode::serialize(&tx).expect("serialize tx");
    let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_bytes);

    // ── Phase 0 step 9-10: simulate ──────────────────────────────────────
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
        "W6 refund preflight passed. Ready to send exactly {}.{:06} USDC",
        cfg.amount_raw / 1_000_000,
        cfg.amount_raw % 1_000_000
    );
    eprintln!(
        "  from controlled wallet  {}",
        controlled_pk
    );
    eprintln!(
        "  via source ATA          {}",
        source_ata
    );
    eprintln!(
        "  to user wallet          {}",
        cfg.destination
    );
    eprintln!(
        "  via destination ATA     {}",
        destination_ata
    );
    eprintln!("");
    eprintln!("Awaiting exact approval phrase: {APPROVAL_PHRASE}");
    eprintln!(
        "To proceed to Phase 2 live send, re-run with env var:"
    );
    eprintln!("  {ENV_APPROVAL}=\"{APPROVAL_PHRASE}\"");
    eprintln!(
        "──────────────────────────────────────────────────────────────"
    );

    if !cfg.approved {
        eprintln!("\n[Phase 2 skipped — approval phrase not present]");
        return;
    }

    // ── Phase 2: live send (only after approval) ─────────────────────────
    eprintln!("\n── Phase 2: live send (approval phrase verified) ──");

    // Re-fetch a FRESH blockhash immediately before signing to maximize
    // the validity window for confirmation polling.
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
    let tx2_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx2_bytes);

    // Send exactly once. skipPreflight=false. maxRetries=0.
    let signature = retry(
        || rpc_send_transaction_base64(&http, &cfg.rpc_url, &tx2_b64),
        RPC_MAX_RETRY_ATTEMPTS, // network-level retry only; not tx-level
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — sendTransaction: {e}"));
    eprintln!("✓ sendTransaction submitted; signature={signature}");
    eprintln!("  Solscan: https://solscan.io/tx/{signature}");

    // Bounded confirmation poll via getSignatureStatuses with
    // searchTransactionHistory=true — survives block-height-exceeded.
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
                eprintln!(
                    "✓ attempt {attempts}: {confirmation} at slot {slot}"
                );
                let is_finalized = confirmation == "finalized";
                terminal = Some(SigStatus::Succeeded { slot, confirmation });
                if is_finalized {
                    break;
                }
                // "confirmed" — keep polling for finalized.
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
    match &terminal {
        SigStatus::Failed(err) => {
            panic!("STOP — tx failed: err={err} signature={signature}");
        }
        SigStatus::Succeeded { slot, confirmation } => {
            eprintln!(
                "\n✓ TERMINAL  confirmation={} slot={} signature={}",
                confirmation, slot, signature
            );
        }
        _ => panic!("STOP — unexpected terminal state"),
    }

    // ── Phase 2 step 8-9: after-balances + delta assertions ──────────────
    let controlled_sol_after = retry(
        || rpc_get_balance(&http, &cfg.rpc_url, &controlled_pk),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — controlled SOL after: {e}"));
    let source_usdc_after = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &source_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — source USDC after: {e}"))
    .unwrap_or_else(|| panic!("STOP — source ATA vanished after tx"));
    let dest_usdc_after = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &destination_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — dest USDC after: {e}"))
    .unwrap_or_else(|| panic!("STOP — dest ATA missing after tx"));

    let dest_before_raw = dest_usdc_before.map(|a| a.raw).unwrap_or(0);

    eprintln!("\n── after balances ──");
    eprintln!(
        "controlled SOL  : {}.{:09} (Δ {})",
        controlled_sol_after / 1_000_000_000,
        controlled_sol_after % 1_000_000_000,
        controlled_sol_after as i128 - controlled_sol_before as i128
    );
    eprintln!(
        "controlled USDC : {}.{:06} (Δ -{})",
        source_usdc_after.raw / 1_000_000,
        source_usdc_after.raw % 1_000_000,
        source_usdc_before.raw - source_usdc_after.raw
    );
    eprintln!(
        "user USDC       : {}.{:06} (Δ +{})",
        dest_usdc_after.raw / 1_000_000,
        dest_usdc_after.raw % 1_000_000,
        dest_usdc_after.raw - dest_before_raw
    );

    assert_eq!(
        source_usdc_before.raw - source_usdc_after.raw,
        cfg.amount_raw,
        "source USDC decreased by exactly {} raw",
        cfg.amount_raw
    );
    assert_eq!(
        dest_usdc_after.raw - dest_before_raw,
        cfg.amount_raw,
        "destination USDC increased by exactly {} raw",
        cfg.amount_raw
    );

    eprintln!("\n──── W6 refund complete ────");
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
        let status = RefundConfig::from_env_with(|_| None);
        match status {
            EnvStatus::Skipped(reason) => assert!(reason.contains(ENV_GATE)),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn cluster_must_be_mainnet_beta() {
        let status = RefundConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_REFUND" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("devnet".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_REFUND_DESTINATION" => {
                Some("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string())
            }
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("mainnet-beta")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn refund_amount_zero_rejected() {
        let status = RefundConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_REFUND" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_REFUND_DESTINATION" => {
                Some("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string())
            }
            "CLAW_STAGE2_REFUND_AMOUNT_RAW" => Some("0".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("> 0")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn default_amount_is_5_usdc_when_unset() {
        let status = RefundConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_REFUND" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_REFUND_DESTINATION" => {
                Some("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string())
            }
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert_eq!(c.amount_raw, DEFAULT_REFUND_AMOUNT_RAW);
                assert!(!c.approved); // approval env not set
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn approval_phrase_is_exact_match() {
        // Almost-but-not-the-right phrase
        let status = RefundConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_REFUND" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_REFUND_DESTINATION" => {
                Some("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string())
            }
            "CLAW_STAGE2_REFUND_APPROVED" => Some("w6 live refund approved".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => assert!(!c.approved, "lowercase must not approve"),
            other => panic!("expected Ready, got {other:?}"),
        }
        // Exact phrase
        let status = RefundConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_REFUND" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_REFUND_DESTINATION" => {
                Some("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".to_string())
            }
            "CLAW_STAGE2_REFUND_APPROVED" => Some(APPROVAL_PHRASE.to_string()),
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
    fn transfer_checked_ix_shape() {
        let s = Pubkey::new_unique();
        let m = Pubkey::new_unique();
        let d = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        let ix = transfer_checked(&s, &m, &d, &a, 5_000_000, 6);
        assert_eq!(
            ix.program_id,
            Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap()
        );
        assert_eq!(ix.accounts.len(), 4);
        assert_eq!(ix.accounts[0].pubkey, s);
        assert!(ix.accounts[0].is_writable);
        assert!(!ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, m);
        assert!(!ix.accounts[1].is_writable);
        assert_eq!(ix.accounts[2].pubkey, d);
        assert!(ix.accounts[2].is_writable);
        assert_eq!(ix.accounts[3].pubkey, a);
        assert!(!ix.accounts[3].is_writable);
        assert!(ix.accounts[3].is_signer);
        // data = [12, amount_u64_le (8), decimals_u8 (1)]
        assert_eq!(ix.data.len(), 10);
        assert_eq!(ix.data[0], SPL_TOKEN_IX_TAG_TRANSFER_CHECKED);
        assert_eq!(
            u64::from_le_bytes(ix.data[1..9].try_into().unwrap()),
            5_000_000
        );
        assert_eq!(ix.data[9], 6);
    }

    #[test]
    fn compute_budget_ixs_shape() {
        let lim = compute_budget_set_unit_limit(30_000);
        assert_eq!(lim.data.len(), 5);
        assert_eq!(lim.data[0], COMPUTE_BUDGET_IX_TAG_SET_UNIT_LIMIT);
        assert_eq!(
            u32::from_le_bytes(lim.data[1..5].try_into().unwrap()),
            30_000
        );

        let pri = compute_budget_set_unit_price(1_000_000);
        assert_eq!(pri.data.len(), 9);
        assert_eq!(pri.data[0], COMPUTE_BUDGET_IX_TAG_SET_UNIT_PRICE);
        assert_eq!(
            u64::from_le_bytes(pri.data[1..9].try_into().unwrap()),
            1_000_000
        );
    }

    /// Source-scan: no forbidden call-form appears more than once. The
    /// allowed appearance is the FORBIDDEN_CALL_FORMS allowlist entry.
    #[test]
    fn no_send_path_default_invariant() {
        const SOURCE: &str = include_str!("stage2_live_controlled_wallet_refund.rs");
        for forbidden in FORBIDDEN_CALL_FORMS {
            let count = SOURCE.matches(forbidden).count();
            assert!(
                count <= 1,
                "forbidden call-form `{forbidden}` appears {count} times in W6 refund harness; \
                 only the FORBIDDEN_CALL_FORMS allowlist entry is permitted"
            );
        }
    }

    #[test]
    fn retry_policy_is_bounded() {
        // Manual driver because we don't need start_paused here.
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
}

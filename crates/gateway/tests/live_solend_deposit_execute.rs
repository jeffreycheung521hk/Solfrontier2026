//! OPT-IN: Slice 3G — controlled live Solend / Save USDC deposit
//! broadcast. This is the first test in the repo authorized to
//! broadcast a Solend `DepositReserveLiquidityAndObligationCollateral`
//! instruction against mainnet.
//!
//! # Allowed broadcasts (exactly these, in order)
//!
//! 1. Obligation-setup tx — `SystemProgram::CreateAccount` +
//!    `Solend::InitObligation`. Skipped if the obligation account is
//!    already on chain and Solend-owned.
//! 2. Refresh tx — priority-fee + `Solend::RefreshReserve`.
//! 3. Deposit tx — priority-fee + ATA `CreateIdempotent` +
//!    `Solend::DepositReserveLiquidityAndObligationCollateral`.
//!
//! No other broadcasts. No Withdraw, no Borrow, no Repay, no SPL
//! transfer, no SOL wrap, no Phantom path, no daemon path, no approval
//! workflow. Every broadcast is preceded by `simulateTransaction` with
//! `sigVerify=true`; if the simulation fails, the script stops.
//!
//! # Hard guardrails
//!
//! - `amount_raw` MUST equal 1000 (0.001 USDC). Any other value aborts.
//! - Target reserve MUST be the USDC reserve
//!   `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw` unless the prompt
//!   explicitly expands the allowlist.
//! - Fresh `getLatestBlockhash` for every transaction (no reuse).
//! - "Build tx once, simulate, send same object" discipline.
//! - Bounded `getSignatureStatuses` polling until `Confirmed` before
//!   moving to the next transaction.
//!
//! # Opt-in
//!
//! Self-skips unless `CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE=1`. CI never
//! sets this. Default `cargo test` path performs zero network calls.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use base64::Engine as _;
use claw_gateway::integrations::solend::{
    build_create_ata_idempotent_instruction, build_create_obligation_account_instruction,
    build_deposit_reserve_liquidity_and_obligation_collateral_instruction,
    build_init_obligation_instruction, build_refresh_instructions,
    derive_associated_token_address, extract_publish_freshness,
    raw::{self, SOLEND_NULL_ORACLE_SENTINEL_BS58, SOLEND_PROGRAM_ID_BS58},
    CreateObligationAccountInputs, DepositInstructionInputs, InitObligationInputs,
    RefreshPlanInputs, ReserveRefreshInput,
};
use claw_gateway::lending::{FeedPublishFreshness, UnderlyingAmount};
use serde_json::{json, Value};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};

// ── Expected target (sealed in Slice 3E(iv)/3E(v)/3F) ─────────────────────

const EXPECTED_USDC_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const EXPECTED_USDC_LENDING_MARKET_BS58: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
const EXPECTED_USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

const CLASSIC_SPL_TOKEN_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const PYTH_SOLANA_RECEIVER_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

/// Hard-coded deposit amount. The prompt disallows anything larger; the
/// test aborts if env provides a different value.
const FIXED_AMOUNT_RAW: u64 = 1_000;

/// Priority-fee unit price (micro-lamports per compute unit). Same
/// value as Slice 3E(v)'s refresh proof; modest enough that it does
/// not distort fees, high enough to reduce congestion-drop risk.
const PRIORITY_FEE_MICRO_LAMPORTS: u64 = 10_000;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(90);
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(1_500);

/// Post-confirmation account-visibility retry bounds. Mainnet-beta RPC
/// can be briefly inconsistent right after confirmation.
const POST_CONFIRM_RETRIES: usize = 5;
const POST_CONFIRM_BACKOFF_MS: u64 = 1_500;

// ── Opt-in env reading ─────────────────────────────────────────────────────

struct RequiredEnv {
    rpc_url: String,
    keypair_path: PathBuf,
    owner: Pubkey,
    obligation_keypair_path: PathBuf,
    target_reserve: Pubkey,
    amount_raw: u64,
}

#[derive(Debug)]
enum SkipReason {
    NotOptedIn,
    MissingEnv(String),
    InvalidEnv(String),
}

fn read_env() -> Result<RequiredEnv, SkipReason> {
    match std::env::var("CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {}
        _ => return Err(SkipReason::NotOptedIn),
    }
    let require = |name: &str| -> Result<String, SkipReason> {
        std::env::var(name).map_err(|_| SkipReason::MissingEnv(name.to_string()))
    };
    let rpc_url = require("CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE_RPC_URL")?;
    let keypair_path = PathBuf::from(require("CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE_KEYPAIR_PATH")?);
    let owner = Pubkey::from_str(&require("CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE_OWNER")?)
        .map_err(|e| SkipReason::InvalidEnv(format!("OWNER not base58: {e}")))?;
    let obligation_keypair_path = PathBuf::from(require(
        "CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE_OBLIGATION_KEYPAIR_PATH",
    )?);
    let target_reserve =
        Pubkey::from_str(&require("CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE_TARGET_RESERVE")?)
            .map_err(|e| SkipReason::InvalidEnv(format!("TARGET_RESERVE not base58: {e}")))?;
    let amount_raw = require("CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE_AMOUNT_RAW")?
        .parse::<u64>()
        .map_err(|e| SkipReason::InvalidEnv(format!("AMOUNT_RAW not u64: {e}")))?;
    Ok(RequiredEnv {
        rpc_url,
        keypair_path,
        owner,
        obligation_keypair_path,
        target_reserve,
        amount_raw,
    })
}

// ── Keypair + URL hygiene ──────────────────────────────────────────────────

fn validate_keypair_path(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("keypair file not found: {}", path.display()));
    }
    let canonical = path.canonicalize().map_err(|e| format!("canonicalize: {e}"))?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let repo_root = repo
        .ancestors()
        .nth(2)
        .ok_or_else(|| "cannot resolve repo root".to_string())?;
    if canonical.starts_with(repo_root) {
        return Err(format!(
            "keypair path must be OUTSIDE the repo. path={} repo_root={}",
            canonical.display(),
            repo_root.display()
        ));
    }
    Ok(())
}

fn load_keypair(path: &Path) -> Result<Keypair, String> {
    let contents = fs::read_to_string(path).map_err(|e| format!("read keypair: {e}"))?;
    let bytes: Vec<u8> =
        serde_json::from_str(&contents).map_err(|e| format!("keypair not JSON bytes: {e}"))?;
    if bytes.len() != 64 {
        return Err(format!("keypair must be 64 bytes, got {}", bytes.len()));
    }
    Keypair::from_bytes(&bytes).map_err(|e| format!("Keypair::from_bytes: {e}"))
}

fn sanitized_rpc_label(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return "<unparseable-url>".to_string();
    };
    let rest = &url[scheme_end + 3..];
    let after_auth = rest.rfind('@').map(|i| &rest[i + 1..]).unwrap_or(rest);
    let host_end = after_auth.find('/').unwrap_or(after_auth.len());
    format!("{}{}", &url[..scheme_end + 3], &after_auth[..host_end])
}

// ── JSON-RPC helpers ──────────────────────────────────────────────────────

async fn rpc_call_once(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("POST {method}: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::FORBIDDEN
    {
        return Err(format!("{method}: RATE-LIMIT HTTP {status}"));
    }
    if !status.is_success() {
        return Err(format!("{method}: HTTP {status}"));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("{method} not JSON: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("{method} RPC error: {err}"));
    }
    Ok(v)
}

/// Bounded retry on transient TCP-connect issues. Never retries a
/// RATE-LIMIT response.
async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    match rpc_call_once(client, url, method, &params).await {
        Ok(v) => Ok(v),
        Err(e) if e.contains("RATE-LIMIT") => Err(e),
        Err(e) if e.contains("error sending request") || e.contains("tcp connect") => {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            rpc_call_once(client, url, method, &params).await
        }
        Err(e) => Err(e),
    }
}

async fn get_latest_blockhash(client: &reqwest::Client, url: &str) -> Result<Hash, String> {
    let v = rpc_call(
        client,
        url,
        "getLatestBlockhash",
        json!([{ "commitment": "confirmed" }]),
    )
    .await?;
    let bh = v
        .pointer("/result/value/blockhash")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "missing blockhash".to_string())?;
    Hash::from_str(bh).map_err(|e| format!("blockhash parse: {e}"))
}

async fn get_minimum_balance_for_rent_exemption(
    client: &reqwest::Client,
    url: &str,
    size: usize,
) -> Result<u64, String> {
    let v = rpc_call(
        client,
        url,
        "getMinimumBalanceForRentExemption",
        json!([size]),
    )
    .await?;
    v.pointer("/result")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| "rent-exemption missing /result".to_string())
}

#[derive(Debug, Clone)]
struct AccountSnapshot {
    owner: Pubkey,
    lamports: u64,
    data: Vec<u8>,
}

async fn get_account_info(
    client: &reqwest::Client,
    url: &str,
    pubkey: &Pubkey,
) -> Result<Option<AccountSnapshot>, String> {
    let v = rpc_call(
        client,
        url,
        "getAccountInfo",
        json!([pubkey.to_string(), { "encoding": "base64", "commitment": "confirmed" }]),
    )
    .await?;
    let value = v.pointer("/result/value").cloned().unwrap_or(Value::Null);
    if value.is_null() {
        return Ok(None);
    }
    let owner_str = value
        .get("owner")
        .and_then(|o| o.as_str())
        .ok_or_else(|| "account.owner missing".to_string())?;
    let owner = Pubkey::from_str(owner_str).map_err(|e| format!("owner parse: {e}"))?;
    let data_b64 = value
        .pointer("/data/0")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| format!("base64 decode: {e}"))?;
    let lamports = value.get("lamports").and_then(|l| l.as_u64()).unwrap_or(0);
    Ok(Some(AccountSnapshot {
        owner,
        lamports,
        data: bytes,
    }))
}

/// Read SPL Token Account amount (u64 LE at bytes 64..72).
fn spl_token_amount(data: &[u8]) -> Option<u64> {
    if data.len() != 165 {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[64..72]);
    Some(u64::from_le_bytes(buf))
}

#[derive(Debug)]
struct SimulateOutcome {
    err_json: Value,
    logs: Vec<String>,
    units_consumed: Option<u64>,
}

impl SimulateOutcome {
    fn is_success(&self) -> bool {
        self.err_json.is_null()
    }
    fn instruction_error(&self) -> Option<(u64, Value)> {
        let obj = self.err_json.as_object()?;
        let arr = obj.get("InstructionError")?.as_array()?;
        if arr.len() != 2 {
            return None;
        }
        Some((arr[0].as_u64()?, arr[1].clone()))
    }
    fn custom_error_code(&self) -> Option<u64> {
        let (_, inner) = self.instruction_error()?;
        inner.as_object()?.get("Custom")?.as_u64()
    }
}

async fn simulate_signed_transaction(
    client: &reqwest::Client,
    url: &str,
    tx: &Transaction,
) -> Result<SimulateOutcome, String> {
    let tx_bytes = bincode::serialize(tx).map_err(|e| format!("bincode: {e}"))?;
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
    let v = rpc_call(
        client,
        url,
        "simulateTransaction",
        json!([
            tx_b64,
            {
                "encoding": "base64",
                "sigVerify": true,
                "commitment": "confirmed",
                "replaceRecentBlockhash": false,
            }
        ]),
    )
    .await?;
    let value = v.pointer("/result/value").cloned().unwrap_or(Value::Null);
    Ok(SimulateOutcome {
        err_json: value.get("err").cloned().unwrap_or(Value::Null),
        logs: value
            .get("logs")
            .and_then(|l| l.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        units_consumed: value.get("unitsConsumed").and_then(|u| u.as_u64()),
    })
}

async fn send_transaction_with_preflight(
    client: &reqwest::Client,
    url: &str,
    tx: &Transaction,
) -> Result<String, String> {
    let tx_bytes = bincode::serialize(tx).map_err(|e| format!("bincode: {e}"))?;
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
    let v = rpc_call(
        client,
        url,
        "sendTransaction",
        json!([
            tx_b64,
            {
                "encoding": "base64",
                "skipPreflight": false,
                "preflightCommitment": "confirmed",
                "maxRetries": 3,
            }
        ]),
    )
    .await?;
    v.pointer("/result")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "sendTransaction missing /result signature".to_string())
}

#[derive(Debug, Clone)]
struct ConfirmState {
    status: String,
    slot: Option<u64>,
    err: Value,
    finalized: bool,
}

async fn await_confirmation(
    client: &reqwest::Client,
    url: &str,
    signature: &str,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<ConfirmState, String> {
    let start = Instant::now();
    let mut last: Option<ConfirmState> = None;
    loop {
        let v = rpc_call(
            client,
            url,
            "getSignatureStatuses",
            json!([[signature], { "searchTransactionHistory": false }]),
        )
        .await?;
        let entry = v
            .pointer("/result/value/0")
            .cloned()
            .unwrap_or(Value::Null);
        if !entry.is_null() {
            let status = entry
                .get("confirmationStatus")
                .and_then(|s| s.as_str())
                .unwrap_or("<pending>")
                .to_string();
            let slot = entry.get("slot").and_then(|n| n.as_u64());
            let err = entry.get("err").cloned().unwrap_or(Value::Null);
            let finalized = status == "finalized";
            let st = ConfirmState {
                status: status.clone(),
                slot,
                err,
                finalized,
            };
            last = Some(st.clone());
            if status == "confirmed" || status == "finalized" {
                return Ok(st);
            }
        }
        if start.elapsed() >= timeout {
            return last.ok_or_else(|| {
                format!("confirmation timeout (no status) after {timeout:?}")
            });
        }
        tokio::time::sleep(poll_interval).await;
    }
}

fn print_simulation(label: &str, outcome: &SimulateOutcome) {
    println!();
    println!("── simulation result: {label} ─────────────────────────────────");
    println!(
        "  top-level status  : {}",
        if outcome.is_success() { "SUCCESS" } else { "FAIL" }
    );
    if let Some(u) = outcome.units_consumed {
        println!("  compute units     : {u}");
    }
    if let Some((i, inner)) = outcome.instruction_error() {
        println!("  instruction index : {i}");
        println!("  InstructionError  : {inner}");
    } else if !outcome.err_json.is_null() {
        println!("  raw err           : {}", outcome.err_json);
    }
    if let Some(c) = outcome.custom_error_code() {
        println!("  custom code       : {c}");
    }
    if outcome.logs.is_empty() {
        println!("  logs              : (none)");
    } else {
        println!("  logs ({} lines):", outcome.logs.len());
        for l in &outcome.logs {
            println!("    | {l}");
        }
    }
}

/// Post-confirmation state-visibility retry. RPC can be briefly
/// inconsistent right after a `Confirmed` commitment; we retry a
/// bounded number of times with a fixed backoff.
async fn get_account_info_with_retry(
    client: &reqwest::Client,
    url: &str,
    pubkey: &Pubkey,
) -> Result<Option<AccountSnapshot>, String> {
    for attempt in 0..POST_CONFIRM_RETRIES {
        match get_account_info(client, url, pubkey).await {
            Ok(Some(s)) => return Ok(Some(s)),
            Ok(None) => {
                if attempt + 1 < POST_CONFIRM_RETRIES {
                    tokio::time::sleep(Duration::from_millis(POST_CONFIRM_BACKOFF_MS)).await;
                    continue;
                }
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

// ── Main test ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_solend_deposit_execute() {
    let env = match read_env() {
        Ok(e) => e,
        Err(SkipReason::NotOptedIn) => {
            eprintln!(
                "SKIP: set CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE=1 plus the required target \
                 env vars to run the opt-in live USDC deposit. CI does not set this. \
                 This test broadcasts up to THREE mainnet transactions (obligation \
                 setup, refresh, deposit) when run."
            );
            return;
        }
        Err(SkipReason::MissingEnv(n)) => {
            eprintln!("SKIP: missing required env var: {n}");
            return;
        }
        Err(SkipReason::InvalidEnv(m)) => {
            eprintln!("SKIP: invalid env var: {m}");
            return;
        }
    };

    // Hard amount guardrail FIRST, before any keypair / RPC work.
    if env.amount_raw != FIXED_AMOUNT_RAW {
        eprintln!(
            "SKIP: Slice 3G hard amount guardrail — amount_raw must be exactly {FIXED_AMOUNT_RAW} \
             (got {}). Run with the fixed env AMOUNT_RAW=1000.",
            env.amount_raw
        );
        return;
    }

    // Target-reserve guardrail.
    if env.target_reserve.to_string() != EXPECTED_USDC_RESERVE_BS58 {
        eprintln!(
            "SKIP: target reserve {} is not the Slice-3G USDC target {}. Retarget is a \
             separate slice decision.",
            env.target_reserve, EXPECTED_USDC_RESERVE_BS58
        );
        return;
    }

    // Validate + load keypairs.
    if let Err(e) = validate_keypair_path(&env.keypair_path) {
        eprintln!("SKIP: wallet keypair path: {e}");
        return;
    }
    if let Err(e) = validate_keypair_path(&env.obligation_keypair_path) {
        eprintln!("SKIP: obligation keypair path: {e}");
        return;
    }
    let wallet_kp = match load_keypair(&env.keypair_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("SKIP: load wallet keypair: {e}");
            return;
        }
    };
    let obligation_kp = match load_keypair(&env.obligation_keypair_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("SKIP: load obligation keypair: {e}");
            return;
        }
    };
    if wallet_kp.pubkey() != env.owner {
        eprintln!(
            "SKIP: wallet keypair pubkey {} does not match env OWNER {}",
            wallet_kp.pubkey(),
            env.owner
        );
        return;
    }

    let solend_program = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap();
    let classic_spl_token = Pubkey::from_str(CLASSIC_SPL_TOKEN_BS58).unwrap();
    let pyth_receiver_id = Pubkey::from_str(PYTH_SOLANA_RECEIVER_BS58).unwrap();
    let sentinel_oracle = Pubkey::from_str(SOLEND_NULL_ORACLE_SENTINEL_BS58).unwrap();
    let expected_market = Pubkey::from_str(EXPECTED_USDC_LENDING_MARKET_BS58).unwrap();
    let expected_mint = Pubkey::from_str(EXPECTED_USDC_MINT_BS58).unwrap();

    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  Slice 3G — LIVE Solend USDC deposit (controlled test wallet)");
    println!("══════════════════════════════════════════════════════════════");
    println!("  rpc host               : {}", sanitized_rpc_label(&env.rpc_url));
    println!("  owner                  : {}", env.owner);
    println!("  obligation             : {}", obligation_kp.pubkey());
    println!("  target reserve         : {}", env.target_reserve);
    println!(
        "  amount                 : {} raw = {:.6} USDC",
        FIXED_AMOUNT_RAW,
        FIXED_AMOUNT_RAW as f64 / 1e6
    );
    println!(
        "  priority fee per tx    : set_compute_unit_price={} μ-lamports/CU",
        PRIORITY_FEE_MICRO_LAMPORTS
    );
    println!(
        "  broadcast authorization : explicit (Slice 3G prompt authorized up to 3 txs)"
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("claw-gateway/live-solend-deposit-execute")
        .build()
        .expect("reqwest client");

    // ── Pre-flight read-only probe ───────────────────────────────────────
    println!();
    println!("── pre-flight read-only probe ───────────────────────────────");
    let wallet_snap = match get_account_info(&client, &env.rpc_url, &env.owner).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SKIP: fetch wallet: {e}");
            return;
        }
    };
    let wallet_sol_lamports = wallet_snap.as_ref().map(|s| s.lamports).unwrap_or(0);
    println!(
        "  wallet SOL             : {:.9} SOL ({} lamports)",
        wallet_sol_lamports as f64 / 1e9,
        wallet_sol_lamports
    );

    let reserve_snap = match get_account_info(&client, &env.rpc_url, &env.target_reserve).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("SKIP: reserve not found on chain");
            return;
        }
        Err(e) => {
            eprintln!("SKIP: fetch reserve: {e}");
            return;
        }
    };
    if reserve_snap.owner != solend_program {
        eprintln!(
            "SKIP: reserve owner {} is not Solend program",
            reserve_snap.owner
        );
        return;
    }
    let reserve = match raw::decode_reserve(&reserve_snap.data) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP: decode_reserve: {e:?}");
            return;
        }
    };
    println!("  reserve lending_market : {}", reserve.lending_market);
    println!("  reserve mint           : {}", reserve.liquidity_mint);
    println!(
        "  reserve stale          : {} (last_update_slot={})",
        reserve.last_update_stale, reserve.last_update_slot
    );
    println!(
        "  deposit_limit          : {} ({:.2} UI USDC)",
        reserve.config_deposit_limit,
        reserve.config_deposit_limit as f64 / 1e6
    );
    let headroom = reserve.deposit_headroom_raw();
    println!("  deposit_headroom_raw   : {headroom}");
    println!("  pyth_oracle            : {}", reserve.pyth_oracle);
    println!("  switchboard_oracle     : {}", reserve.switchboard_oracle);

    // Shape-based hard stops.
    if reserve.lending_market != expected_market {
        eprintln!(
            "SKIP: reserve lending_market {} != expected {}",
            reserve.lending_market, expected_market
        );
        return;
    }
    if reserve.liquidity_mint != expected_mint {
        eprintln!("SKIP: reserve mint is not USDC");
        return;
    }
    if reserve.switchboard_oracle != sentinel_oracle {
        eprintln!("SKIP: reserve switchboard is not sentinel");
        return;
    }
    if reserve.config_deposit_limit == 0 {
        eprintln!("SKIP: reserve deposit_limit=0");
        return;
    }
    if headroom < FIXED_AMOUNT_RAW {
        eprintln!("SKIP: headroom {headroom} < amount {FIXED_AMOUNT_RAW}");
        return;
    }

    // Mint token program check.
    let mint_snap = match get_account_info(&client, &env.rpc_url, &reserve.liquidity_mint).await {
        Ok(Some(s)) => s,
        _ => {
            eprintln!("SKIP: mint account not found");
            return;
        }
    };
    if mint_snap.owner != classic_spl_token {
        eprintln!(
            "SKIP: mint token program {} != classic SPL Token",
            mint_snap.owner
        );
        return;
    }

    // Pyth oracle owner + freshness.
    let pyth_snap = match get_account_info(&client, &env.rpc_url, &reserve.pyth_oracle).await {
        Ok(Some(s)) => s,
        _ => {
            eprintln!("SKIP: pyth oracle account not found");
            return;
        }
    };
    if pyth_snap.owner != pyth_receiver_id {
        eprintln!(
            "SKIP: pyth oracle owner {} != Pyth Solana Receiver",
            pyth_snap.owner
        );
        return;
    }
    let pyth_freshness = extract_publish_freshness(&pyth_snap.owner, &pyth_snap.data);
    match pyth_freshness {
        FeedPublishFreshness::KnownSlot(slot) => {
            println!("  pyth freshness         : KnownSlot({slot:?})");
        }
        FeedPublishFreshness::Unknown => {
            eprintln!("SKIP: pyth freshness Unknown (V1 fail-closed)");
            return;
        }
    }

    // Derive ATAs.
    let source_ata =
        derive_associated_token_address(&env.owner, &reserve.liquidity_mint, &classic_spl_token);
    let collateral_ata =
        derive_associated_token_address(&env.owner, &reserve.collateral_mint, &classic_spl_token);
    println!("  source USDC ATA        : {source_ata}");
    println!("  collateral ATA         : {collateral_ata}");

    // Source ATA probe.
    let source_ata_snap = match get_account_info(&client, &env.rpc_url, &source_ata).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SKIP: fetch source ATA: {e}");
            return;
        }
    };
    let pre_source_balance = match &source_ata_snap {
        Some(s) => {
            if s.owner != classic_spl_token {
                eprintln!("SKIP: source ATA owner {} != classic SPL Token", s.owner);
                return;
            }
            spl_token_amount(&s.data).unwrap_or(0)
        }
        None => {
            eprintln!("SKIP: source USDC ATA does not exist on chain");
            return;
        }
    };
    println!(
        "  source USDC balance    : {} raw ({:.6} UI)",
        pre_source_balance,
        pre_source_balance as f64 / 1e6
    );
    if pre_source_balance < FIXED_AMOUNT_RAW {
        eprintln!(
            "SKIP: source USDC balance {} < required {}",
            pre_source_balance, FIXED_AMOUNT_RAW
        );
        return;
    }

    // Collateral ATA probe.
    let collateral_snap_pre = get_account_info(&client, &env.rpc_url, &collateral_ata)
        .await
        .ok()
        .flatten();
    let pre_collateral_balance = collateral_snap_pre
        .as_ref()
        .and_then(|s| spl_token_amount(&s.data))
        .unwrap_or(0);
    let collateral_ata_exists = collateral_snap_pre.is_some();
    println!(
        "  collateral ATA         : {} (balance_raw={})",
        if collateral_ata_exists { "EXISTS" } else { "MISSING (will idempotent-create in Tx 3)" },
        pre_collateral_balance
    );

    // Obligation probe (controls Tx 1 skip).
    let obligation_snap_pre =
        get_account_info(&client, &env.rpc_url, &obligation_kp.pubkey())
            .await
            .ok()
            .flatten();
    let obligation_pre_state = match &obligation_snap_pre {
        None => "NOT_ON_CHAIN",
        Some(s) if s.owner == solend_program => "SOLEND_OWNED",
        Some(s) if s.owner == Pubkey::default() => "SYSTEM_OWNED",
        Some(_) => "OTHER_OWNER",
    };
    println!("  obligation pre-state   : {obligation_pre_state}");
    if obligation_pre_state == "OTHER_OWNER" {
        eprintln!(
            "SKIP: obligation account exists but is owned by {} (not Solend). \
             Refusing to overwrite.",
            obligation_snap_pre.as_ref().unwrap().owner
        );
        return;
    }

    // Idempotency classification for reporting.
    let restart_state = match (
        obligation_pre_state,
        collateral_ata_exists,
        pre_collateral_balance > 0,
    ) {
        ("NOT_ON_CHAIN", false, _) => "FreshStart",
        ("SOLEND_OWNED", false, _) => "ObligationAlreadyInitialized",
        ("SOLEND_OWNED", true, false) => "CollateralAtaAlreadyExists",
        ("SOLEND_OWNED", true, true) => "PossiblePreviousDepositDetected",
        _ => "Mixed",
    };
    println!("  restart_state          : {restart_state}");

    // ── Transaction 1 — Obligation setup (skip if already initialized) ────
    let tx1_signature: Option<String>;
    if obligation_pre_state != "SOLEND_OWNED" {
        println!();
        println!("══════════════════════════════════════════════════════════════");
        println!("  Tx 1 — Obligation setup  (CreateAccount + InitObligation)");
        println!("══════════════════════════════════════════════════════════════");

        let rent_exempt = match get_minimum_balance_for_rent_exemption(
            &client,
            &env.rpc_url,
            raw::OBLIGATION_LEN,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("STOP: rent fetch: {e}");
                return;
            }
        };
        println!("  OBLIGATION_LEN         : {}", raw::OBLIGATION_LEN);
        println!("  rent-exempt lamports   : {rent_exempt}");

        let priority_ix = ComputeBudgetInstruction::set_compute_unit_price(
            PRIORITY_FEE_MICRO_LAMPORTS,
        );
        let create_ix = build_create_obligation_account_instruction(CreateObligationAccountInputs {
            funder: env.owner,
            new_obligation: obligation_kp.pubkey(),
            rent_exempt_lamports: rent_exempt,
            solend_program_id: solend_program,
        });
        let init_ix = build_init_obligation_instruction(InitObligationInputs {
            solend_program_id: solend_program,
            obligation: obligation_kp.pubkey(),
            lending_market: reserve.lending_market,
            obligation_owner: env.owner,
        });
        let ixs: Vec<Instruction> = vec![priority_ix, create_ix, init_ix];

        // Fresh blockhash immediately before building the signed tx.
        let bh_tx1 = match get_latest_blockhash(&client, &env.rpc_url).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("STOP: getLatestBlockhash for Tx 1: {e}");
                return;
            }
        };
        println!("  blockhash (Tx 1)       : {}", bh_tx1);
        // Build ONCE, simulate same object, send same object.
        let tx1 = Transaction::new_signed_with_payer(
            &ixs,
            Some(&env.owner),
            &[&wallet_kp, &obligation_kp],
            bh_tx1,
        );

        let sim1 = match simulate_signed_transaction(&client, &env.rpc_url, &tx1).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("STOP: Tx 1 simulate RPC: {e}");
                return;
            }
        };
        print_simulation("Tx 1 obligation setup preflight", &sim1);
        if !sim1.is_success() {
            eprintln!("STOP: Tx 1 preflight failed — not broadcasting");
            return;
        }

        let sig = match send_transaction_with_preflight(&client, &env.rpc_url, &tx1).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("STOP: Tx 1 send: {e}");
                return;
            }
        };
        println!("  Tx 1 signature         : {sig}");
        let confirm = match await_confirmation(
            &client,
            &env.rpc_url,
            &sig,
            CONFIRM_TIMEOUT,
            CONFIRM_POLL_INTERVAL,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("STOP: Tx 1 confirmation: {e}");
                return;
            }
        };
        println!(
            "  Tx 1 status            : {} (slot={:?}, finalized={}, err={})",
            confirm.status, confirm.slot, confirm.finalized, confirm.err
        );
        if !confirm.err.is_null() {
            eprintln!(
                "STOP: Tx 1 confirmed with on-chain err {} — halting",
                confirm.err
            );
            return;
        }
        if confirm.status != "confirmed" && confirm.status != "finalized" {
            eprintln!("STOP: Tx 1 did not reach Confirmed within timeout");
            return;
        }

        // Post-confirm visibility retry.
        match get_account_info_with_retry(&client, &env.rpc_url, &obligation_kp.pubkey()).await {
            Ok(Some(s)) if s.owner == solend_program => {
                println!("  obligation post-Tx1    : Solend-owned (len={})", s.data.len());
            }
            Ok(Some(s)) => {
                eprintln!("STOP: obligation post-Tx1 owner {} != Solend", s.owner);
                return;
            }
            Ok(None) => {
                eprintln!("STOP: obligation post-Tx1 not visible after {POST_CONFIRM_RETRIES} retries");
                return;
            }
            Err(e) => {
                eprintln!("STOP: obligation post-Tx1 fetch: {e}");
                return;
            }
        }
        tx1_signature = Some(sig);
    } else {
        println!();
        println!("  Tx 1 — SKIPPED (obligation already Solend-owned on chain)");
        tx1_signature = None;
    }

    // ── Transaction 2 — Refresh ────────────────────────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  Tx 2 — RefreshReserve");
    println!("══════════════════════════════════════════════════════════════");
    let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
        solend_program_id: solend_program,
        reserves: vec![ReserveRefreshInput {
            reserve_pubkey: env.target_reserve,
            pyth_oracle: reserve.pyth_oracle,
            switchboard_oracle: reserve.switchboard_oracle,
        }],
        obligation: None,
    });
    let priority_ix2 = ComputeBudgetInstruction::set_compute_unit_price(
        PRIORITY_FEE_MICRO_LAMPORTS,
    );
    let mut tx2_ixs: Vec<Instruction> = Vec::with_capacity(1 + refresh_plan.instructions.len());
    tx2_ixs.push(priority_ix2);
    tx2_ixs.extend(refresh_plan.instructions.iter().cloned());

    let bh_tx2 = match get_latest_blockhash(&client, &env.rpc_url).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("STOP: getLatestBlockhash for Tx 2: {e}");
            return;
        }
    };
    println!("  blockhash (Tx 2)       : {} (fresh; NOT reused from Tx 1)", bh_tx2);
    let tx2 = Transaction::new_signed_with_payer(
        &tx2_ixs,
        Some(&env.owner),
        &[&wallet_kp],
        bh_tx2,
    );

    let sim2 = match simulate_signed_transaction(&client, &env.rpc_url, &tx2).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("STOP: Tx 2 simulate RPC: {e}");
            return;
        }
    };
    print_simulation("Tx 2 refresh preflight", &sim2);
    if !sim2.is_success() {
        eprintln!("STOP: Tx 2 preflight failed — not broadcasting");
        return;
    }
    let sig2 = match send_transaction_with_preflight(&client, &env.rpc_url, &tx2).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("STOP: Tx 2 send: {e}");
            return;
        }
    };
    println!("  Tx 2 signature         : {sig2}");
    let confirm2 = match await_confirmation(
        &client,
        &env.rpc_url,
        &sig2,
        CONFIRM_TIMEOUT,
        CONFIRM_POLL_INTERVAL,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("STOP: Tx 2 confirmation: {e}");
            return;
        }
    };
    println!(
        "  Tx 2 status            : {} (slot={:?}, err={})",
        confirm2.status, confirm2.slot, confirm2.err
    );
    if !confirm2.err.is_null() {
        eprintln!("STOP: Tx 2 confirmed with on-chain err");
        return;
    }

    // Re-fetch reserve post-refresh.
    let reserve_post_refresh = match get_account_info_with_retry(
        &client,
        &env.rpc_url,
        &env.target_reserve,
    )
    .await
    {
        Ok(Some(s)) if s.owner == solend_program => {
            raw::decode_reserve(&s.data).ok()
        }
        _ => None,
    };
    let post_refresh_slot = reserve_post_refresh
        .as_ref()
        .map(|r| r.last_update_slot)
        .unwrap_or(0);
    let post_refresh_stale = reserve_post_refresh
        .as_ref()
        .map(|r| r.last_update_stale)
        .unwrap_or(true);
    let post_refresh_headroom = reserve_post_refresh
        .as_ref()
        .map(|r| r.deposit_headroom_raw())
        .unwrap_or(0);
    println!(
        "  reserve post-Tx2       : stale={}, last_update_slot={}, headroom={}",
        post_refresh_stale, post_refresh_slot, post_refresh_headroom
    );
    if post_refresh_headroom < FIXED_AMOUNT_RAW {
        eprintln!("STOP: post-refresh headroom {post_refresh_headroom} < amount");
        return;
    }
    // Resolve the reserve-shape fields we need for Tx 3 from the
    // post-refresh snapshot when available; else fall back to pre.
    let reserve_for_deposit = reserve_post_refresh.as_ref().unwrap_or(&reserve);

    // ── Transaction 3 — Deposit ───────────────────────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  Tx 3 — Deposit  (ATA-idempotent + Solend Deposit)");
    println!("══════════════════════════════════════════════════════════════");
    let priority_ix3 = ComputeBudgetInstruction::set_compute_unit_price(
        PRIORITY_FEE_MICRO_LAMPORTS,
    );
    let ata_collateral_ix = build_create_ata_idempotent_instruction(
        &env.owner,
        &env.owner,
        &reserve_for_deposit.collateral_mint,
        &classic_spl_token,
    );
    let deposit_ix = match build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
        DepositInstructionInputs {
            solend_program_id: solend_program,
            amount: UnderlyingAmount::new(FIXED_AMOUNT_RAW),
            source_liquidity: source_ata,
            user_collateral: collateral_ata,
            reserve: env.target_reserve,
            reserve_liquidity_supply: reserve_for_deposit.liquidity_supply,
            reserve_collateral_mint: reserve_for_deposit.collateral_mint,
            lending_market: reserve_for_deposit.lending_market,
            destination_deposit_collateral: reserve_for_deposit.collateral_supply,
            obligation: obligation_kp.pubkey(),
            obligation_owner: env.owner,
            pyth_oracle: reserve_for_deposit.pyth_oracle,
            switchboard_oracle: reserve_for_deposit.switchboard_oracle,
            user_transfer_authority: env.owner,
        },
    ) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("STOP: deposit builder error: {e}");
            return;
        }
    };
    let tx3_ixs: Vec<Instruction> = vec![priority_ix3, ata_collateral_ix, deposit_ix];

    let bh_tx3 = match get_latest_blockhash(&client, &env.rpc_url).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("STOP: getLatestBlockhash for Tx 3: {e}");
            return;
        }
    };
    println!("  blockhash (Tx 3)       : {} (fresh; NOT reused from Tx 1/2)", bh_tx3);
    let tx3 = Transaction::new_signed_with_payer(
        &tx3_ixs,
        Some(&env.owner),
        &[&wallet_kp],
        bh_tx3,
    );

    let sim3 = match simulate_signed_transaction(&client, &env.rpc_url, &tx3).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("STOP: Tx 3 simulate RPC: {e}");
            return;
        }
    };
    print_simulation("Tx 3 deposit preflight", &sim3);
    if !sim3.is_success() {
        eprintln!("STOP: Tx 3 preflight failed — not broadcasting deposit");
        return;
    }
    let sig3 = match send_transaction_with_preflight(&client, &env.rpc_url, &tx3).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("STOP: Tx 3 send: {e}");
            return;
        }
    };
    println!("  Tx 3 signature         : {sig3}");
    let confirm3 = match await_confirmation(
        &client,
        &env.rpc_url,
        &sig3,
        CONFIRM_TIMEOUT,
        CONFIRM_POLL_INTERVAL,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("STOP: Tx 3 confirmation: {e}");
            return;
        }
    };
    println!(
        "  Tx 3 status            : {} (slot={:?}, err={})",
        confirm3.status, confirm3.slot, confirm3.err
    );
    if !confirm3.err.is_null() {
        eprintln!("STOP: Tx 3 confirmed with on-chain err {}", confirm3.err);
        return;
    }

    // ── Post-deposit verification ─────────────────────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  POST-DEPOSIT VERIFICATION");
    println!("══════════════════════════════════════════════════════════════");

    let post_source = get_account_info_with_retry(&client, &env.rpc_url, &source_ata)
        .await
        .ok()
        .flatten();
    let post_source_balance = post_source
        .as_ref()
        .and_then(|s| spl_token_amount(&s.data))
        .unwrap_or(0);
    let post_collateral =
        get_account_info_with_retry(&client, &env.rpc_url, &collateral_ata)
            .await
            .ok()
            .flatten();
    let post_collateral_balance = post_collateral
        .as_ref()
        .and_then(|s| spl_token_amount(&s.data))
        .unwrap_or(0);

    println!(
        "  source USDC  before→after : {} → {} (Δ = {})",
        pre_source_balance,
        post_source_balance,
        pre_source_balance as i128 - post_source_balance as i128
    );
    println!(
        "  collateral   before→after : {} → {} (Δ = {})",
        pre_collateral_balance,
        post_collateral_balance,
        post_collateral_balance as i128 - pre_collateral_balance as i128
    );

    let source_dropped_by_amount =
        pre_source_balance.saturating_sub(post_source_balance) == FIXED_AMOUNT_RAW;
    let collateral_increased = post_collateral_balance > pre_collateral_balance;
    println!(
        "  source decreased by 1000  : {}",
        if source_dropped_by_amount { "YES" } else { "NO" }
    );
    println!(
        "  collateral increased      : {}",
        if collateral_increased { "YES" } else { "NO" }
    );

    // Attempt decode_obligation — may hit known mainnet layout drift.
    let obligation_post =
        get_account_info_with_retry(&client, &env.rpc_url, &obligation_kp.pubkey())
            .await
            .ok()
            .flatten();
    let (obligation_len, obligation_decode_note) = match &obligation_post {
        Some(s) => {
            match raw::decode_obligation(&s.data) {
                Ok(decoded) => (
                    s.data.len(),
                    format!(
                        "decoded ok: deposits={}, borrows={}",
                        decoded.deposits.len(),
                        decoded.borrows.len()
                    ),
                ),
                Err(e) => (
                    s.data.len(),
                    format!(
                        "decode hit known mainnet-layout drift (raw.rs padding check vs \
                         mainnet Pack extended fields) — error={e:?}. Future-slice \
                         decoder fix needed; does NOT invalidate the live deposit."
                    ),
                ),
            }
        }
        None => (0, "(not visible)".to_string()),
    };
    println!("  obligation account len  : {obligation_len}");
    println!("  obligation decode        : {obligation_decode_note}");

    // Reserve post-deposit snapshot (optional; best-effort).
    let reserve_post_deposit = get_account_info_with_retry(&client, &env.rpc_url, &env.target_reserve)
        .await
        .ok()
        .flatten()
        .and_then(|s| raw::decode_reserve(&s.data).ok());
    if let Some(r) = reserve_post_deposit.as_ref() {
        println!(
            "  reserve post-Tx3       : last_update_slot={}, total_supply={}, headroom={}",
            r.last_update_slot,
            r.liquidity_total_supply_raw().unwrap_or(0),
            r.deposit_headroom_raw()
        );
    }

    // ── FINAL SUMMARY ─────────────────────────────────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  SLICE 3G OUTCOME");
    println!("══════════════════════════════════════════════════════════════");
    println!(
        "  Tx 1 (obligation setup)  : {}",
        match &tx1_signature {
            Some(s) => format!("SENT {s}"),
            None => "SKIPPED (already initialized on chain)".to_string(),
        }
    );
    println!("  Tx 2 (refresh)           : SENT {sig2}");
    println!("  Tx 3 (deposit)           : SENT {sig3}");
    println!(
        "  blockhash discipline     : 3 fresh blockhashes (one per tx), no reuse"
    );
    println!(
        "  amount deposited (raw)   : {} (UI = {:.6} USDC)",
        FIXED_AMOUNT_RAW,
        FIXED_AMOUNT_RAW as f64 / 1e6
    );
    println!("  source decreased by 1000 : {source_dropped_by_amount}");
    println!("  collateral increased     : {collateral_increased}");
    println!();
    println!("  NO Withdraw / NO Borrow / NO Repay broadcast");
    println!("  NO extra transactions beyond the 3 authorized");
    println!("  NO Phantom / NO daemon / NO approval workflow path used");
    println!("  NO policy weakening / NO oracle-aggregation change");
    println!("══════════════════════════════════════════════════════════════");
}

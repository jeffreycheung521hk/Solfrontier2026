//! OPT-IN: Slice 3E(ii) — controlled live RefreshReserve proof.
//!
//! ## What this test IS
//!
//! A **read → simulate → optionally broadcast → confirm → re-fetch**
//! check for exactly one standalone `RefreshReserve` transaction on
//! mainnet. The goal is to prove whether the selected wSOL target
//! reserve can clear its stale / `Custom(14)` MathOverflow wall via a
//! real on-chain refresh, and then — if and only if the refresh is
//! confirmed — re-simulate the first-deposit bundle to observe the
//! next real blocker.
//!
//! ## Permission boundary
//!
//! The ONLY transaction this test may broadcast is the single
//! `RefreshReserve` ix for the env-specified target reserve. The test
//! never broadcasts:
//!
//! - deposit
//! - InitObligation
//! - ATA creation
//! - source-liquidity transfer
//! - SOL wrap / `sync_native`
//!
//! `simulateTransaction` is called before every broadcast. If the
//! preflight simulation fails, the test stops — it does NOT force-send
//! and does NOT use `skip_preflight`.
//!
//! ## Opt-in
//!
//! Self-skips unless every required env var is set. CI never sets
//! these. The default `cargo test -p claw-gateway --test
//! live_solend_refresh_proof` invocation performs no network calls and
//! prints `SKIP: …`.

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
    raw::{self, SOLEND_PROGRAM_ID_BS58},
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

const DEFAULT_AMOUNT_RAW: u64 = 1_000;

// Classic SPL Token program id. Both the ATA derivation and the
// hard-stop on the target reserve's liquidity-mint token program check
// against this pubkey — Slice 3E V1 scope is single-token-program only.
const CLASSIC_SPL_TOKEN_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// Pyth Solana Receiver program id. Used to hard-stop on the reserve's
// pyth oracle account owner.
const PYTH_SOLANA_RECEIVER_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

/// Slice 3E(v): modest priority-fee ceiling to reduce mainnet-congestion
/// drop risk on the one authorized RefreshReserve broadcast. This is a
/// ComputeBudget `set_compute_unit_price` value expressed in micro-
/// lamports per compute unit.
const REFRESH_PRIORITY_FEE_MICRO_LAMPORTS: u64 = 10_000;

// Confirmation-polling bounds. Keep conservative: mainnet-beta often
// reaches Confirmed within a few seconds.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(90);
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(1_500);

// ── Opt-in env reading ─────────────────────────────────────────────────────

struct RequiredEnv {
    rpc_url: String,
    keypair_path: PathBuf,
    owner: Pubkey,
    target_reserve: Pubkey,
    run_post_refresh_deposit_sim: bool,
    obligation_keypair_path: Option<PathBuf>,
    amount_raw: u64,
}

#[derive(Debug)]
enum SkipReason {
    NotOptedIn,
    MissingEnv(String),
    InvalidEnv(String),
}

fn read_env() -> Result<RequiredEnv, SkipReason> {
    match std::env::var("CLAW_LIVE_SOLEND_REFRESH_PROOF") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {}
        _ => return Err(SkipReason::NotOptedIn),
    }
    let require = |name: &str| -> Result<String, SkipReason> {
        std::env::var(name).map_err(|_| SkipReason::MissingEnv(name.to_string()))
    };
    let rpc_url = require("CLAW_LIVE_SOLEND_REFRESH_PROOF_RPC_URL")?;
    let keypair_path = PathBuf::from(require("CLAW_LIVE_SOLEND_REFRESH_PROOF_KEYPAIR_PATH")?);
    let owner = Pubkey::from_str(&require("CLAW_LIVE_SOLEND_REFRESH_PROOF_OWNER")?)
        .map_err(|e| SkipReason::InvalidEnv(format!("OWNER not base58: {e}")))?;
    let target_reserve =
        Pubkey::from_str(&require("CLAW_LIVE_SOLEND_REFRESH_PROOF_TARGET_RESERVE")?)
            .map_err(|e| SkipReason::InvalidEnv(format!("TARGET_RESERVE not base58: {e}")))?;
    let run_post_refresh_deposit_sim = matches!(
        std::env::var("CLAW_LIVE_SOLEND_REFRESH_PROOF_RUN_POST_REFRESH_DEPOSIT_SIM").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let obligation_keypair_path = std::env::var(
        "CLAW_LIVE_SOLEND_REFRESH_PROOF_OBLIGATION_KEYPAIR_PATH",
    )
    .ok()
    .filter(|s| !s.is_empty())
    .map(PathBuf::from);
    let amount_raw = std::env::var("CLAW_LIVE_SOLEND_REFRESH_PROOF_AMOUNT_RAW")
        .ok()
        .map(|s| {
            s.parse::<u64>()
                .map_err(|e| SkipReason::InvalidEnv(format!("AMOUNT_RAW not u64: {e}")))
        })
        .transpose()?
        .unwrap_or(DEFAULT_AMOUNT_RAW);
    Ok(RequiredEnv {
        rpc_url,
        keypair_path,
        owner,
        target_reserve,
        run_post_refresh_deposit_sim,
        obligation_keypair_path,
        amount_raw,
    })
}

// ── Keypair + URL hygiene ──────────────────────────────────────────────────

/// Reject keypair paths that appear to live inside this repo.
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
    let contents = fs::read_to_string(path).map_err(|e| format!("read keypair file: {e}"))?;
    let bytes: Vec<u8> = serde_json::from_str(&contents)
        .map_err(|e| format!("keypair file is not a JSON byte array: {e}"))?;
    if bytes.len() != 64 {
        return Err(format!(
            "keypair file must contain 64 bytes, got {}",
            bytes.len()
        ));
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

// ── JSON-RPC helpers (test-local) ─────────────────────────────────────────

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("POST {method}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{method}: HTTP {}", resp.status()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("{method} response not JSON: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("{method} RPC error: {err}"));
    }
    Ok(v)
}

async fn get_latest_blockhash(client: &reqwest::Client, url: &str) -> Result<(Hash, u64), String> {
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
    let last_valid = v
        .pointer("/result/value/lastValidBlockHeight")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    Ok((
        Hash::from_str(bh).map_err(|e| format!("blockhash parse: {e}"))?,
        last_valid,
    ))
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
        .ok_or_else(|| "rent-exemption response missing /result".to_string())
}

async fn get_account_info(
    client: &reqwest::Client,
    url: &str,
    pubkey: &Pubkey,
) -> Result<Option<(Vec<u8>, Pubkey, u64)>, String> {
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
        .ok_or_else(|| "account.data[0] missing".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| format!("base64 decode: {e}"))?;
    let lamports = value.get("lamports").and_then(|l| l.as_u64()).unwrap_or(0);
    Ok(Some((bytes, owner, lamports)))
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
        let idx = arr[0].as_u64()?;
        Some((idx, arr[1].clone()))
    }
    fn custom_error_code(&self) -> Option<u64> {
        let (_, inner) = self.instruction_error()?;
        inner.as_object()?.get("Custom")?.as_u64()
    }
}

/// Route via `simulateTransaction` only. Never touches `sendTransaction`.
async fn simulate_signed_transaction(
    client: &reqwest::Client,
    url: &str,
    tx: &Transaction,
) -> Result<SimulateOutcome, String> {
    let tx_bytes = bincode::serialize(tx).map_err(|e| format!("bincode serialize: {e}"))?;
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
    let err = value.get("err").cloned().unwrap_or(Value::Null);
    let logs: Vec<String> = value
        .get("logs")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let units_consumed = value.get("unitsConsumed").and_then(|u| u.as_u64());
    Ok(SimulateOutcome {
        err_json: err,
        logs,
        units_consumed,
    })
}

/// Broadcast. Uses NORMAL preflight (skip_preflight=false). The caller
/// must have already called `simulate_signed_transaction` on the SAME
/// signed payload and gotten `is_success()`.
async fn send_transaction_with_preflight(
    client: &reqwest::Client,
    url: &str,
    tx: &Transaction,
) -> Result<String, String> {
    let tx_bytes = bincode::serialize(tx).map_err(|e| format!("bincode serialize: {e}"))?;
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
        .ok_or_else(|| "sendTransaction response missing /result signature".to_string())
}

#[derive(Debug, Clone)]
struct ConfirmState {
    status: String,
    slot: Option<u64>,
    confirmations: Option<u64>,
    err: Value,
    finalized: bool,
}

/// Bounded poll on `getSignatureStatuses` until the signature is at
/// least `Confirmed` or the timeout expires. Returns the last observed
/// state regardless of success.
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
            let confirmations = entry.get("confirmations").and_then(|n| n.as_u64());
            let err = entry.get("err").cloned().unwrap_or(Value::Null);
            let finalized = status == "finalized";
            let st = ConfirmState {
                status: status.clone(),
                slot,
                confirmations,
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
                format!("confirmation timeout (no status returned) after {timeout:?}")
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
    if let Some(units) = outcome.units_consumed {
        println!("  compute units     : {units}");
    }
    if let Some((idx, inner)) = outcome.instruction_error() {
        println!("  instruction index : {idx}");
        println!("  InstructionError  : {inner}");
    } else if !outcome.err_json.is_null() {
        println!("  raw err           : {}", outcome.err_json);
    }
    if let Some(code) = outcome.custom_error_code() {
        println!("  custom code       : {code}");
    }
    if outcome.logs.is_empty() {
        println!("  logs              : (none returned)");
    } else {
        println!("  logs ({} lines):", outcome.logs.len());
        for l in &outcome.logs {
            println!("    | {l}");
        }
    }
}

fn classify_refresh_blocker(outcome: &SimulateOutcome) -> &'static str {
    if outcome.is_success() {
        return "(none — simulation succeeded)";
    }
    let logs = outcome.logs.join("\n").to_lowercase();
    if outcome.custom_error_code() == Some(14)
        || logs.contains("math operation overflow")
        || logs.contains("mathoverflow")
    {
        return "MathOverflow (Custom(14))";
    }
    if logs.contains("reserve is stale")
        || logs.contains("stale reserve")
        || logs.contains("stale oracle")
        || logs.contains("oracle price is stale")
    {
        return "StaleReserveOrOracle";
    }
    if logs.contains("oracle unknown") || logs.contains("invalid oracle") {
        return "OracleUnknown";
    }
    if outcome.instruction_error().is_some() {
        return "UnknownProgramFailure";
    }
    "AccountMetaMismatch"
}

// ── Main test ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_solend_refresh_proof() {
    let env = match read_env() {
        Ok(e) => e,
        Err(SkipReason::NotOptedIn) => {
            eprintln!(
                "SKIP: set CLAW_LIVE_SOLEND_REFRESH_PROOF=1 plus the required target env vars \
                 to run the opt-in live refresh proof. CI does not set this. This test may \
                 broadcast exactly ONE standalone RefreshReserve transaction iff preflight \
                 simulation succeeds."
            );
            return;
        }
        Err(SkipReason::MissingEnv(name)) => {
            eprintln!("SKIP: missing required env var: {name}");
            return;
        }
        Err(SkipReason::InvalidEnv(msg)) => {
            eprintln!("SKIP: invalid env var: {msg}");
            return;
        }
    };

    // Slice 3E(v): the old wSOL-pubkey whitelist was replaced with a
    // shape-based guardrail enforced after the reserve is fetched and
    // decoded (see hard-stop block below). Any Solend reserve that is
    // PythOnly + Pyth-Solana-Receiver + classic SPL Token + fresh Pyth
    // feed + positive headroom is allowed.

    if let Err(e) = validate_keypair_path(&env.keypair_path) {
        eprintln!("SKIP: keypair path validation failed: {e}");
        return;
    }
    let keypair = match load_keypair(&env.keypair_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("SKIP: could not load keypair: {e}");
            return;
        }
    };
    if keypair.pubkey() != env.owner {
        eprintln!(
            "SKIP: keypair pubkey {} does not match CLAW_LIVE_SOLEND_REFRESH_PROOF_OWNER {}",
            keypair.pubkey(),
            env.owner
        );
        return;
    }

    let solend_program = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap();
    let classic_spl_token_id = Pubkey::from_str(CLASSIC_SPL_TOKEN_BS58).unwrap();
    let pyth_receiver_id = Pubkey::from_str(PYTH_SOLANA_RECEIVER_BS58).unwrap();
    let sentinel_oracle = Pubkey::from_str(raw::SOLEND_NULL_ORACLE_SENTINEL_BS58).unwrap();

    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  Slice 3E — live RefreshReserve proof");
    println!("══════════════════════════════════════════════════════════════");
    println!("  rpc host           : {}", sanitized_rpc_label(&env.rpc_url));
    println!("  owner              : {}", env.owner);
    println!("  target reserve     : {}", env.target_reserve);
    println!("  keypair signer     : {} (outside repo)", keypair.pubkey());
    println!(
        "  post-refresh sim   : {}",
        if env.run_post_refresh_deposit_sim { "YES (if refresh confirms)" } else { "no" }
    );
    println!("  broadcast allowed  : 1 tx max (RefreshReserve), iff preflight passes");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("claw-gateway/live-solend-refresh-proof")
        .build()
        .expect("reqwest client");

    // ── Pre-refresh read-only probe ──────────────────────────────────────
    println!();
    println!("── PRE-REFRESH reserve snapshot ──────────────────────────────");
    let (pre_bytes, pre_owner, _) =
        match get_account_info(&client, &env.rpc_url, &env.target_reserve).await {
            Ok(Some(v)) => v,
            Ok(None) => panic!("target reserve not found on chain"),
            Err(e) => panic!("fetch reserve: {e}"),
        };
    assert_eq!(pre_owner, solend_program, "reserve must be Solend-owned");
    let pre = raw::decode_reserve(&pre_bytes).expect("decode_reserve pre");
    let pre_total = pre.liquidity_total_supply_raw().unwrap_or(0);
    let pre_headroom = pre.deposit_headroom_raw();
    println!("  reserve            : {}", env.target_reserve);
    println!("  lending_market     : {}", pre.lending_market);
    println!("  mint               : {} (dec={})", pre.liquidity_mint, pre.liquidity_mint_decimals);
    println!("  stale              : {}", pre.last_update_stale);
    println!("  last_update_slot   : {}", pre.last_update_slot);
    println!("  deposit_limit      : {}", pre.config_deposit_limit);
    println!("  total_supply (raw) : {pre_total}");
    println!("  headroom (raw)     : {pre_headroom}");
    println!("  pyth_oracle        : {}", pre.pyth_oracle);
    println!("  switchboard_oracle : {}", pre.switchboard_oracle);

    // Slice 3E(v) hard-stop: shape-based gate replacing the wSOL-pubkey
    // whitelist. We do NOT check against a specific market/mint, but we
    // DO refuse anything outside the V1 oracle + token-program envelope
    // so that the single authorized broadcast cannot accidentally go to
    // a shape Claw has not verified. Hard-stops:
    //   (a) token program for the reserve's liquidity mint must be
    //       classic SPL Token (slice scope: single-token-program only);
    //   (b) Switchboard must be sentinel / absent (no Switchboard
    //       decoder in V1);
    //   (c) Pyth oracle account must be owned by Pyth Solana Receiver
    //       (Claw's only verified freshness decoder path);
    //   (d) Pyth freshness must decode as KnownSlot (Unknown is
    //       RequireFreshState-failing by V1 policy);
    //   (e) deposit_limit must be non-zero;
    //   (f) deposit_headroom must satisfy amount_raw.
    // Any failure here prints a clear SKIP reason and returns without
    // broadcasting. No panic.

    // (a) Token program check on the liquidity mint.
    println!();
    println!("── PRE-REFRESH hard-stop checks ─────────────────────────────");
    let mint_owner = match get_account_info(&client, &env.rpc_url, &pre.liquidity_mint).await {
        Ok(Some((_, owner, _))) => owner,
        Ok(None) => {
            eprintln!("SKIP: reserve's liquidity mint not found on chain");
            return;
        }
        Err(e) => {
            eprintln!("SKIP: fetch mint owner: {e}");
            return;
        }
    };
    println!("  mint                : {}", pre.liquidity_mint);
    println!("  mint token program  : {mint_owner}");
    if mint_owner != classic_spl_token_id {
        eprintln!(
            "SKIP: liquidity mint's token program is {mint_owner}, expected classic \
             SPL Token {classic_spl_token_id}. V1 deposit path is scoped to classic \
             SPL Token only."
        );
        return;
    }
    println!("  (a) classic SPL Token           : OK");

    // (b) Switchboard sentinel check.
    if pre.switchboard_oracle != sentinel_oracle {
        eprintln!(
            "SKIP: reserve's Switchboard oracle is {}, not the sentinel. V1 scope \
             requires Switchboard sentinel; no decoder implemented.",
            pre.switchboard_oracle
        );
        return;
    }
    println!("  (b) Switchboard sentinel        : OK");

    // (c) + (d) Pyth Receiver owner + freshness. Combined in one fetch.
    println!();
    println!("── PRE-REFRESH pyth-receiver freshness ───────────────────────");
    let pre_pyth_freshness = match get_account_info(&client, &env.rpc_url, &pre.pyth_oracle).await {
        Ok(Some((bytes, owner, _))) => {
            if owner != pyth_receiver_id {
                eprintln!(
                    "SKIP: reserve's Pyth oracle is owned by {owner}, not Pyth Solana \
                     Receiver {pyth_receiver_id}. V1 scope requires Pyth Solana \
                     Receiver; no classic-Pyth decoder implemented."
                );
                return;
            }
            let freshness = extract_publish_freshness(&owner, &bytes);
            match freshness {
                FeedPublishFreshness::KnownSlot(slot) => {
                    println!("  pyth owner          : {owner}");
                    println!("  freshness           : KnownSlot({slot:?})");
                    println!("  (c) Pyth Solana Receiver owner  : OK");
                    println!("  (d) freshness KnownSlot          : OK");
                    Some(freshness)
                }
                FeedPublishFreshness::Unknown => {
                    eprintln!(
                        "SKIP: Pyth oracle decoded as Unknown freshness. V1 policy is \
                         fail-closed on Unknown; no broadcast."
                    );
                    return;
                }
            }
        }
        Ok(None) => {
            eprintln!("SKIP: Pyth oracle account not found on chain");
            return;
        }
        Err(e) => {
            eprintln!("SKIP: pyth fetch error: {e}");
            return;
        }
    };
    let _ = pre_pyth_freshness; // reserved for future logging

    // (e) deposit_limit > 0
    if pre.config_deposit_limit == 0 {
        eprintln!("SKIP: reserve deposit_limit == 0 (deposit-disabled via config)");
        return;
    }
    println!("  (e) deposit_limit > 0           : OK ({})", pre.config_deposit_limit);

    // (f) headroom satisfies amount_raw
    if pre.deposit_headroom_raw() < env.amount_raw {
        eprintln!(
            "SKIP: deposit_headroom_raw={} < amount_raw={} (cap exhausted)",
            pre.deposit_headroom_raw(),
            env.amount_raw
        );
        return;
    }
    println!(
        "  (f) headroom >= amount_raw      : OK (headroom={}, amount={})",
        pre.deposit_headroom_raw(),
        env.amount_raw
    );

    // ── Build refresh plan via existing pure builder ─────────────────────
    println!();
    println!("── building refresh plan via existing build_refresh_instructions ──");
    let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
        solend_program_id: solend_program,
        reserves: vec![ReserveRefreshInput {
            reserve_pubkey: env.target_reserve,
            pyth_oracle: pre.pyth_oracle,
            switchboard_oracle: pre.switchboard_oracle,
        }],
        obligation: None, // first-deposit path; no RefreshObligation
    });
    println!("  ix count           : {}", refresh_plan.instructions.len());
    println!("  reserves refreshed : {:?}", refresh_plan.reserves_refreshed);
    println!(
        "  obligation         : {}",
        refresh_plan
            .obligation_refreshed
            .map(|p| p.to_string())
            .unwrap_or_else(|| "(none — standalone refresh)".into())
    );

    // Slice 3E(v): prepend a modest ComputeBudget priority-fee ix so a
    // valid refresh transaction is less likely to be dropped by mainnet
    // congestion. No base-fee override; unit price in micro-lamports per
    // compute unit.
    let priority_fee_ix: Instruction = ComputeBudgetInstruction::set_compute_unit_price(
        REFRESH_PRIORITY_FEE_MICRO_LAMPORTS,
    );
    let mut final_ixs: Vec<Instruction> = Vec::with_capacity(1 + refresh_plan.instructions.len());
    final_ixs.push(priority_fee_ix);
    final_ixs.extend(refresh_plan.instructions.iter().cloned());
    println!(
        "  priority fee       : set_compute_unit_price={} micro-lamports/CU",
        REFRESH_PRIORITY_FEE_MICRO_LAMPORTS
    );

    // ── Fetch blockhash + sign the refresh tx (built ONCE) ───────────────
    println!();
    println!("── fetching latest blockhash ────────────────────────────────");
    let (blockhash, last_valid) = match get_latest_blockhash(&client, &env.rpc_url).await {
        Ok(v) => v,
        Err(e) => panic!("FAIL (opt-in): getLatestBlockhash: {e}"),
    };
    let bh_s = blockhash.to_string();
    println!(
        "  blockhash          : {}…{} (last_valid={last_valid})",
        &bh_s[..8.min(bh_s.len())],
        &bh_s[bh_s.len().saturating_sub(6)..]
    );
    // Build the Transaction ONCE. This exact `refresh_tx` object is
    // passed by reference to both simulate and (if preflight passes)
    // send. No rebuild. No re-sign. No alternate signed payload.
    let refresh_tx = Transaction::new_signed_with_payer(
        &final_ixs,
        Some(&env.owner),
        &[&keypair],
        blockhash,
    );
    println!(
        "  signed ix count    : {} (priority-fee + refresh)",
        final_ixs.len()
    );

    // ── Preflight simulation — consumes &refresh_tx by reference ─────────
    let refresh_sim = match simulate_signed_transaction(&client, &env.rpc_url, &refresh_tx).await {
        Ok(v) => v,
        Err(e) => panic!("FAIL (opt-in): refresh simulateTransaction RPC: {e}"),
    };
    print_simulation("refresh preflight", &refresh_sim);
    let refresh_classification = classify_refresh_blocker(&refresh_sim);
    println!("  blocker classification: {refresh_classification}");

    if !refresh_sim.is_success() {
        println!();
        println!("══════════════════════════════════════════════════════════════");
        println!("  OUTCOME (A): preflight failed — NO BROADCAST");
        println!("══════════════════════════════════════════════════════════════");
        println!("  Blocker Identified: target reserve cannot be refreshed by");
        println!("  standalone RefreshReserve under current on-chain state.");
        println!("  Classification: {refresh_classification}");
        println!();
        println!("  no broadcast       : confirmed");
        println!("  no deposit broadcast: confirmed");
        println!("  no ATA creation    : confirmed");
        println!("  no InitObligation  : confirmed");
        println!("  no source funding  : confirmed");
        println!();
        println!(
            "  next recommended step: target reselection and/or Solend refresh"
        );
        println!("                        arithmetic / oracle-freshness investigation.");
        println!("══════════════════════════════════════════════════════════════");
        return;
    }

    // Preflight passed — proceed to broadcast the EXACT signed payload.
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  preflight PASSED — broadcasting the exact signed refresh tx");
    println!("══════════════════════════════════════════════════════════════");
    let signature = match send_transaction_with_preflight(&client, &env.rpc_url, &refresh_tx).await
    {
        Ok(sig) => sig,
        Err(e) => {
            println!();
            println!("══════════════════════════════════════════════════════════════");
            println!("  OUTCOME (C): send failed after successful preflight");
            println!("══════════════════════════════════════════════════════════════");
            println!("  send error        : {e}");
            println!("  no follow-up tx   : confirmed (no retry, no replacement)");
            println!("══════════════════════════════════════════════════════════════");
            return;
        }
    };
    println!("  signature          : {signature}");

    // ── Confirmation polling ─────────────────────────────────────────────
    println!();
    println!(
        "── waiting for Confirmed (timeout={:?}, poll={:?}) ────────",
        CONFIRM_TIMEOUT, CONFIRM_POLL_INTERVAL
    );
    let confirm = match await_confirmation(
        &client,
        &env.rpc_url,
        &signature,
        CONFIRM_TIMEOUT,
        CONFIRM_POLL_INTERVAL,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            println!();
            println!("══════════════════════════════════════════════════════════════");
            println!("  OUTCOME (D): confirmation polling error");
            println!("══════════════════════════════════════════════════════════════");
            println!("  signature          : {signature}");
            println!("  poll error         : {e}");
            println!("  no stale re-fetch proof claimed");
            println!("══════════════════════════════════════════════════════════════");
            return;
        }
    };
    println!("  status             : {}", confirm.status);
    if let Some(s) = confirm.slot {
        println!("  slot               : {s}");
    }
    if let Some(c) = confirm.confirmations {
        println!("  confirmations      : {c}");
    }
    println!("  finalized reached? : {}", confirm.finalized);
    println!("  on-chain err?      : {}", confirm.err);
    let ok = confirm.status == "confirmed" || confirm.status == "finalized";
    if !ok {
        println!();
        println!("══════════════════════════════════════════════════════════════");
        println!("  OUTCOME (D): confirmation timeout before Confirmed commitment");
        println!("══════════════════════════════════════════════════════════════");
        println!("  signature          : {signature}");
        println!("  last status        : {}", confirm.status);
        println!("  no stale re-fetch proof claimed");
        println!("══════════════════════════════════════════════════════════════");
        return;
    }
    if !confirm.err.is_null() {
        println!();
        println!("══════════════════════════════════════════════════════════════");
        println!("  OUTCOME: confirmed with on-chain error; refresh did NOT apply");
        println!("══════════════════════════════════════════════════════════════");
        println!("  signature          : {signature}");
        println!("  on-chain err       : {}", confirm.err);
        println!("══════════════════════════════════════════════════════════════");
        return;
    }

    // ── Post-refresh re-fetch proof ──────────────────────────────────────
    println!();
    println!("── POST-REFRESH reserve snapshot (re-fetched after Confirmed) ─");
    let (post_bytes, _, _) =
        match get_account_info(&client, &env.rpc_url, &env.target_reserve).await {
            Ok(Some(v)) => v,
            Ok(None) => panic!("target reserve disappeared post-refresh?"),
            Err(e) => panic!("post-refresh fetch: {e}"),
        };
    let post = raw::decode_reserve(&post_bytes).expect("decode_reserve post");
    let post_total = post.liquidity_total_supply_raw().unwrap_or(0);
    let post_headroom = post.deposit_headroom_raw();
    println!("  stale              : {} → {}", pre.last_update_stale, post.last_update_stale);
    println!("  last_update_slot   : {} → {}", pre.last_update_slot, post.last_update_slot);
    println!(
        "  last_update_slot Δ : {}",
        post.last_update_slot as i128 - pre.last_update_slot as i128
    );
    println!("  deposit_limit      : {} → {}", pre.config_deposit_limit, post.config_deposit_limit);
    println!("  total_supply (raw) : {pre_total} → {post_total}");
    println!("  headroom (raw)     : {pre_headroom} → {post_headroom}");

    // Post-refresh Pyth freshness.
    match get_account_info(&client, &env.rpc_url, &post.pyth_oracle).await {
        Ok(Some((bytes, owner, _))) => {
            let freshness = extract_publish_freshness(&owner, &bytes);
            println!(
                "  pyth freshness     : {}",
                match freshness {
                    FeedPublishFreshness::KnownSlot(slot) => format!("KnownSlot({slot:?})"),
                    FeedPublishFreshness::Unknown => "Unknown".to_string(),
                }
            );
        }
        Ok(None) => println!("  pyth freshness     : oracle account not found"),
        Err(e) => println!("  pyth fetch error   : {e}"),
    }

    let slot_advanced = post.last_update_slot > pre.last_update_slot;
    let stale_cleared = pre.last_update_stale && !post.last_update_stale;
    println!();
    println!("  refresh evidence   :");
    println!("    last_update slot increased?  {slot_advanced}");
    println!("    stale marker cleared?        {stale_cleared}");
    println!("    headroom still positive?     {}", post_headroom > 0);
    println!();
    println!("  NOTE: policy freshness is a separate property; this slice does");
    println!("        not re-run the three-layer gate. RequireFreshState would");
    println!("        still run against a freshly decoded snapshot built from");
    println!("        these post-refresh bytes.");

    // ── OPTIONAL post-refresh deposit dry simulation ─────────────────────
    if !env.run_post_refresh_deposit_sim {
        println!();
        println!("══════════════════════════════════════════════════════════════");
        println!("  OUTCOME (B): refresh broadcast + confirmed");
        println!("══════════════════════════════════════════════════════════════");
        println!("  signature          : {signature}");
        println!("  slot_advanced      : {slot_advanced}");
        println!("  stale_cleared      : {stale_cleared}");
        println!("  Custom(14) cleared : {}", slot_advanced);
        println!("  post-deposit sim   : skipped (env flag off)");
        println!();
        println!("  next recommended step: controlled wSOL source-liquidity funding");
        println!("                        / wrapping in a future slice.");
        println!("══════════════════════════════════════════════════════════════");
        return;
    }

    // Load obligation keypair if provided.
    let obligation_kp = if let Some(path) = env.obligation_keypair_path.as_ref() {
        if let Err(e) = validate_keypair_path(path) {
            println!("  post-sim skipped   : obligation keypair path invalid: {e}");
            None
        } else {
            match load_keypair(path) {
                Ok(kp) => Some(kp),
                Err(e) => {
                    println!("  post-sim skipped   : could not load obligation keypair: {e}");
                    None
                }
            }
        }
    } else {
        None
    };
    let obligation_kp = match obligation_kp {
        Some(kp) => kp,
        None => {
            println!();
            println!("══════════════════════════════════════════════════════════════");
            println!("  OUTCOME (B): refresh confirmed; post-deposit sim SKIPPED");
            println!("══════════════════════════════════════════════════════════════");
            println!("  signature          : {signature}");
            println!("  reason             : no obligation keypair provided");
            println!();
            println!("  next recommended step: supply");
            println!("    CLAW_LIVE_SOLEND_REFRESH_PROOF_OBLIGATION_KEYPAIR_PATH");
            println!("    and rerun, OR proceed to controlled wSOL source funding.");
            println!("══════════════════════════════════════════════════════════════");
            return;
        }
    };

    // Build the exact same bundle the Slice 3E(i) dry-sim harness builds.
    println!();
    println!("── POST-REFRESH deposit bundle simulation (signed, not broadcast)");
    let classic_spl_token = Pubkey::from_str(CLASSIC_SPL_TOKEN_BS58).unwrap();
    let source_ata =
        derive_associated_token_address(&env.owner, &post.liquidity_mint, &classic_spl_token);
    let user_collat_ata =
        derive_associated_token_address(&env.owner, &post.collateral_mint, &classic_spl_token);
    println!("  source wSOL ATA    : {source_ata}");
    println!("  user collat ATA    : {user_collat_ata}");
    println!("  obligation pubkey  : {}", obligation_kp.pubkey());

    let rent_exempt = match get_minimum_balance_for_rent_exemption(
        &client,
        &env.rpc_url,
        raw::OBLIGATION_LEN,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            println!("  post-sim aborted   : rent fetch: {e}");
            return;
        }
    };
    println!("  rent-exempt lamps  : {rent_exempt}");

    let create_ix = build_create_obligation_account_instruction(CreateObligationAccountInputs {
        funder: env.owner,
        new_obligation: obligation_kp.pubkey(),
        rent_exempt_lamports: rent_exempt,
        solend_program_id: solend_program,
    });
    let init_ix = build_init_obligation_instruction(InitObligationInputs {
        solend_program_id: solend_program,
        obligation: obligation_kp.pubkey(),
        lending_market: post.lending_market,
        obligation_owner: env.owner,
    });
    let ata_src_ix = build_create_ata_idempotent_instruction(
        &env.owner,
        &env.owner,
        &post.liquidity_mint,
        &classic_spl_token,
    );
    let ata_collat_ix = build_create_ata_idempotent_instruction(
        &env.owner,
        &env.owner,
        &post.collateral_mint,
        &classic_spl_token,
    );
    let deposit_ix = match build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
        DepositInstructionInputs {
            solend_program_id: solend_program,
            amount: UnderlyingAmount::new(env.amount_raw),
            source_liquidity: source_ata,
            user_collateral: user_collat_ata,
            reserve: env.target_reserve,
            reserve_liquidity_supply: post.liquidity_supply,
            reserve_collateral_mint: post.collateral_mint,
            lending_market: post.lending_market,
            destination_deposit_collateral: post.collateral_supply,
            obligation: obligation_kp.pubkey(),
            obligation_owner: env.owner,
            pyth_oracle: post.pyth_oracle,
            switchboard_oracle: post.switchboard_oracle,
            user_transfer_authority: env.owner,
        },
    ) {
        Ok(ix) => ix,
        Err(e) => {
            println!("  post-sim aborted   : deposit builder: {e}");
            return;
        }
    };

    let bundle = vec![create_ix, init_ix, ata_src_ix, ata_collat_ix, deposit_ix];
    println!("  bundle ix count    : {}", bundle.len());

    // New blockhash for this sim — the sim is separate from the confirmed
    // refresh and may run past the refresh's blockhash window.
    let (sim_blockhash, _) = match get_latest_blockhash(&client, &env.rpc_url).await {
        Ok(v) => v,
        Err(e) => {
            println!("  post-sim aborted   : blockhash fetch: {e}");
            return;
        }
    };
    let sim_tx = Transaction::new_signed_with_payer(
        &bundle,
        Some(&env.owner),
        &[&keypair, &obligation_kp],
        sim_blockhash,
    );
    let sim_outcome = match simulate_signed_transaction(&client, &env.rpc_url, &sim_tx).await {
        Ok(v) => v,
        Err(e) => {
            println!("  post-sim aborted   : simulate RPC: {e}");
            return;
        }
    };
    print_simulation("post-refresh deposit bundle", &sim_outcome);
    let next_blocker = match (
        sim_outcome.is_success(),
        sim_outcome.custom_error_code(),
        sim_outcome
            .logs
            .iter()
            .any(|l| l.to_lowercase().contains("insufficient")),
    ) {
        (true, _, _) => "(bundle sim succeeded — no next blocker)",
        (false, Some(14), _) => "Custom(14) MathOverflow (refresh not actually effective)",
        (false, _, true) => "InsufficientFunds (source wSOL ATA unfunded)",
        (false, Some(code), _) => {
            println!("  next blocker custom code: {code}");
            "UnknownProgramFailure"
        }
        (false, None, _) => "UnknownTransactionError",
    };

    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  OUTCOME (B): refresh broadcast + confirmed + post-deposit sim");
    println!("══════════════════════════════════════════════════════════════");
    println!("  signature          : {signature}");
    println!("  slot_advanced      : {slot_advanced}");
    println!("  stale_cleared      : {stale_cleared}");
    println!("  Custom(14) in dep. : {}",
        if sim_outcome.custom_error_code() == Some(14) { "STILL PRESENT" } else { "cleared" });
    println!("  next blocker       : {next_blocker}");
    println!();
    println!("  confirmation disciplines:");
    println!("    waited before re-fetch : YES (bounded poll to Confirmed)");
    println!("    timeout occurred       : no");
    println!("    on-chain err           : none");
    println!();
    println!("  NO deposit broadcast");
    println!("  NO InitObligation broadcast");
    println!("  NO ATA creation broadcast");
    println!("  NO source funding / SOL wrap / sync_native");
    println!("  NO Phantom / daemon / approval workflow");
    println!("══════════════════════════════════════════════════════════════");
}

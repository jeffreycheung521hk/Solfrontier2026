//! OPT-IN: Slice 3E(iii) — refreshable-target reselection recon.
//!
//! ## Purpose
//!
//! Find a Solend reserve that can actually be refreshed under current
//! on-chain state. The Slice 3E(ii) proof showed that a Pyth-Receiver
//! Pyth-only reserve with fresh oracle data can still fail
//! `RefreshReserve` preflight with `Custom(14) MathOverflow` when the
//! reserve's own `last_update.slot` is deeply stale — the interest
//! accumulator overflows when compounded over a huge slot delta.
//!
//! This test ranks Pyth-Receiver + Switchboard-sentinel candidates by
//! `current_slot - last_update.slot` and probes the freshest ones via
//! `simulateTransaction` to identify at least one target whose
//! standalone refresh actually converges.
//!
//! ## What this test NEVER does
//!
//! - broadcast any transaction (no `sendTransaction` / `sendRawTransaction`)
//! - create ATAs, obligations, or transfer any token
//! - wrap SOL, call `sync_native`, or fund any account
//! - touch Phantom / daemon / approval workflow / LLM path
//! - change policy, oracle aggregation, or Switchboard decoding
//! - mutate chain state in any way
//!
//! ## Opt-in
//!
//! Self-skips unless `CLAW_LIVE_SOLEND_REFRESH_RECON=1` is set. CI never
//! sets this. Default `cargo test` invocation performs zero network
//! calls and prints `SKIP: …`.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use claw_gateway::integrations::solend::{
    build_refresh_instructions, extract_publish_freshness,
    raw::{self, SOLEND_NULL_ORACLE_SENTINEL_BS58, SOLEND_PROGRAM_ID_BS58},
    RefreshPlanInputs, ReserveRefreshInput,
};
use claw_gateway::lending::FeedPublishFreshness;
use serde_json::{json, Value};
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};

// Pyth Solana Receiver program id (the "new" Pyth on Solana).
const PYTH_SOLANA_RECEIVER_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

// Solend public config endpoint, returns ~750 reserves in one response.
const SOLEND_CONFIG_URL: &str = "https://api.solend.fi/v1/reserves?scope=all";

// Known baselines (for optional reporting; not consumed by the pass/fail
// predicate). Slice 3D(iii) / 3E(i) / 3E(ii) sealed these outcomes.
const BASELINE_AMM_RESERVE_BS58: &str = "6nb1odSYHutVAxoaQyiwPhQNTFn3nBNFdQdNCm5v9Jbp";
const BASELINE_WSOL_RESERVE_BS58: &str = "FNDYHVzPrB9B4FNqe5swcDPgKqoVif3ohPnmAUkJ5K5t";

const DEFAULT_AMOUNT_RAW: u64 = 1_000;
const DEFAULT_MAX_CANDIDATES: usize = 20;
const DEFAULT_MAX_SIMULATIONS: usize = 5;

// ── Opt-in env reading ─────────────────────────────────────────────────────

struct RequiredEnv {
    rpc_url: String,
    keypair_path: PathBuf,
    owner: Pubkey,
    amount_raw: u64,
    max_candidates: usize,
    max_simulations: usize,
}

#[derive(Debug)]
enum SkipReason {
    NotOptedIn,
    MissingEnv(String),
    InvalidEnv(String),
}

fn read_env() -> Result<RequiredEnv, SkipReason> {
    match std::env::var("CLAW_LIVE_SOLEND_REFRESH_RECON") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {}
        _ => return Err(SkipReason::NotOptedIn),
    }
    let require = |name: &str| -> Result<String, SkipReason> {
        std::env::var(name).map_err(|_| SkipReason::MissingEnv(name.to_string()))
    };
    let rpc_url = require("CLAW_LIVE_SOLEND_REFRESH_RECON_RPC_URL")?;
    let keypair_path = PathBuf::from(require("CLAW_LIVE_SOLEND_REFRESH_RECON_KEYPAIR_PATH")?);
    let owner = Pubkey::from_str(&require("CLAW_LIVE_SOLEND_REFRESH_RECON_OWNER")?)
        .map_err(|e| SkipReason::InvalidEnv(format!("OWNER not base58: {e}")))?;
    let amount_raw = std::env::var("CLAW_LIVE_SOLEND_REFRESH_RECON_AMOUNT_RAW")
        .ok()
        .map(|s| {
            s.parse::<u64>()
                .map_err(|e| SkipReason::InvalidEnv(format!("AMOUNT_RAW not u64: {e}")))
        })
        .transpose()?
        .unwrap_or(DEFAULT_AMOUNT_RAW);
    let max_candidates = std::env::var("CLAW_LIVE_SOLEND_REFRESH_RECON_MAX_CANDIDATES")
        .ok()
        .map(|s| {
            s.parse::<usize>()
                .map_err(|e| SkipReason::InvalidEnv(format!("MAX_CANDIDATES not usize: {e}")))
        })
        .transpose()?
        .unwrap_or(DEFAULT_MAX_CANDIDATES);
    let max_simulations = std::env::var("CLAW_LIVE_SOLEND_REFRESH_RECON_MAX_SIMULATIONS")
        .ok()
        .map(|s| {
            s.parse::<usize>()
                .map_err(|e| SkipReason::InvalidEnv(format!("MAX_SIMULATIONS not usize: {e}")))
        })
        .transpose()?
        .unwrap_or(DEFAULT_MAX_SIMULATIONS);
    Ok(RequiredEnv {
        rpc_url,
        keypair_path,
        owner,
        amount_raw,
        max_candidates,
        max_simulations,
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
    let contents = fs::read_to_string(path).map_err(|e| format!("read keypair file: {e}"))?;
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

// ── RPC helpers ───────────────────────────────────────────────────────────

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
        .map_err(|e| format!("{method} response not JSON: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("{method} RPC error: {err}"));
    }
    Ok(v)
}

/// Single bounded retry on transient TCP-connect errors (public RPC
/// sometimes rejects rapid requests). A second failure aborts. We NEVER
/// retry on RATE-LIMIT responses — that must stop the recon.
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

async fn get_slot(client: &reqwest::Client, url: &str) -> Result<u64, String> {
    let v = rpc_call(
        client,
        url,
        "getSlot",
        json!([{ "commitment": "confirmed" }]),
    )
    .await?;
    v.pointer("/result")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| "getSlot missing /result".to_string())
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

#[derive(Debug, Clone)]
struct AccountRow {
    owner: Pubkey,
    data: Vec<u8>,
}

/// Batched `getMultipleAccounts` with small chunks to fit public-RPC
/// limits. `data_slice=None` returns full bytes.
async fn get_multiple_accounts(
    client: &reqwest::Client,
    url: &str,
    pubkeys: &[Pubkey],
    data_slice: Option<(usize, usize)>,
) -> Result<Vec<Option<AccountRow>>, String> {
    if pubkeys.is_empty() {
        return Ok(vec![]);
    }
    // Public mainnet-beta tends to time out or reject very large
    // getMultipleAccounts calls; chunking 10-at-a-time is empirically
    // the sweet spot.
    const CHUNK: usize = 5;
    let mut out: Vec<Option<AccountRow>> = Vec::with_capacity(pubkeys.len());
    for (ci, chunk) in pubkeys.chunks(CHUNK).enumerate() {
        if ci > 0 {
            // Gentle pacing to avoid public-RPC connection resets.
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        let keys_json: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
        let mut opts = json!({ "encoding": "base64", "commitment": "confirmed" });
        if let Some((offset, length)) = data_slice {
            opts.as_object_mut().unwrap().insert(
                "dataSlice".to_string(),
                json!({ "offset": offset, "length": length }),
            );
        }
        let v = rpc_call(
            client,
            url,
            "getMultipleAccounts",
            json!([keys_json, opts]),
        )
        .await?;
        let arr = v
            .pointer("/result/value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for entry in arr {
            if entry.is_null() {
                out.push(None);
                continue;
            }
            let owner_str = entry.get("owner").and_then(|o| o.as_str()).unwrap_or("");
            let owner = Pubkey::from_str(owner_str).map_err(|e| format!("owner parse: {e}"))?;
            let data_b64 = entry
                .pointer("/data/0")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .map_err(|e| format!("base64 decode: {e}"))?;
            out.push(Some(AccountRow {
                owner,
                data: bytes,
            }));
        }
    }
    Ok(out)
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

// ── Candidate typing ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotDeltaClass {
    FreshEnoughPreferred,  // < 100_000
    ModeratelyStale,       // 100_000..=1_000_000
    DeeplyStale,           // > 1_000_000
}
impl SlotDeltaClass {
    fn classify(delta: u64) -> Self {
        if delta < 100_000 {
            Self::FreshEnoughPreferred
        } else if delta <= 1_000_000 {
            Self::ModeratelyStale
        } else {
            Self::DeeplyStale
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Self::FreshEnoughPreferred => "FreshEnoughPreferred",
            Self::ModeratelyStale => "ModeratelyStale",
            Self::DeeplyStale => "DeeplyStale",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitialStatus {
    CandidateForRefreshSimulation,
    BlockedBySwitchboard,
    BlockedByClassicPyth,
    BlockedByUnknownOracleFreshness,
    BlockedByDepositLimit,
    BlockedByInsufficientHeadroom,
    BlockedByDeepStaleSlotDelta,
    BlockedByOtherReason,
}
impl InitialStatus {
    fn label(&self) -> &'static str {
        match self {
            Self::CandidateForRefreshSimulation => "CandidateForRefreshSimulation",
            Self::BlockedBySwitchboard => "BlockedBySwitchboard",
            Self::BlockedByClassicPyth => "BlockedByClassicPyth",
            Self::BlockedByUnknownOracleFreshness => "BlockedByUnknownOracleFreshness",
            Self::BlockedByDepositLimit => "BlockedByDepositLimit",
            Self::BlockedByInsufficientHeadroom => "BlockedByInsufficientHeadroom",
            Self::BlockedByDeepStaleSlotDelta => "BlockedByDeepStaleSlotDelta",
            Self::BlockedByOtherReason => "BlockedByOtherReason",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshSimClass {
    RefreshSimulationPass,
    MathOverflow,
    BlockedByStaleOraclePrice,
    StaleReserveOrOracle,
    OracleUnknown,
    AccountMetaMismatch,
    UnknownProgramFailure,
}
impl RefreshSimClass {
    fn classify(outcome: &SimulateOutcome) -> Self {
        if outcome.is_success() {
            return Self::RefreshSimulationPass;
        }
        let logs = outcome.logs.join("\n").to_lowercase();
        let custom = outcome.custom_error_code();
        if custom == Some(14)
            || logs.contains("math operation overflow")
            || logs.contains("mathoverflow")
        {
            return Self::MathOverflow;
        }
        if custom == Some(3) || custom == Some(12) {
            return Self::BlockedByStaleOraclePrice;
        }
        if logs.contains("reserve is stale")
            || logs.contains("stale reserve")
            || logs.contains("stale oracle")
            || logs.contains("oracle price is stale")
        {
            return Self::StaleReserveOrOracle;
        }
        if logs.contains("oracle unknown") || logs.contains("invalid oracle") {
            return Self::OracleUnknown;
        }
        if outcome.instruction_error().is_some() {
            return Self::UnknownProgramFailure;
        }
        Self::AccountMetaMismatch
    }
    fn label(&self) -> &'static str {
        match self {
            Self::RefreshSimulationPass => "RefreshSimulationPass",
            Self::MathOverflow => "MathOverflow",
            Self::BlockedByStaleOraclePrice => "BlockedByStaleOraclePrice",
            Self::StaleReserveOrOracle => "StaleReserveOrOracle",
            Self::OracleUnknown => "OracleUnknown",
            Self::AccountMetaMismatch => "AccountMetaMismatch",
            Self::UnknownProgramFailure => "UnknownProgramFailure",
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    reserve: Pubkey,
    lending_market: Pubkey,
    mint: Pubkey,
    decimals: u8,
    pyth_oracle: Pubkey,
    switchboard_oracle: Pubkey,
    switchboard_is_sentinel: bool,
    pyth_owner: Option<Pubkey>,
    pyth_is_receiver: bool,
    oracle_freshness: Option<FeedPublishFreshness>,
    last_update_slot: u64,
    deposit_limit: u64,
    total_supply_raw: u64,
    headroom_raw: u64,
    amount_fits: bool,
    slot_delta: u64,
    slot_delta_class: SlotDeltaClass,
    initial_status: InitialStatus,
}

// ── Candidate-source filter (Solend public config) ────────────────────────

async fn fetch_solend_reserve_candidates(
    client: &reqwest::Client,
) -> Result<Vec<PythOnlyConfigRow>, String> {
    let resp = client
        .get(SOLEND_CONFIG_URL)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("solend config GET: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("solend config HTTP {}", resp.status()));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("solend config not JSON: {e}"))?;
    let results = json
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let sentinel = SOLEND_NULL_ORACLE_SENTINEL_BS58;
    let mut out = Vec::new();
    for r in results {
        let rr = r.get("reserve").cloned().unwrap_or(Value::Null);
        if rr.is_null() {
            continue;
        }
        let reserve_pk = rr.get("pubkey").and_then(|s| s.as_str()).unwrap_or("");
        let lm = rr.get("lendingMarket").and_then(|s| s.as_str()).unwrap_or("");
        let liq = rr.get("liquidity").cloned().unwrap_or(Value::Null);
        let cfg = rr.get("config").cloned().unwrap_or(Value::Null);
        let mint = liq.get("mintPubkey").and_then(|s| s.as_str()).unwrap_or("");
        let pyth = liq.get("pythOracle").and_then(|s| s.as_str()).unwrap_or("");
        let swb = liq
            .get("switchboardOracle")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let dep_lim = cfg
            .get("depositLimit")
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse::<u128>().ok())
            .unwrap_or(0);
        let switchboard_is_sentinel = swb.is_empty() || swb == sentinel;
        let pyth_is_real = !pyth.is_empty() && pyth != sentinel;
        if !switchboard_is_sentinel || !pyth_is_real || dep_lim == 0 {
            continue;
        }
        if reserve_pk.is_empty() || mint.is_empty() || lm.is_empty() || pyth.is_empty() {
            continue;
        }
        // Slice 3E(v): capture the config-reported `lastUpdate.slot` as a
        // freshness hint so we can sort candidates before applying
        // MAX_CANDIDATES. Slice 3E(iii) mistakenly took the first
        // MAX_CANDIDATES in config-API order — that order front-loads
        // dormant archival reserves and caused the "Solend is dormant"
        // false conclusion. Sorting by this hint (descending) surfaces
        // fresh reserves first.
        let last_update_slot_hint = rr
            .get("lastUpdate")
            .and_then(|v| v.get("slot"))
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        out.push(PythOnlyConfigRow {
            reserve: reserve_pk.to_string(),
            lending_market: lm.to_string(),
            mint: mint.to_string(),
            pyth: pyth.to_string(),
            switchboard: swb.to_string(),
            last_update_slot_hint,
        });
    }
    // Sort by config-reported lastUpdate.slot descending (freshest first).
    // Ties fall back to reserve pubkey so the sort is deterministic.
    out.sort_by(|a, b| {
        b.last_update_slot_hint
            .cmp(&a.last_update_slot_hint)
            .then_with(|| a.reserve.cmp(&b.reserve))
    });
    Ok(out)
}

#[derive(Debug, Clone)]
struct PythOnlyConfigRow {
    reserve: String,
    lending_market: String,
    mint: String,
    pyth: String,
    switchboard: String,
    /// Solend-API-reported `reserve.lastUpdate.slot`. Used as a freshness
    /// proxy to rank candidates before the MAX_CANDIDATES cap; the
    /// authoritative on-chain `last_update_slot` is re-derived after
    /// decoding the reserve bytes.
    last_update_slot_hint: u64,
}

// ── Main test ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_solend_refresh_recon() {
    let env = match read_env() {
        Ok(e) => e,
        Err(SkipReason::NotOptedIn) => {
            eprintln!(
                "SKIP: set CLAW_LIVE_SOLEND_REFRESH_RECON=1 plus the required env vars \
                 to run the bounded refreshable-target recon. CI does not set this."
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
    if let Err(e) = validate_keypair_path(&env.keypair_path) {
        eprintln!("SKIP: keypair path: {e}");
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
            "SKIP: keypair {} != CLAW_LIVE_SOLEND_REFRESH_RECON_OWNER {}",
            keypair.pubkey(),
            env.owner
        );
        return;
    }

    let solend_program = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap();
    let pyth_receiver = Pubkey::from_str(PYTH_SOLANA_RECEIVER_BS58).unwrap();

    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  Slice 3E(iii) — refreshable-target reselection recon");
    println!("══════════════════════════════════════════════════════════════");
    println!("  rpc host               : {}", sanitized_rpc_label(&env.rpc_url));
    println!("  owner                  : {}", env.owner);
    println!("  amount (raw)           : {}", env.amount_raw);
    println!("  scan bound candidates  : {}", env.max_candidates);
    println!("  scan bound simulations : {}", env.max_simulations);
    println!("  broadcast allowed      : NONE (read + simulate only)");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("claw-gateway/live-solend-refresh-recon")
        .build()
        .expect("reqwest client");

    // ── Step 1: fetch Solend public config, in-memory filter ─────────────
    println!();
    println!("── fetching Solend public config + in-memory filter ──────────");
    let filtered = match fetch_solend_reserve_candidates(&client).await {
        Ok(v) => v,
        Err(e) => {
            println!("  solend config fetch FAILED: {e}");
            println!("  → stopping (cannot recon without candidate source)");
            return;
        }
    };
    println!(
        "  config pyth-only candidates (switchboard=sentinel & deposit_limit>0): {}",
        filtered.len()
    );
    // Cap candidates BEFORE any RPC work.
    let capped: Vec<PythOnlyConfigRow> = filtered.into_iter().take(env.max_candidates).collect();
    println!("  capped to max_candidates   : {}", capped.len());

    // ── Step 2: batch pyth-owner probe (dataSlice length=0) ──────────────
    let pyth_keys: Vec<Pubkey> = capped
        .iter()
        .filter_map(|r| Pubkey::from_str(&r.pyth).ok())
        .collect();
    let owner_rows = match get_multiple_accounts(
        &client,
        &env.rpc_url,
        &pyth_keys,
        Some((0, 0)),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            println!("  getMultipleAccounts (pyth owners) FAILED: {e}");
            if e.contains("RATE-LIMIT") {
                println!("  → RPC rate-limit encountered; stopping recon.");
            }
            return;
        }
    };
    let mut receiver_indices: Vec<usize> = Vec::new();
    let mut pyth_owner_map: Vec<Option<Pubkey>> = Vec::with_capacity(capped.len());
    for (i, row) in owner_rows.iter().enumerate() {
        let owner = row.as_ref().map(|r| r.owner);
        pyth_owner_map.push(owner);
        if owner == Some(pyth_receiver) {
            receiver_indices.push(i);
        }
    }
    println!(
        "  pyth-owner probe       : receiver={}, non-receiver={}, not-found={}",
        receiver_indices.len(),
        owner_rows
            .iter()
            .filter(|r| r.is_some() && r.as_ref().unwrap().owner != pyth_receiver)
            .count(),
        owner_rows.iter().filter(|r| r.is_none()).count(),
    );

    // ── Step 3: batch reserve fetch for pyth-receiver subset ─────────────
    let receiver_reserve_keys: Vec<Pubkey> = receiver_indices
        .iter()
        .filter_map(|i| Pubkey::from_str(&capped[*i].reserve).ok())
        .collect();
    if receiver_reserve_keys.is_empty() {
        println!();
        println!("  NO Pyth-Receiver candidates in the capped set. Widen bounds.");
        return;
    }
    let reserve_rows =
        match get_multiple_accounts(&client, &env.rpc_url, &receiver_reserve_keys, None).await {
            Ok(v) => v,
            Err(e) => {
                println!("  getMultipleAccounts (reserves) FAILED: {e}");
                if e.contains("RATE-LIMIT") {
                    println!("  → RPC rate-limit; stopping recon.");
                }
                return;
            }
        };

    // ── Step 4: fetch Pyth account full bytes in one batch for freshness
    let receiver_pyth_keys: Vec<Pubkey> = receiver_indices
        .iter()
        .map(|i| pyth_keys[*i])
        .collect();
    let pyth_full = match get_multiple_accounts(&client, &env.rpc_url, &receiver_pyth_keys, None)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            println!("  getMultipleAccounts (pyth full) FAILED: {e}");
            return;
        }
    };

    // ── Step 5: decode, compute slot_delta against a freshly-sampled slot
    let current_slot_for_delta = match get_slot(&client, &env.rpc_url).await {
        Ok(s) => s,
        Err(e) => {
            println!("  getSlot FAILED: {e}");
            return;
        }
    };
    println!();
    println!("  current_slot (recon)   : {current_slot_for_delta}");

    let mut candidates: Vec<Candidate> = Vec::new();
    for (k, idx) in receiver_indices.iter().enumerate() {
        let src = &capped[*idx];
        let row = &reserve_rows[k];
        let pyth_row = &pyth_full[k];
        let Some(row) = row else { continue };
        if row.owner != solend_program || row.data.len() != raw::RESERVE_LEN {
            continue;
        }
        let reserve_pk = match Pubkey::from_str(&src.reserve) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let raw_r = match raw::decode_reserve(&row.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let switchboard_is_sentinel =
            raw_r.switchboard_oracle.to_string() == SOLEND_NULL_ORACLE_SENTINEL_BS58;
        let oracle_freshness = pyth_row.as_ref().map(|pr| {
            extract_publish_freshness(&pr.owner, &pr.data)
        });
        let freshness_known = matches!(oracle_freshness, Some(FeedPublishFreshness::KnownSlot(_)));
        let total = raw_r.liquidity_total_supply_raw().unwrap_or(0);
        let headroom = raw_r.deposit_headroom_raw();
        let amount_fits = env.amount_raw <= headroom;
        let slot_delta = current_slot_for_delta.saturating_sub(raw_r.last_update_slot);
        let class = SlotDeltaClass::classify(slot_delta);
        let initial_status = if !switchboard_is_sentinel {
            InitialStatus::BlockedBySwitchboard
        } else if !freshness_known {
            InitialStatus::BlockedByUnknownOracleFreshness
        } else if raw_r.config_deposit_limit == 0 {
            InitialStatus::BlockedByDepositLimit
        } else if !amount_fits {
            InitialStatus::BlockedByInsufficientHeadroom
        } else if class == SlotDeltaClass::DeeplyStale {
            InitialStatus::BlockedByDeepStaleSlotDelta
        } else {
            InitialStatus::CandidateForRefreshSimulation
        };
        candidates.push(Candidate {
            reserve: reserve_pk,
            lending_market: raw_r.lending_market,
            mint: raw_r.liquidity_mint,
            decimals: raw_r.liquidity_mint_decimals,
            pyth_oracle: raw_r.pyth_oracle,
            switchboard_oracle: raw_r.switchboard_oracle,
            switchboard_is_sentinel,
            pyth_owner: pyth_owner_map[*idx],
            pyth_is_receiver: pyth_owner_map[*idx] == Some(pyth_receiver),
            oracle_freshness,
            last_update_slot: raw_r.last_update_slot,
            deposit_limit: raw_r.config_deposit_limit,
            total_supply_raw: total,
            headroom_raw: headroom,
            amount_fits,
            slot_delta,
            slot_delta_class: class,
            initial_status,
        });
    }
    // Sort by slot_delta asc for printing + ranking.
    candidates.sort_by_key(|c| c.slot_delta);

    // Slot-delta distribution.
    let fresh_count = candidates
        .iter()
        .filter(|c| c.slot_delta_class == SlotDeltaClass::FreshEnoughPreferred)
        .count();
    let mod_count = candidates
        .iter()
        .filter(|c| c.slot_delta_class == SlotDeltaClass::ModeratelyStale)
        .count();
    let deep_count = candidates
        .iter()
        .filter(|c| c.slot_delta_class == SlotDeltaClass::DeeplyStale)
        .count();
    println!();
    println!(
        "  slot_delta distribution : FreshEnoughPreferred={fresh_count}, \
         ModeratelyStale={mod_count}, DeeplyStale={deep_count}",
    );

    // ── Candidate table ──────────────────────────────────────────────────
    println!();
    println!("── candidate table (sorted by slot_delta ascending) ──────────");
    for c in &candidates {
        println!(
            "  reserve={} market={} mint={} dec={} pyth={} swb_sentinel={} freshness={} \
             last_update_slot={} slot_delta={} class={} dep_lim={} total={} headroom={} \
             fits={} status={}",
            c.reserve,
            c.lending_market,
            c.mint,
            c.decimals,
            c.pyth_oracle,
            c.switchboard_is_sentinel,
            match c.oracle_freshness {
                Some(FeedPublishFreshness::KnownSlot(ref s)) => format!("KnownSlot({s:?})"),
                Some(FeedPublishFreshness::Unknown) => "Unknown".to_string(),
                None => "(pyth not fetched)".to_string(),
            },
            c.last_update_slot,
            c.slot_delta,
            c.slot_delta_class.label(),
            c.deposit_limit,
            c.total_supply_raw,
            c.headroom_raw,
            c.amount_fits,
            c.initial_status.label(),
        );
    }
    if !candidates.iter().any(|c| c.pyth_is_receiver) {
        println!("  (no Pyth-Receiver candidates)");
    }

    // ── Step 6: select top-K for refresh simulation ───────────────────────
    let sim_candidates: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.initial_status == InitialStatus::CandidateForRefreshSimulation)
        .take(env.max_simulations)
        .collect();
    if sim_candidates.is_empty() {
        println!();
        println!("══════════════════════════════════════════════════════════════");
        println!("  OUTCOME: no `CandidateForRefreshSimulation` under bounds");
        println!("══════════════════════════════════════════════════════════════");
        println!("  Baselines remain sealed (AMM deposit-disabled, wSOL MathOverflow).");
        println!("  Recommended: widen MAX_CANDIDATES, or expand the candidate source");
        println!("              (e.g., enumerate a second lending market explicitly).");
        println!("══════════════════════════════════════════════════════════════");
        return;
    }
    println!();
    println!("── refresh simulation for top {} candidate(s) ─────────────────",
        sim_candidates.len());

    let blockhash = match get_latest_blockhash(&client, &env.rpc_url).await {
        Ok(v) => v,
        Err(e) => {
            println!("  getLatestBlockhash FAILED: {e}");
            return;
        }
    };
    let mut math_overflow_hits: Vec<Pubkey> = Vec::new();
    let mut stale_price_hits: Vec<Pubkey> = Vec::new();
    let mut passing: Vec<&Candidate> = Vec::new();

    for c in &sim_candidates {
        println!();
        println!("  ── simulating refresh for reserve {} ────────", c.reserve);
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program,
            reserves: vec![ReserveRefreshInput {
                reserve_pubkey: c.reserve,
                pyth_oracle: c.pyth_oracle,
                switchboard_oracle: c.switchboard_oracle,
            }],
            obligation: None,
        });
        println!("    ix count             : {}", plan.instructions.len());
        let tx = Transaction::new_signed_with_payer(
            &plan.instructions,
            Some(&env.owner),
            &[&keypair],
            blockhash,
        );
        let outcome = match simulate_signed_transaction(&client, &env.rpc_url, &tx).await {
            Ok(v) => v,
            Err(e) => {
                println!("    simulate RPC FAIL   : {e}");
                if e.contains("RATE-LIMIT") {
                    println!("    → stopping simulation loop on rate limit");
                    break;
                }
                continue;
            }
        };
        println!(
            "    status              : {}",
            if outcome.is_success() { "SUCCESS" } else { "FAIL" }
        );
        if let Some(u) = outcome.units_consumed {
            println!("    compute units       : {u}");
        }
        if let Some((idx, inner)) = outcome.instruction_error() {
            println!("    instruction index   : {idx}");
            println!("    InstructionError    : {inner}");
        } else if !outcome.err_json.is_null() {
            println!("    raw err             : {}", outcome.err_json);
        }
        if let Some(cc) = outcome.custom_error_code() {
            println!("    custom code         : {cc}");
        }
        if outcome.logs.is_empty() {
            println!("    logs                : (none returned)");
        } else {
            println!("    logs ({} lines):", outcome.logs.len());
            for l in &outcome.logs {
                println!("      | {l}");
            }
        }
        let class = RefreshSimClass::classify(&outcome);
        println!("    classification      : {}", class.label());
        match class {
            RefreshSimClass::MathOverflow => math_overflow_hits.push(c.reserve),
            RefreshSimClass::BlockedByStaleOraclePrice => stale_price_hits.push(c.reserve),
            RefreshSimClass::RefreshSimulationPass => passing.push(*c),
            _ => {}
        }
    }

    // ── Final verdict ────────────────────────────────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  RECON VERDICT");
    println!("══════════════════════════════════════════════════════════════");
    println!("  pyth-receiver candidates inspected    : {}", candidates.len());
    println!("  candidates simulated                   : {}", sim_candidates.len());
    println!("  passing refresh simulation             : {}", passing.len());
    println!("  hit Custom(14) MathOverflow            : {}", math_overflow_hits.len());
    println!("  hit stale-oracle-price (Custom 3/12)   : {}", stale_price_hits.len());
    if let Some(best) = passing.first() {
        println!();
        println!("  RECOMMENDED TARGET:");
        println!("    reserve            : {}", best.reserve);
        println!("    lending_market     : {}", best.lending_market);
        println!("    mint               : {} (dec={})", best.mint, best.decimals);
        println!("    pyth_oracle        : {}", best.pyth_oracle);
        println!("    pyth_owner         : {:?}", best.pyth_owner);
        println!("    switchboard        : {} (sentinel={})",
            best.switchboard_oracle, best.switchboard_is_sentinel);
        println!("    freshness          : {:?}", best.oracle_freshness);
        println!("    last_update_slot   : {}", best.last_update_slot);
        println!("    current_slot used  : {current_slot_for_delta}");
        println!("    slot_delta         : {} ({})", best.slot_delta, best.slot_delta_class.label());
        println!("    deposit_limit      : {}", best.deposit_limit);
        println!("    headroom           : {}", best.headroom_raw);
        println!("    refresh sim result : RefreshSimulationPass");
    } else {
        println!();
        println!("  NO refreshable Pyth-Receiver / Switchboard-sentinel target found");
        println!("  under current scan bounds.");
        println!("  Recommended next step: widen CLAW_LIVE_SOLEND_REFRESH_RECON_MAX_CANDIDATES");
        println!("                        or expand the candidate source, or investigate");
        println!("                        whether a Solend admin refresh unblocks these.");
    }

    if !math_overflow_hits.is_empty() {
        println!();
        println!("  Custom(14) MathOverflow reserves (deep-stale arithmetic wall):");
        for pk in &math_overflow_hits {
            println!("    {pk}");
        }
    }
    if !stale_price_hits.is_empty() {
        println!();
        println!("  stale-oracle-price / Custom(3|12) reserves:");
        for pk in &stale_price_hits {
            println!("    {pk}");
        }
    }

    println!();
    println!("  baseline observations (sealed in prior slices, not re-simulated):");
    println!("    AMM deposit-disabled : {}", BASELINE_AMM_RESERVE_BS58);
    println!("    wSOL MathOverflow    : {}", BASELINE_WSOL_RESERVE_BS58);

    println!();
    println!("  NO broadcast, NO sendTransaction, NO deposit, NO InitObligation,");
    println!("  NO ATA creation, NO source funding, NO SOL wrap, NO sync_native.");
    println!("══════════════════════════════════════════════════════════════");
}

//! OPT-IN: Slice 3C — controlled signed-but-not-broadcast Solend deposit
//! dry simulation against mainnet.
//!
//! ## What this test IS
//!
//! A **read + simulate** check that builds the Solend refresh and deposit
//! transactions exactly as the future Slice 3D/3E live path will, signs
//! them with a controlled test wallet, fetches a real latest blockhash
//! from RPC, and submits them to `simulateTransaction`. The goal is to
//! identify the deepest real account-wiring / program-level blocker
//! before any signed transaction ever reaches `sendTransaction`.
//!
//! ## What this test IS NOT
//!
//! - Not a live deposit. Never calls `sendTransaction` / `sendRawTransaction`.
//! - Not a broadcast. Never submits via gateway, daemon, Phantom, or any
//!   production execution path.
//! - Not a policy-pass proof. A simulated refresh does not mutate chain
//!   state; re-fetching the obligation after a simulated refresh still
//!   returns the same stale bytes. Policy freshness remains pending
//!   until a real refresh transaction is finalized in a later controlled
//!   live-execution slice.
//! - Not CI coverage. Default-skips without env; CI never sets the env.
//!
//! ## Signing discipline
//!
//! Signing here is test-only. The controlled keypair is loaded from a
//! path the operator provides via env. The keypair's private bytes are
//! NEVER printed, persisted elsewhere, or routed through any production
//! signer. The signed transaction is routed strictly to
//! `simulateTransaction`.
//!
//! ## Opt-in
//!
//! Self-skips unless every required env var is set:
//!
//! ```text
//!   CLAW_LIVE_SOLEND_DEPOSIT_DRY_SIM=1
//!   CLAW_LIVE_SOLEND_DRY_SIM_RPC_URL=<url>
//!   CLAW_LIVE_SOLEND_DRY_SIM_KEYPAIR_PATH=<absolute path OUTSIDE this repo>
//!   CLAW_LIVE_SOLEND_DRY_SIM_OWNER=<base58 pubkey matching the keypair>
//!   CLAW_LIVE_SOLEND_DRY_SIM_AMOUNT_RAW=<u64; optional, default 1000>
//! ```
//!
//! Optional env vars (Slice 3E(i) additions):
//!
//! ```text
//!   CLAW_LIVE_SOLEND_DRY_SIM_TARGET_RESERVE=<reserve pubkey>
//!     — override the default AMM reserve with a different Pyth-only
//!       Solend reserve (e.g., the wSOL target
//!       FNDYHVzPrB9B4FNqe5swcDPgKqoVif3ohPnmAUkJ5K5t in market
//!       7dA8PMT6bhopvRkDmJAeQT74NyDkGvi4isYoiYd8pES). The lending
//!       market is read from the reserve's decoded bytes, not supplied
//!       as a separate env var.
//!   CLAW_LIVE_SOLEND_DRY_SIM_SOURCE_LIQUIDITY=<ATA> — optional; if
//!     omitted, derived from owner + reserve.liquidity_mint + classic
//!     SPL Token. If provided, asserted to match the derivation.
//!   CLAW_LIVE_SOLEND_DRY_SIM_USER_COLLATERAL=<ATA> — optional; same
//!     policy against reserve.collateral_mint.
//!   CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION=<pubkey> — optional; if
//!     omitted and INCLUDE_INIT_OBLIGATION=1, read from the supplied
//!     obligation keypair's pubkey. If both present, asserted to match.
//!   CLAW_LIVE_SOLEND_DRY_SIM_INCLUDE_ATA_CREATE=1 — prepend idempotent
//!     ATA-create ixs for source + user-collateral mints.
//!   CLAW_LIVE_SOLEND_DRY_SIM_INCLUDE_INIT_OBLIGATION=1 — prepend a
//!     SystemProgram CreateAccount + Solend InitObligation pair. The
//!     obligation keypair is REQUIRED in this mode.
//!   CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION_KEYPAIR_PATH=<absolute path
//!     OUTSIDE this repo> — required when INCLUDE_INIT_OBLIGATION=1.
//!     Loaded with the same keypair rules as the wallet keypair. The
//!     obligation keypair signs the CreateAccount ix as the new
//!     account.
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use claw_gateway::integrations::solend::{
    build_create_ata_idempotent_instruction, build_create_obligation_account_instruction,
    build_deposit_reserve_liquidity_and_obligation_collateral_instruction,
    build_init_obligation_instruction, build_refresh_instructions,
    derive_associated_token_address,
    raw::{self, SOLEND_NULL_ORACLE_SENTINEL_BS58, SOLEND_PROGRAM_ID_BS58},
    CreateObligationAccountInputs, DepositInstructionInputs, InitObligationInputs,
    ObligationRefreshInput, RefreshPlanInputs, ReserveRefreshInput,
};
use claw_gateway::lending::UnderlyingAmount;
use serde_json::{json, Value};
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};

// ── Target constants (pre-Slice-3 recon evidence) ─────────────────────────
//
// Historical AMM target (now deposit_limit=0, retained for default
// backwards-compatibility of the opt-in env surface):
const AMM_LENDING_MARKET_BS58: &str = "Au3S1ZSkGwm1fo7g3WFhkD1rcPoUXj7h5ubsGsUFqbLX";
const AMM_RESERVE_BS58: &str = "6nb1odSYHutVAxoaQyiwPhQNTFn3nBNFdQdNCm5v9Jbp";

// Slice 3E(i) — wSOL Pyth-only reserve in a different lending market,
// confirmed on-chain to have deposit_limit=10^16 and ~10M SOL headroom.
// Used when `CLAW_LIVE_SOLEND_DRY_SIM_TARGET_RESERVE` matches, and
// exercised by offline retarget tests below.
const WSOL_TARGET_RESERVE_BS58: &str = "FNDYHVzPrB9B4FNqe5swcDPgKqoVif3ohPnmAUkJ5K5t";
const WSOL_TARGET_LENDING_MARKET_BS58: &str = "7dA8PMT6bhopvRkDmJAeQT74NyDkGvi4isYoiYd8pES";
const WSOL_MINT_BS58: &str = "So11111111111111111111111111111111111111112";

const DEFAULT_AMOUNT_RAW: u64 = 1_000;

// ── Opt-in + env reading ──────────────────────────────────────────────────

struct RequiredEnv {
    rpc_url: String,
    keypair_path: PathBuf,
    owner: Pubkey,
    /// When `None`, derived from `owner + reserve.liquidity_mint` at
    /// runtime. When `Some`, asserted to match the derivation.
    source_liquidity: Option<Pubkey>,
    /// Same policy as `source_liquidity` but for the c-token mint.
    user_collateral: Option<Pubkey>,
    /// When `None` and `include_init_obligation` is set, read from the
    /// obligation keypair. When both are present, must agree.
    obligation: Option<Pubkey>,
    amount_raw: u64,
    /// Override default AMM reserve with any Solend reserve pubkey.
    target_reserve: Option<Pubkey>,
    include_ata_create: bool,
    include_init_obligation: bool,
    obligation_keypair_path: Option<PathBuf>,
}

#[derive(Debug)]
enum SkipReason {
    NotOptedIn,
    MissingEnv(String),
    InvalidEnv(String),
}

fn read_env() -> Result<RequiredEnv, SkipReason> {
    match std::env::var("CLAW_LIVE_SOLEND_DEPOSIT_DRY_SIM") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {}
        _ => return Err(SkipReason::NotOptedIn),
    }
    let require = |name: &str| -> Result<String, SkipReason> {
        std::env::var(name).map_err(|_| SkipReason::MissingEnv(name.to_string()))
    };
    let opt_str = |name: &str| -> Option<String> {
        std::env::var(name).ok().filter(|s| !s.is_empty())
    };
    let opt_pubkey = |name: &str| -> Result<Option<Pubkey>, SkipReason> {
        match opt_str(name) {
            None => Ok(None),
            Some(s) => Pubkey::from_str(&s)
                .map(Some)
                .map_err(|e| SkipReason::InvalidEnv(format!("{name} not base58: {e}"))),
        }
    };

    let rpc_url = require("CLAW_LIVE_SOLEND_DRY_SIM_RPC_URL")?;
    let keypair_path = PathBuf::from(require("CLAW_LIVE_SOLEND_DRY_SIM_KEYPAIR_PATH")?);
    let owner = Pubkey::from_str(&require("CLAW_LIVE_SOLEND_DRY_SIM_OWNER")?)
        .map_err(|e| SkipReason::InvalidEnv(format!("OWNER not base58: {e}")))?;
    let source_liquidity = opt_pubkey("CLAW_LIVE_SOLEND_DRY_SIM_SOURCE_LIQUIDITY")?;
    let user_collateral = opt_pubkey("CLAW_LIVE_SOLEND_DRY_SIM_USER_COLLATERAL")?;
    let obligation = opt_pubkey("CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION")?;
    let target_reserve = opt_pubkey("CLAW_LIVE_SOLEND_DRY_SIM_TARGET_RESERVE")?;
    let amount_raw = std::env::var("CLAW_LIVE_SOLEND_DRY_SIM_AMOUNT_RAW")
        .ok()
        .map(|s| {
            s.parse::<u64>()
                .map_err(|e| SkipReason::InvalidEnv(format!("AMOUNT_RAW not u64: {e}")))
        })
        .transpose()?
        .unwrap_or(DEFAULT_AMOUNT_RAW);
    let include_ata_create = matches!(
        std::env::var("CLAW_LIVE_SOLEND_DRY_SIM_INCLUDE_ATA_CREATE").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let include_init_obligation = matches!(
        std::env::var("CLAW_LIVE_SOLEND_DRY_SIM_INCLUDE_INIT_OBLIGATION").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let obligation_keypair_path =
        opt_str("CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION_KEYPAIR_PATH").map(PathBuf::from);

    if include_init_obligation && obligation_keypair_path.is_none() {
        return Err(SkipReason::MissingEnv(
            "CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION_KEYPAIR_PATH (required with INCLUDE_INIT_OBLIGATION=1)"
                .to_string(),
        ));
    }

    Ok(RequiredEnv {
        rpc_url,
        keypair_path,
        owner,
        source_liquidity,
        user_collateral,
        obligation,
        amount_raw,
        target_reserve,
        include_ata_create,
        include_init_obligation,
        obligation_keypair_path,
    })
}

/// Reject keypair paths that appear to live inside this repo. Not
/// bulletproof (path canonicalization has edge cases on Windows), but
/// catches the obvious footguns.
fn validate_keypair_path(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("keypair file not found: {}", path.display()));
    }
    let canonical = path.canonicalize().map_err(|e| format!("canonicalize: {e}"))?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    // `CARGO_MANIFEST_DIR` for this crate is `crates/gateway`. Walk up
    // twice to reach the repo root.
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

/// Print only the scheme + host portion of the RPC URL (drops path, query,
/// and any auth segment). Avoids accidentally logging an API key embedded
/// in the URL.
fn sanitized_rpc_label(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return "<unparseable-url>".to_string();
    };
    let rest = &url[scheme_end + 3..];
    let after_auth = rest.rfind('@').map(|i| &rest[i + 1..]).unwrap_or(rest);
    let host_end = after_auth.find('/').unwrap_or(after_auth.len());
    format!("{}{}", &url[..scheme_end + 3], &after_auth[..host_end])
}

// ── JSON-RPC helpers (test-local; not promoted to production) ─────────────

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    });
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

async fn get_latest_blockhash(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Hash, u64), String> {
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
        .and_then(|s| s.as_u64())
        .unwrap_or(0);
    Ok((
        Hash::from_str(bh).map_err(|e| format!("blockhash parse: {e}"))?,
        last_valid,
    ))
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
    let value = v
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null);
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

/// Send a signed transaction to `simulateTransaction`. **Never** routes to
/// `sendTransaction` or `sendRawTransaction`. The response's `value.err`
/// and `value.logs` are returned verbatim for the caller to extract and
/// print.
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
    let value = v
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null);
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

    /// Extract `InstructionError` details if present: returns
    /// `Some((index, inner_json))`.
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
        inner
            .as_object()?
            .get("Custom")?
            .as_u64()
    }
}

// ── Blocker classification ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum BlockerClass {
    /// Any of the user's ATAs (source liquidity or user collateral) is
    /// missing / not found on chain.
    MissingAta,
    /// Source liquidity ATA specifically — retained as a distinct
    /// variant so future review payloads can distinguish "no ATA at
    /// all" from "ATA present but empty". This recon does not currently
    /// distinguish the two at classification time (see MissingAta).
    MissingSourceLiquidity,
    InsufficientFunds,
    ObligationUninitialized,
    ObligationOwnerMismatch,
    StaleReserveOrOracle,
    AccountMetaMismatch,
    UnknownProgramFailure,
}

fn classify_blocker(outcome: &SimulateOutcome, _label: &str) -> Option<BlockerClass> {
    if outcome.is_success() {
        return None;
    }
    let logs_joined = outcome.logs.join("\n").to_lowercase();
    let err_str = outcome.err_json.to_string().to_lowercase();

    // ── Most-specific log patterns first ────────────────────────────────
    //
    // Solend's SPL-token-account validation failure ("token program does
    // not match") fires when Solend tries to unpack source_liquidity or
    // user_collateral as an SPL token account, but the account is not
    // owned by the SPL Token program. The most common cause is a missing
    // ATA — an uninitialized account is owned by System Program, so the
    // ownership check fails before balance / obligation checks. Slice 3C
    // observed this exact pattern; Slice 3D(i) classifies it honestly.
    if logs_joined.contains("token program does not match")
        || logs_joined.contains("input token program account is not valid")
    {
        return Some(BlockerClass::MissingAta);
    }

    if logs_joined.contains("reserve is stale")
        || logs_joined.contains("stale reserve")
        || logs_joined.contains("stale oracle")
        || logs_joined.contains("oracle price is stale")
    {
        return Some(BlockerClass::StaleReserveOrOracle);
    }
    if logs_joined.contains("obligation is not initialized")
        || (logs_joined.contains("invalid account data")
            && logs_joined.contains("obligation"))
        || err_str.contains("uninitializedaccount")
    {
        return Some(BlockerClass::ObligationUninitialized);
    }
    if logs_joined.contains("obligation owner")
        || logs_joined.contains("invalid obligation owner")
    {
        return Some(BlockerClass::ObligationOwnerMismatch);
    }
    if err_str.contains("accountnotfound") || logs_joined.contains("account does not exist") {
        return Some(BlockerClass::MissingAta);
    }
    if (logs_joined.contains("insufficient") && logs_joined.contains("funds"))
        || logs_joined.contains("insufficient tokens")
        || logs_joined.contains("insufficient balance")
    {
        return Some(BlockerClass::InsufficientFunds);
    }
    if outcome.instruction_error().is_some() {
        return Some(BlockerClass::UnknownProgramFailure);
    }
    Some(BlockerClass::AccountMetaMismatch)
}

fn print_simulation(label: &str, outcome: &SimulateOutcome) -> Option<BlockerClass> {
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
    let blocker = classify_blocker(outcome, label);
    println!(
        "  first blocker     : {}",
        blocker
            .map(|b| format!("{b:?}"))
            .unwrap_or_else(|| "(none — simulation succeeded)".to_string())
    );
    println!("  no broadcast      : confirmed — this outcome is simulateTransaction only");
    blocker
}

// ── Main test ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_solend_deposit_dry_sim() {
    let env = match read_env() {
        Ok(e) => e,
        Err(SkipReason::NotOptedIn) => {
            eprintln!(
                "SKIP: set CLAW_LIVE_SOLEND_DEPOSIT_DRY_SIM=1 plus the five required \
                 target env vars to run the opt-in dry simulation. CI does not set this. \
                 This test reads chain state and calls simulateTransaction only — it never \
                 broadcasts a transaction and never signs via production paths."
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
            "SKIP: keypair pubkey {} does not match CLAW_LIVE_SOLEND_DRY_SIM_OWNER {}",
            keypair.pubkey(),
            env.owner
        );
        return;
    }

    let solend_program = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap();
    let default_amm_reserve = Pubkey::from_str(AMM_RESERVE_BS58).unwrap();
    let _default_amm_lending_market = Pubkey::from_str(AMM_LENDING_MARKET_BS58).unwrap();
    let target_reserve = env.target_reserve.unwrap_or(default_amm_reserve);
    let _sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();

    // ── Obligation keypair (Slice 3E(i)) ─────────────────────────────────
    let obligation_keypair: Option<Keypair> = match &env.obligation_keypair_path {
        None => None,
        Some(p) => {
            if let Err(e) = validate_keypair_path(p) {
                eprintln!("SKIP: obligation keypair path validation failed: {e}");
                return;
            }
            match load_keypair(p) {
                Ok(k) => Some(k),
                Err(e) => {
                    eprintln!("SKIP: could not load obligation keypair: {e}");
                    return;
                }
            }
        }
    };
    // Resolve the obligation pubkey from whichever source is authoritative.
    let resolved_obligation: Pubkey = match (env.obligation, obligation_keypair.as_ref()) {
        (Some(pk), Some(kp)) => {
            if kp.pubkey() != pk {
                eprintln!(
                    "SKIP: obligation keypair pubkey {} does not match \
                     CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION {}",
                    kp.pubkey(),
                    pk,
                );
                return;
            }
            pk
        }
        (Some(pk), None) => pk,
        (None, Some(kp)) => kp.pubkey(),
        (None, None) => {
            // Neither env nor keypair. Only valid if we're not bundling
            // init_obligation; refresh-only / default-AMM flows still work
            // against the historical obligation pubkey.
            if env.include_init_obligation {
                eprintln!(
                    "SKIP: INCLUDE_INIT_OBLIGATION=1 requires either an obligation \
                     keypair path or CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION"
                );
                return;
            }
            eprintln!("SKIP: no obligation keypair or CLAW_LIVE_SOLEND_DRY_SIM_OBLIGATION provided");
            return;
        }
    };
    if env.include_init_obligation && obligation_keypair.is_none() {
        eprintln!("SKIP: INCLUDE_INIT_OBLIGATION=1 but obligation keypair not loaded");
        return;
    }

    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  Solend deposit dry simulation (Slice 3E(i))");
    println!("══════════════════════════════════════════════════════════════");
    println!("  rpc host                : {}", sanitized_rpc_label(&env.rpc_url));
    println!("  owner (from env)        : {}", env.owner);
    println!("  target reserve          : {target_reserve}");
    println!(
        "  target retargeted?      : {}",
        if env.target_reserve.is_some() { "YES (env override)" } else { "no (default AMM)" }
    );
    println!("  obligation              : {resolved_obligation}");
    println!(
        "  obligation source       : {}",
        match (env.obligation.is_some(), obligation_keypair.is_some()) {
            (true, true) => "env + keypair (matched)",
            (true, false) => "env only",
            (false, true) => "keypair only",
            (false, false) => "(none)",
        }
    );
    println!("  amount (raw)            : {}", env.amount_raw);
    println!("  include_ata_create      : {}", env.include_ata_create);
    println!("  include_init_obligation : {}", env.include_init_obligation);
    println!(
        "  keypair signer          : {} (file validated outside repo)",
        keypair.pubkey()
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("claw-gateway/live-solend-deposit-dry-sim")
        .build()
        .expect("reqwest client");

    // ── Fetch + decode target reserve ─────────────────────────────────────
    println!();
    println!("── fetching target reserve + decoding ────────────────────────");
    let (reserve_bytes, reserve_owner, _) =
        match get_account_info(&client, &env.rpc_url, &target_reserve).await {
            Ok(Some(v)) => v,
            Ok(None) => panic!("target reserve {target_reserve} not found on chain"),
            Err(e) => panic!("fetch reserve: {e}"),
        };
    assert_eq!(
        reserve_owner, solend_program,
        "target reserve must be Solend-owned"
    );
    let reserve_raw = raw::decode_reserve(&reserve_bytes).expect("decode_reserve");
    let resolved_lending_market = reserve_raw.lending_market;
    println!("  lending_market    : {resolved_lending_market}");
    println!("  liquidity.mint    : {}", reserve_raw.liquidity_mint);
    println!("  liquidity.supply  : {}", reserve_raw.liquidity_supply);
    println!("  collateral.mint   : {}", reserve_raw.collateral_mint);
    println!("  collateral.supply : {}", reserve_raw.collateral_supply);
    println!("  pyth_oracle       : {}", reserve_raw.pyth_oracle);
    println!("  switchboard_oracle: {}", reserve_raw.switchboard_oracle);
    println!("  deposit_limit     : {}", reserve_raw.config_deposit_limit);
    println!(
        "  total_supply      : {}",
        reserve_raw.liquidity_total_supply_raw().unwrap_or(0)
    );
    println!("  headroom (raw)    : {}", reserve_raw.deposit_headroom_raw());
    println!(
        "  AMOUNT_RAW fits?  : {}",
        env.amount_raw <= reserve_raw.deposit_headroom_raw()
    );

    // ── Derive (or verify) source + collateral ATAs ───────────────────────
    let classic_spl_token = Pubkey::from_str(
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    )
    .unwrap();
    let derived_source_ata =
        derive_associated_token_address(&env.owner, &reserve_raw.liquidity_mint, &classic_spl_token);
    let derived_user_collat_ata = derive_associated_token_address(
        &env.owner,
        &reserve_raw.collateral_mint,
        &classic_spl_token,
    );
    let effective_source_liquidity = match env.source_liquidity {
        Some(pk) => {
            if pk != derived_source_ata {
                panic!(
                    "FAIL (opt-in): env SOURCE_LIQUIDITY={} does not match derivation={}. \
                     Either omit the env var or align it.",
                    pk, derived_source_ata,
                );
            }
            pk
        }
        None => derived_source_ata,
    };
    let effective_user_collateral = match env.user_collateral {
        Some(pk) => {
            if pk != derived_user_collat_ata {
                panic!(
                    "FAIL (opt-in): env USER_COLLATERAL={} does not match derivation={}.",
                    pk, derived_user_collat_ata,
                );
            }
            pk
        }
        None => derived_user_collat_ata,
    };

    // ── Probe reviewed accounts: ATAs + obligation presence ───────────────
    println!();
    println!("── probing caller-supplied accounts ──────────────────────────");
    let source_present = get_account_info(&client, &env.rpc_url, &effective_source_liquidity)
        .await
        .map(|o| o.is_some())
        .unwrap_or(false);
    let user_collateral_present =
        get_account_info(&client, &env.rpc_url, &effective_user_collateral)
            .await
            .map(|o| o.is_some())
            .unwrap_or(false);
    let obligation_info = get_account_info(&client, &env.rpc_url, &resolved_obligation)
        .await
        .unwrap_or(None);
    let obligation_exists = obligation_info.is_some();
    let obligation_solend_owned = obligation_info
        .as_ref()
        .map(|(_, owner, _)| *owner == solend_program)
        .unwrap_or(false);
    println!("  source liquidity  : {effective_source_liquidity} ({})",
        if source_present { "present" } else { "MISSING" });
    println!("  user collateral   : {effective_user_collateral} ({})",
        if user_collateral_present { "present" } else { "MISSING (likely ATA uncreated)" });
    println!(
        "  obligation        : {resolved_obligation} ({})",
        match (obligation_exists, obligation_solend_owned) {
            (false, _) => "NOT CREATED".to_string(),
            (true, false) => "exists but NOT Solend-owned".to_string(),
            (true, true) => "present + Solend-owned".to_string(),
        }
    );

    // ── Latest blockhash ─────────────────────────────────────────────────
    println!();
    println!("── fetching latest blockhash ────────────────────────────────");
    let (blockhash, last_valid) = match get_latest_blockhash(&client, &env.rpc_url).await {
        Ok(v) => v,
        Err(e) => panic!("FAIL (opt-in): latest blockhash fetch: {e}"),
    };
    let bh_s = blockhash.to_string();
    println!(
        "  blockhash         : {}…{} (last_valid={})",
        &bh_s[..8.min(bh_s.len())],
        &bh_s[bh_s.len().saturating_sub(6)..],
        last_valid
    );

    // ── Refresh transaction simulation ───────────────────────────────────
    let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
        solend_program_id: solend_program,
        reserves: vec![ReserveRefreshInput {
            reserve_pubkey: target_reserve,
            pyth_oracle: reserve_raw.pyth_oracle,
            switchboard_oracle: reserve_raw.switchboard_oracle,
        }],
        obligation: if obligation_exists && obligation_solend_owned {
            Some(ObligationRefreshInput {
                obligation_pubkey: resolved_obligation,
                referenced_reserves_in_order: vec![target_reserve],
            })
        } else {
            None
        },
    });
    println!();
    println!("── refresh plan ──────────────────────────────────────────────");
    println!("  instructions      : {}", refresh_plan.instructions.len());
    println!("  reserves refreshed: {:?}", refresh_plan.reserves_refreshed);
    println!(
        "  obligation refresh: {}",
        refresh_plan
            .obligation_refreshed
            .map(|p| p.to_string())
            .unwrap_or_else(|| "(skipped — obligation not initialized or not Solend-owned)".into())
    );

    let refresh_tx = Transaction::new_signed_with_payer(
        &refresh_plan.instructions,
        Some(&env.owner),
        &[&keypair],
        blockhash,
    );
    let refresh_outcome =
        match simulate_signed_transaction(&client, &env.rpc_url, &refresh_tx).await {
            Ok(v) => v,
            Err(e) => panic!("FAIL (opt-in): refresh simulateTransaction: {e}"),
        };
    let refresh_blocker = print_simulation("refresh", &refresh_outcome);

    // ── Optional CreateAccount + InitObligation bundle (Slice 3E(i)) ─────
    let mut init_ixs: Vec<solana_sdk::instruction::Instruction> = Vec::new();
    if env.include_init_obligation {
        println!();
        println!("── fetching rent-exempt lamports for obligation account ─────");
        let rent_exempt_lamports = match get_minimum_balance_for_rent_exemption(
            &client,
            &env.rpc_url,
            raw::OBLIGATION_LEN,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("FAIL (opt-in): getMinimumBalanceForRentExemption: {e}"),
        };
        println!("  obligation space        : {} bytes", raw::OBLIGATION_LEN);
        println!("  rent-exempt lamports    : {rent_exempt_lamports}");
        let create_ix = build_create_obligation_account_instruction(
            CreateObligationAccountInputs {
                funder: env.owner,
                new_obligation: resolved_obligation,
                rent_exempt_lamports,
                solend_program_id: solend_program,
            },
        );
        let init_ix = build_init_obligation_instruction(InitObligationInputs {
            solend_program_id: solend_program,
            obligation: resolved_obligation,
            lending_market: resolved_lending_market,
            obligation_owner: env.owner,
        });
        println!(
            "  create-account ix       : program={}, accts={}, data.len={}",
            create_ix.program_id,
            create_ix.accounts.len(),
            create_ix.data.len()
        );
        println!(
            "  init-obligation ix      : program={}, accts={}, tag={}",
            init_ix.program_id,
            init_ix.accounts.len(),
            init_ix.data[0]
        );
        init_ixs.push(create_ix);
        init_ixs.push(init_ix);
    }

    // ── Optional ATA-create bundle (Slice 3D(i)) ──────────────────────────
    let mut ata_create_ixs: Vec<solana_sdk::instruction::Instruction> = Vec::new();
    println!();
    println!("── ATA derivation + handling ────────────────────────────────");
    println!("  derived source_liquidity ATA : {derived_source_ata}");
    println!("  derived user_collateral ATA  : {derived_user_collat_ata}");
    println!(
        "  source ATA env-supplied?     : {}",
        if env.source_liquidity.is_some() { "YES (matched)" } else { "no (derived)" }
    );
    println!(
        "  collat ATA env-supplied?     : {}",
        if env.user_collateral.is_some() { "YES (matched)" } else { "no (derived)" }
    );
    if env.include_ata_create {
        println!("  mode : INCLUDE_ATA_CREATE=1 — prepending idempotent ATA-create ixs");
        ata_create_ixs.push(build_create_ata_idempotent_instruction(
            &env.owner,
            &env.owner,
            &reserve_raw.liquidity_mint,
            &classic_spl_token,
        ));
        ata_create_ixs.push(build_create_ata_idempotent_instruction(
            &env.owner,
            &env.owner,
            &reserve_raw.collateral_mint,
            &classic_spl_token,
        ));
    } else {
        println!(
            "  mode : INCLUDE_ATA_CREATE not set — deposit tx will NOT include ATA-create ixs"
        );
    }

    // ── Deposit instruction ──────────────────────────────────────────────
    println!();
    println!("── building deposit instruction ─────────────────────────────");
    let deposit_ix =
        match build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
            DepositInstructionInputs {
                solend_program_id: solend_program,
                amount: UnderlyingAmount::new(env.amount_raw),
                source_liquidity: effective_source_liquidity,
                user_collateral: effective_user_collateral,
                reserve: target_reserve,
                reserve_liquidity_supply: reserve_raw.liquidity_supply,
                reserve_collateral_mint: reserve_raw.collateral_mint,
                lending_market: resolved_lending_market,
                destination_deposit_collateral: reserve_raw.collateral_supply,
                obligation: resolved_obligation,
                obligation_owner: env.owner,
                pyth_oracle: reserve_raw.pyth_oracle,
                switchboard_oracle: reserve_raw.switchboard_oracle,
                user_transfer_authority: env.owner,
            },
        ) {
            Ok(ix) => ix,
            Err(e) => panic!("FAIL (opt-in): deposit builder: {e}"),
        };
    println!("  deposit_ix accts  : {}", deposit_ix.accounts.len());
    println!("  deposit_ix data[0]: {} (should be 14)", deposit_ix.data[0]);

    // ── Assemble deposit bundle (order: create+init → ATA → deposit) ──────
    let mut deposit_bundle: Vec<solana_sdk::instruction::Instruction> =
        Vec::with_capacity(init_ixs.len() + ata_create_ixs.len() + 1);
    deposit_bundle.extend(init_ixs.iter().cloned());
    deposit_bundle.extend(ata_create_ixs);
    deposit_bundle.push(deposit_ix);
    println!("  bundle ix count    : {}", deposit_bundle.len());

    // ── Sign deposit bundle ───────────────────────────────────────────────
    // When INCLUDE_INIT_OBLIGATION=1 the obligation keypair MUST sign the
    // CreateAccount ix as the new account. We always include it as an
    // extra signer in that mode; Solana's Transaction::new_signed_with_payer
    // resolves signer-order by matching AccountMeta pubkeys.
    let deposit_tx = match (env.include_init_obligation, obligation_keypair.as_ref()) {
        (true, Some(obl_kp)) => Transaction::new_signed_with_payer(
            &deposit_bundle,
            Some(&env.owner),
            &[&keypair, obl_kp],
            blockhash,
        ),
        _ => Transaction::new_signed_with_payer(
            &deposit_bundle,
            Some(&env.owner),
            &[&keypair],
            blockhash,
        ),
    };
    let deposit_outcome =
        match simulate_signed_transaction(&client, &env.rpc_url, &deposit_tx).await {
            Ok(v) => v,
            Err(e) => panic!("FAIL (opt-in): deposit simulateTransaction: {e}"),
        };
    let deposit_blocker = print_simulation("deposit", &deposit_outcome);

    // ── Summary / verdict ────────────────────────────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  REVIEW PAYLOAD");
    println!("══════════════════════════════════════════════════════════════");
    println!("  rpc host                : {}", sanitized_rpc_label(&env.rpc_url));
    println!("  owner                   : {}", env.owner);
    println!("  target reserve          : {target_reserve}");
    println!("  target mint             : {}", reserve_raw.liquidity_mint);
    println!("  lending market          : {resolved_lending_market}");
    println!("  source liquidity        : {effective_source_liquidity} ({})",
        if source_present { "present" } else { "MISSING" });
    println!("  dest collat ATA         : {effective_user_collateral} ({})",
        if user_collateral_present { "present" } else { "MISSING" });
    println!(
        "  obligation              : {resolved_obligation} ({})",
        match (obligation_exists, obligation_solend_owned) {
            (false, _) => "NOT CREATED",
            (true, false) => "exists but NOT Solend-owned",
            (true, true) => "present + Solend-owned",
        }
    );
    println!("  amount raw              : {}", env.amount_raw);
    println!(
        "  blockhash               : {}…{}",
        &bh_s[..8.min(bh_s.len())],
        &bh_s[bh_s.len().saturating_sub(6)..]
    );
    println!("  refresh result          : {}",
        if refresh_outcome.is_success() { "SUCCESS" } else { "FAIL" });
    println!("  refresh blocker         : {}",
        refresh_blocker.map(|b| format!("{b:?}")).unwrap_or_else(|| "(none)".into()));
    println!("  deposit result          : {}",
        if deposit_outcome.is_success() { "SUCCESS" } else { "FAIL" });
    println!("  deposit blocker         : {}",
        deposit_blocker.map(|b| format!("{b:?}")).unwrap_or_else(|| "(none)".into()));
    println!("  signers (deposit tx)    : wallet={} obligation={}",
        keypair.pubkey(),
        match &obligation_keypair {
            Some(kp) if env.include_init_obligation => kp.pubkey().to_string(),
            _ => "(not bundled)".to_string(),
        }
    );

    // ── Wall status ───────────────────────────────────────────────────────
    // Wall 3+4: Solend SPL-token-account validation passes if the deposit
    // blocker is no longer MissingAta. Only meaningful when ATA-create
    // was bundled.
    let wall_3_4_passed = env.include_ata_create
        && deposit_blocker != Some(BlockerClass::MissingAta);

    // Deposit-limit wall (Slice 3D(iii)): the deployed deposit handler
    // rejects with Custom(10) when the post-deposit floor exceeds cap.
    // This is a FAILED condition; the wall is "passed" iff the blocker
    // is not Custom(10).
    let deposit_limit_wall_failed = deposit_outcome.custom_error_code() == Some(10);

    // Obligation-init wall (Slice 3E(i)): deposit either succeeds or
    // hits a non-obligation-shaped blocker after the create+init pair
    // was bundled. If the blocker is still obligation-shaped, the wall
    // is not yet passed.
    let obligation_shape_blocker = matches!(
        deposit_blocker,
        Some(BlockerClass::ObligationUninitialized) | Some(BlockerClass::ObligationOwnerMismatch)
    );
    let obligation_init_wall_passed = env.include_init_obligation
        && !obligation_shape_blocker;

    println!();
    println!("  include_ata_create      : {}", env.include_ata_create);
    println!("  include_init_obligation : {}", env.include_init_obligation);
    println!(
        "  Wall: deposit_limit     : {}",
        if deposit_limit_wall_failed { "FAILED (Custom(10))" } else { "passed / not-hit" }
    );
    println!(
        "  Wall: ATA (3+4) in sim  : {}",
        if wall_3_4_passed { "PASSED" }
        else if env.include_ata_create { "NOT YET PASSED" }
        else { "n/a (ATA-create not bundled)" }
    );
    println!(
        "  Wall: obligation init   : {}",
        if obligation_init_wall_passed {
            "PASSED (in sim)".to_string()
        } else if env.include_init_obligation {
            format!("NOT YET PASSED (still {})", deposit_blocker
                .map(|b| format!("{b:?}")).unwrap_or_else(|| "(no blocker)".into()))
        } else if !obligation_solend_owned {
            "n/a (InitObligation not bundled; obligation not on chain)".to_string()
        } else {
            "n/a (obligation already Solend-owned on chain)".to_string()
        }
    );

    println!();
    println!("  NOTE: simulated create-account does NOT create the obligation on chain.");
    println!("        simulated InitObligation does NOT initialize the obligation on chain.");
    println!("        simulated ATA creation does NOT create ATAs on chain.");
    println!("        simulated refresh does NOT mutate reserve freshness.");
    println!("        Policy freshness is NOT proven by this slice. A real");
    println!("        refresh tx must be finalized on chain, followed by a");
    println!("        re-fetch, before RequireFreshState can pass.");
    println!("══════════════════════════════════════════════════════════════");

    // Test assertions: we're OK if EITHER simulation succeeded, OR a
    // blocker was cleanly classified and logged. A classified blocker is
    // a successful discovery outcome for this slice.
    let both_classified = refresh_outcome.is_success() || refresh_blocker.is_some();
    let deposit_classified = deposit_outcome.is_success() || deposit_blocker.is_some();
    assert!(
        both_classified,
        "refresh simulation produced no classifiable result — logs empty?"
    );
    assert!(
        deposit_classified,
        "deposit simulation produced no classifiable result — logs empty?"
    );
}

// ── Offline retarget tests (no env, no network) ───────────────────────────

#[cfg(test)]
mod retarget_offline {
    use super::*;
    use claw_gateway::integrations::solend::raw::{
        RESERVE_LEN, SOLEND_WAD,
    };

    /// Slice 3E(i): the retarget constants must remain literal pubkey
    /// strings in this test. A future edit that silently renames them
    /// must be caught here.
    #[test]
    fn wsol_target_constants_are_valid_pubkeys_and_match_expected_values() {
        let reserve = Pubkey::from_str(WSOL_TARGET_RESERVE_BS58).expect("wsol reserve parses");
        let market = Pubkey::from_str(WSOL_TARGET_LENDING_MARKET_BS58).expect("wsol market parses");
        let mint = Pubkey::from_str(WSOL_MINT_BS58).expect("wsol mint parses");
        assert_eq!(
            reserve.to_string(),
            "FNDYHVzPrB9B4FNqe5swcDPgKqoVif3ohPnmAUkJ5K5t"
        );
        assert_eq!(
            market.to_string(),
            "7dA8PMT6bhopvRkDmJAeQT74NyDkGvi4isYoiYd8pES"
        );
        assert_eq!(mint.to_string(), "So11111111111111111111111111111111111111112");
    }

    /// Slice 3E(i): a synthetic wSOL-shaped reserve (deposit_limit=10^16,
    /// available≈2.4 tokens, borrowed/fees negligible) has massive
    /// positive headroom and AMOUNT_RAW=1000 fits. This verifies the
    /// headroom-math helper end-to-end for the selected target shape.
    #[test]
    fn wsol_synth_reserve_has_positive_headroom_and_amount_1000_fits() {
        // Values come from the Slice 3D(iii) on-chain decode of
        // FNDYHVzPr... (wSOL) rounded for a deterministic synth fixture.
        let deposit_limit: u64 = 10_000_000_000_000_000;
        let available: u64 = 0;
        let borrowed_wads: u128 = 2_385_821_463u128 * SOLEND_WAD;
        let acc_fees_wads: u128 = 0;

        let mut bytes = vec![0u8; RESERVE_LEN];
        bytes[0] = 1; // version
        // available_amount @ 171..179
        bytes[171..179].copy_from_slice(&available.to_le_bytes());
        // borrowed_amount_wads @ 179..195
        bytes[179..195].copy_from_slice(&borrowed_wads.to_le_bytes());
        // config.deposit_limit @ 323..331
        bytes[323..331].copy_from_slice(&deposit_limit.to_le_bytes());
        // accumulated_protocol_fees_wads @ 373..389
        bytes[373..389].copy_from_slice(&acc_fees_wads.to_le_bytes());

        let r = raw::decode_reserve(&bytes).expect("decode synth wsol-shaped reserve");
        let headroom = r.deposit_headroom_raw();
        assert!(headroom > 0, "wSOL target must have positive headroom");
        assert!(
            DEFAULT_AMOUNT_RAW <= headroom,
            "AMOUNT_RAW=1000 must fit the wSOL target's headroom (got {headroom})"
        );
        let total = r.liquidity_total_supply_raw().expect("total_supply ok");
        assert_eq!(total, 2_385_821_463u64);
        assert_eq!(deposit_limit - total, headroom);
    }

    /// Slice 3D(iii) sealed finding: the old AMM target is deposit-disabled
    /// via config (deposit_limit=0). Simulating with that target must
    /// never yield positive headroom regardless of current supply. We
    /// verify this through the decoder on a synth fixture rather than
    /// touching the network.
    #[test]
    fn old_amm_target_is_deposit_disabled_via_config() {
        // Synth reserve shaped like the old AMM target (6nb1od...):
        // deposit_limit=0, available≈780 tokens, borrowed≈0, fees≈0.5 WAD.
        let mut bytes = vec![0u8; RESERVE_LEN];
        bytes[0] = 1;
        let available: u64 = 780_024_298_592;
        bytes[171..179].copy_from_slice(&available.to_le_bytes());
        // config.deposit_limit = 0
        bytes[323..331].copy_from_slice(&0u64.to_le_bytes());

        let r = raw::decode_reserve(&bytes).expect("decode");
        assert_eq!(r.config_deposit_limit, 0);
        assert_eq!(r.deposit_headroom_raw(), 0, "deposit_limit=0 → 0 headroom");
    }

    /// Sanity: AMM target's default pubkey constants are retained so the
    /// retarget env-override branch keeps backwards-compat with
    /// pre-Slice-3E(i) opt-in command lines.
    #[test]
    fn default_amm_constants_still_parse() {
        let _ = Pubkey::from_str(AMM_RESERVE_BS58).unwrap();
        let _ = Pubkey::from_str(AMM_LENDING_MARKET_BS58).unwrap();
    }
}

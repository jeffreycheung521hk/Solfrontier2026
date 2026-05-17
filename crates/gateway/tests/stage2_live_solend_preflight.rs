//! OPT-IN: Stage 2 W5a-0 — Helius live READ-ONLY Solend account preflight.
//!
//! W5a-0 is account-only. It NEVER signs, NEVER builds a signed
//! transaction, NEVER simulates, and NEVER sends. It answers:
//!
//! - Is the selected cluster reachable through Helius standard RPC?
//! - Is the deployed `clawsol-authority` program present?
//! - Do the Solend reserve / obligation / token / oracle accounts exist?
//! - Are their owners the official Solend / SPL Token program ids?
//! - Do P5a-equivalent layout decoders accept the on-chain bytes?
//! - Does the obligation owner equal the delegated wallet (NOT the user
//!   main wallet)?
//! - Is the delegated cToken account non-zero?
//! - Does the selected cluster match the P5c pinned tuple?
//!
//! Live transaction signing / simulation / send is W5a-1 and W5b, not
//! this file.
//!
//! # Required opt-in env vars
//!
//! | Var | Purpose |
//! |---|---|
//! | `CLAW_STAGE2_LIVE_PREFLIGHT=1` | Master gate. Without it the live test self-skips. |
//! | `HELIUS_RPC_URL` *or* `CLAW_STAGE2_RPC_URL` | Helius standard RPC endpoint (NOT Helius Sender). |
//! | `CLAW_STAGE2_CLUSTER` | `mainnet-beta` \| `devnet` \| `localnet`. |
//!
//! # Optional account/env inputs (each check self-skips if absent)
//!
//! | Var | Purpose |
//! |---|---|
//! | `CLAW_STAGE2_PROGRAM_ID` | Deployed `clawsol-authority` program id |
//! | `CLAW_STAGE2_USER_PUBKEY` | User main wallet (obligation owner MUST NOT equal this) |
//! | `CLAW_STAGE2_DELEGATED_WALLET_PUBKEY` | Delegated wallet (obligation owner MUST equal this) |
//! | `CLAW_STAGE2_AUTHORIZATION_PDA` | Authorization PDA, if known |
//! | `CLAW_STAGE2_DELEGATED_OBLIGATION` | Obligation account pubkey |
//! | `CLAW_STAGE2_DELEGATED_CTOKEN_ACCOUNT` | Delegated cToken (cUSDC) SPL token account |
//! | `CLAW_STAGE2_DESTINATION_TOKEN_ACCOUNT` | Destination liquidity (USDC) SPL token account |
//!
//! # Not required
//!
//! `CLAW_STAGE2_DELEGATED_KEYPAIR_PATH` is NOT consulted in W5a-0. If
//! a keypair path is needed by a future slice (W5a-1 simulation, or W5b
//! send), that slice owns the keypair plumbing; this file is account-
//! only on purpose.
//!
//! # Concurrency / rate-limit posture
//!
//! Helius free tier rate-limits quickly. The live test must be run
//! single-threaded:
//!
//! ```text
//! cargo test -p claw-gateway --test stage2_live_solend_preflight \
//!     -- --nocapture --test-threads=1
//! ```
//!
//! The live test issues sequential RPC calls and uses a bounded retry
//! with exponential backoff for HTTP 429 / 5xx. It never retries
//! forever.
//!
//! # Safety posture (load-bearing)
//!
//! - No `send_raw_transaction` / `send_and_confirm_transaction` call.
//! - No `skipPreflight`. No Helius Sender URLs.
//! - No `signTransaction`. No keypair loaded.
//! - No Jupiter API call (`api.jup.ag` is not contacted).
//! - No state-store mutation. Read-only RPC only.
//! - All callsites are documented inline. The
//!   `no_send_path_in_this_file_default_invariant` unit test scans
//!   this very file's source for forbidden call-forms.

use std::env;
use std::str::FromStr;
use std::time::Duration;

use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

// ── env var names (single source of truth) ────────────────────────────────

const ENV_GATE: &str = "CLAW_STAGE2_LIVE_PREFLIGHT";
const ENV_RPC_PRIMARY: &str = "HELIUS_RPC_URL";
const ENV_RPC_SECONDARY: &str = "CLAW_STAGE2_RPC_URL";
const ENV_CLUSTER: &str = "CLAW_STAGE2_CLUSTER";
const ENV_PROGRAM_ID: &str = "CLAW_STAGE2_PROGRAM_ID";
const ENV_USER_PUBKEY: &str = "CLAW_STAGE2_USER_PUBKEY";
const ENV_DELEGATED_WALLET: &str = "CLAW_STAGE2_DELEGATED_WALLET_PUBKEY";
const ENV_AUTH_PDA: &str = "CLAW_STAGE2_AUTHORIZATION_PDA";
const ENV_DELEGATED_OBLIGATION: &str = "CLAW_STAGE2_DELEGATED_OBLIGATION";
const ENV_DELEGATED_CTOKEN: &str = "CLAW_STAGE2_DELEGATED_CTOKEN_ACCOUNT";
const ENV_DESTINATION_TOKEN: &str = "CLAW_STAGE2_DESTINATION_TOKEN_ACCOUNT";

// ── P5c pinned tuple (mainnet-beta, USDC) ─────────────────────────────────
//
// Source of truth: `crate::stage2_executor::DEMO_*_BS58` constants
// and `programs/clawsol-authority/src/solend_cpi_builder.rs`. Re-pinned
// here as &str so this test file is self-contained.

const PIN_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const PIN_LENDING_MARKET_BS58: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
const PIN_LIQUIDITY_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const PIN_CTOKEN_MINT_BS58: &str = "993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk";
const PIN_PYTH_ORACLE_BS58: &str = "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX";
const PIN_SWITCHBOARD_SENTINEL_BS58: &str = "nu11111111111111111111111111111111111111111";

// ── Well-known program ids ────────────────────────────────────────────────

const SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
const SPL_TOKEN_PROGRAM_ID_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const BPF_LOADER_UPGRADEABLE_BS58: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
const BPF_LOADER_BS58: &str = "BPFLoader2111111111111111111111111111111111";

// ── Layout constants (P5a-equivalent; cited byte offsets) ─────────────────

const RESERVE_LEN: usize = 619;
const OBLIGATION_LEN: usize = 1300;
const SPL_TOKEN_LEN: usize = 165;

// Reserve offsets (see crates/gateway/src/integrations/solend/raw.rs)
const RES_VERSION_OFF: usize = 0;
const RES_LAST_UPDATE_SLOT_OFF: usize = 1;
const RES_LAST_UPDATE_STALE_OFF: usize = 9;
const RES_LENDING_MARKET_OFF: usize = 10;
const RES_LIQ_MINT_OFF: usize = 42;
const RES_LIQ_PYTH_OFF: usize = 107;
const RES_LIQ_SWITCHBOARD_OFF: usize = 139;
const RES_COLL_MINT_OFF: usize = 227;

// Obligation offsets (see crates/gateway/src/integrations/solend/raw.rs)
const OBL_VERSION_OFF: usize = 0;
const OBL_LAST_UPDATE_SLOT_OFF: usize = 1;
const OBL_LENDING_MARKET_OFF: usize = 10;
const OBL_OWNER_OFF: usize = 42;

// SPL Token account layout (165 bytes; see SPL Token program docs)
const SPL_MINT_OFF: usize = 0;
const SPL_OWNER_OFF: usize = 32;
const SPL_AMOUNT_OFF: usize = 64;

// ── Retry policy ──────────────────────────────────────────────────────────

const MAX_RETRY_ATTEMPTS: usize = 3;
const INITIAL_BACKOFF_MS: u64 = 500;
const REQUEST_TIMEOUT_MS: u64 = 15_000;

// ── Cluster enum ──────────────────────────────────────────────────────────

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

// ── Env reader ────────────────────────────────────────────────────────────

#[derive(Debug)]
enum EnvStatus {
    /// Master gate or essential config absent — caller should `eprintln!`
    /// the reason and return Ok(()) without making network calls.
    Skipped(&'static str),
    /// Configuration is internally inconsistent in a way the test refuses
    /// to paper over (e.g., cluster=devnet but mainnet pinned tuple
    /// requested). Caller should fail the test.
    Mismatch(String),
    /// Ready to attempt the preflight.
    Ready(Box<PreflightConfig>),
}

#[derive(Debug)]
struct PreflightConfig {
    rpc_url: String,
    cluster: Cluster,
    program_id: Option<Pubkey>,
    user: Option<Pubkey>,
    delegated_wallet: Option<Pubkey>,
    auth_pda: Option<Pubkey>,
    delegated_obligation: Option<Pubkey>,
    delegated_ctoken: Option<Pubkey>,
    destination_token: Option<Pubkey>,
}

impl PreflightConfig {
    fn from_env_with<F>(getter: F) -> EnvStatus
    where
        F: Fn(&str) -> Option<String>,
    {
        if getter(ENV_GATE).as_deref() != Some("1") {
            return EnvStatus::Skipped(
                "CLAW_STAGE2_LIVE_PREFLIGHT not set to 1",
            );
        }
        let rpc_url = match getter(ENV_RPC_PRIMARY).or_else(|| getter(ENV_RPC_SECONDARY)) {
            Some(u) if !u.trim().is_empty() => u,
            _ => {
                return EnvStatus::Skipped(
                    "neither HELIUS_RPC_URL nor CLAW_STAGE2_RPC_URL is set",
                )
            }
        };
        let cluster_str = match getter(ENV_CLUSTER) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return EnvStatus::Skipped(
                    "CLAW_STAGE2_CLUSTER not set",
                )
            }
        };
        let cluster = match Cluster::parse(cluster_str.trim()) {
            Some(c) => c,
            None => {
                return EnvStatus::Mismatch(format!(
                    "CLAW_STAGE2_CLUSTER='{}' is not one of mainnet-beta | devnet | localnet",
                    cluster_str
                ))
            }
        };
        // W5a-0 mainnet-beta is what the P5c pinned tuple targets.
        // devnet / localnet are accepted only as "I know what I am doing"
        // by the user; the live test will still run RPC connectivity but
        // will print a Mismatch line for any pinned-tuple comparison so
        // the operator sees the cluster vs tuple delta.
        let program_id = getter(ENV_PROGRAM_ID).and_then(|s| parse_pubkey(&s));
        let user = getter(ENV_USER_PUBKEY).and_then(|s| parse_pubkey(&s));
        let delegated_wallet = getter(ENV_DELEGATED_WALLET).and_then(|s| parse_pubkey(&s));
        let auth_pda = getter(ENV_AUTH_PDA).and_then(|s| parse_pubkey(&s));
        let delegated_obligation =
            getter(ENV_DELEGATED_OBLIGATION).and_then(|s| parse_pubkey(&s));
        let delegated_ctoken = getter(ENV_DELEGATED_CTOKEN).and_then(|s| parse_pubkey(&s));
        let destination_token = getter(ENV_DESTINATION_TOKEN).and_then(|s| parse_pubkey(&s));

        EnvStatus::Ready(Box::new(PreflightConfig {
            rpc_url,
            cluster,
            program_id,
            user,
            delegated_wallet,
            auth_pda,
            delegated_obligation,
            delegated_ctoken,
            destination_token,
        }))
    }
}

fn parse_pubkey(s: &str) -> Option<Pubkey> {
    Pubkey::from_str(s.trim()).ok()
}

/// Redact an RPC URL for logging — strip any `api-key=…` query parameter
/// and any user-info portion. Returns a stable shape so reviewers can
/// confirm we never echoed a key.
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
            // Find each `name=value` pair; redact value if name matches.
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

// ── Retry / backoff ───────────────────────────────────────────────────────

#[derive(Debug)]
struct RetryOutcome<T> {
    attempts: usize,
    result: T,
}

/// Bounded retry. Calls `op` up to `max_attempts` times. Sleeps
/// `initial_backoff * 2^(attempt-1)` between attempts. Returns the
/// first `Ok` it sees, or the last `Err` after all attempts are
/// exhausted. NEVER loops indefinitely.
async fn retry_with_backoff<F, Fut, T, E>(
    op: F,
    max_attempts: usize,
    initial_backoff: Duration,
) -> RetryOutcome<Result<T, E>>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut last: Option<E> = None;
    let mut attempt = 0;
    while attempt < max_attempts {
        attempt += 1;
        match op().await {
            Ok(v) => return RetryOutcome { attempts: attempt, result: Ok(v) },
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
    RetryOutcome {
        attempts: attempt,
        // Safe: loop exited only after at least one Err was recorded.
        result: Err(last.expect("retry_with_backoff exited without recording an error")),
    }
}

// ── RPC helpers (reqwest + raw JSON-RPC; standard Helius, NOT Sender) ─────

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

async fn rpc_post(
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

async fn rpc_get_version(client: &reqwest::Client, url: &str) -> Result<String, RpcErr> {
    let v = rpc_post(
        client,
        url,
        json!({"jsonrpc":"2.0","id":1,"method":"getVersion"}),
    )
    .await?;
    let ver = v["result"]["solana-core"]
        .as_str()
        .ok_or_else(|| RpcErr::Body("missing result.solana-core".into()))?
        .to_string();
    Ok(ver)
}

async fn rpc_get_slot(client: &reqwest::Client, url: &str) -> Result<u64, RpcErr> {
    let v = rpc_post(
        client,
        url,
        json!({"jsonrpc":"2.0","id":1,"method":"getSlot"}),
    )
    .await?;
    v["result"]
        .as_u64()
        .ok_or_else(|| RpcErr::Body("missing result u64".into()))
}

async fn rpc_get_latest_blockhash(
    client: &reqwest::Client,
    url: &str,
) -> Result<(String, u64), RpcErr> {
    let v = rpc_post(
        client,
        url,
        json!({"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash"}),
    )
    .await?;
    let blockhash = v["result"]["value"]["blockhash"]
        .as_str()
        .ok_or_else(|| RpcErr::Body("missing blockhash".into()))?
        .to_string();
    let lvbh = v["result"]["value"]["lastValidBlockHeight"]
        .as_u64()
        .ok_or_else(|| RpcErr::Body("missing lastValidBlockHeight".into()))?;
    Ok((blockhash, lvbh))
}

#[derive(Debug)]
struct RawAccountInfo {
    data: Vec<u8>,
    owner: Pubkey,
    executable: bool,
    lamports: u64,
}

async fn rpc_get_account(
    client: &reqwest::Client,
    url: &str,
    pubkey: &Pubkey,
) -> Result<Option<RawAccountInfo>, RpcErr> {
    let v = rpc_post(
        client,
        url,
        json!({
            "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
            "params":[pubkey.to_string(),{"encoding":"base64","commitment":"confirmed"}]
        }),
    )
    .await?;
    let val = &v["result"]["value"];
    if val.is_null() {
        return Ok(None);
    }
    let lamports = val["lamports"]
        .as_u64()
        .ok_or_else(|| RpcErr::Body("missing lamports".into()))?;
    let owner_str = val["owner"]
        .as_str()
        .ok_or_else(|| RpcErr::Body("missing owner".into()))?;
    let owner = Pubkey::from_str(owner_str)
        .map_err(|e| RpcErr::Body(format!("bad owner pubkey: {e}")))?;
    let executable = val["executable"].as_bool().unwrap_or(false);
    let data_arr = val["data"]
        .as_array()
        .ok_or_else(|| RpcErr::Body("data not array".into()))?;
    let b64 = data_arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| RpcErr::Body("data[0] not string".into()))?;
    let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
        .map_err(|e| RpcErr::Body(format!("base64 decode: {e}")))?;
    Ok(Some(RawAccountInfo {
        data,
        owner,
        executable,
        lamports,
    }))
}

// ── Layout decoders (P5a-equivalent; NO Borsh; NO SPL pack/unpack) ────────

#[derive(Debug, PartialEq, Eq)]
struct ReserveView {
    version: u8,
    last_update_slot: u64,
    last_update_stale: bool,
    lending_market: Pubkey,
    liquidity_mint: Pubkey,
    pyth_oracle: Pubkey,
    switchboard_oracle: Pubkey,
    ctoken_mint: Pubkey,
}

fn decode_reserve(data: &[u8]) -> Result<ReserveView, &'static str> {
    if data.len() < RESERVE_LEN {
        return Err("reserve data shorter than RESERVE_LEN (619)");
    }
    let version = data[RES_VERSION_OFF];
    if version == 0 {
        return Err("reserve version=0 (uninitialised)");
    }
    let last_update_slot = read_u64_le(data, RES_LAST_UPDATE_SLOT_OFF);
    let last_update_stale = data[RES_LAST_UPDATE_STALE_OFF] != 0;
    let lending_market = read_pubkey(data, RES_LENDING_MARKET_OFF);
    let liquidity_mint = read_pubkey(data, RES_LIQ_MINT_OFF);
    let pyth_oracle = read_pubkey(data, RES_LIQ_PYTH_OFF);
    let switchboard_oracle = read_pubkey(data, RES_LIQ_SWITCHBOARD_OFF);
    let ctoken_mint = read_pubkey(data, RES_COLL_MINT_OFF);
    Ok(ReserveView {
        version,
        last_update_slot,
        last_update_stale,
        lending_market,
        liquidity_mint,
        pyth_oracle,
        switchboard_oracle,
        ctoken_mint,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ObligationView {
    version: u8,
    last_update_slot: u64,
    lending_market: Pubkey,
    owner: Pubkey,
}

fn decode_obligation(data: &[u8]) -> Result<ObligationView, &'static str> {
    if data.len() < OBLIGATION_LEN {
        return Err("obligation data shorter than OBLIGATION_LEN (1300)");
    }
    let version = data[OBL_VERSION_OFF];
    if version == 0 {
        return Err("obligation version=0 (uninitialised)");
    }
    let last_update_slot = read_u64_le(data, OBL_LAST_UPDATE_SLOT_OFF);
    let lending_market = read_pubkey(data, OBL_LENDING_MARKET_OFF);
    let owner = read_pubkey(data, OBL_OWNER_OFF);
    Ok(ObligationView {
        version,
        last_update_slot,
        lending_market,
        owner,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct SplTokenView {
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
}

fn decode_spl_token_account(data: &[u8]) -> Result<SplTokenView, &'static str> {
    if data.len() != SPL_TOKEN_LEN {
        return Err("SPL token account is not exactly 165 bytes (Token-2022 or non-token account?)");
    }
    let mint = read_pubkey(data, SPL_MINT_OFF);
    let owner = read_pubkey(data, SPL_OWNER_OFF);
    let amount = read_u64_le(data, SPL_AMOUNT_OFF);
    Ok(SplTokenView { mint, owner, amount })
}

fn read_pubkey(data: &[u8], off: usize) -> Pubkey {
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&data[off..off + 32]);
    Pubkey::new_from_array(buf)
}

fn read_u64_le(data: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[off..off + 8]);
    u64::from_le_bytes(b)
}

// ── Pinned-tuple verification ─────────────────────────────────────────────

/// Returns `Ok(())` iff the cluster/pinned-tuple combination is one the
/// preflight is willing to attempt. Mainnet-beta + the P5c USDC tuple is
/// always OK. Devnet / localnet + the mainnet tuple is rejected unless
/// the cluster has been explicitly forked from mainnet — this preflight
/// does not assume that.
fn check_cluster_tuple_consistency(
    cluster: Cluster,
    pinned_reserve: &str,
) -> Result<(), String> {
    match cluster {
        Cluster::MainnetBeta => {
            if pinned_reserve == PIN_RESERVE_BS58 {
                Ok(())
            } else {
                Err(format!(
                    "mainnet-beta selected but pinned reserve {} is not the P5c USDC tuple",
                    pinned_reserve
                ))
            }
        }
        Cluster::Devnet | Cluster::Localnet => {
            // The pinned mainnet USDC reserve does not exist on devnet
            // unless the cluster was explicitly forked from mainnet.
            // W5a-0 refuses to assume that — operator must run
            // their own cluster-clone preflight first.
            if pinned_reserve == PIN_RESERVE_BS58 {
                Err(format!(
                    "cluster={:?} but pinned reserve is the mainnet-beta P5c tuple; \
                     refusing to assume mainnet accounts exist on this cluster",
                    cluster
                ))
            } else {
                Ok(())
            }
        }
    }
}

// ── Source-of-truth invariant for unit test #6 ────────────────────────────
//
// The unit test `no_send_path_in_this_file_default_invariant` scans this
// very file for forbidden call-forms. Each entry below is the EXACT
// substring the test searches for; comments mentioning the bare name
// (without paren or argument context) are allowed because they appear in
// the doc-comment safety posture and explanatory text.
const FORBIDDEN_CALL_FORMS: &[&str] = &[
    "send_raw_transaction(",
    "send_raw_v0_transaction(",
    "send_and_confirm_transaction(",
    "sendRawTransaction(",
    "skipPreflight: true",
    "maxRetries:",
    "api.jup.ag/",
    "window.solana",
    "signTransaction(",
    "Helius-Sender",
    "helius-sender",
];

// ──────────────────────────────────────────────────────────────────────────
// Live test (env-gated)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage2_w5a0_live_solend_preflight() {
    let status = PreflightConfig::from_env_with(|k| env::var(k).ok());
    let cfg = match status {
        EnvStatus::Skipped(reason) => {
            eprintln!(
                "[skip] stage2_w5a0_live_solend_preflight: {reason}; \
                 set CLAW_STAGE2_LIVE_PREFLIGHT=1 + HELIUS_RPC_URL + CLAW_STAGE2_CLUSTER to enable."
            );
            return;
        }
        EnvStatus::Mismatch(msg) => {
            panic!("preflight refuses to run: {msg}");
        }
        EnvStatus::Ready(c) => c,
    };

    if let Err(msg) = check_cluster_tuple_consistency(cfg.cluster, PIN_RESERVE_BS58) {
        panic!("STOP: cluster/tuple mismatch — {msg}");
    }

    eprintln!("──── W5a-0 live preflight (read-only, no keypair) ────");
    eprintln!("cluster        : {:?}", cfg.cluster);
    eprintln!("rpc            : {}", redact_url(&cfg.rpc_url));
    eprintln!("program_id     : {}", opt_pk(&cfg.program_id));
    eprintln!("user           : {}", opt_pk(&cfg.user));
    eprintln!("delegated      : {}", opt_pk(&cfg.delegated_wallet));
    eprintln!("auth_pda       : {}", opt_pk(&cfg.auth_pda));
    eprintln!("obligation     : {}", opt_pk(&cfg.delegated_obligation));
    eprintln!("delegated_ct   : {}", opt_pk(&cfg.delegated_ctoken));
    eprintln!("dest_token     : {}", opt_pk(&cfg.destination_token));

    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .expect("build reqwest client");

    // ── Phase 1: RPC connectivity ────────────────────────────────────────
    eprintln!("\n── Phase 1: RPC connectivity ──");
    let ver = retry_with_backoff(
        || rpc_get_version(&http, &cfg.rpc_url),
        MAX_RETRY_ATTEMPTS,
        Duration::from_millis(INITIAL_BACKOFF_MS),
    )
    .await;
    match ver.result {
        Ok(v) => eprintln!("✓ getVersion           solana-core {v} (attempts={})", ver.attempts),
        Err(e) => panic!("✗ getVersion failed after {} attempts: {e}", ver.attempts),
    }
    let slot = retry_with_backoff(
        || rpc_get_slot(&http, &cfg.rpc_url),
        MAX_RETRY_ATTEMPTS,
        Duration::from_millis(INITIAL_BACKOFF_MS),
    )
    .await;
    let chain_slot = match slot.result {
        Ok(s) => {
            eprintln!("✓ getSlot              slot={s} (attempts={})", slot.attempts);
            s
        }
        Err(e) => panic!("✗ getSlot failed after {} attempts: {e}", slot.attempts),
    };
    let bh = retry_with_backoff(
        || rpc_get_latest_blockhash(&http, &cfg.rpc_url),
        MAX_RETRY_ATTEMPTS,
        Duration::from_millis(INITIAL_BACKOFF_MS),
    )
    .await;
    match bh.result {
        Ok((blockhash, lvbh)) => eprintln!(
            "✓ getLatestBlockhash   blockhash={blockhash} lastValidBlockHeight={lvbh}"
        ),
        Err(e) => eprintln!("⚠ getLatestBlockhash failed (non-fatal at W5a-0): {e}"),
    }

    // ── Phase 2: account existence + owner cross-check ───────────────────
    eprintln!("\n── Phase 2: account existence + owner cross-check ──");

    let solend_program = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap();
    let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
    let bpf_upgradeable = Pubkey::from_str(BPF_LOADER_UPGRADEABLE_BS58).unwrap();
    let bpf_loader2 = Pubkey::from_str(BPF_LOADER_BS58).unwrap();

    // Helper: fetch + report account; returns the data for downstream
    // decoders.
    async fn fetch(
        http: &reqwest::Client,
        url: &str,
        label: &str,
        pk: &Pubkey,
    ) -> Option<RawAccountInfo> {
        let r = retry_with_backoff(
            || rpc_get_account(http, url, pk),
            MAX_RETRY_ATTEMPTS,
            Duration::from_millis(INITIAL_BACKOFF_MS),
        )
        .await;
        match r.result {
            Ok(Some(acc)) => {
                eprintln!(
                    "✓ {label}  pubkey={pk}  owner={}  exec={}  data={}B  lamports={}",
                    acc.owner,
                    acc.executable,
                    acc.data.len(),
                    acc.lamports,
                );
                Some(acc)
            }
            Ok(None) => {
                eprintln!("✗ {label}  pubkey={pk}  ACCOUNT_NOT_FOUND");
                None
            }
            Err(e) => {
                eprintln!(
                    "✗ {label}  pubkey={pk}  RPC_ERROR after {} attempts: {e}",
                    r.attempts
                );
                None
            }
        }
    }

    // Solend program itself
    let solend_acc = fetch(&http, &cfg.rpc_url, "solend_program", &solend_program).await;
    if let Some(a) = solend_acc.as_ref() {
        assert!(
            a.executable || a.owner == bpf_upgradeable || a.owner == bpf_loader2,
            "Solend program account is not executable and not BPF-loader-owned"
        );
    } else {
        panic!("STOP: Solend program not deployed on this cluster");
    }

    // clawsol-authority program (if env supplied)
    if let Some(pid) = cfg.program_id.as_ref() {
        let acc = fetch(&http, &cfg.rpc_url, "clawsol_authority", pid).await;
        match acc {
            Some(a) => {
                let ok = a.executable
                    || a.owner == bpf_upgradeable
                    || a.owner == bpf_loader2;
                if !ok {
                    eprintln!(
                        "⚠ clawsol_authority pubkey {pid} exists but is neither executable \
                         nor BPF-loader-owned (owner={}). May not be a program account.",
                        a.owner
                    );
                }
            }
            None => panic!("STOP: clawsol_authority program not deployed on this cluster"),
        }
    } else {
        eprintln!("· clawsol_authority    skipped (CLAW_STAGE2_PROGRAM_ID not set)");
    }

    // Pinned reserve
    let pinned_reserve_pk = Pubkey::from_str(PIN_RESERVE_BS58).unwrap();
    let reserve_acc = fetch(&http, &cfg.rpc_url, "pinned_reserve", &pinned_reserve_pk).await;
    if let Some(a) = reserve_acc.as_ref() {
        assert_eq!(
            a.owner, solend_program,
            "pinned reserve owner is not the Solend program id"
        );
    } else if matches!(cfg.cluster, Cluster::MainnetBeta) {
        panic!("STOP: pinned mainnet USDC reserve not found on mainnet-beta");
    }

    // Pinned Pyth oracle
    let pyth_pk = Pubkey::from_str(PIN_PYTH_ORACLE_BS58).unwrap();
    let pyth_acc = fetch(&http, &cfg.rpc_url, "pinned_pyth_oracle", &pyth_pk).await;
    if let Some(a) = pyth_acc.as_ref() {
        // Pyth oracle ownership is informational only — we do not pin a
        // specific Pyth program id in W5a-0. If the owner is the
        // System Program or BPFLoader, that is suspicious; otherwise
        // accept and report.
        eprintln!(
            "· pyth_oracle owner    {} (informational — not enforced at W5a-0)",
            a.owner
        );
    } else if matches!(cfg.cluster, Cluster::MainnetBeta) {
        panic!("STOP: pinned Pyth oracle not found on mainnet-beta");
    }

    // Switchboard sentinel
    let sb_pk = Pubkey::from_str(PIN_SWITCHBOARD_SENTINEL_BS58).unwrap();
    let sb_acc = fetch(&http, &cfg.rpc_url, "switchboard_sentinel", &sb_pk).await;
    eprintln!(
        "· switchboard_sentinel {} (P5c sentinel — null oracle slot is expected to be a placeholder)",
        if sb_acc.is_some() { "present" } else { "absent (sentinel placeholder)" }
    );

    // Obligation
    let obligation_acc = if let Some(obl) = cfg.delegated_obligation.as_ref() {
        fetch(&http, &cfg.rpc_url, "obligation", obl).await
    } else {
        eprintln!("· obligation           skipped (CLAW_STAGE2_DELEGATED_OBLIGATION not set)");
        None
    };
    if let Some(a) = obligation_acc.as_ref() {
        assert_eq!(
            a.owner, solend_program,
            "obligation owner (account-data owner) is not the Solend program id"
        );
    }

    // Delegated cToken account
    let dct_acc = if let Some(t) = cfg.delegated_ctoken.as_ref() {
        fetch(&http, &cfg.rpc_url, "delegated_ctoken", t).await
    } else {
        eprintln!(
            "· delegated_ctoken     skipped (CLAW_STAGE2_DELEGATED_CTOKEN_ACCOUNT not set)"
        );
        None
    };
    if let Some(a) = dct_acc.as_ref() {
        assert_eq!(
            a.owner, spl_token,
            "delegated cToken account owner is not the SPL Token program id"
        );
    }

    // Destination token account
    let dest_acc = if let Some(t) = cfg.destination_token.as_ref() {
        fetch(&http, &cfg.rpc_url, "destination_token", t).await
    } else {
        eprintln!(
            "· destination_token    skipped (CLAW_STAGE2_DESTINATION_TOKEN_ACCOUNT not set)"
        );
        None
    };
    if let Some(a) = dest_acc.as_ref() {
        assert_eq!(
            a.owner, spl_token,
            "destination token account owner is not the SPL Token program id"
        );
    }

    // ── Phase 3: P5a-equivalent layout decode + invariants ───────────────
    eprintln!("\n── Phase 3: P5a layout decode + cross-pin invariants ──");

    let pinned_lending_market = Pubkey::from_str(PIN_LENDING_MARKET_BS58).unwrap();
    let pinned_liquidity_mint = Pubkey::from_str(PIN_LIQUIDITY_MINT_BS58).unwrap();
    let pinned_ctoken_mint = Pubkey::from_str(PIN_CTOKEN_MINT_BS58).unwrap();
    let pinned_pyth = Pubkey::from_str(PIN_PYTH_ORACLE_BS58).unwrap();

    if let Some(acc) = reserve_acc.as_ref() {
        match decode_reserve(&acc.data) {
            Ok(rv) => {
                eprintln!(
                    "✓ reserve decode       version={} stale={} last_update_slot={} (chain_slot={})",
                    rv.version, rv.last_update_stale, rv.last_update_slot, chain_slot
                );
                assert_eq!(
                    rv.lending_market, pinned_lending_market,
                    "reserve.lending_market mismatch"
                );
                assert_eq!(
                    rv.liquidity_mint, pinned_liquidity_mint,
                    "reserve.liquidity_mint mismatch"
                );
                assert_eq!(
                    rv.ctoken_mint, pinned_ctoken_mint,
                    "reserve.ctoken_mint mismatch"
                );
                assert_eq!(rv.pyth_oracle, pinned_pyth, "reserve.pyth_oracle mismatch");
                eprintln!("✓ reserve cross-pin    lending_market / liquidity_mint / ctoken_mint / pyth_oracle all match P5c");
            }
            Err(e) => panic!("✗ reserve decode failed: {e}"),
        }
    }

    let mut obligation_owner: Option<Pubkey> = None;
    if let Some(acc) = obligation_acc.as_ref() {
        match decode_obligation(&acc.data) {
            Ok(ov) => {
                eprintln!(
                    "✓ obligation decode    version={} lending_market={} owner={} last_update_slot={}",
                    ov.version, ov.lending_market, ov.owner, ov.last_update_slot
                );
                assert_eq!(
                    ov.lending_market, pinned_lending_market,
                    "obligation.lending_market mismatch"
                );
                obligation_owner = Some(ov.owner);
            }
            Err(e) => panic!("✗ obligation decode failed: {e}"),
        }
    }

    // Critical invariant: obligation owner = delegated wallet, NOT user
    if let (Some(owner), Some(delegated)) =
        (obligation_owner.as_ref(), cfg.delegated_wallet.as_ref())
    {
        if owner != delegated {
            panic!(
                "STOP: obligation.owner ({owner}) != delegated_wallet ({delegated}) — \
                 W5a-0 refuses to proceed"
            );
        }
        eprintln!("✓ obligation.owner == delegated_wallet ({owner})");
    } else {
        eprintln!(
            "· obligation/delegated-owner cross-check skipped (one of obligation, \
             delegated wallet env not set)"
        );
    }
    if let (Some(owner), Some(user_pk)) =
        (obligation_owner.as_ref(), cfg.user.as_ref())
    {
        if owner == user_pk {
            panic!(
                "STOP: obligation.owner ({owner}) == user main wallet — \
                 W5a-0 refuses to proceed (obligation must be owned by the delegated wallet)"
            );
        }
        eprintln!("✓ obligation.owner != user main wallet");
    }

    let mut delegated_ctoken_amount: Option<u64> = None;
    if let Some(acc) = dct_acc.as_ref() {
        match decode_spl_token_account(&acc.data) {
            Ok(tv) => {
                eprintln!(
                    "✓ delegated_ctoken     mint={} owner={} amount={}",
                    tv.mint, tv.owner, tv.amount
                );
                assert_eq!(tv.mint, pinned_ctoken_mint, "delegated cToken mint mismatch");
                if let Some(delegated) = cfg.delegated_wallet.as_ref() {
                    if &tv.owner != delegated {
                        panic!(
                            "STOP: delegated cToken account owner ({}) != delegated_wallet ({}) — refusing to proceed",
                            tv.owner, delegated
                        );
                    }
                }
                delegated_ctoken_amount = Some(tv.amount);
                if tv.amount == 0 {
                    eprintln!(
                        "⚠ delegated cToken amount = 0 (live withdraw rehearsal at W5a-1+ will have nothing to withdraw)"
                    );
                }
            }
            Err(e) => panic!("✗ delegated cToken decode failed: {e}"),
        }
    }

    if let Some(acc) = dest_acc.as_ref() {
        match decode_spl_token_account(&acc.data) {
            Ok(tv) => {
                eprintln!(
                    "✓ destination_token    mint={} owner={} amount={}",
                    tv.mint, tv.owner, tv.amount
                );
                assert_eq!(
                    tv.mint, pinned_liquidity_mint,
                    "destination token mint must be the liquidity mint (USDC)"
                );
                if let Some(user_pk) = cfg.user.as_ref() {
                    if &tv.owner != user_pk {
                        eprintln!(
                            "⚠ destination token owner {} != user main wallet {} — \
                             confirm this is intentional",
                            tv.owner, user_pk
                        );
                    }
                }
            }
            Err(e) => panic!("✗ destination token decode failed: {e}"),
        }
    }

    // ── Phase 4: W4 request shape check (report-only) ────────────────────
    eprintln!("\n── Phase 4: W4 request-shape check ──");
    eprintln!(
        "· The crate exposes `claw_gateway::stage2_executor::DEMO_*_BS58` constants \
         and `DemoSolendExecutionFixture::mainnet_beta_demo_usdc()`. The pinned \
         pubkeys decoded above were cross-checked against these constants by the \
         existing W4 unit tests (`stage2_executor::tests::demo_fixture_*`)."
    );
    eprintln!(
        "· W5a-0 does NOT instantiate Stage2ExecuteActionRequest because doing so \
         would require a real rule_id and canonical_rule_hash — those belong to \
         the state-store, not to a read-only preflight. The W4 builder remains the \
         single source of truth; this preflight only verifies that its account \
         inputs exist and have the expected shapes on chain."
    );

    // ── Phase 5: simulation/signing readiness (account-only) ─────────────
    eprintln!("\n── Phase 5: simulation/signing readiness ──");
    eprintln!(
        "· simulation/signing not attempted: no delegated keypair provided; \
         W5a-0 is account-only by design."
    );
    eprintln!(
        "· W5a-1 (simulation) would need: delegated keypair, latest blockhash \
         fetched immediately before tx assembly, serialized transaction, \
         compute-unit estimate, optional Helius priority-fee estimate."
    );
    eprintln!(
        "· If Helius priority-fee estimate is used at W5a-1+, remember the unit is \
         micro-lamports per CU: total_priority_fee_lamports = estimated_cu * \
         priority_fee_micro_lamports / 1_000_000."
    );

    eprintln!(
        "\n──── W5a-0 preflight done — chain_slot={} delegated_ctoken_amount={:?} ────",
        chain_slot, delegated_ctoken_amount
    );
}

fn opt_pk(p: &Option<Pubkey>) -> String {
    match p {
        Some(pk) => pk.to_string(),
        None => "(not set)".to_string(),
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Unit tests (always run; non-live; no network)
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b58: &str) -> Pubkey {
        Pubkey::from_str(b58).unwrap()
    }

    // 1. Without the master gate, env reader returns `Skipped`.
    #[test]
    fn live_preflight_requires_env_gate() {
        let status = PreflightConfig::from_env_with(|_| None);
        match status {
            EnvStatus::Skipped(reason) => assert!(reason.contains(ENV_GATE)),
            other => panic!("expected Skipped without gate, got: {other:?}"),
        }
        // Gate alone is insufficient — must also have RPC URL + cluster.
        let status = PreflightConfig::from_env_with(|k| {
            if k == ENV_GATE { Some("1".to_string()) } else { None }
        });
        match status {
            EnvStatus::Skipped(reason) => assert!(reason.contains("RPC_URL")),
            other => panic!("expected Skipped without RPC URL, got: {other:?}"),
        }
    }

    // 2. Devnet/localnet + mainnet pinned reserve = refuse-to-proceed.
    #[test]
    fn rejects_cluster_tuple_mismatch() {
        let r = check_cluster_tuple_consistency(Cluster::Devnet, PIN_RESERVE_BS58);
        assert!(r.is_err(), "devnet + mainnet pinned reserve must error");
        assert!(r.unwrap_err().contains("mainnet"));

        let r = check_cluster_tuple_consistency(Cluster::Localnet, PIN_RESERVE_BS58);
        assert!(r.is_err(), "localnet + mainnet pinned reserve must error");

        // Devnet + a hypothetical devnet-style reserve is OK at this gate.
        let r = check_cluster_tuple_consistency(
            Cluster::Devnet,
            "11111111111111111111111111111111",
        );
        assert!(r.is_ok());

        // Mainnet + mainnet pinned reserve is OK.
        let r = check_cluster_tuple_consistency(Cluster::MainnetBeta, PIN_RESERVE_BS58);
        assert!(r.is_ok());
    }

    // 3. Without RPC URL, env reader returns `Skipped` with a specific
    //    reason — not a panic, not a Mismatch.
    #[test]
    fn rejects_missing_rpc_url() {
        let status = PreflightConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_PREFLIGHT" => Some("1".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Skipped(reason) => {
                assert!(
                    reason.contains("RPC_URL"),
                    "skip reason should mention RPC URL, got: {reason}"
                );
            }
            other => panic!("expected Skipped, got: {other:?}"),
        }
    }

    // 4. With gate + RPC + cluster but no keypair / no account env, the
    //    config is still `Ready` — W5a-0 is account-only, no keypair
    //    required.
    #[test]
    fn no_keypair_is_valid_for_w5a0_account_only() {
        let status = PreflightConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_PREFLIGHT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/?api-key=XYZ".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(cfg) => {
                assert_eq!(cfg.cluster, Cluster::MainnetBeta);
                assert!(cfg.program_id.is_none());
                assert!(cfg.user.is_none());
                assert!(cfg.delegated_wallet.is_none());
                assert!(cfg.delegated_obligation.is_none());
                assert!(cfg.delegated_ctoken.is_none());
                assert!(cfg.destination_token.is_none());
            }
            other => panic!("expected Ready, got: {other:?}"),
        }
    }

    // 5. The decoders read the right fields from the right offsets.
    #[test]
    fn account_decode_uses_p5a_layout() {
        // ── Reserve ──
        let mut r = vec![0u8; RESERVE_LEN];
        r[RES_VERSION_OFF] = 1;
        r[RES_LAST_UPDATE_SLOT_OFF..RES_LAST_UPDATE_SLOT_OFF + 8]
            .copy_from_slice(&42u64.to_le_bytes());
        r[RES_LAST_UPDATE_STALE_OFF] = 1;
        let lm = pk(PIN_LENDING_MARKET_BS58);
        r[RES_LENDING_MARKET_OFF..RES_LENDING_MARKET_OFF + 32].copy_from_slice(&lm.to_bytes());
        let lmint = pk(PIN_LIQUIDITY_MINT_BS58);
        r[RES_LIQ_MINT_OFF..RES_LIQ_MINT_OFF + 32].copy_from_slice(&lmint.to_bytes());
        let pyth = pk(PIN_PYTH_ORACLE_BS58);
        r[RES_LIQ_PYTH_OFF..RES_LIQ_PYTH_OFF + 32].copy_from_slice(&pyth.to_bytes());
        let sb = pk(PIN_SWITCHBOARD_SENTINEL_BS58);
        r[RES_LIQ_SWITCHBOARD_OFF..RES_LIQ_SWITCHBOARD_OFF + 32].copy_from_slice(&sb.to_bytes());
        let ctm = pk(PIN_CTOKEN_MINT_BS58);
        r[RES_COLL_MINT_OFF..RES_COLL_MINT_OFF + 32].copy_from_slice(&ctm.to_bytes());
        let rv = decode_reserve(&r).expect("reserve decode");
        assert_eq!(rv.version, 1);
        assert_eq!(rv.last_update_slot, 42);
        assert!(rv.last_update_stale);
        assert_eq!(rv.lending_market, lm);
        assert_eq!(rv.liquidity_mint, lmint);
        assert_eq!(rv.pyth_oracle, pyth);
        assert_eq!(rv.switchboard_oracle, sb);
        assert_eq!(rv.ctoken_mint, ctm);

        // ── Obligation ──
        let mut o = vec![0u8; OBLIGATION_LEN];
        o[OBL_VERSION_OFF] = 2;
        o[OBL_LAST_UPDATE_SLOT_OFF..OBL_LAST_UPDATE_SLOT_OFF + 8]
            .copy_from_slice(&99u64.to_le_bytes());
        let lm2 = pk(PIN_LENDING_MARKET_BS58);
        o[OBL_LENDING_MARKET_OFF..OBL_LENDING_MARKET_OFF + 32].copy_from_slice(&lm2.to_bytes());
        let owner = pk("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW");
        o[OBL_OWNER_OFF..OBL_OWNER_OFF + 32].copy_from_slice(&owner.to_bytes());
        let ov = decode_obligation(&o).expect("obligation decode");
        assert_eq!(ov.version, 2);
        assert_eq!(ov.last_update_slot, 99);
        assert_eq!(ov.lending_market, lm2);
        assert_eq!(ov.owner, owner);

        // ── SPL Token account ──
        let mut t = vec![0u8; SPL_TOKEN_LEN];
        let mint = pk(PIN_CTOKEN_MINT_BS58);
        t[SPL_MINT_OFF..SPL_MINT_OFF + 32].copy_from_slice(&mint.to_bytes());
        let towner = pk("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW");
        t[SPL_OWNER_OFF..SPL_OWNER_OFF + 32].copy_from_slice(&towner.to_bytes());
        t[SPL_AMOUNT_OFF..SPL_AMOUNT_OFF + 8].copy_from_slice(&123_456_789u64.to_le_bytes());
        let tv = decode_spl_token_account(&t).expect("spl token decode");
        assert_eq!(tv.mint, mint);
        assert_eq!(tv.owner, towner);
        assert_eq!(tv.amount, 123_456_789);

        // ── Boundary checks ──
        assert!(decode_reserve(&vec![0u8; RESERVE_LEN - 1]).is_err());
        assert!(decode_obligation(&vec![0u8; OBLIGATION_LEN - 1]).is_err());
        assert!(decode_spl_token_account(&vec![0u8; SPL_TOKEN_LEN - 1]).is_err());
        assert!(decode_spl_token_account(&vec![0u8; SPL_TOKEN_LEN + 1]).is_err());
    }

    // 6. This file contains no forbidden send / sign / Jupiter call-forms.
    //    Scans the on-disk source via include_str! so the test fails if a
    //    future edit accidentally introduces one.
    #[test]
    fn no_send_path_in_this_file_default_invariant() {
        const SOURCE: &str = include_str!("stage2_live_solend_preflight.rs");
        for forbidden in FORBIDDEN_CALL_FORMS {
            // Allow the FORBIDDEN_CALL_FORMS table itself to mention each
            // token verbatim once (as a string literal). Anything beyond
            // that single mention is a real call-site and must fail.
            let count = SOURCE.matches(forbidden).count();
            assert!(
                count <= 1,
                "forbidden call-form `{forbidden}` appears {count} times in preflight source; \
                 W5a-0 must contain at most the FORBIDDEN_CALL_FORMS allowlist entry"
            );
        }
    }

    // 7. Retry policy gives up after max_attempts and never loops forever.
    #[tokio::test(start_paused = true)]
    async fn retry_policy_is_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let calls_ref = calls.clone();
        let r: RetryOutcome<Result<(), &'static str>> = retry_with_backoff(
            || {
                let c = calls_ref.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err::<(), &'static str>("always fails")
                }
            },
            MAX_RETRY_ATTEMPTS,
            Duration::from_millis(INITIAL_BACKOFF_MS),
        )
        .await;
        assert_eq!(r.attempts, MAX_RETRY_ATTEMPTS);
        assert_eq!(calls.load(Ordering::SeqCst), MAX_RETRY_ATTEMPTS);
        assert!(r.result.is_err());

        // Success-on-first-attempt: must not call beyond once.
        let calls2 = std::sync::Arc::new(AtomicUsize::new(0));
        let calls2_ref = calls2.clone();
        let r2: RetryOutcome<Result<&'static str, &'static str>> = retry_with_backoff(
            || {
                let c = calls2_ref.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok::<&'static str, &'static str>("ok")
                }
            },
            MAX_RETRY_ATTEMPTS,
            Duration::from_millis(INITIAL_BACKOFF_MS),
        )
        .await;
        assert_eq!(r2.attempts, 1);
        assert_eq!(calls2.load(Ordering::SeqCst), 1);
        assert_eq!(r2.result.unwrap(), "ok");
    }

    // 8. URL redaction strips api-key (and similar) query parameters.
    #[test]
    fn redact_url_strips_api_key() {
        let r = redact_url("https://mainnet.helius-rpc.com/?api-key=abc123XYZ");
        assert!(r.contains("***REDACTED***"));
        assert!(!r.contains("abc123XYZ"));

        let r = redact_url("https://example.invalid/");
        assert_eq!(r, "https://example.invalid/");

        let r = redact_url("https://example.invalid/?api-key=k&commitment=processed");
        assert!(r.contains("***REDACTED***"));
        assert!(!r.contains("=k&"));
        assert!(r.contains("commitment=processed"));
    }

    // 9. The pinned tuple in this file equals the W4 demo fixture
    //    constants, locked-in by string equality (parity guard against
    //    silent drift).
    #[test]
    fn pinned_tuple_strings_match_w4_demo_fixture_constants() {
        // claw_gateway::stage2_executor::DEMO_*_BS58 are the source of
        // truth. If they ever change without a corresponding update here,
        // this test fails loudly.
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
        assert_eq!(
            PIN_SWITCHBOARD_SENTINEL_BS58,
            claw_gateway::stage2_executor::DEMO_SWITCHBOARD_ORACLE_BS58
        );
    }
}

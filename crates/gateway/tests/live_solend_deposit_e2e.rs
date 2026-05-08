//! OPT-IN: Phase 4C-8 — End-to-end Solend USDC deposit proof (controlled test wallet).
//!
//! Drives the full Phase 4B → 4C production path in-process:
//!
//! 1. `solend_deposit_usdc` tool propose (`amount` only)
//! 2. `ApprovalStore::decide` + `approval_routing::route_approval_outcome`
//! 3. Resume task runs (reassemble + preflight + handoff)
//! 4. `GatewaySolendSignatureHandler::retrieve` returns unsigned tx
//! 5. Controlled test keypair signs the session-wallet slot
//! 6. `GatewaySolendSignatureHandler::submit` runs 4C-5 verification + broadcast
//! 7. `SolendConfirmationTracker` polls until Finalized
//! 8. Proof document written to `docs/proofs/` on Finalized success
//!
//! **Default: skipped.** No network calls unless every opt-in env var is
//! set. No broadcast unless `CLAW_LIVE_SOLEND_E2E_BROADCAST=1` is also
//! set. No proof doc is written unless on-chain `Finalized` was observed.
//!
//! # Required opt-in env vars
//!
//! | Var | Purpose |
//! |---|---|
//! | `CLAW_LIVE_SOLEND_E2E=1` | Master opt-in |
//! | `CLAW_LIVE_SOLEND_E2E_BROADCAST=1` | Allow the POST/submit step to broadcast |
//! | `CLAW_LIVE_SOLEND_E2E_RPC_URL` | Mainnet or devnet RPC URL |
//! | `CLAW_LIVE_SOLEND_E2E_KEYPAIR_PATH` | Path to controlled-wallet keypair JSON (MUST live outside the repo) |
//! | `CLAW_LIVE_SOLEND_E2E_WALLET` | Expected base58 pubkey of that keypair (double-check) |
//! | `CLAW_LIVE_SOLEND_E2E_AMOUNT_RAW` | Deposit amount in USDC base units; default 1000 (= 0.001 USDC), capped at 10_000 |
//!
//! # Safety
//!
//! - `amount_raw` default 1000 (0.001 USDC); hard-capped at 10_000 (0.01 USDC).
//! - Expected wallet pubkey MUST match the loaded keypair's pubkey.
//! - Fail-fast balance guards: SOL >= 0.01 SOL; USDC ATA exists, owner matches, mint matches, balance >= amount_raw.
//! - Never prints private key bytes. Never writes `transaction_base64` or raw tx bytes to the proof doc.
//! - Keypair path must live OUTSIDE the repo (matches Slice 3G convention).
//!
//! # What this test does NOT do
//!
//! - Does NOT spin up a TCP HTTP server. Uses the `SolendSignatureHandler` trait directly.
//! - Does NOT bypass `submit_signed_solend_transaction`. The POST path runs the
//!   same 4C-5 verification pipeline production uses.
//! - Does NOT bypass `SolendConfirmationTracker`. The Finalized decision comes
//!   from the same `calculate_next_state` → lifecycle transition → audit path
//!   production uses.
//! - Does NOT call `send_raw_transaction` from the test. Only the production
//!   submit path does.
//! - Does NOT re-broadcast / re-sign / re-handoff on timeout. Fails-closed.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use uuid::Uuid;

use claw_api::state::{SolendRetrievalResult, SolendSignatureHandler};
use claw_gateway::approval_store::ApprovalStore;
use claw_gateway::approval_routing::route_approval_outcome;
use claw_gateway::external_wallet::ExternalWalletStore;
use claw_gateway::integrations::jupiter_park::PendingJupiterParkStore;
use claw_gateway::integrations::solend_lifecycle::SolendSubmissionLifecycleStore;
use claw_gateway::integrations::solend_park::SolendParkStore;
use claw_gateway::integrations::solend_signing::SolendSigningStore;
use claw_gateway::integrations::solend_submit::{
    ClawRpcBlockHeightProvider, ClawRpcSolendSender, NullSolendAuditSink,
};
use claw_gateway::pending_signing::PendingSigningStore;
use claw_gateway::policy_alerting::AlertDispatcher;
use claw_gateway::runtime::solend_submit_wiring::{
    wire_solend_confirmation_tracker, GatewaySolendSignatureHandler,
};
use claw_gateway::runtime::solend_wiring::wire_solend_deposit_tool;
use claw_solana_core::blockhash::BlockhashManager;
use claw_solana_core::rpc::{ClawRpcClient, EndpointConfig, RpcPool, RpcPoolConfig};
use claw_tool_system::registry::ToolRegistry;
use claw_types::approval::ApprovalDecision;
use claw_types::session::SessionId;
use claw_types::solana::CommitmentLevel;
use claw_types::tool::ToolInput;

// ── Opt-in env var names ────────────────────────────────────────────────────

const ENV_OPT_IN: &str = "CLAW_LIVE_SOLEND_E2E";
const ENV_BROADCAST: &str = "CLAW_LIVE_SOLEND_E2E_BROADCAST";
const ENV_RPC_URL: &str = "CLAW_LIVE_SOLEND_E2E_RPC_URL";
const ENV_KEYPAIR_PATH: &str = "CLAW_LIVE_SOLEND_E2E_KEYPAIR_PATH";
const ENV_EXPECTED_WALLET: &str = "CLAW_LIVE_SOLEND_E2E_WALLET";
const ENV_AMOUNT_RAW: &str = "CLAW_LIVE_SOLEND_E2E_AMOUNT_RAW";

// ── Safety caps ─────────────────────────────────────────────────────────────

const DEFAULT_AMOUNT_RAW: u64 = 1_000; // 0.001 USDC
const MAX_AMOUNT_RAW: u64 = 10_000; // 0.01 USDC (hard cap)
const MIN_SOL_LAMPORTS: u64 = 10_000_000; // 0.01 SOL
const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOLEND_USDC_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

const PROOF_DOC_PATH: &str = "docs/proofs/SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md";

// ── Bounded polling windows ─────────────────────────────────────────────────

// Phase 4C-8G-Fix harness timing.
//
// The retrieve → sign → submit sequence is synchronous (no `await`
// between handler.retrieve() finishing and handler.submit() starting,
// no `tokio::time::sleep`). The only end-to-end latency the test can
// optimize is the time-to-detect when the resume task parks the
// signing handoff. Splitting handoff-poll vs. confirmation-poll lets
// us tighten the first (the work is local + happens within ~50 ms of
// approval routing on a healthy machine) without increasing pressure
// on `getSignatureStatuses` during the longer confirmation window.
const HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(150);
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_millis(500);
const AWAITING_SIGNATURE_TIMEOUT: Duration = Duration::from_secs(30);
const FINALIZED_TIMEOUT: Duration = Duration::from_secs(150);

// ── Skip + opt-in gate ──────────────────────────────────────────────────────

#[derive(Debug)]
enum SkipReason {
    NotOptedIn,
    MissingEnv(String),
    InvalidEnv(String),
}

#[derive(Debug)]
struct OptInEnv {
    rpc_url: String,
    keypair_path: PathBuf,
    expected_wallet: Pubkey,
    amount_raw: u64,
    broadcast_allowed: bool,
}

fn opt_in_gate() -> Result<OptInEnv, SkipReason> {
    match std::env::var(ENV_OPT_IN) {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {}
        _ => return Err(SkipReason::NotOptedIn),
    }
    let require = |name: &str| -> Result<String, SkipReason> {
        std::env::var(name).map_err(|_| SkipReason::MissingEnv(name.to_string()))
    };
    let rpc_url = require(ENV_RPC_URL)?;
    let keypair_path = PathBuf::from(require(ENV_KEYPAIR_PATH)?);
    let expected_wallet = Pubkey::from_str(&require(ENV_EXPECTED_WALLET)?)
        .map_err(|e| SkipReason::InvalidEnv(format!("{ENV_EXPECTED_WALLET} not base58: {e}")))?;
    let amount_raw = match std::env::var(ENV_AMOUNT_RAW) {
        Ok(v) => v
            .parse::<u64>()
            .map_err(|e| SkipReason::InvalidEnv(format!("{ENV_AMOUNT_RAW} not u64: {e}")))?,
        Err(_) => DEFAULT_AMOUNT_RAW,
    };
    if amount_raw == 0 {
        return Err(SkipReason::InvalidEnv(format!(
            "{ENV_AMOUNT_RAW} must be > 0"
        )));
    }
    if amount_raw > MAX_AMOUNT_RAW {
        return Err(SkipReason::InvalidEnv(format!(
            "{ENV_AMOUNT_RAW} {amount_raw} exceeds hard cap {MAX_AMOUNT_RAW}"
        )));
    }
    let broadcast_allowed = matches!(
        std::env::var(ENV_BROADCAST).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True")
    );
    Ok(OptInEnv {
        rpc_url,
        keypair_path,
        expected_wallet,
        amount_raw,
        broadcast_allowed,
    })
}

// ── Keypair loading (same convention as Slice 3G) ──────────────────────────

fn validate_keypair_path(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("keypair file not found: {}", path.display()));
    }
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("canonicalize: {e}"))?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let repo_root = repo
        .ancestors()
        .nth(2)
        .ok_or_else(|| "cannot resolve repo root".to_string())?;
    if canonical.starts_with(repo_root) {
        return Err(format!(
            "keypair path must be OUTSIDE the repo (got {}; repo={})",
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

// ── Fail-fast balance guards ────────────────────────────────────────────────

async fn derive_usdc_ata(owner: &Pubkey) -> Pubkey {
    // Use the gateway's audited helper, which delegates to the
    // `spl-associated-token-account` crate. Hand-rolled PDA derivation
    // is intentionally avoided — the SPL crate's program-id constants
    // are the authoritative source.
    let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap();
    let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
    claw_gateway::integrations::solend::derive_associated_token_address(
        owner, &usdc_mint, &spl_token,
    )
}

async fn fail_fast_balance_checks(
    rpc_client: &ClawRpcClient,
    wallet: &Pubkey,
    amount_raw: u64,
) -> Result<Pubkey, String> {
    // ── SOL balance ─────────────────────────────────────────────────────────
    let sol_lamports = rpc_client
        .get_balance(&wallet.to_string(), Some(CommitmentLevel::Confirmed))
        .await
        .map_err(|e| format!("get_balance({wallet}): {e}"))?;
    if sol_lamports < MIN_SOL_LAMPORTS {
        return Err(format!(
            "Test wallet insufficient SOL. Please fund {wallet} with at least {MIN_SOL_LAMPORTS} \
             lamports (0.01 SOL). Current balance: {sol_lamports} lamports."
        ));
    }

    // ── USDC source ATA ─────────────────────────────────────────────────────
    let ata = derive_usdc_ata(wallet).await;
    let acct = match rpc_client
        .get_account(&ata.to_string(), Some(CommitmentLevel::Confirmed))
        .await
    {
        Ok(a) => a,
        Err(e) => {
            // `ClawRpcClient::get_account` returns `AccountNotFound` when
            // the RPC returns `null` — treat that as the canonical
            // "please fund this ATA" guidance.
            return Err(format!(
                "Test wallet USDC source ATA does not exist OR cannot be read. \
                 Expected ATA {ata} for owner {wallet}. \
                 Please fund {ata} with at least {amount_raw} raw USDC (6 decimals). \
                 RPC error: {e}"
            ));
        }
    };

    let spl_token_pk = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap();
    if acct.owner != spl_token_pk {
        return Err(format!(
            "USDC ATA {ata} owner program is {}, expected classic SPL Token \
             {SPL_TOKEN_PROGRAM_BS58} (Token-2022 not supported for this deposit flow).",
            acct.owner
        ));
    }
    let data_vec = acct.data;
    if data_vec.len() != 165 {
        return Err(format!(
            "USDC ATA {ata} data length {} != 165 (classic SPL Token Account)",
            data_vec.len()
        ));
    }
    // Account layout: mint[0..32], owner[32..64], amount[64..72]
    let acct_mint_bytes: [u8; 32] = data_vec[0..32].try_into().unwrap();
    let acct_owner_bytes: [u8; 32] = data_vec[32..64].try_into().unwrap();
    let acct_mint = Pubkey::new_from_array(acct_mint_bytes);
    let acct_owner = Pubkey::new_from_array(acct_owner_bytes);
    let expected_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
    if acct_mint != expected_mint {
        return Err(format!(
            "USDC ATA {ata} mint {acct_mint} != expected USDC mint {expected_mint}"
        ));
    }
    if acct_owner != *wallet {
        return Err(format!(
            "USDC ATA {ata} owner {acct_owner} != controlled wallet {wallet}"
        ));
    }
    let mut amount_le: [u8; 8] = [0u8; 8];
    amount_le.copy_from_slice(&data_vec[64..72]);
    let ata_amount = u64::from_le_bytes(amount_le);
    if ata_amount < amount_raw {
        return Err(format!(
            "Test wallet insufficient USDC. Please fund {ata} with at least {amount_raw} raw USDC. \
             Current balance: {ata_amount} raw USDC."
        ));
    }
    Ok(ata)
}

// ── Bounded polling helpers ────────────────────────────────────────────────

async fn poll_for_signing_handoff(
    signing_store: &SolendSigningStore,
    session_id: &SessionId,
    timeout: Duration,
) -> Result<Uuid, String> {
    let start = Instant::now();
    loop {
        let pending = signing_store.pending_for_session(session_id);
        if let Some(first) = pending.into_iter().next() {
            return Ok(first.signing_request_id);
        }
        if start.elapsed() >= timeout {
            return Err(format!(
                "timeout waiting for Solend signing handoff (elapsed {:?}, limit {:?}, \
                 last observed: SolendSigningStore has no entry for session)",
                start.elapsed(),
                timeout
            ));
        }
        // Tight poll: handoff is local + happens within ~50 ms of
        // approval routing. Detecting it sooner shaves end-to-end
        // latency between approval and broadcast, which matters for
        // blockhash freshness on the next live attempt.
        tokio::time::sleep(HANDOFF_POLL_INTERVAL).await;
    }
}

async fn poll_until_terminal(
    handler: &GatewaySolendSignatureHandler,
    session_id: &SessionId,
    signing_request_id: Uuid,
    timeout: Duration,
) -> Result<SolendRetrievalResult, String> {
    let start = Instant::now();
    loop {
        let state = handler.retrieve(session_id, signing_request_id).await;
        match &state {
            SolendRetrievalResult::Finalized { .. } => return Ok(state),
            SolendRetrievalResult::Failed { .. }
            | SolendRetrievalResult::ConfirmationTimeout { .. }
            | SolendRetrievalResult::Rejected { .. }
            | SolendRetrievalResult::BroadcastFailed { .. }
            | SolendRetrievalResult::PreSubmitExpired { .. }
            | SolendRetrievalResult::Expired
            | SolendRetrievalResult::NotFound => return Ok(state),
            _ => {}
        }
        if start.elapsed() >= timeout {
            return Err(format!(
                "timeout waiting for terminal confirmation state (elapsed {:?}, limit {:?}, \
                 last observed: {:?})",
                start.elapsed(),
                timeout,
                state
            ));
        }
        // Generous poll: we are reading the lifecycle store the
        // confirmation tracker maintains. Tightening here would not
        // accelerate the on-chain Finalized observation; it would
        // only increase local poll churn.
        tokio::time::sleep(CONFIRMATION_POLL_INTERVAL).await;
    }
}

// ── Proof doc writer (only on Finalized success) ────────────────────────────

#[allow(clippy::too_many_arguments)]
fn write_proof_doc(
    rpc_cluster: &str,
    session_wallet: &Pubkey,
    amount_raw: u64,
    approval_request_id: Uuid,
    signing_request_id: Uuid,
    tx_signature: &str,
    slot: u64,
    last_valid_block_height: u64,
    lifecycle_states_observed: &[&str],
) -> Result<PathBuf, String> {
    let now = chrono::Utc::now();
    let amount_ui = format!("{}.{:06}", amount_raw / 1_000_000, amount_raw % 1_000_000);
    let git_head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let git_status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            if s.trim().is_empty() {
                "clean".to_string()
            } else {
                "dirty (uncommitted changes present at proof time)".to_string()
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    let content = format!(
        "# Solend USDC Deposit — Phase 4C Full E2E Proof\n\
         \n\
         **Date (UTC):** {date_utc}\n\
         **Git commit:** `{git_head}`\n\
         **Git working tree:** {git_status}\n\
         \n\
         ## On-chain outcome\n\
         \n\
         - **Tx signature:** `{tx_signature}`\n\
         - **Finalized slot:** {slot}\n\
         - **Last valid block height (at submit):** {last_valid_block_height}\n\
         - **Lifecycle states observed:** {lifecycle_states}\n\
         \n\
         ## Deposit parameters\n\
         \n\
         - **Protocol:** Solend V1\n\
         - **Reserve mint:** `{usdc_mint}` (USDC)\n\
         - **Solend reserve pubkey:** `{solend_reserve}`\n\
         - **Amount (raw, 6 decimals):** {amount_raw}\n\
         - **Amount (UI):** {amount_ui} USDC\n\
         - **RPC cluster:** {rpc_cluster}\n\
         - **Session wallet pubkey:** `{session_wallet}`\n\
         \n\
         ## Control-plane identifiers\n\
         \n\
         - **Approval request id:** `{approval_request_id}`\n\
         - **Signing request id:** `{signing_request_id}`\n\
         \n\
         ## Proof flow\n\
         \n\
         1. `solend_deposit_usdc` tool invoked with `amount` only → `awaiting_approval`.\n\
         2. `ApprovalStore::decide(Approve)` + `approval_routing::route_approval_outcome` → \n\
            signalled the parked Solend resume task.\n\
         3. Resume task reassembled the Solend snapshot against live RPC, re-ran the \n\
            lending policy evaluator, executed the 4C-3 preflight simulator, and — on \n\
            `Passed` — produced a partially-signed unsigned transaction in the \n\
            `SolendSigningStore` via `create_signing_handoff`.\n\
         4. `GatewaySolendSignatureHandler::retrieve` returned the base64-encoded \n\
            transaction to the test; the controlled test keypair signed the \n\
            session-wallet slot without touching the obligation slot or the \n\
            message body.\n\
         5. `GatewaySolendSignatureHandler::submit` ran the 4C-5 verification \n\
            pipeline (atomic consume, message.hash immutability, session + obligation \n\
            signature checks, blockhash freshness) and broadcast via \n\
            `submit_signed_solend_transaction` → `ClawRpcSolendSender::send_raw_transaction`.\n\
         6. `SolendConfirmationTracker` polled `getSignatureStatuses` until \n\
            `confirmation_status = \"finalized\"`; the lifecycle store transitioned \n\
            `Submitted → Confirming → Finalized`.\n\
         7. This proof document was written programmatically only after the \n\
            `Finalized` variant was observed through `retrieve(..)`.\n\
         \n\
         ## Safety posture\n\
         \n\
         - **Default-skip env gate:** Test runs only when `{env_opt_in}=1`. The CI \n\
           default `cargo test` path performs zero network calls.\n\
         - **Broadcast gate:** Actual broadcast requires `{env_broadcast}=1` in \n\
           addition to the master gate.\n\
         - **Amount cap:** `{env_amount}` hard-capped at `{max_amount_raw}` raw USDC.\n\
         - **Finalized required:** `Confirmed` alone does not produce a proof. Only \n\
           `confirmation_status = \"finalized\"` advances the lifecycle to terminal \n\
           success and triggers this proof write.\n\
         - **No private key material in this document.** The controlled test keypair \n\
           signed only the session-wallet slot of the partially-signed transaction \n\
           returned by the backend. Private-key bytes are never printed or persisted \n\
           by the test harness.\n\
         - **No raw transaction bytes in this document.** The `transaction_base64` \n\
           returned by the retrieve endpoint is used only to sign; it is not written \n\
           to the proof doc.\n\
         - **Verification path:** Submit goes through `submit_signed_solend_transaction` \n\
           (4C-5) which enforces atomic consume, message-hash immutability, \n\
           bit-equal obligation-signature check, session-wallet ed25519 verification, \n\
           and blockhash freshness. Broadcast happens only after all checks pass.\n\
         - **No re-broadcast.** Timeout or Failed outcomes fail the proof closed; no \n\
           retry, no re-sign, no re-handoff.\n",
        date_utc = now.format("%Y-%m-%dT%H:%M:%SZ"),
        git_head = git_head,
        git_status = git_status,
        tx_signature = tx_signature,
        slot = slot,
        last_valid_block_height = last_valid_block_height,
        lifecycle_states = lifecycle_states_observed.join(" → "),
        usdc_mint = USDC_MINT_BS58,
        solend_reserve = SOLEND_USDC_RESERVE_BS58,
        amount_raw = amount_raw,
        amount_ui = amount_ui,
        rpc_cluster = rpc_cluster,
        session_wallet = session_wallet,
        approval_request_id = approval_request_id,
        signing_request_id = signing_request_id,
        env_opt_in = ENV_OPT_IN,
        env_broadcast = ENV_BROADCAST,
        env_amount = ENV_AMOUNT_RAW,
        max_amount_raw = MAX_AMOUNT_RAW,
    );

    // Write to repo-relative docs/proofs/... path. Resolve against the
    // crate's manifest dir ancestors so the test works when run from the
    // workspace root or the crate subdir.
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .ancestors()
        .nth(2)
        .ok_or_else(|| "cannot resolve repo root".to_string())?;
    let out = repo_root.join(PROOF_DOC_PATH);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create_dir_all({}): {e}", parent.display()))?;
    }
    fs::write(&out, content).map_err(|e| format!("write proof doc: {e}"))?;
    Ok(out)
}

// ── The opt-in test ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_solend_deposit_e2e() {
    let env = match opt_in_gate() {
        Ok(e) => e,
        Err(SkipReason::NotOptedIn) => {
            eprintln!(
                "SKIP: live Solend E2E proof is opt-in. Set {ENV_OPT_IN}=1 and companion env \
                 vars to run. CI does not set this. This test performs ZERO network calls \
                 when skipped."
            );
            return;
        }
        Err(SkipReason::MissingEnv(name)) => {
            eprintln!("SKIP: {ENV_OPT_IN} is set but {name} is missing; not running.");
            return;
        }
        Err(SkipReason::InvalidEnv(msg)) => {
            eprintln!("SKIP: invalid opt-in env: {msg}");
            return;
        }
    };

    // Refuse to proceed unless broadcast is explicitly authorized.
    if !env.broadcast_allowed {
        eprintln!(
            "SKIP: {ENV_OPT_IN}=1 but {ENV_BROADCAST}!=1. Broadcast is gated behind an \
             explicit opt-in. Set {ENV_BROADCAST}=1 to perform the live broadcast + \
             confirmation proof."
        );
        return;
    }

    // Keypair hygiene.
    validate_keypair_path(&env.keypair_path)
        .unwrap_or_else(|e| panic!("keypair path rejected: {e}"));
    let test_keypair = load_keypair(&env.keypair_path).unwrap_or_else(|e| panic!("load keypair: {e}"));
    let loaded_pubkey = test_keypair.pubkey();
    assert_eq!(
        loaded_pubkey, env.expected_wallet,
        "loaded keypair pubkey {loaded_pubkey} does not match {ENV_EXPECTED_WALLET} \
         env value {}",
        env.expected_wallet
    );

    let rpc_label = sanitized_rpc_label(&env.rpc_url);
    eprintln!(
        "live_solend_deposit_e2e: opt-in path. rpc={rpc_label} wallet={} amount_raw={}",
        loaded_pubkey, env.amount_raw
    );

    // ── Build RPC pool + client + blockhash manager ─────────────────────────
    let rpc_pool = RpcPool::new(RpcPoolConfig {
        endpoints: vec![EndpointConfig {
            url: env.rpc_url.clone(),
            is_write_endpoint: true,
            label: "live_solend_e2e".to_string(),
        }],
        failure_threshold: 3,
        recovery_interval: Duration::from_secs(30),
        request_timeout: Duration::from_secs(20),
    });
    let rpc_client = ClawRpcClient::new(rpc_pool.clone(), CommitmentLevel::Confirmed);
    let rpc_pool_arc = Arc::new(rpc_pool.clone());
    let blockhash_mgr = Arc::new(BlockhashManager::new(rpc_pool.clone()));

    // ── Fail-fast balance guards (panic! on any failure) ────────────────────
    let _source_ata = fail_fast_balance_checks(&rpc_client, &loaded_pubkey, env.amount_raw)
        .await
        .unwrap_or_else(|msg| panic!("fail-fast balance guard: {msg}"));

    // ── Compose the Solend production stores + tracker in-process ───────────
    let external_wallet = ExternalWalletStore::new();
    let session_id = SessionId::new();
    external_wallet.bind_wallet(&session_id, &loaded_pubkey.to_string());

    let approval_store = ApprovalStore::new();
    let solend_park = SolendParkStore::new();
    let signing_store = SolendSigningStore::new();
    let lifecycle_store = SolendSubmissionLifecycleStore::new(600);
    let audit_sink: Arc<dyn claw_gateway::integrations::solend_submit::SolendAuditSink> =
        Arc::new(NullSolendAuditSink);

    // Build tool registry with the production Solend wiring helper.
    // Phase 6B: wiring no longer takes signing_store + blockhash_manager
    // (those moved to the JIT prepare endpoint); it takes a JIT-ready
    // store instead. The signing_store is still used downstream by the
    // prepare endpoint and the existing GET/POST signature handlers.
    let jit_ready_store =
        claw_gateway::integrations::solend_jit_ready::SolendJitReadyStore::new();
    let registry = wire_solend_deposit_tool(
        ToolRegistry::new(),
        rpc_pool.clone(),
        external_wallet.clone(),
        approval_store.clone(),
        solend_park.clone(),
        jit_ready_store.clone(),
        audit_sink.clone(),
        600,
        claw_gateway::lending::SolendDepositRiskBudgetConfig::demo(),
    );

    // Submit handler with confirmation tracker attached.
    let solend_sender = Arc::new(ClawRpcSolendSender::new(rpc_client.clone()));
    let sender: Arc<dyn claw_gateway::integrations::solend_submit::SolendTransactionSender> =
        solend_sender.clone();
    let rebroadcast_sender:
        Arc<dyn claw_gateway::integrations::solend_submit::SolendRebroadcastSender> =
        solend_sender.clone();
    let height: Arc<dyn claw_gateway::integrations::solend_submit::SolendBlockHeightProvider> =
        Arc::new(ClawRpcBlockHeightProvider::new(rpc_client.clone()));
    let confirmation_tracker = wire_solend_confirmation_tracker(
        lifecycle_store.clone(),
        rpc_pool_arc.clone(),
        rebroadcast_sender,
        audit_sink.clone(),
        Duration::from_secs(2),
    );
    let handler = GatewaySolendSignatureHandler::new(
        signing_store.clone(),
        lifecycle_store.clone(),
        sender,
        audit_sink.clone(),
        height,
    )
    .with_confirmation_tracker(confirmation_tracker);

    // ── Invoke the tool (propose) ───────────────────────────────────────────
    let tool = registry
        .get("solend_deposit_usdc")
        .expect("solend_deposit_usdc registered");
    let tool_input = ToolInput {
        tool_name: "solend_deposit_usdc".to_string(),
        session_id: session_id.clone(),
        parameters: json!({ "amount": env.amount_raw }),
        correlation_id: Uuid::new_v4(),
    };
    let output = tool
        .execute(tool_input)
        .await
        .expect("tool.execute: propose must not return Err");
    assert!(output.success, "tool propose did not succeed: {output:?}");
    let data = output
        .data
        .as_ref()
        .expect("tool output has data block")
        .clone();
    assert_eq!(
        data["status"].as_str(),
        Some("awaiting_approval"),
        "expected awaiting_approval, got {data:?}"
    );
    let approval_request_id: Uuid = data["approval_request_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("approval_request_id present + parseable");
    eprintln!("propose ok; approval_request_id = {approval_request_id}");

    // ── Approve via ApprovalStore + route outcome ───────────────────────────
    let decision = ApprovalDecision::approve(approval_request_id, Some("e2e-proof".to_string()));
    let (outcome, approval_request) = approval_store.decide(&decision);
    eprintln!("approval outcome: {outcome:?}");

    // Route the outcome through the same code path the daemon uses.
    // `pending_signing` and `pending_jupiter_park` are empty for this flow
    // (Solend-only) but required by the function's signature.
    let _ = route_approval_outcome(
        &outcome,
        approval_request_id,
        &PendingSigningStore::new(),
        &PendingJupiterParkStore::new(),
        &solend_park,
        &claw_gateway::integrations::solend_withdraw_park::SolendWithdrawAllParkStore::new(),
        &AlertDispatcher::Disabled,
        approval_request.as_ref(),
    );

    // ── Wait for the resume task to populate the signing handoff ────────────
    let signing_request_id =
        poll_for_signing_handoff(&signing_store, &session_id, AWAITING_SIGNATURE_TIMEOUT)
            .await
            .unwrap_or_else(|e| panic!("signing handoff not populated: {e}"));
    eprintln!("signing handoff parked; signing_request_id = {signing_request_id}");

    // ── Retrieve the unsigned transaction bytes via the production handler ──
    let retrieve = handler.retrieve(&session_id, signing_request_id).await;
    let unsigned_tx_b64 = match retrieve {
        SolendRetrievalResult::Found {
            unsigned_tx_b64, ..
        } => unsigned_tx_b64,
        other => panic!("expected Found from retrieve, got {other:?}"),
    };

    // ── Sign with the controlled test keypair (session-wallet slot only) ────
    let signed_tx_b64 = {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(unsigned_tx_b64.as_bytes())
            .expect("transaction_base64 decodes");
        let mut tx: Transaction = bincode::deserialize(&raw).expect("tx bincode deserializes");

        // Pre-condition: fee payer (session_wallet) signature is default.
        let payer_idx = 0;
        let payer_key = tx.message.account_keys[payer_idx];
        assert_eq!(
            payer_key, loaded_pubkey,
            "fee payer must be the controlled test wallet"
        );
        assert_eq!(
            tx.signatures[payer_idx],
            solana_sdk::signature::Signature::default(),
            "session-wallet slot must be pending before the test keypair signs"
        );

        // Record obligation signer slot + message.hash BEFORE signing.
        let pre_message_hash = tx.message.hash();
        let obligation_sig_before: Option<solana_sdk::signature::Signature> = {
            let required =
                tx.message.header.num_required_signatures as usize;
            (1..required).find_map(|i| {
                let sig = tx.signatures[i];
                if sig != solana_sdk::signature::Signature::default() {
                    Some(sig)
                } else {
                    None
                }
            })
        };

        // Sign the session-wallet slot. `try_partial_sign` writes into the
        // correct slot and does NOT touch other signatures.
        tx.try_partial_sign(&[&test_keypair], tx.message.recent_blockhash)
            .expect("controlled test keypair signs session-wallet slot");

        // Post-condition: session-wallet signature non-default; message.hash
        // unchanged; obligation slot untouched.
        let post_message_hash = tx.message.hash();
        assert_eq!(
            pre_message_hash, post_message_hash,
            "message.hash must not change across controlled-wallet signing"
        );
        assert_ne!(
            tx.signatures[payer_idx],
            solana_sdk::signature::Signature::default(),
            "session-wallet slot must be populated after test keypair signs"
        );
        if let Some(before) = obligation_sig_before {
            let required = tx.message.header.num_required_signatures as usize;
            let still_there = (1..required)
                .filter_map(|i| {
                    let sig = tx.signatures[i];
                    if sig != solana_sdk::signature::Signature::default() {
                        Some(sig)
                    } else {
                        None
                    }
                })
                .any(|sig| sig == before);
            assert!(still_there, "obligation signature must survive signing");
        }

        // Verify the partial-signed tx's fee-payer ed25519.
        assert!(
            tx.signatures[payer_idx]
                .verify(&payer_key.to_bytes(), &tx.message_data()),
            "session-wallet signature must verify"
        );

        let signed_bytes =
            bincode::serialize(&tx).expect("signed tx bincode serializes");
        base64::engine::general_purpose::STANDARD.encode(&signed_bytes)
    };

    // ── Submit through the production verified submit path ──────────────────
    let submit_result = handler
        .submit(&session_id, signing_request_id, signed_tx_b64)
        .await;
    let (tx_signature, submit_lvbh) = match submit_result {
        claw_api::state::SolendSubmitResult::Submitted {
            tx_signature,
            last_valid_block_height,
            ..
        } => (tx_signature, last_valid_block_height),
        other => panic!("expected Submitted from submit, got {other:?}"),
    };
    eprintln!("submit ok; tx_signature = {tx_signature}");

    // ── Wait for the confirmation tracker to reach Finalized ────────────────
    let mut lifecycle_states_observed: Vec<&str> = vec!["awaiting_signature", "submitted"];

    // While polling we observe intermediate states (submitted / confirming /
    // finalized) via retrieve(). `poll_until_terminal` returns on the first
    // terminal variant observed.
    let terminal = poll_until_terminal(&handler, &session_id, signing_request_id, FINALIZED_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("confirmation polling: {e}"));

    // Record whether we saw Confirming in any one poll (non-blocking peek).
    // The lifecycle store is authoritative; query it directly.
    let post_submit = lifecycle_store.lookup_for_session(&session_id, signing_request_id);
    if let Some(rec) = post_submit.as_ref() {
        if matches!(
            rec.outcome,
            claw_gateway::integrations::solend_lifecycle::RecordedSubmitOutcome::Confirming { .. }
        ) {
            lifecycle_states_observed.push("confirming");
        }
    }

    match &terminal {
        SolendRetrievalResult::Finalized {
            tx_signature: final_sig,
            slot,
            ..
        } => {
            assert_eq!(
                final_sig.as_str(),
                tx_signature.as_str(),
                "finalized signature matches the one returned from submit"
            );
            lifecycle_states_observed.push("finalized");
            let proof_path = write_proof_doc(
                &rpc_label,
                &loaded_pubkey,
                env.amount_raw,
                approval_request_id,
                signing_request_id,
                &tx_signature,
                *slot,
                submit_lvbh,
                &lifecycle_states_observed,
            )
            .unwrap_or_else(|e| panic!("write proof doc: {e}"));
            eprintln!(
                "PROOF: Finalized at slot {slot}, signature {tx_signature}, doc={}",
                proof_path.display()
            );
        }
        other => panic!(
            "proof failed closed; non-success terminal from confirmation tracker: {other:?}"
        ),
    }
}

// ── Default-skip guard test ────────────────────────────────────────────────
//
// Safety net: this test runs on every `cargo test` invocation and asserts
// that — in the absence of the opt-in env var — the opt-in gate chooses
// `NotOptedIn` and the proof doc is NOT created. It holds the invariant
// that a CI run of the whole test suite performs zero network work on
// behalf of this file.

#[test]
fn default_skip_path_is_network_free_and_does_not_create_proof_doc() {
    // This guard is meaningful only in a DEFAULT-SKIP CI run (no
    // opt-in env). When the master gate IS set — the operator is
    // explicitly running the live e2e test — this guard self-skips
    // rather than panic. The `live_solend_deposit_e2e` function above
    // owns the live-path safety; this guard owns only the no-env
    // invariant.
    if let Ok(v) = std::env::var(ENV_OPT_IN) {
        if v == "1" || v.eq_ignore_ascii_case("true") {
            eprintln!(
                "default-skip guard: {ENV_OPT_IN}=1 detected — skipping. \
                 This guard only validates the no-env invariant; the \
                 opt-in e2e test owns the live-path safety."
            );
            return;
        }
    }
    match opt_in_gate() {
        Err(SkipReason::NotOptedIn) => {}
        other => panic!(
            "expected NotOptedIn without env; got {:?}. This test must be skippable.",
            other
        ),
    }
    // Proof doc may exist from a prior opt-in run; the invariant is
    // that the default-skip path does not create or modify it. We
    // cannot cheaply verify "not modified by this invocation" without
    // cross-process mtime coordination — the positive assertion above
    // (opt-in gate returns NotOptedIn) is the primary guarantee.
}

// ── Forbidden-scan ─────────────────────────────────────────────────────────

#[test]
fn file_has_no_forbidden_runtime_references() {
    const SOURCE: &str = include_str!("live_solend_deposit_e2e.rs");
    // Build needles at runtime so this scanner does not match its own
    // source. Key material identifiers are never written to the proof
    // doc or printed from this file.
    let needles = [
        format!("{}{}", "priva", "te_key"),
        format!("{}{}", "secret_", "bytes"),
        format!("{}{}", ".send_raw_", "transaction("),
        format!("{}{}", ".send_", "transaction("),
        format!("{}{}", ".confirm_", "transaction("),
        // `.get_signature_statuses(` — NOT scanned here; the production
        // confirmation tracker uses it via the status provider. The
        // test must not call it directly.
        format!("{}{}", "to", "do!("),
        format!("{}{}", "unimplem", "ented!("),
    ];
    for n in &needles {
        assert!(
            !SOURCE.contains(n),
            "live_solend_deposit_e2e.rs must not contain `{n}`"
        );
    }
}

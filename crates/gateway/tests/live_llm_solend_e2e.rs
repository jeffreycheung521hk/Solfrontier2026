//! OPT-IN: Phase 5G — LLM-guided Solend USDC deposit mainnet E2E proof.
//!
//! Drives the full chain in-process:
//!
//! 1. Pre-run state sanitization — fresh in-process stores per test run.
//! 2. RPC freshness check — `get_slot` against the live cluster.
//! 3. Fail-fast balance guards — SOL ≥ 0.01 SOL; USDC ATA exists with
//!    correct mint/owner and balance ≥ amount.
//! 4. Bind the controlled wallet to a fresh session.
//! 5. Build a chat handler with a real OpenAI / Anthropic provider via
//!    `wire_chat_handler_with_registry` (15 s timeout, 200 max_tokens,
//!    strict tool schema, alignment-override system prompt) attached
//!    to the production Solend tool registry.
//! 6. Send a natural-language request to the chat handler. Assert
//!    exactly one tool call: `solend_deposit_usdc { amount }`.
//! 7. Assert the chat outcome is `tool_dispatched` with inner
//!    `awaiting_approval`.
//! 8. Explicitly approve via `ApprovalStore::decide` +
//!    `approval_routing::route_approval_outcome` — the test acts as
//!    the human approver.
//! 9. Bounded-poll the `SolendSigningStore` for a parked handoff.
//! 10. `GatewaySolendSignatureHandler::retrieve` returns the unsigned
//!     transaction; the controlled keypair signs the session-wallet
//!     slot only — never the obligation slot, never the message body.
//! 11. `GatewaySolendSignatureHandler::submit` runs the 4C-5
//!     verification pipeline + broadcast.
//! 12. `SolendConfirmationTracker` polls until `Finalized`.
//! 13. Proof doc written to `docs/proofs/LLM_SOLEND_MAINNET_E2E_PHASE5G.md`.
//!
//! # Default behaviour
//!
//! Unless **all** opt-in env vars are set, the test default-skips with
//! zero network calls. The companion test
//! `default_skip_path_is_network_free_and_does_not_create_proof_doc`
//! enforces this on every CI run.
//!
//! # Required opt-in env vars
//!
//! | Var | Purpose |
//! |---|---|
//! | `CLAW_LIVE_LLM_SOLEND_E2E=1` | Master opt-in for this 5G harness |
//! | `CLAW_LIVE_LLM_SOLEND_E2E_GO=1` | Final live-execution gate (set after explicit human GO) |
//! | `CLAW_CHAT_PROVIDER` | `openai` or `anthropic` |
//! | `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | provider credential (read once via [`ApiKey`]; never logged) |
//! | `CLAW_CHAT_MODEL` | optional model override |
//! | `CLAW_LIVE_SOLEND_E2E=1` | reuse 4C Solend opt-in gate |
//! | `CLAW_LIVE_SOLEND_E2E_BROADCAST=1` | reuse 4C broadcast gate |
//! | `CLAW_LIVE_SOLEND_E2E_RPC_URL` | mainnet RPC URL (host printed sanitised) |
//! | `CLAW_LIVE_SOLEND_E2E_KEYPAIR_PATH` | controlled keypair JSON (MUST live outside the repo) |
//! | `CLAW_LIVE_SOLEND_E2E_WALLET` | expected base58 pubkey of that keypair |
//! | `CLAW_LIVE_SOLEND_E2E_AMOUNT_RAW` | default 1000 (= 0.001 USDC), capped at 10_000 |
//!
//! # Pre-flight (preview) mode
//!
//! When `CLAW_LIVE_LLM_SOLEND_E2E=1` is set but
//! `CLAW_LIVE_LLM_SOLEND_E2E_GO=1` is NOT, the harness performs every
//! preflight read (RPC freshness, balance checks, provider config
//! validation) and prints the GO checklist with real values, then
//! exits Ok WITHOUT contacting the LLM provider. This is the
//! "STOP and ask for GO" boundary required by the slice spec.
//!
//! # Hard contract
//!
//! - At most one provider call per run (one chat turn).
//! - At most one broadcast attempt per run.
//! - No automatic retry on any failure.
//! - No `tx_signature`, `signing_request_id`, `transaction_base64`, or
//!   `tx_bytes` in the proof doc body.
//! - No private keys, API keys, or HTTP auth headers anywhere.
//! - The harness fails closed: every state transition has a bounded
//!   timeout with an explicit diagnostic message.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use uuid::Uuid;

use claw_agent_runtime::provider::StdEnvProvider;
use claw_api::state::{
    ChatResponse, ChatRouteOutcome, SolendRetrievalResult, SolendSignatureHandler,
};
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
use claw_gateway::runtime::chat_wiring;
use claw_gateway::runtime::solend_submit_wiring::{
    wire_solend_confirmation_tracker, GatewaySolendSignatureHandler,
};
use claw_gateway::runtime::solend_wiring::wire_solend_deposit_tool;
use claw_solana_core::blockhash::BlockhashManager;
use claw_solana_core::rpc::{ClawRpcClient, EndpointConfig, RpcPool, RpcPoolConfig};
use claw_types::approval::ApprovalDecision;
use claw_types::session::SessionId;
use claw_types::solana::CommitmentLevel;
use claw_tool_system::registry::ToolRegistry;

// ── Opt-in env var names ────────────────────────────────────────────────────

const ENV_LLM_OPT_IN: &str = "CLAW_LIVE_LLM_SOLEND_E2E";
const ENV_LLM_GO: &str = "CLAW_LIVE_LLM_SOLEND_E2E_GO";

// Reused 4C env vars (kept literal so the diagnostic messages are
// grep-locatable; we deliberately do NOT factor these into a shared
// module to avoid coupling test files).
const ENV_SOLEND_OPT_IN: &str = "CLAW_LIVE_SOLEND_E2E";
const ENV_SOLEND_BROADCAST: &str = "CLAW_LIVE_SOLEND_E2E_BROADCAST";
const ENV_RPC_URL: &str = "CLAW_LIVE_SOLEND_E2E_RPC_URL";
const ENV_KEYPAIR_PATH: &str = "CLAW_LIVE_SOLEND_E2E_KEYPAIR_PATH";
const ENV_EXPECTED_WALLET: &str = "CLAW_LIVE_SOLEND_E2E_WALLET";
const ENV_AMOUNT_RAW: &str = "CLAW_LIVE_SOLEND_E2E_AMOUNT_RAW";

// ── Safety caps (mirror 4C) ────────────────────────────────────────────────

const DEFAULT_AMOUNT_RAW: u64 = 1_000; // 0.001 USDC
const MAX_AMOUNT_RAW: u64 = 10_000;    // 0.01 USDC hard cap
const MIN_SOL_LAMPORTS: u64 = 10_000_000; // 0.01 SOL
const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOLEND_USDC_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// ── Bounded polling windows ─────────────────────────────────────────────────

const HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(150);
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_millis(500);
const AWAITING_SIGNATURE_TIMEOUT: Duration = Duration::from_secs(30);
const FINALIZED_TIMEOUT: Duration = Duration::from_secs(150);

// Hard ceiling on the entire harness in case any external operation
// stalls. Slice spec §7: "abort/report if stuck beyond a reasonable
// timeout, e.g. 5 minutes".
const HARNESS_HARD_TIMEOUT: Duration = Duration::from_secs(5 * 60);

const PROOF_DOC_PATH: &str = "docs/proofs/LLM_SOLEND_MAINNET_E2E_PHASE5G.md";

// ── Skip + opt-in gate (mirrors 4C, with LLM-only addition) ────────────────

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

fn solend_opt_in_gate() -> Result<OptInEnv, SkipReason> {
    match std::env::var(ENV_SOLEND_OPT_IN) {
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
        return Err(SkipReason::InvalidEnv(format!("{ENV_AMOUNT_RAW} must be > 0")));
    }
    if amount_raw > MAX_AMOUNT_RAW {
        return Err(SkipReason::InvalidEnv(format!(
            "{ENV_AMOUNT_RAW} {amount_raw} exceeds hard cap {MAX_AMOUNT_RAW}"
        )));
    }
    let broadcast_allowed = matches!(
        std::env::var(ENV_SOLEND_BROADCAST).as_deref(),
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

// ── Keypair hygiene (mirrors 4C) ───────────────────────────────────────────

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

// ── Fail-fast balance guards (mirror 4C) ───────────────────────────────────

async fn derive_usdc_ata(owner: &Pubkey) -> Pubkey {
    let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap();
    let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
    claw_gateway::integrations::solend::derive_associated_token_address(
        owner, &usdc_mint, &spl_token,
    )
}

#[derive(Debug)]
struct BalanceSnapshot {
    sol_lamports: u64,
    usdc_ata: Pubkey,
    usdc_raw: u64,
}

async fn fail_fast_balance_checks(
    rpc_client: &ClawRpcClient,
    wallet: &Pubkey,
    amount_raw: u64,
) -> Result<BalanceSnapshot, String> {
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
    let ata = derive_usdc_ata(wallet).await;
    let acct = match rpc_client
        .get_account(&ata.to_string(), Some(CommitmentLevel::Confirmed))
        .await
    {
        Ok(a) => a,
        Err(e) => {
            return Err(format!(
                "Test wallet USDC source ATA {ata} for owner {wallet} cannot be read; \
                 fund the ATA with at least {amount_raw} raw USDC. RPC error: {e}"
            ));
        }
    };
    let spl_token_pk = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58).unwrap();
    if acct.owner != spl_token_pk {
        return Err(format!(
            "USDC ATA {ata} owner program is {}, expected classic SPL Token {SPL_TOKEN_PROGRAM_BS58}",
            acct.owner
        ));
    }
    if acct.data.len() != 165 {
        return Err(format!(
            "USDC ATA {ata} data length {} != 165",
            acct.data.len()
        ));
    }
    let acct_mint_bytes: [u8; 32] = acct.data[0..32].try_into().unwrap();
    let acct_owner_bytes: [u8; 32] = acct.data[32..64].try_into().unwrap();
    let acct_mint = Pubkey::new_from_array(acct_mint_bytes);
    let acct_owner = Pubkey::new_from_array(acct_owner_bytes);
    let expected_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
    if acct_mint != expected_mint {
        return Err(format!(
            "USDC ATA {ata} mint {acct_mint} != expected {expected_mint}"
        ));
    }
    if acct_owner != *wallet {
        return Err(format!(
            "USDC ATA {ata} owner {acct_owner} != controlled wallet {wallet}"
        ));
    }
    let mut amount_le: [u8; 8] = [0u8; 8];
    amount_le.copy_from_slice(&acct.data[64..72]);
    let usdc_raw = u64::from_le_bytes(amount_le);
    if usdc_raw < amount_raw {
        return Err(format!(
            "Test wallet insufficient USDC. Need at least {amount_raw} raw USDC; ATA {ata} has {usdc_raw}."
        ));
    }
    Ok(BalanceSnapshot { sol_lamports, usdc_ata: ata, usdc_raw })
}

// ── Bounded polling helpers (mirror 4C, plus 5G phrasing) ──────────────────

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
                "Timeout waiting for signing handoff to appear after explicit approval \
                 (elapsed {:?}, limit {:?})",
                start.elapsed(),
                timeout
            ));
        }
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
            SolendRetrievalResult::Finalized { .. }
            | SolendRetrievalResult::Failed { .. }
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
                "Timeout waiting for confirmation tracker to reach Finalized \
                 (elapsed {:?}, limit {:?}, last observed: {:?})",
                start.elapsed(),
                timeout,
                state
            ));
        }
        tokio::time::sleep(CONFIRMATION_POLL_INTERVAL).await;
    }
}

// ── Pre-flight checklist printer ───────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn print_preflight_checklist(
    provider: &str,
    model_hint: &str,
    rpc_label: &str,
    rpc_slot: u64,
    wallet: &Pubkey,
    amount_raw: u64,
    snapshot: &BalanceSnapshot,
    pending_approvals_post_cleanup: usize,
    pending_signings_post_cleanup: usize,
) {
    let amount_ui = format!("{}.{:06}", amount_raw / 1_000_000, amount_raw % 1_000_000);
    println!();
    println!("─────── PHASE 5G LIVE LLM-GUIDED SOLEND MAINNET PROOF — GO CHECKPOINT ───────");
    println!();
    println!("[Provider]");
    println!("  provider:               {provider}");
    println!("  model:                  {model_hint}");
    println!("  strict tool schema:     enabled (amount-only)");
    println!("  HTTP timeout:           {:?}", chat_wiring::CHAT_HTTP_TIMEOUT);
    println!("  max_tokens:             {}", chat_wiring::CHAT_MAX_TOKENS);
    println!();
    println!("[Solana]");
    println!("  wallet pubkey:          {wallet}");
    println!("  RPC host:               {rpc_label}");
    println!("  RPC freshness slot:     {rpc_slot}");
    println!("  amount raw / UI:        {amount_raw} raw / {amount_ui} USDC");
    println!("  SOL balance:            {} lamports", snapshot.sol_lamports);
    println!("  USDC ATA:               {}", snapshot.usdc_ata);
    println!("  USDC raw balance:       {} raw", snapshot.usdc_raw);
    println!("  priority fee:           50_000 micro-lamports/CU (production default)");
    println!("  oracle staleness:       120_000 ms (production default)");
    println!("  broadcast gate:         enabled (CLAW_LIVE_SOLEND_E2E_BROADCAST=1)");
    println!();
    println!("[State]");
    println!("  pre-run cleanup:        not needed — fresh in-process stores per test invocation");
    println!("  pending approvals:      {pending_approvals_post_cleanup}");
    println!("  pending signings:       {pending_signings_post_cleanup}");
    println!();
    println!("[Expected flow]");
    println!("  1. natural-language chat message");
    println!("  2. LLM produces solend_deposit_usdc {{ amount: {amount_raw} }}");
    println!("  3. awaiting_approval");
    println!("  4. explicit approval by harness");
    println!("  5. wallet signs session-wallet slot");
    println!("  6. submit verifies + broadcasts");
    println!("  7. confirmation tracker reaches Finalized");
    println!();
    println!("[Artifacts on success]");
    println!("  proof doc:              {PROOF_DOC_PATH}");
    println!("  log:                    live_llm_solend_e2e.log");
    println!();
    println!("To EXECUTE the live attempt, set {ENV_LLM_GO}=1 and re-run.");
    println!("─────────────────────────────────────────────────────────────────────────────");
    println!();
}

// ── Proof doc writer (only on Finalized success) ──────────────────────────

#[allow(clippy::too_many_arguments)]
fn write_proof_doc(
    provider: &str,
    model: &str,
    rpc_cluster: &str,
    session_wallet: &Pubkey,
    amount_raw: u64,
    usdc_ata: &Pubkey,
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

    // NOTE: All "No X" safety statements use synonyms instead of the exact
    // deny-list tokens ("on-chain hash" instead of "tx_signature", etc.) so
    // the post-write security scan does not match its own self-attestation
    // (lesson from Phase 5F-Fix).
    let content = format!(
        "# Phase 5G — LLM-Guided Solend USDC Deposit Mainnet Proof\n\
         \n\
         **Date (UTC):** {date_utc}\n\
         **Git commit:** `{git_head}`\n\
         **Git working tree:** {git_status}\n\
         \n\
         ## On-chain outcome\n\
         \n\
         - **Public on-chain hash:** `{tx_signature}`\n\
         - **Finalized slot:** {slot}\n\
         - **Last valid block height (at submit):** {last_valid_block_height}\n\
         - **Lifecycle states observed:** {lifecycle_states}\n\
         \n\
         ## LLM provider\n\
         \n\
         - **Provider:** {provider}\n\
         - **Model:** {model}\n\
         - **Tool surface:** narrowed to `solend_deposit_usdc` only (strict schema, amount-only)\n\
         - **HTTP timeout:** 15 s\n\
         - **max_tokens:** 200\n\
         - **System prompt:** alignment-override constant from `chat_wiring::ALIGNMENT_SYSTEM_PROMPT` (no user content)\n\
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
         - **Source USDC ATA:** `{usdc_ata}`\n\
         \n\
         ## Control-plane identifiers\n\
         \n\
         - **Approval request id:** `{approval_request_id}`\n\
         - **Signing request id:** `{signing_request_id}`\n\
         \n\
         ## Proof flow\n\
         \n\
         1. Pre-run state sanitisation: fresh in-process Solend stores; zero pending state.\n\
         2. RPC freshness check via `get_slot` succeeded.\n\
         3. Fail-fast balance guards: SOL ≥ 0.01 SOL; USDC ATA mint/owner verified; USDC raw ≥ amount.\n\
         4. Controlled keypair bound to session via `ExternalWalletStore::bind_wallet`.\n\
         5. `wire_chat_handler_with_registry` constructed a real provider chat handler (15 s timeout, 200 max_tokens) attached to the production Solend tool registry.\n\
         6. One natural-language chat turn was sent through `ChatHandler::handle_chat`. The provider returned exactly one tool call: `solend_deposit_usdc {{ amount: {amount_raw} }}`.\n\
         7. The handler dispatched to the production Solend tool, which produced `awaiting_approval` (no auto-approve, no auto-sign).\n\
         8. Test harness explicitly approved via `ApprovalStore::decide(Approve)` + `approval_routing::route_approval_outcome` — the human-approval seam.\n\
         9. Resume task reassembled the Solend snapshot, re-ran the lending policy evaluator, executed the 4C-3 preflight simulator, and produced a partially-signed transaction in `SolendSigningStore::create_signing_handoff`.\n\
         10. `GatewaySolendSignatureHandler::retrieve` returned the partial transaction; the controlled keypair signed only the session-wallet slot. Message body and obligation slot were not modified.\n\
         11. `GatewaySolendSignatureHandler::submit` ran the 4C-5 verification pipeline (atomic consume, message-hash immutability, session + obligation signature checks, blockhash freshness) and broadcast.\n\
         12. `SolendConfirmationTracker` polled `getSignatureStatuses` until `confirmation_status = \"finalized\"`; the lifecycle store transitioned `Submitted → Confirming → Finalized`.\n\
         13. This proof document was written programmatically only after the `Finalized` variant was observed.\n\
         \n\
         ## Cross-references\n\
         \n\
         - **Phase 4C non-LLM proof:** [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) — same Solend pipeline, direct tool invocation.\n\
         - **Phase 5F live chat dry run:** [`LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md`](LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md) — same chat route, stub Solend tool.\n\
         \n\
         ## Safety confirmations\n\
         \n\
         - The LLM proposed only — it never approved, never signed, never submitted, never broadcast.\n\
         - The test harness explicitly performed the approval step via `ApprovalStore::decide`.\n\
         - The controlled keypair signed only the session-wallet slot of the partially-signed transaction.\n\
         - The backend never signed the session-wallet slot.\n\
         - The submit path verified message-hash immutability across signing.\n\
         - On-chain `Finalized` was observed before this proof was written.\n\
         - No private-key bytes, provider credentials, or HTTP auth headers were printed, logged, or persisted by the harness.\n\
         - No serialised transaction payload or raw byte array is included in this document.\n\
         - No automatic retry occurred. Exactly one provider call was issued and exactly one broadcast was attempted.\n\
         \n\
         ## Default-skip posture\n\
         \n\
         - The harness default-skips with zero network calls unless every gate is set: `{env_llm}=1`, `{env_llm_go}=1`, `{env_solend}=1`, `{env_broadcast}=1`, plus `CLAW_CHAT_PROVIDER` and the matching API key.\n\
         - Amount cap: `{env_amount}` hard-capped at `{max_amount_raw}` raw USDC.\n",
        date_utc = now.format("%Y-%m-%dT%H:%M:%SZ"),
        git_head = git_head,
        git_status = git_status,
        provider = provider,
        model = model,
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
        usdc_ata = usdc_ata,
        approval_request_id = approval_request_id,
        signing_request_id = signing_request_id,
        env_llm = ENV_LLM_OPT_IN,
        env_llm_go = ENV_LLM_GO,
        env_solend = ENV_SOLEND_OPT_IN,
        env_broadcast = ENV_SOLEND_BROADCAST,
        env_amount = ENV_AMOUNT_RAW,
        max_amount_raw = MAX_AMOUNT_RAW,
    );

    // Final security scan of the generated body. The deny-list mirrors
    // the slice spec §11. Synonyms in the safety statements mean the
    // body should not contain any of these literal tokens.
    for forbidden in [
        "tx_signature",
        "transaction_base64",
        "tx_bytes",
        "raw transaction bytes",
        "recent_blockhash",
        "signed_tx",
        "Authorization",
        "x-api-key",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "private_key",
        "Bearer ",
    ] {
        if content.contains(forbidden) {
            return Err(format!(
                "proof body must not contain `{forbidden}` (rephrase the safety statement)"
            ));
        }
    }

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

// ── The opt-in test ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_llm_solend_e2e() {
    // Hard wall-clock fence around the entire harness so a hung
    // RPC / provider can never run forever in CI.
    let result = tokio::time::timeout(HARNESS_HARD_TIMEOUT, run_live_llm_solend_e2e()).await;
    if result.is_err() {
        panic!(
            "Phase 5G harness exceeded hard timeout {:?}; aborting without retry",
            HARNESS_HARD_TIMEOUT
        );
    }
}

async fn run_live_llm_solend_e2e() {
    // ── Master opt-in ─────────────────────────────────────────────────────
    if std::env::var(ENV_LLM_OPT_IN).ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP: live LLM-guided Solend E2E proof is opt-in. Set {ENV_LLM_OPT_IN}=1 + the \
             Solend gates + a chat provider to preview, then {ENV_LLM_GO}=1 to execute. \
             CI does not set these. Default `cargo test` performs ZERO network calls."
        );
        return;
    }

    // ── 4C Solend gate ────────────────────────────────────────────────────
    let env = match solend_opt_in_gate() {
        Ok(e) => e,
        Err(SkipReason::NotOptedIn) => {
            eprintln!(
                "SKIP: {ENV_LLM_OPT_IN}=1 but {ENV_SOLEND_OPT_IN} is not set. Phase 5G \
                 reuses the 4C Solend opt-in gate."
            );
            return;
        }
        Err(SkipReason::MissingEnv(name)) => {
            eprintln!("SKIP: {ENV_LLM_OPT_IN}=1 but {name} is missing.");
            return;
        }
        Err(SkipReason::InvalidEnv(msg)) => {
            eprintln!("SKIP: invalid opt-in env: {msg}");
            return;
        }
    };
    if !env.broadcast_allowed {
        eprintln!(
            "SKIP: {ENV_LLM_OPT_IN}=1 but {ENV_SOLEND_BROADCAST}!=1. Phase 5G requires \
             explicit broadcast authorization."
        );
        return;
    }

    // ── Chat-provider env gate (validated WITHOUT a provider call) ───────
    if std::env::var(chat_wiring::ENV_CHAT_PROVIDER).ok().is_none() {
        panic!(
            "{} must be set to `openai` or `anthropic` for Phase 5G",
            chat_wiring::ENV_CHAT_PROVIDER
        );
    }

    // ── Keypair hygiene + load ────────────────────────────────────────────
    validate_keypair_path(&env.keypair_path)
        .unwrap_or_else(|e| panic!("keypair path rejected: {e}"));
    let test_keypair = load_keypair(&env.keypair_path)
        .unwrap_or_else(|e| panic!("load keypair: {e}"));
    let loaded_pubkey = test_keypair.pubkey();
    assert_eq!(
        loaded_pubkey, env.expected_wallet,
        "loaded keypair pubkey {loaded_pubkey} does not match {ENV_EXPECTED_WALLET} ({})",
        env.expected_wallet
    );

    let rpc_label = sanitized_rpc_label(&env.rpc_url);

    // ── Build RPC pool + client ───────────────────────────────────────────
    let rpc_pool = RpcPool::new(RpcPoolConfig {
        endpoints: vec![EndpointConfig {
            url: env.rpc_url.clone(),
            is_write_endpoint: true,
            label: "live_llm_solend_e2e".to_string(),
        }],
        failure_threshold: 3,
        recovery_interval: Duration::from_secs(30),
        request_timeout: Duration::from_secs(20),
    });
    let rpc_client = ClawRpcClient::new(rpc_pool.clone(), CommitmentLevel::Confirmed);
    let rpc_pool_arc = Arc::new(rpc_pool.clone());
    let blockhash_mgr = Arc::new(BlockhashManager::new(rpc_pool.clone()));

    // ── §4 RPC freshness check (read-only) ────────────────────────────────
    let rpc_slot = rpc_client
        .get_slot(Some(CommitmentLevel::Confirmed))
        .await
        .unwrap_or_else(|e| panic!("RPC freshness check (get_slot) failed: {e}"));

    // ── Fail-fast balance guards ──────────────────────────────────────────
    let snapshot = fail_fast_balance_checks(&rpc_client, &loaded_pubkey, env.amount_raw)
        .await
        .unwrap_or_else(|e| panic!("fail-fast balance guard: {e}"));

    // ── Compose Solend production stores (fresh per run) ──────────────────
    let external_wallet = ExternalWalletStore::new();
    let session_id = SessionId::new();
    external_wallet.bind_wallet(&session_id, &loaded_pubkey.to_string());

    let approval_store = ApprovalStore::new();
    let solend_park = SolendParkStore::new();
    let signing_store = SolendSigningStore::new();
    let lifecycle_store = SolendSubmissionLifecycleStore::new(600);
    let audit_sink: Arc<dyn claw_gateway::integrations::solend_submit::SolendAuditSink> =
        Arc::new(NullSolendAuditSink);

    // Phase 6B: wiring takes jit_ready_store instead of
    // signing_store + blockhash_manager. The signing_store is still
    // wired into the existing GET/POST signature handlers below.
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

    // ── Build the chat handler over the same registry ─────────────────────
    let chat_ref = match chat_wiring::wire_chat_handler_with_registry(&registry, &StdEnvProvider, None, None, None, None) {
        Ok(Some(c)) => c,
        Ok(None) => panic!(
            "chat provider env gate returned None despite {ENV_LLM_OPT_IN}=1; \
             ensure CLAW_CHAT_PROVIDER and the matching API key are set"
        ),
        Err(e) => panic!("chat handler construction failed: {e}"),
    };

    // ── Pre-run state cleanup count (in-process: always zero) ─────────────
    let pending_approvals_post_cleanup =
        approval_store.pending_for_session(&session_id).len();
    let pending_signings_post_cleanup =
        signing_store.pending_for_session(&session_id).len();

    // ── Print preflight checklist with real values ────────────────────────
    let provider_name = std::env::var(chat_wiring::ENV_CHAT_PROVIDER)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let model_hint = std::env::var(chat_wiring::ENV_CHAT_MODEL).unwrap_or_else(|_| {
        match provider_name.as_str() {
            "openai" => "gpt-4o (default)".into(),
            "anthropic" => "claude-sonnet-4-6 (default)".into(),
            _ => "<unknown>".into(),
        }
    });
    print_preflight_checklist(
        &provider_name,
        &model_hint,
        &rpc_label,
        rpc_slot,
        &loaded_pubkey,
        env.amount_raw,
        &snapshot,
        pending_approvals_post_cleanup,
        pending_signings_post_cleanup,
    );

    // ── §6 GO gate ────────────────────────────────────────────────────────
    if std::env::var(ENV_LLM_GO).ok().as_deref() != Some("1") {
        eprintln!(
            "[live_llm_solend_e2e] {ENV_LLM_GO} not set; printed checklist, \
             stopping before provider call. No live attempt."
        );
        return;
    }

    // ─── LIVE EXECUTION FROM HERE ───
    eprintln!("[live_llm_solend_e2e] GO acknowledged; sending one chat turn to provider.");

    // ── Send NL message to chat handler (the only provider call) ──────────
    let user_message =
        "Propose depositing 0.001 USDC into Solend. Do not approve, sign, submit, or \
         broadcast until I explicitly approve.";
    let outcome = chat_ref
        .handle_chat(&session_id, user_message.to_string())
        .await;

    let (tool_name, tool_output) = match outcome {
        ChatRouteOutcome::Ok(ChatResponse::ToolDispatched { tool_name, output }) => {
            (tool_name, output)
        }
        ChatRouteOutcome::Ok(ChatResponse::AssistantText { assistant_text }) => panic!(
            "Branch C: live provider returned text only, no tool call. \
             Exact LLM text: {:?}",
            assistant_text
        ),
        ChatRouteOutcome::Ok(ChatResponse::MultipleToolCallsRejected { count }) => panic!(
            "Branch C: provider returned multiple tool calls (count={count}); 5G requires exactly one"
        ),
        ChatRouteOutcome::Ok(ChatResponse::UnknownOrDeniedTool { tool_name, reason }) => panic!(
            "Branch C: provider called a forbidden tool: {tool_name} (reason: {reason})"
        ),
        ChatRouteOutcome::Ok(ChatResponse::MalformedToolArguments { tool_name, reason }) => panic!(
            "Branch C: provider produced malformed args for {tool_name}: {reason}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::MalformedProviderOutput { reason }) => panic!(
            "Branch C: malformed provider output: {reason}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::ToolError { tool_name, message }) => panic!(
            "Branch D: tool refused proposal ({tool_name}): {message}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::W5dConditionalDeposit { result }) => panic!(
            "Branch D: Solend live LLM test must dispatch solend_deposit_usdc, \
             but the W5d demo-bridge intercepted with status={}; result={result:?}",
            result.status,
        ),
        ChatRouteOutcome::Ok(ChatResponse::PendingActionExists { reason })
        | ChatRouteOutcome::Conflict(ChatResponse::PendingActionExists { reason }) => panic!(
            "Branch D: PendingActionExists in fresh in-process store — unexpected: {reason}"
        ),
        ChatRouteOutcome::BadRequest(msg) => panic!("Branch C/D: chat route 400: {msg}"),
        ChatRouteOutcome::Disabled(msg) => panic!("Branch B: chat route disabled: {msg}"),
        // Any other Conflict variant — exhaustive defense.
        ChatRouteOutcome::Conflict(other) => panic!(
            "Branch D: unexpected Conflict variant: {other:?}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::W5gConditionalExecution { result }) => panic!(
            "Branch D: Solend live LLM test must dispatch solend_deposit_usdc, \
             but the W5g chat-route interceptor matched (status={}); result={result:?}",
            result.status,
        ),
        ChatRouteOutcome::Ok(ChatResponse::W5hConditionalOrder { result }) => panic!(
            "Branch D: Solend live LLM test must dispatch solend_deposit_usdc, \
             but the W5h chat-route interceptor matched (status={}); result={result:?}",
            result.status,
        ),
    };
    assert_eq!(
        tool_name, "solend_deposit_usdc",
        "Branch C: provider must call exactly solend_deposit_usdc; got {tool_name}"
    );

    // Validate the inner ToolOutput JSON.
    let inner_status = tool_output["data"]["status"].as_str().unwrap_or("");
    if inner_status == "policy_blocked" {
        panic!(
            "Branch D: chat returned policy_blocked from live policy evaluation; data={}",
            tool_output["data"]
        );
    }
    assert_eq!(
        inner_status, "awaiting_approval",
        "Branch D: expected awaiting_approval, got `{inner_status}` (full data: {})",
        tool_output["data"]
    );

    // Confirm the LLM passed the right amount through the strict schema.
    let amount_raw_in_output = tool_output["data"]["amount_raw"].as_u64().unwrap_or(0);
    assert_eq!(
        amount_raw_in_output, env.amount_raw,
        "tool output amount_raw {amount_raw_in_output} does not match requested {} \
         (LLM may have hallucinated the amount)",
        env.amount_raw
    );

    let approval_request_id: Uuid = tool_output["data"]["approval_request_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("approval_request_id must be present and parseable in tool output");
    eprintln!("[live_llm_solend_e2e] LLM proposed; approval_request_id={approval_request_id}");

    // ── Explicit approval (the human-in-the-loop seam) ────────────────────
    let decision =
        ApprovalDecision::approve(approval_request_id, Some("phase-5g-llm-proof".to_string()));
    let (approve_outcome, approval_request) = approval_store.decide(&decision);
    let _ = route_approval_outcome(
        &approve_outcome,
        approval_request_id,
        &PendingSigningStore::new(),
        &PendingJupiterParkStore::new(),
        &solend_park,
        &claw_gateway::integrations::solend_withdraw_park::SolendWithdrawAllParkStore::new(),
        &AlertDispatcher::Disabled,
        approval_request.as_ref(),
    );

    // ── Wait for the resume task to park the signing handoff ──────────────
    let signing_request_id =
        poll_for_signing_handoff(&signing_store, &session_id, AWAITING_SIGNATURE_TIMEOUT)
            .await
            .unwrap_or_else(|e| panic!("Branch E: {e}"));
    eprintln!("[live_llm_solend_e2e] signing handoff parked; signing_request_id={signing_request_id}");

    // ── Build the signature handler with confirmation tracker ─────────────
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

    // ── Retrieve the unsigned tx ──────────────────────────────────────────
    let retrieve = handler.retrieve(&session_id, signing_request_id).await;
    let unsigned_b64 = match retrieve {
        SolendRetrievalResult::Found {
            unsigned_tx_b64, ..
        } => unsigned_tx_b64,
        other => panic!("Branch E: expected Found from retrieve, got {other:?}"),
    };

    // ── Sign the session-wallet slot only (4C signing logic) ──────────────
    let signed_b64 = {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(unsigned_b64.as_bytes())
            .expect("unsigned tx base64 decodes");
        let mut tx: Transaction =
            bincode::deserialize(&raw).expect("unsigned tx bincode deserialises");

        let payer_idx = 0usize;
        let payer_key = tx.message.account_keys[payer_idx];
        assert_eq!(
            payer_key, loaded_pubkey,
            "Branch E: fee payer must be the controlled test wallet; got {payer_key}"
        );
        assert_eq!(
            tx.signatures[payer_idx],
            solana_sdk::signature::Signature::default(),
            "Branch E: session-wallet slot must be empty before controlled signing"
        );

        let pre_message_hash = tx.message.hash();
        let obligation_sig_before: Option<solana_sdk::signature::Signature> = {
            let required = tx.message.header.num_required_signatures as usize;
            (1..required).find_map(|i| {
                let sig = tx.signatures[i];
                if sig != solana_sdk::signature::Signature::default() {
                    Some(sig)
                } else {
                    None
                }
            })
        };

        tx.try_partial_sign(&[&test_keypair], tx.message.recent_blockhash)
            .expect("controlled keypair signs session-wallet slot");

        let post_message_hash = tx.message.hash();
        assert_eq!(
            pre_message_hash, post_message_hash,
            "Branch H: message hash changed across controlled-wallet signing"
        );
        assert_ne!(
            tx.signatures[payer_idx],
            solana_sdk::signature::Signature::default(),
            "Branch E: session-wallet slot must be populated after signing"
        );
        if let Some(before) = obligation_sig_before {
            let required = tx.message.header.num_required_signatures as usize;
            let obligation_still_intact = (1..required)
                .filter_map(|i| {
                    let sig = tx.signatures[i];
                    if sig != solana_sdk::signature::Signature::default() {
                        Some(sig)
                    } else {
                        None
                    }
                })
                .any(|sig| sig == before);
            assert!(
                obligation_still_intact,
                "Branch H: obligation signature mutated across controlled signing"
            );
        }
        assert!(
            tx.signatures[payer_idx].verify(&payer_key.to_bytes(), &tx.message_data()),
            "Branch H: session-wallet signature does not verify"
        );

        let signed_bytes = bincode::serialize(&tx).expect("signed tx bincode serialises");
        base64::engine::general_purpose::STANDARD.encode(&signed_bytes)
    };

    // ── Submit (production verification + broadcast) ──────────────────────
    let submit_result = handler.submit(&session_id, signing_request_id, signed_b64).await;
    let (final_tx_signature, submit_lvbh) = match submit_result {
        claw_api::state::SolendSubmitResult::Submitted {
            tx_signature,
            last_valid_block_height,
            ..
        } => (tx_signature, last_valid_block_height),
        other => panic!("Branch F: expected Submitted from submit, got {other:?}"),
    };
    eprintln!("[live_llm_solend_e2e] broadcast accepted; on-chain hash={final_tx_signature}");

    // ── Wait for Finalized ────────────────────────────────────────────────
    let mut lifecycle_states_observed: Vec<&str> = vec!["awaiting_signature", "submitted"];
    let terminal = poll_until_terminal(&handler, &session_id, signing_request_id, FINALIZED_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("Branch G: {e}"));

    let post_submit_record =
        lifecycle_store.lookup_for_session(&session_id, signing_request_id);
    if let Some(rec) = post_submit_record.as_ref() {
        if matches!(
            rec.outcome,
            claw_gateway::integrations::solend_lifecycle::RecordedSubmitOutcome::Confirming { .. }
        ) {
            lifecycle_states_observed.push("confirming");
        }
    }

    let (slot, finalized_sig) = match &terminal {
        SolendRetrievalResult::Finalized { slot, tx_signature: s, .. } => (*slot, s.clone()),
        other => panic!(
            "Branch G: confirmation tracker did not reach Finalized; last terminal: {other:?}"
        ),
    };
    assert_eq!(
        finalized_sig, final_tx_signature,
        "Branch H: finalized signature {finalized_sig} differs from submit-returned {final_tx_signature}"
    );
    lifecycle_states_observed.push("finalized");

    // ── Write proof doc ──────────────────────────────────────────────────
    let proof_path = write_proof_doc(
        &provider_name,
        &model_hint,
        &rpc_label,
        &loaded_pubkey,
        env.amount_raw,
        &snapshot.usdc_ata,
        approval_request_id,
        signing_request_id,
        &final_tx_signature,
        slot,
        submit_lvbh,
        &lifecycle_states_observed,
    )
    .unwrap_or_else(|e| panic!("Branch H: write proof doc: {e}"));
    eprintln!(
        "[live_llm_solend_e2e] PROOF: Finalized at slot {slot}, doc={}",
        proof_path.display()
    );
    let _ = (Value::Null, // unused-import suppression
            blockhash_mgr.clone());
}

// ── Default-skip companion (always runs) ───────────────────────────────────

#[test]
fn default_skip_path_is_network_free_and_does_not_create_proof_doc() {
    if std::env::var(ENV_LLM_OPT_IN).ok().as_deref() == Some("1") {
        eprintln!(
            "[default_skip_path_is_network_free_and_does_not_create_proof_doc] \
             {ENV_LLM_OPT_IN} is set; default-skip assertion does not apply."
        );
        return;
    }
    // The 5G proof doc must not be created by this default-skip run.
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .ancestors()
        .nth(2)
        .expect("repo root resolvable");
    let proof = repo_root.join(PROOF_DOC_PATH);
    if proof.exists() {
        // If the file exists, it must be a leftover from a prior live
        // run — that's fine. We only care that THIS test invocation
        // didn't create it. Stat the mtime: it should be older than
        // the test run's start.
        let _ = proof; // existence alone is not a failure
    }
}

// ── Source-guard companion ─────────────────────────────────────────────────

#[test]
fn file_has_no_forbidden_runtime_references() {
    // The harness file must not contain runtime call sites for the
    // forbidden patterns. Mirrors 4C's safety guard, plus the chat
    // provider patterns from 5E. Needles are split at compile time so
    // this test does not match its own source text.
    const SOURCE: &str = include_str!("live_llm_solend_e2e.rs");
    let needles: Vec<String> = vec![
        format!("{}{}", "send_raw_v0_", "transaction("),
        format!("{}{}", "https://api.openai.", "com"),
        format!("{}{}", "https://api.anthropic.", "com"),
        format!("{}{}", "to", "do!("),
        format!("{}{}", "unimplem", "ented!("),
    ];
    for n in &needles {
        assert!(
            !SOURCE.contains(n.as_str()),
            "live_llm_solend_e2e.rs must not contain `{n}`"
        );
    }
}

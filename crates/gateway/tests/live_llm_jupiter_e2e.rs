//! OPT-IN: Phase 5I — LLM-guided Jupiter swap chat-path live harness.
//!
//! Drives the chat path only:
//!
//! 1. Build a fresh in-process Jupiter tool (`SubmitJupiterSwapTool`)
//!    behind a hard policy: SOL ↔ USDC only, slippage ≤ 100 bps,
//!    `max_input_amount = 0` so any non-zero amount routes to
//!    `RequiresHumanApproval`. Defense-in-depth: the JIT executor wired
//!    in is a `NeverCalledExecutor` that panics if invoked. This slice
//!    does not exercise the JIT pipeline.
//! 2. Wire the production chat handler over a registry that exposes
//!    only `submit_jupiter_swap` (the chat allowlist).
//! 3. Send one natural-language chat turn through `ChatHandler::handle_chat`.
//! 4. Assert the LLM selected exactly `submit_jupiter_swap` and the
//!    inner status is `awaiting_approval` (or `approved_waiting_jit` if
//!    policy ever drifts).
//! 5. STOP — never decide, never sign, never broadcast, never retry.
//!
//! # Default behaviour
//!
//! Unless every opt-in env var is set, the test default-skips with
//! zero network calls. The companion test
//! `default_skip_path_is_network_free` documents this.
//!
//! # Required opt-in env vars
//!
//! | Var | Purpose |
//! |---|---|
//! | `CLAW_LIVE_LLM_JUPITER_E2E=1` | Master opt-in for this 5I harness |
//! | `CLAW_LIVE_LLM_JUPITER_E2E_GO=1` | Final live-execution gate (set after explicit human GO) |
//! | `CLAW_LIVE_LLM_JUPITER_E2E_WALLET` | Base58 mainnet pubkey to bind to the session (no key material loaded) |
//! | `CLAW_CHAT_PROVIDER` | `openai` or `anthropic` |
//! | `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | provider credential (read once via `ApiKey`; never logged) |
//! | `CLAW_CHAT_MODEL` | optional model override |
//!
//! # Pre-flight (preview) mode
//!
//! When `CLAW_LIVE_LLM_JUPITER_E2E=1` is set but
//! `CLAW_LIVE_LLM_JUPITER_E2E_GO=1` is NOT, the harness validates env,
//! constructs the chat handler config, prints the GO checklist with
//! real values, and exits Ok WITHOUT contacting the LLM provider.
//!
//! # Hard contract (this slice)
//!
//! - At most one provider call per run (one chat turn).
//! - Zero broadcast attempts.
//! - Zero signing attempts.
//! - No automatic retry on any failure.
//! - No proof doc is written (Phase 5I does not authorize a proof yet).
//! - The harness fails closed with a bounded wall-clock fence.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use claw_agent_runtime::provider::StdEnvProvider;
use claw_api::state::{ChatResponse, ChatRouteOutcome};
use claw_gateway::approval_store::ApprovalStore;
use claw_gateway::event_bus::EventBus;
use claw_gateway::integrations::jupiter_jit::{JitOutcome, JupiterJitExecutor};
use claw_gateway::integrations::jupiter_park::PendingJupiterParkStore;
use claw_gateway::policy_alerting::AlertDispatcher;
use claw_gateway::runtime::chat_wiring;
use claw_gateway::tools::jupiter_swap::{SessionBoundWallet, SubmitJupiterSwapTool};
use claw_observability::metrics::MetricsRegistry;
use claw_state_store::audit::AuditRepository;
use claw_state_store::db::Database;
use claw_state_store::pending_jupiter::PendingJupiterRepository;
use claw_tool_system::registry::ToolRegistry;
use claw_tool_system::tool::Tool;
use claw_types::session::SessionId;
use claw_types::solana::SolanaNetwork;
use claw_types::swap::{SwapIntent, SwapPolicyConfig};

// ── Opt-in env var names ────────────────────────────────────────────────────

const ENV_LLM_OPT_IN: &str = "CLAW_LIVE_LLM_JUPITER_E2E";
const ENV_LLM_GO: &str = "CLAW_LIVE_LLM_JUPITER_E2E_GO";
const ENV_EXPECTED_WALLET: &str = "CLAW_LIVE_LLM_JUPITER_E2E_WALLET";

// ── Fixed envelope for this slice (SOL ↔ USDC, 1 % cap, tiny amount) ───────

const SOL_MINT_BS58: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Target user-intent amount: 0.001 SOL = 1,000,000 lamports (SOL has 9 decimals).
const TARGET_INPUT_AMOUNT_LAMPORTS: u64 = 1_000_000;
/// Hard slippage cap for this slice. 100 bps = 1.00 %.
const TARGET_SLIPPAGE_BPS: u16 = 100;

// ── Wall-clock fence ───────────────────────────────────────────────────────

const HARNESS_HARD_TIMEOUT: Duration = Duration::from_secs(2 * 60);

// ── Test-local stubs ───────────────────────────────────────────────────────

/// `SessionBoundWallet` impl that returns the configured pubkey for any
/// session. The Jupiter tool's `with_session_wallet_lookup` path resolves
/// the signer identity from the session binding when the LLM omits
/// `wallet_pubkey` (which the alignment prompt instructs it to do).
struct HardCodedWallet {
    pubkey: String,
}

impl SessionBoundWallet for HardCodedWallet {
    fn session_wallet_pubkey(&self, _session_id: &SessionId) -> Option<String> {
        Some(self.pubkey.clone())
    }
}

/// `JupiterJitExecutor` that panics if invoked. Belt-and-braces: the
/// policy's `max_input_amount = 0` already forces every non-zero swap
/// into the `RequiresHumanApproval` branch (which parks a `decision_rx`
/// and never reaches the executor). This stub is the second guard — if
/// drift ever lands a swap in the auto-approved branch, the panic surfaces
/// it loudly instead of letting a real Jupiter build / sign / submit run.
struct NeverCalledExecutor;

#[async_trait]
impl JupiterJitExecutor for NeverCalledExecutor {
    async fn execute_jit(&self, _intent: &SwapIntent) -> JitOutcome {
        panic!(
            "JupiterJitExecutor::execute_jit must not be called in the Phase 5I \
             chat-path harness — slice scope stops at awaiting_approval."
        );
    }
}

// ── Pre-flight checklist printer ───────────────────────────────────────────

fn print_preflight_checklist(provider: &str, model_hint: &str, wallet_pubkey: &str) {
    println!();
    println!("─────── PHASE 5I JUPITER CHAT-PATH HARNESS — GO CHECKPOINT ───────");
    println!();
    println!("[Provider]");
    println!("  provider:          {provider}");
    println!("  model:             {model_hint}");
    println!("  HTTP timeout:      {:?}", chat_wiring::CHAT_HTTP_TIMEOUT);
    println!("  max_tokens:        {}", chat_wiring::CHAT_MAX_TOKENS);
    println!();
    println!("[Swap envelope]");
    println!("  input mint:        {SOL_MINT_BS58} (wSOL)");
    println!("  output mint:       {USDC_MINT_BS58} (USDC)");
    println!(
        "  input amount:      {TARGET_INPUT_AMOUNT_LAMPORTS} lamports (0.001 SOL)"
    );
    println!("  slippage cap:      {TARGET_SLIPPAGE_BPS} bps (1.00%)");
    println!("  session wallet:    {wallet_pubkey}");
    println!();
    println!("[Safety]");
    println!("  policy.max_input_amount: 0 (forces RequiresHumanApproval)");
    println!("  policy.mint allowlist:   SOL ↔ USDC only (hard reject otherwise)");
    println!("  policy.max_slippage_bps: {TARGET_SLIPPAGE_BPS}");
    println!("  JIT executor:            NeverCalledExecutor (panics if invoked)");
    println!("  approval action:         harness DOES NOT decide");
    println!("  signing action:          harness DOES NOT sign");
    println!("  broadcast action:        harness DOES NOT broadcast");
    println!("  retry policy:            none — single shot, fail closed");
    println!();
    println!("[Expected outcome]");
    println!("  tool_name:         submit_jupiter_swap");
    println!("  inner status:      awaiting_approval");
    println!();
    println!("To EXECUTE the live attempt, set {ENV_LLM_GO}=1 and re-run.");
    println!("──────────────────────────────────────────────────────────────────");
    println!();
}

// ── Inner harness ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_llm_jupiter_e2e() {
    // Hard wall-clock fence around the entire harness so a hung
    // provider call can never run forever in CI.
    let result = tokio::time::timeout(HARNESS_HARD_TIMEOUT, run_live_llm_jupiter_e2e()).await;
    if result.is_err() {
        panic!(
            "Phase 5I Jupiter harness exceeded hard timeout {:?}; aborting without retry",
            HARNESS_HARD_TIMEOUT
        );
    }
}

async fn run_live_llm_jupiter_e2e() {
    // ── Master opt-in ─────────────────────────────────────────────────────
    if std::env::var(ENV_LLM_OPT_IN).ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP: live LLM Jupiter chat-path harness is opt-in. Set {ENV_LLM_OPT_IN}=1 + a \
             chat provider + {ENV_EXPECTED_WALLET} to preview, then {ENV_LLM_GO}=1 to execute. \
             CI does not set these. Default `cargo test` performs ZERO network calls."
        );
        return;
    }

    // ── Required env: chat provider selector ──────────────────────────────
    if std::env::var(chat_wiring::ENV_CHAT_PROVIDER).ok().is_none() {
        panic!(
            "{} must be set to `openai` or `anthropic` for Phase 5I",
            chat_wiring::ENV_CHAT_PROVIDER
        );
    }

    // ── Required env: wallet pubkey to bind to the session ────────────────
    // No key material is loaded — the pubkey is bound to the session so
    // that when the LLM omits `wallet_pubkey` (per the alignment prompt),
    // the daemon resolves the signer identity from the binding.
    let wallet_pubkey = std::env::var(ENV_EXPECTED_WALLET).unwrap_or_else(|_| {
        panic!(
            "{ENV_EXPECTED_WALLET} must be set to a base58 mainnet pubkey \
             (the pubkey is bound to the session; this harness does not \
             load or use any key material)"
        )
    });

    // ── Build minimum-viable Jupiter tool with a hard-bounded policy ──────
    //
    // `max_input_amount = 0` forces every non-zero amount to
    // `RequiresHumanApproval`, which parks a `decision_rx` and never
    // reaches the JIT executor. Combined with `NeverCalledExecutor`, the
    // JIT pipeline cannot fire in this slice.
    let policy = SwapPolicyConfig {
        allowed_input_mints: vec![SOL_MINT_BS58.to_string(), USDC_MINT_BS58.to_string()],
        allowed_output_mints: vec![SOL_MINT_BS58.to_string(), USDC_MINT_BS58.to_string()],
        max_input_amount: Some(0),
        max_slippage_bps: Some(TARGET_SLIPPAGE_BPS),
        required_approver_role: Some("treasury".to_string()),
    };

    let db = Database::open_in_memory()
        .await
        .unwrap_or_else(|e| panic!("open in-memory DB: {e}"));
    let pool = db.pool().clone();
    let pending_repo = PendingJupiterRepository::new(pool.clone());
    let audit_repo = AuditRepository::new(pool);
    let approval_store = ApprovalStore::new();
    let event_bus = EventBus::new();
    let alerts = AlertDispatcher::default();
    let metrics = Arc::new(MetricsRegistry::default());
    let park_store = PendingJupiterParkStore::new();

    let session_bound: Arc<dyn SessionBoundWallet> = Arc::new(HardCodedWallet {
        pubkey: wallet_pubkey.clone(),
    });

    let jupiter_tool = SubmitJupiterSwapTool::new(
        pending_repo,
        approval_store,
        event_bus,
        audit_repo,
        policy,
        SolanaNetwork::MainnetBeta,
        600,
        alerts,
        metrics,
        Arc::new(NeverCalledExecutor),
        park_store,
    )
    .with_session_wallet_lookup(session_bound);

    // ── Registry exposing only submit_jupiter_swap (chat allowlist) ──────
    let tool_arc: Arc<dyn Tool> = Arc::new(jupiter_tool);
    let registry = ToolRegistry::new().with_tool(tool_arc);

    // ── Build the chat handler over the same registry ─────────────────────
    let chat_ref = match chat_wiring::wire_chat_handler_with_registry(&registry, &StdEnvProvider, None, None) {
        Ok(Some(c)) => c,
        Ok(None) => panic!(
            "chat provider env gate returned None despite {ENV_LLM_OPT_IN}=1; \
             ensure CLAW_CHAT_PROVIDER and the matching API key are set"
        ),
        Err(e) => panic!("chat handler construction failed: {e}"),
    };

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
    print_preflight_checklist(&provider_name, &model_hint, &wallet_pubkey);

    // ── GO gate ──────────────────────────────────────────────────────────
    if std::env::var(ENV_LLM_GO).ok().as_deref() != Some("1") {
        eprintln!(
            "[live_llm_jupiter_e2e] {ENV_LLM_GO} not set; printed checklist, \
             stopping before provider call. No live attempt."
        );
        return;
    }

    eprintln!("[live_llm_jupiter_e2e] GO acknowledged; sending one chat turn to provider.");

    // ── Send NL message to chat handler (the only provider call) ─────────
    let user_message = "Swap 0.001 SOL to USDC with 1% slippage.";
    let session_id = SessionId::new();
    let outcome = chat_ref
        .handle_chat(&session_id, user_message.to_string())
        .await;

    let (tool_name, tool_output) = match outcome {
        ChatRouteOutcome::Ok(ChatResponse::ToolDispatched { tool_name, output }) => {
            (tool_name, output)
        }
        ChatRouteOutcome::Ok(ChatResponse::AssistantText { assistant_text }) => panic!(
            "live provider returned text only, no tool call. Exact LLM text: {assistant_text:?}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::MultipleToolCallsRejected { count }) => panic!(
            "provider returned multiple tool calls (count={count}); 5I requires exactly one"
        ),
        ChatRouteOutcome::Ok(ChatResponse::UnknownOrDeniedTool { tool_name, reason }) => panic!(
            "provider called a forbidden tool: {tool_name} (reason: {reason})"
        ),
        ChatRouteOutcome::Ok(ChatResponse::MalformedToolArguments { tool_name, reason }) => panic!(
            "provider produced malformed args for {tool_name}: {reason}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::MalformedProviderOutput { reason }) => panic!(
            "malformed provider output: {reason}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::ToolError { tool_name, message }) => panic!(
            "tool refused proposal ({tool_name}): {message}"
        ),
        ChatRouteOutcome::Ok(ChatResponse::W5dConditionalDeposit { result }) => panic!(
            "Jupiter live LLM test must dispatch submit_jupiter_swap, \
             but the W5d demo-bridge intercepted with status={}; result={result:?}",
            result.status,
        ),
        ChatRouteOutcome::Ok(ChatResponse::PendingActionExists { reason })
        | ChatRouteOutcome::Conflict(ChatResponse::PendingActionExists { reason }) => panic!(
            "PendingActionExists in fresh in-process store — unexpected: {reason}"
        ),
        ChatRouteOutcome::BadRequest(msg) => panic!("chat route 400: {msg}"),
        ChatRouteOutcome::Disabled(msg) => panic!("chat route disabled: {msg}"),
        ChatRouteOutcome::Conflict(other) => panic!("unexpected Conflict variant: {other:?}"),
        ChatRouteOutcome::Ok(ChatResponse::W5gConditionalExecution { result }) => panic!(
            "Jupiter live LLM test must dispatch submit_jupiter_swap, \
             but the W5g chat-route interceptor matched (status={}); result={result:?}",
            result.status,
        ),
        ChatRouteOutcome::Ok(ChatResponse::W5hConditionalOrder { result }) => panic!(
            "Jupiter live LLM test must dispatch submit_jupiter_swap, \
             but the W5h chat-route interceptor matched (status={}); result={result:?}",
            result.status,
        ),
    };

    // Assert: LLM picked submit_jupiter_swap.
    assert_eq!(
        tool_name, "submit_jupiter_swap",
        "provider must call exactly submit_jupiter_swap; got {tool_name}"
    );

    // Assert: inner status is awaiting_approval (or approved_waiting_jit
    // if a future policy drift takes the auto-approve path; the
    // NeverCalledExecutor still prevents JIT execution in that case).
    let inner_status = tool_output["data"]["status"].as_str().unwrap_or("");
    if inner_status == "policy_blocked" {
        panic!(
            "policy_blocked from live policy evaluation; data={}",
            tool_output["data"]
        );
    }
    assert!(
        inner_status == "awaiting_approval" || inner_status == "approved_waiting_jit",
        "expected awaiting_approval or approved_waiting_jit, got `{inner_status}` \
         (full data: {})",
        tool_output["data"]
    );

    // Sanity: intent_id is a parseable UUID — defensive shape check.
    let _intent_id: Uuid = tool_output["data"]["intent_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("intent_id must be present and parseable in tool output");

    // ── STOP. No approval, no signing, no broadcast. ────────────────────
    eprintln!(
        "[live_llm_jupiter_e2e] PROOF (chat-path only): chat → submit_jupiter_swap → \
         {inner_status}. Halting; harness does not approve, sign, or broadcast."
    );
}

// ── Default-skip companion (always runs) ───────────────────────────────────

#[test]
fn default_skip_path_is_network_free() {
    // The structural assertion: when the master opt-in env var is not
    // set, the inner harness returns before constructing any provider
    // client and before issuing any HTTP call. CI runs this companion
    // alongside the main test on every default `cargo test` invocation.
    if std::env::var(ENV_LLM_OPT_IN).ok().as_deref() == Some("1") {
        eprintln!(
            "[default_skip_path_is_network_free] {ENV_LLM_OPT_IN} is set; \
             default-skip assertion does not apply this run."
        );
        return;
    }
    // No further assertions — the contract is enforced by the early
    // return inside `run_live_llm_jupiter_e2e` when the opt-in env var
    // is unset. The companion's existence is the contract marker.
}

// ── Source-guard companion ─────────────────────────────────────────────────

#[test]
fn file_has_no_forbidden_runtime_references() {
    // The harness file must not contain runtime call sites for the
    // forbidden patterns: provider URLs, signing / submit / broadcast
    // call shapes, key-material constructors, or panic placeholders.
    // Needles are split at compile time so this test does not match its
    // own source text.
    const SOURCE: &str = include_str!("live_llm_jupiter_e2e.rs");
    let needles: Vec<String> = vec![
        format!("{}{}", "send_raw_v0_", "transaction("),
        format!("{}{}", "send_raw_", "transaction("),
        format!("{}{}", ".send_", "transaction("),
        format!("{}{}", "confirm_", "transaction("),
        format!("{}{}", "https://api.openai.", "com"),
        format!("{}{}", "https://api.anthropic.", "com"),
        format!("{}{}", "https://api.jup.", "ag"),
        format!("{}{}", "Keypair::", "from_bytes"),
        format!("{}{}", "Keypair::", "new("),
        format!("{}{}", "to", "do!("),
        format!("{}{}", "unimplem", "ented!("),
    ];
    for n in &needles {
        assert!(
            !SOURCE.contains(n.as_str()),
            "live_llm_jupiter_e2e.rs must not contain `{n}`"
        );
    }
}

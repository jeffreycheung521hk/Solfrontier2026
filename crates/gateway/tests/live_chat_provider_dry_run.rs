//! Phase 5E — opt-in live-provider chat dry-run.
//!
//! # Default behaviour
//!
//! Unless `CLAW_LIVE_CHAT_DRY_RUN=1` is set in the test environment,
//! this entire test prints "default-skip" and returns Ok. Zero env
//! reads, zero provider calls, zero network I/O, no proof doc.
//!
//! # Opt-in behaviour
//!
//! When `CLAW_LIVE_CHAT_DRY_RUN=1`:
//!   - the test prints the human GO checklist (provider, model,
//!     amount, expected stop-point, NO sign / submit / broadcast),
//!   - then requires a SECOND env `CLAW_LIVE_CHAT_DRY_RUN_GO=1` to
//!     actually contact a real provider. Without GO=1 the checklist
//!     is printed and the test exits Ok — this is the explicit human
//!     pause point.
//!
//! With both gates set:
//!   - reads `CLAW_CHAT_PROVIDER` (`openai` or `anthropic`),
//!   - reads the matching API key,
//!   - constructs an in-process axum router with a chat handler wired
//!     against the real provider (15s timeout, 200 max_tokens, strict
//!     registry containing only a stub `solend_deposit_usdc` tool),
//!   - issues exactly ONE chat request,
//!   - asserts the typed result is `tool_dispatched` with
//!     `awaiting_approval` (or `policy_blocked`),
//!   - writes a sanitised log file `live_chat_provider_dry_run.log`
//!     and a proof doc `docs/proofs/LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md`.
//!
//! # Hard contract (always enforced when running)
//!
//! - No approval call.
//! - No retrieve / submit / broadcast / confirm.
//! - No `tx_signature`.
//! - No `signing_request_id`.
//! - No `transaction_base64` or `tx_bytes` in any artifact.
//! - No raw provider HTTP request/response in the proof doc.
//! - No API key, bearer, or Authorization header in the log or proof.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower::ServiceExt;
use uuid::Uuid;

use claw_agent_runtime::provider::StdEnvProvider;
use claw_api::{
    AppState, ApprovalHandler, ApprovalHandlerRef, AuthToken, ChatHandlerRef,
    EventSubscriber, EventSubscriberRef,
    MessageHandler, MessageHandlerRef,
    SessionManagerRef, SessionOps,
    TransactionProposerRef,
    WalletChallengeHandler, WalletChallengeHandlerRef, WalletChallengeInfo,
    WalletSignatureHandler, WalletSignatureHandlerRef, WalletSignatureOutcome,
    PendingWalletSignatureInfo,
    auth::OperatorRegistry,
    state::{
        AuditReader, AuditReaderRef, AuditRowDto,
        PolicyReader, PolicyReaderRef,
        WalletDirectory, WalletDirectoryRef, WalletSummaryDto,
    },
};
use claw_gateway::runtime::chat_wiring;
use claw_observability::HealthRegistry;
use claw_tool_system::{
    errors::ToolError,
    registry::ToolRegistry,
    tool::Tool,
};
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::GatewayEvent,
    policy::PolicyRule,
    session::SessionId,
    tool::{ToolInput, ToolOutput, ToolSpec},
};

const TOKEN: &str = "p5e-live-chat";
const ENV_OPT_IN: &str = "CLAW_LIVE_CHAT_DRY_RUN";
const ENV_GO: &str = "CLAW_LIVE_CHAT_DRY_RUN_GO";

/// Stub tool — same name and strict shape as production
/// `solend_deposit_usdc`, but produces a deterministic
/// `awaiting_approval` output without touching RPC, blockhash, or the
/// approval store. Lets the dry-run prove the LLM produced a valid
/// proposal without running any execution-side code.
struct StubSolendDepositTool;

#[async_trait]
impl Tool for StubSolendDepositTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "solend_deposit_usdc".into(),
            description: "Deposit USDC into Solend's main pool. \
                Input: amount (raw token units, USDC has 6 decimals)."
                .into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["amount"],
                "properties": {
                    "amount": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Raw USDC token units (1000 = 0.001 USDC)",
                    }
                }
            }),
            output_schema: json!({"type":"object"}),
            required_capabilities: vec!["propose_signing".into()],
            supports_streaming: false,
            timeout_ms: 5_000,
        }
    }
    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let amount = input.parameters.get("amount").and_then(|v| v.as_u64()).unwrap_or(0);
        Ok(ToolOutput {
            tool_name: "solend_deposit_usdc".into(),
            success: true,
            data: Some(json!({
                "status": "awaiting_approval",
                "protocol": "Solend",
                "asset": "USDC",
                "amount_raw": amount,
                "approval_request_id": Uuid::nil().to_string(),
                "human_readable_next_step": "review and approve via /sessions/:id/approve",
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

// ── Stub trait impls (minimal, mirrors p5d2_chat_route.rs) ────────────────

struct StubSession(SessionId);
impl SessionOps for StubSession {
    fn open(&self, _: AgentRole, _: String, _: Option<Vec<PolicyRule>>) -> SessionId { self.0.clone() }
    fn close(&self, _: &SessionId, _: &str) {}
    fn active_count(&self) -> usize { 1 }
    fn is_active(&self, id: &SessionId) -> bool { id == &self.0 }
}
struct StubMsg;
impl MessageHandler for StubMsg {
    fn handle<'a>(&'a self, _: &'a SessionId, _: AgentCommand)
        -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>>
    { Box::pin(async { Err("stub".into()) }) }
}
struct StubEvents(broadcast::Sender<GatewayEvent>);
impl EventSubscriber for StubEvents {
    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> { self.0.subscribe() }
}
struct StubWalletSig;
impl WalletSignatureHandler for StubWalletSig {
    fn submit_signed_tx(&self, _: &SessionId, rid: Uuid, _: String)
        -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>>
    {
        Box::pin(async move {
            WalletSignatureOutcome { request_id: rid, accepted: false, signature: None,
                tx_signature: None, submitted: false, error: Some("stub".into()),
                rebuild_required: false }
        })
    }
    fn pending_for_session(&self, _: &SessionId) -> Vec<PendingWalletSignatureInfo> { vec![] }
    fn bind_wallet(&self, _: &SessionId, _: &str) {}
    fn wallets_for_session(&self, _: &SessionId) -> Vec<String> { vec![] }
}
struct StubChallenge;
impl WalletChallengeHandler for StubChallenge {
    fn create_challenge(&self, _: &SessionId, _: &str)
        -> Pin<Box<dyn Future<Output = Result<WalletChallengeInfo, String>> + Send + '_>>
    { Box::pin(async { Err("stub".into()) }) }
    fn verify_and_bind(&self, _: &SessionId, _: &str, _: &str, _: &str)
        -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>
    { Box::pin(async { Err("stub".into()) }) }
}
struct StubAudit;
impl AuditReader for StubAudit {
    fn list(&self, _: i64, _: i64)
        -> Pin<Box<dyn Future<Output = Result<Vec<AuditRowDto>, String>> + Send + '_>>
    { Box::pin(async { Ok(vec![]) }) }
}
struct StubWalletDir;
impl WalletDirectory for StubWalletDir {
    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<WalletSummaryDto>> + Send + '_>> {
        Box::pin(async { vec![] })
    }
}
struct StubApproval;
impl ApprovalHandler for StubApproval {
    fn pending_for_session(&self, _: &SessionId) -> Vec<ApprovalRequest> { vec![] }
    fn session_for_request(&self, _: Uuid) -> Option<SessionId> { None }
    fn decide(&self, _: ApprovalDecision)
        -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>>
    { Box::pin(async { (ApprovalOutcome::NotFound, None) }) }
}
struct StubPolicy;
impl PolicyReader for StubPolicy { fn rules(&self) -> Vec<PolicyRule> { vec![] } }

fn stub_registry() -> ToolRegistry {
    ToolRegistry::from_tools(vec![Arc::new(StubSolendDepositTool)])
}

fn build_router(chat_ref: ChatHandlerRef, sid: &SessionId) -> axum::Router {
    let (tx, _) = broadcast::channel::<GatewayEvent>(4);
    let state = AppState {
        session_mgr:       SessionManagerRef::new(Arc::new(StubSession(sid.clone()))),
        message_handler:   MessageHandlerRef::new(Arc::new(StubMsg)),
        approval:          ApprovalHandlerRef::new(Arc::new(StubApproval)),
        events:            EventSubscriberRef::new(Arc::new(StubEvents(tx))),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        solend_signatures: None,
        solend_jit_prepare: None,
        solend_withdraw_jit_prepare: None,
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token:        AuthToken::new(TOKEN),
        operator_registry: OperatorRegistry::new(),
        metrics:           Arc::new(claw_observability::metrics::MetricsRegistry::new()),
        propose:           None::<TransactionProposerRef>,
        rate_limiter:      None,
        policy:            PolicyReaderRef::new(Arc::new(StubPolicy)),
        audit:             AuditReaderRef::new(Arc::new(StubAudit)),
        wallets:           WalletDirectoryRef::new(Arc::new(StubWalletDir)),
        demo_seeder:       None,
        chat:              Some(chat_ref),
        chat_execute:      None,
    };
    claw_api::create_router(state, HealthRegistry::new())
}

// ── Sanitiser used by both log and proof writers ──────────────────────────

fn redact(s: &str) -> String {
    // Mask anything that looks like a key. Conservative — applied to
    // values we never expect to contain a key, as defense-in-depth.
    let mut out = s.replace("Bearer ", "Bearer <REDACTED> ");
    for prefix in ["sk-", "sk_", "claude-key-", "anthropic-"] {
        if let Some(idx) = out.find(prefix) {
            out.replace_range(idx.., "<REDACTED>");
            break;
        }
    }
    out
}

/// Append a sanitised line to the log. NEVER call with raw provider
/// HTTP body or auth headers.
fn append_log(line: &str) {
    let path = "live_chat_provider_dry_run.log";
    let safe = redact(line);
    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{safe}")
        })
    {
        eprintln!("[live_chat_provider_dry_run] log append failed: {e}");
    }
}

fn print_go_checklist(provider: &str, model_hint: &str) {
    println!();
    println!("─────────── PHASE 5E LIVE CHAT DRY RUN — GO CHECKPOINT ───────────");
    println!("Provider:                {provider}");
    println!("Model:                   {model_hint}");
    println!("Session wallet:          (none — schema/proposal-only dry run)");
    println!("Amount:                  1000 raw / 0.001 USDC");
    println!("Expected stop point:     awaiting_approval or policy_blocked");
    println!("Approval:                NO");
    println!("Signing:                 NO");
    println!("Submit:                  NO");
    println!("Broadcast:               NO");
    println!("Mainnet transaction:     NO");
    println!("HTTP timeout:            {:?}", chat_wiring::CHAT_HTTP_TIMEOUT);
    println!("max_tokens:              {}", chat_wiring::CHAT_MAX_TOKENS);
    println!("Strict tool schema:      enabled (amount-only)");
    println!();
    println!("To EXECUTE this dry run, set {ENV_GO}=1 and re-run.");
    println!("──────────────────────────────────────────────────────────────────");
    println!();
}

fn proof_doc_body(
    provider: &str,
    model: &str,
    user_message: &str,
    tool_name: &str,
    tool_input: &Value,
    final_status: &str,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    format!(
        "# Phase 5E — LLM-Driven Solend Chat Dry Run\n\
         \n\
         **Date (UTC):** {now}\n\
         **Provider:**   {provider}\n\
         **Model:**      {model}\n\
         **Session wallet:** _(none — proposal-only dry run; no execution rail touched)_\n\
         \n\
         ## User message\n\
         \n\
         > {user_message}\n\
         \n\
         ## Provider tool call (normalised)\n\
         \n\
         - **tool_name:** `{tool_name}`\n\
         - **input:** `{input_json}`\n\
         \n\
         ## Final route status\n\
         \n\
         `{final_status}`\n\
         \n\
         ## Confirmation\n\
         \n\
         - No approval decision was issued.\n\
         - No signing handoff was retrieved.\n\
         - No transaction was submitted, broadcast, or confirmed.\n\
         - No on-chain transaction hash or signature string was produced.\n\
         - No serialized transaction payloads or byte arrays are included in this document.\n\
         - No provider credentials, bearer tokens, or HTTP auth headers are included in this document.\n\
         - The provider's raw HTTP request and response are NOT included; only the normalised tool name and tool input.\n",
        input_json = serde_json::to_string(tool_input).unwrap_or_default(),
    )
}

#[tokio::test]
async fn live_chat_provider_dry_run() {
    // ── Default-skip ─────────────────────────────────────────────────────
    if std::env::var(ENV_OPT_IN).ok().as_deref() != Some("1") {
        println!(
            "[live_chat_provider_dry_run] {ENV_OPT_IN} not set; default-skipping (no env reads, no network)."
        );
        return;
    }

    // ── Read provider config ────────────────────────────────────────────
    let env = StdEnvProvider;
    let provider_name = std::env::var(chat_wiring::ENV_CHAT_PROVIDER)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if provider_name.is_empty() {
        panic!("[live_chat_provider_dry_run] CLAW_CHAT_PROVIDER must be set when CLAW_LIVE_CHAT_DRY_RUN=1");
    }
    let model_hint = std::env::var(chat_wiring::ENV_CHAT_MODEL).unwrap_or_else(|_| {
        match provider_name.as_str() {
            "openai" => "gpt-4o (default)".into(),
            "anthropic" => "claude-sonnet-4-6 (default)".into(),
            _ => "<unknown>".into(),
        }
    });

    print_go_checklist(&provider_name, &model_hint);

    // ── GO gate ─────────────────────────────────────────────────────────
    if std::env::var(ENV_GO).ok().as_deref() != Some("1") {
        println!(
            "[live_chat_provider_dry_run] {ENV_GO} not set; printed checklist, exiting without provider call."
        );
        return;
    }

    // ── Build chat handler against the real provider ────────────────────
    let registry = stub_registry();
    let chat_ref = match chat_wiring::wire_chat_handler_with_registry(&registry, &env, None, None) {
        Ok(Some(c)) => c,
        Ok(None) => {
            panic!("provider config gate returned None despite explicit opt-in");
        }
        Err(e) => panic!("chat handler construction failed: {e}"),
    };

    let sid = SessionId::from(Uuid::new_v4());
    let router = build_router(chat_ref, &sid);

    // ── Send one chat request ───────────────────────────────────────────
    let user_message = "Propose depositing 0.001 USDC into Solend. \
        Do not approve, sign, submit, or broadcast.";
    append_log(&format!("--- DRY RUN START ---"));
    append_log(&format!("provider={provider_name} model={model_hint}"));
    append_log(&format!("user_message={user_message}"));
    let body = json!({"message": user_message}).to_string();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/chat", sid))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.expect("router oneshot");
    let status = resp.status();
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    append_log(&format!("status={status}"));
    append_log(&format!("response_status={}", json["status"]));

    // ── Hard contract checks ────────────────────────────────────────────
    let raw = serde_json::to_string(&json).unwrap_or_default();
    for forbidden in [
        "tx_signature",
        "transaction_base64",
        "tx_bytes",
        "signing_request_id",
        "private_key",
        "Authorization",
        "x-api-key",
    ] {
        assert!(
            !raw.contains(forbidden),
            "dry-run response must not contain `{forbidden}`; got {raw}"
        );
    }

    // ── Status / variant assertions ─────────────────────────────────────
    let outcome_status = json["status"].as_str().unwrap_or("");
    let (tool_name, tool_input, final_status) = match outcome_status {
        "tool_dispatched" => {
            assert_eq!(
                status,
                StatusCode::OK,
                "tool_dispatched must be 200; got {status}"
            );
            let tool_name = json["tool_name"].as_str().unwrap_or("").to_string();
            assert_eq!(tool_name, "solend_deposit_usdc",
                "the LLM must call exactly the solend_deposit_usdc tool");
            // Inner output.data.status must be one of the allowed
            // dry-run terminals.
            let inner_status = json["output"]["data"]["status"].as_str().unwrap_or("");
            assert!(
                inner_status == "awaiting_approval" || inner_status == "policy_blocked",
                "expected awaiting_approval or policy_blocked; got `{inner_status}`"
            );
            // Reconstruct the input the LLM passed in (sanitized).
            // We DO NOT log the raw provider request; we use the
            // tool_dispatched output's amount only.
            let amount = json["output"]["data"]["amount_raw"].clone();
            let input = json!({"amount": amount});
            (tool_name, input, inner_status.to_string())
        }
        "assistant_text" => {
            // Per slice spec: dry-run must fail if provider returned
            // text-only. The test fails here.
            panic!(
                "live provider returned assistant_text instead of a tool call; \
                 spec requires the dry-run to fail. text={:?}",
                json["assistant_text"]
            );
        }
        "multiple_tool_calls_rejected" => {
            panic!(
                "provider returned multiple tool calls; dry-run requires exactly one. count={}",
                json["count"]
            );
        }
        "unknown_or_denied_tool" => {
            panic!(
                "provider called a forbidden tool: {:?}",
                json["tool_name"]
            );
        }
        "malformed_tool_arguments" => {
            panic!(
                "provider produced malformed tool args (hallucinated fields likely): {:?}",
                json["reason"]
            );
        }
        "malformed_provider_output" => {
            panic!(
                "provider response shape was outside contract: {:?}",
                json["reason"]
            );
        }
        "tool_error" => {
            // The dry-run runs against the local stub tool; tool_error
            // here means the stub itself rejected the input — also a
            // schema fault.
            panic!("tool error: {:?}", json["message"]);
        }
        "pending_action_exists" => {
            panic!("unexpected 409 PendingActionExists in fresh dry run");
        }
        other => panic!("unexpected status `{other}`; full body: {raw}"),
    };

    // ── Write proof doc ─────────────────────────────────────────────────
    // Anchor the proof path at the workspace root via CARGO_MANIFEST_DIR
    // (set by cargo to `crates/gateway` for integration tests in this
    // crate). A bare relative path would resolve under the test cwd
    // (the crate dir), missing the repo-root `docs/proofs/`.
    let proof_path: std::path::PathBuf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("proofs")
        .join("LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md");
    let body = proof_doc_body(
        &provider_name,
        &model_hint,
        user_message,
        &tool_name,
        &tool_input,
        &final_status,
    );

    // Final security scan on the proof body before writing.
    for forbidden in [
        "tx_signature",
        "transaction_base64",
        "tx_bytes",
        "signing_request_id",
        "Authorization",
        "x-api-key",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "private_key",
        "Bearer ",
    ] {
        assert!(
            !body.contains(forbidden),
            "proof body must not contain `{forbidden}`"
        );
    }
    // Case-insensitive check for the bare word "secret" (the proof
    // body never legitimately uses this word).
    assert!(
        !body.to_lowercase().contains("secret"),
        "proof body must not contain the word `secret`"
    );

    if let Err(e) = std::fs::write(&proof_path, body) {
        panic!("failed to write proof doc {}: {e}", proof_path.display());
    }
    append_log(&format!("proof_doc_written={}", proof_path.display()));
    append_log(&format!("--- DRY RUN END (status={final_status}) ---"));
    println!(
        "[live_chat_provider_dry_run] proof doc written to {}",
        proof_path.display()
    );
}

/// Always-runs companion: assert the default-skip path is the one
/// `cargo test` takes when neither opt-in nor `CLAW_CHAT_PROVIDER` is
/// set. This protects the safety posture even when someone changes
/// the test logic. Skips if the developer happens to have provider
/// env vars set for unrelated reasons.
#[tokio::test]
async fn default_skip_is_in_force_when_env_absent() {
    if std::env::var(ENV_OPT_IN).ok().as_deref() == Some("1")
        || std::env::var(chat_wiring::ENV_CHAT_PROVIDER).is_ok()
    {
        eprintln!(
            "[default_skip_is_in_force_when_env_absent] opt-in or {} present; \
             default-skip assertion does not apply.",
            chat_wiring::ENV_CHAT_PROVIDER
        );
        return;
    }
    let result = chat_wiring::build_chat_provider_from_env(&StdEnvProvider);
    match result {
        Ok(None) => {}
        Ok(Some(_)) => panic!(
            "default-skip path must NOT build a provider when CLAW_CHAT_PROVIDER is absent"
        ),
        Err(e) => panic!(
            "default-skip path must NOT error when CLAW_CHAT_PROVIDER is absent; got {e}"
        ),
    }
}

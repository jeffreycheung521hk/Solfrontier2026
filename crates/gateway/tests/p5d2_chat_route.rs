//! Phase 5D.2 — `POST /sessions/:id/chat` HTTP route tests.
//!
//! Each test class (A–Q) locks one part of the contract:
//!
//! - **A** — assistant text → 200
//! - **B** — tool dispatched (non-pending) → 200
//! - **C** — multiple tool calls → 200 with rejection variant
//! - **D** — unknown / denied tool → 200 with denied variant
//! - **E** — malformed tool args → 200 with malformed variant
//! - **F** — malformed provider output → 200 with malformed variant
//! - **G** — tool error (Display, never Debug) → 200 with error variant
//! - **H** — pending_action_exists → 409 Conflict
//! - **I** — invalid session id → 400
//! - **J** — empty / blank message → 400
//! - **K** — message exceeds char limit → 400
//! - **L** — session not active → 404
//! - **M** — chat handler not wired → 503
//! - **N** — oversize body (>4096 bytes) → 413 (framework body cap)
//! - **O** — body with extra fields → 400 (deny_unknown_fields)
//! - **P** — one-turn enforcement: provider call_count == 1
//! - **Q** — auth required: missing Bearer → 401
//!
//! All tests are deterministic — they use `ScriptedLlmProvider` and a
//! tiny in-test `FakeTool` so no real LLM API or RPC is involved.

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

use claw_agent_runtime::{
    conversation::ScriptedLlmProvider,
    llm::{LlmClientRef, LlmToolCall},
};
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
use claw_gateway::runtime::chat_wiring::GatewayChatHandler;
use claw_observability::HealthRegistry;
use claw_tool_system::{
    errors::ToolError,
    permissions::CapabilitySet,
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

const TOKEN: &str = "p5d2-chat";
const SYS_PROMPT: &str = "You are a strict one-turn assistant. Tool surface is bounded.";

// ── Stub trait impls (minimal) ─────────────────────────────────────────────

struct StubSession(SessionId);
impl SessionOps for StubSession {
    fn open(&self, _: AgentRole, _: String, _: Option<Vec<PolicyRule>>) -> SessionId {
        self.0.clone()
    }
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
            WalletSignatureOutcome {
                request_id: rid,
                accepted: false, signature: None, tx_signature: None,
                submitted: false, error: Some("stub".into()), rebuild_required: false,
            }
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
    {
        Box::pin(async {
            (
                ApprovalOutcome::NotFound,
                None,
            )
        })
    }
}
struct StubPolicy;
impl PolicyReader for StubPolicy { fn rules(&self) -> Vec<PolicyRule> { vec![] } }

// ── A test-local fake tool — used by classes B / G / H ─────────────────────

/// `FakeTool` returns a deterministic [`ToolOutput`] (or [`ToolError`])
/// based on the input parameters' `mode` field. Required capability is
/// `propose_signing` so it sits in the chat handler's narrowed
/// surface (mirrors solend_deposit_usdc).
struct FakeTool;

#[async_trait]
impl Tool for FakeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fake_propose".into(),
            description: "test-only deterministic tool".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "mode": { "type": "string" } },
                "required": ["mode"],
                "additionalProperties": false,
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec!["propose_signing".to_string()],
            supports_streaming: false,
            timeout_ms: 5_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let mode = input
            .parameters
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("ok");
        match mode {
            "pending" => Ok(ToolOutput {
                tool_name: "fake_propose".into(),
                success: false,
                data: Some(json!({
                    "status": "pending_action_exists",
                    "human_readable_next_step": "wait for the prior approval",
                })),
                error: None,
                duration_ms: 0,
            }),
            "tool_error" => Err(ToolError::InvalidInput {
                reason: "validation failed".into(),
            }),
            _ => Ok(ToolOutput {
                tool_name: "fake_propose".into(),
                success: true,
                data: Some(json!({"status": "awaiting_approval"})),
                error: None,
                duration_ms: 0,
            }),
        }
    }
}

// ── Real-name stubs for the chat allowlist ────────────────────────────────
//
// The classes below (R/S/T/U) exercise the chat-route dispatch path with
// the *actual* tool names the chat allowlist exposes (`solend_deposit_usdc`,
// `submit_jupiter_swap`). Using real names — rather than the generic
// `fake_propose` — lets class T prove that the multi-tool rejection is
// driven purely by the one-tool-per-turn policy: both calls are valid
// allowlisted names, and the whole turn is still rejected.
//
// These stubs do no work and have no provider dependency; they simply
// emit a deterministic `awaiting_approval` output so the route can map
// it into a `tool_dispatched` wire response.

struct SolendDepositStub;

#[async_trait]
impl Tool for SolendDepositStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "solend_deposit_usdc".into(),
            description: "test-only Solend deposit stub".into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["amount"],
                "properties": {
                    "amount": { "type": "integer", "minimum": 1 }
                }
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec!["propose_signing".to_string()],
            supports_streaming: false,
            timeout_ms: 5_000,
        }
    }

    async fn execute(&self, _: ToolInput) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            tool_name: "solend_deposit_usdc".into(),
            success: true,
            data: Some(json!({"status": "awaiting_approval"})),
            error: None,
            duration_ms: 0,
        })
    }
}

struct JupiterSwapStub;

#[async_trait]
impl Tool for JupiterSwapStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "submit_jupiter_swap".into(),
            description: "test-only Jupiter swap stub".into(),
            input_schema: json!({
                "type": "object",
                "required": ["input_mint", "output_mint", "input_amount", "slippage_bps"],
                "properties": {
                    "input_mint":    { "type": "string" },
                    "output_mint":   { "type": "string" },
                    "input_amount":  { "type": "integer", "minimum": 1 },
                    "slippage_bps":  { "type": "integer", "minimum": 0, "maximum": 10000 },
                    "wallet_pubkey": { "type": "string" },
                    "description":   { "type": "string" }
                }
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec!["propose_signing".to_string()],
            supports_streaming: false,
            timeout_ms: 30_000,
        }
    }

    async fn execute(&self, _: ToolInput) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            tool_name: "submit_jupiter_swap".into(),
            success: true,
            data: Some(json!({"status": "awaiting_approval"})),
            error: None,
            duration_ms: 0,
        })
    }
}

// ── Test harness ───────────────────────────────────────────────────────────

struct Ctx {
    router: axum::Router,
    sid: SessionId,
    /// Optional handle for tests that need to assert call_count on the
    /// scripted provider after the request.
    scripted: Option<Arc<ScriptedLlmProvider>>,
}

fn caps_for_chat() -> CapabilitySet {
    let mut set = CapabilitySet::empty();
    set.grant(claw_tool_system::permissions::Capability::ProposeSigning);
    set
}

fn registry_with_fake_tool() -> ToolRegistry {
    ToolRegistry::from_tools(vec![Arc::new(FakeTool)])
}

/// Registry that exposes BOTH chat-allowlisted tool names with their
/// real names. Used by the Jupiter / multi-tool / unknown-tool tests
/// (classes R / S / T / U) to verify the dispatch / rejection paths
/// hold for the real allowlist surface.
fn registry_with_solend_and_jupiter_stubs() -> ToolRegistry {
    ToolRegistry::from_tools(vec![
        Arc::new(SolendDepositStub),
        Arc::new(JupiterSwapStub),
    ])
}

async fn build_ctx(scripted: Option<Arc<ScriptedLlmProvider>>) -> Ctx {
    build_ctx_with_registry(scripted, registry_with_fake_tool()).await
}

async fn build_ctx_with_registry(
    scripted: Option<Arc<ScriptedLlmProvider>>,
    registry: ToolRegistry,
) -> Ctx {
    let sid = SessionId::from(Uuid::new_v4());
    let (tx, _) = broadcast::channel::<GatewayEvent>(4);

    let chat_ref: Option<ChatHandlerRef> = scripted.clone().map(|p| {
        let llm: LlmClientRef = p;
        GatewayChatHandler::new(
            llm,
            registry,
            SYS_PROMPT.to_string(),
            caps_for_chat(),
        )
        .into_handler_ref()
    });

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
        chat:              chat_ref,
        chat_execute:      None,
        chat_funding_confirm: None,
        chat_refund:       None,
        chat_order_status:       None,
        chat_intent_finalize:    None,
    };
    let router = claw_api::create_router(state, HealthRegistry::new());
    Ctx { router, sid, scripted }
}

fn chat_uri(sid: &SessionId) -> String {
    format!("/sessions/{}/chat", sid)
}

fn authed_post(uri: String, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

async fn send(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn scripted_assistant(text: &str) -> Arc<ScriptedLlmProvider> {
    Arc::new(ScriptedLlmProvider::assistant_text(text))
}

fn scripted_tool_call(tool: &str, input: Value) -> Arc<ScriptedLlmProvider> {
    Arc::new(ScriptedLlmProvider::tool_calls(vec![LlmToolCall {
        id: "call_1".into(),
        tool_name: tool.into(),
        input,
    }]))
}

fn scripted_two_tools() -> Arc<ScriptedLlmProvider> {
    Arc::new(ScriptedLlmProvider::tool_calls(vec![
        LlmToolCall { id: "a".into(), tool_name: "fake_propose".into(), input: json!({"mode":"ok"}) },
        LlmToolCall { id: "b".into(), tool_name: "fake_propose".into(), input: json!({"mode":"ok"}) },
    ]))
}

// ────────────────────────────── Tests ────────────────────────────────────

// Class A — assistant text → 200
#[tokio::test]
async fn class_a_assistant_text_returns_200_with_text() {
    let prov = scripted_assistant("hello there");
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"hi"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "assistant_text");
    assert_eq!(body["assistant_text"], "hello there");
}

// Class B — tool dispatched (non-pending) → 200
#[tokio::test]
async fn class_b_tool_dispatched_returns_200_with_output() {
    let prov = scripted_tool_call("fake_propose", json!({"mode":"ok"}));
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"propose"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "fake_propose");
    assert_eq!(body["output"]["data"]["status"], "awaiting_approval");
}

// Class C — multiple tool calls → 200 with rejection variant
#[tokio::test]
async fn class_c_multiple_tool_calls_returns_200_with_rejection() {
    let prov = scripted_two_tools();
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"do two things"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "multiple_tool_calls_rejected");
    assert_eq!(body["count"], 2);
}

// Class D — unknown / denied tool → 200 with denied variant
#[tokio::test]
async fn class_d_unknown_tool_returns_200_with_denied() {
    let prov = scripted_tool_call("nonexistent_tool", json!({}));
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"x"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "unknown_or_denied_tool");
    assert_eq!(body["tool_name"], "nonexistent_tool");
}

// Class E — malformed tool args → 200 with malformed variant
#[tokio::test]
async fn class_e_malformed_tool_args_returns_200_with_malformed() {
    // Provider returns a non-object input (a JSON array instead of object).
    let prov = scripted_tool_call("fake_propose", json!(["not", "an", "object"]));
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"x"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "malformed_tool_arguments");
    assert_eq!(body["tool_name"], "fake_propose");
}

// Class F — malformed provider output → 200 with malformed variant
#[tokio::test]
async fn class_f_provider_error_returns_200_with_malformed() {
    // Empty queue — provider call returns an error; handler maps that to
    // MalformedProviderOutput.
    let prov = Arc::new(ScriptedLlmProvider::new(vec![]));
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"x"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "malformed_provider_output");
}

// Class G — tool error → 200 with error variant; uses Display, not Debug
#[tokio::test]
async fn class_g_tool_error_returns_200_with_display_message() {
    let prov = scripted_tool_call("fake_propose", json!({"mode":"tool_error"}));
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"x"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_error");
    assert_eq!(body["tool_name"], "fake_propose");
    let msg = body["message"].as_str().unwrap_or("");
    assert!(!msg.is_empty(), "message must be non-empty");
    // Display formatter, NOT Debug — so the wire shape never contains
    // internal Rust struct/enum names like `ToolError {`.
    assert!(!msg.contains("ToolError {"), "leaked Debug shape: {msg}");
    assert!(!msg.contains("InvalidInput {"), "leaked Debug shape: {msg}");
}

// Class H — pending_action_exists → 409 Conflict
#[tokio::test]
async fn class_h_pending_action_exists_returns_409() {
    let prov = scripted_tool_call("fake_propose", json!({"mode":"pending"}));
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"propose again"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::CONFLICT, "body={body:#}");
    assert_eq!(body["status"], "pending_action_exists");
    assert!(body["reason"].as_str().unwrap_or("").contains("approval"));
}

// Class I — invalid session id → 400
#[tokio::test]
async fn class_i_invalid_session_id_returns_400() {
    let ctx = build_ctx(Some(scripted_assistant("ok"))).await;
    let req = authed_post(
        "/sessions/not-a-uuid/chat".to_string(),
        json!({"message":"x"}).to_string(),
    );
    let (status, _) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Class J — empty / blank message → 400
#[tokio::test]
async fn class_j_empty_message_returns_400() {
    let ctx = build_ctx(Some(scripted_assistant("ok"))).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"   \n   "}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body:#}");
}

// Class K — message exceeds char limit (4000 chars) → 400
#[tokio::test]
async fn class_k_oversized_message_returns_400() {
    let ctx = build_ctx(Some(scripted_assistant("ok"))).await;
    // 4001 ASCII chars — passes the 4096-byte body cap once wrapped in
    // `{"message":"...."}` only if we keep the JSON envelope small.
    // Use a 4001-char message; at the framework body layer the total
    // body size is ~4015 bytes which IS under the 4096 cap, so the
    // request reaches the handler and the handler enforces the
    // char-level cap.
    let big = "a".repeat(4001);
    let req = authed_post(chat_uri(&ctx.sid), json!({"message": big}).to_string());
    let (status, _) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Class L — session not active → 404
#[tokio::test]
async fn class_l_inactive_session_returns_404() {
    let ctx = build_ctx(Some(scripted_assistant("ok"))).await;
    let other = SessionId::from(Uuid::new_v4());
    let req = authed_post(chat_uri(&other), json!({"message":"x"}).to_string());
    let (status, _) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// Class M — chat handler not wired → 503
#[tokio::test]
async fn class_m_no_chat_handler_returns_503() {
    let ctx = build_ctx(None).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"x"}).to_string());
    let (status, _) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// Class N — oversize body (> 4096 bytes) → 413 from framework
#[tokio::test]
async fn class_n_oversize_body_rejected_by_framework() {
    let ctx = build_ctx(Some(scripted_assistant("ok"))).await;
    // 8 KiB raw body — well over the 4096-byte cap. The handler must
    // never even run; axum's DefaultBodyLimit returns 413 first.
    let big_msg = "a".repeat(8 * 1024);
    let body = json!({"message": big_msg}).to_string();
    let req = authed_post(chat_uri(&ctx.sid), body);
    let resp = ctx.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "framework should reject oversize bodies before the handler runs"
    );
}

// Class O — extra fields in body → 400 (deny_unknown_fields)
#[tokio::test]
async fn class_o_extra_fields_rejected() {
    let ctx = build_ctx(Some(scripted_assistant("ok"))).await;
    let body = json!({"message":"x", "evil": "drop_table_users"}).to_string();
    let req = authed_post(chat_uri(&ctx.sid), body);
    let (status, _) = send(&ctx.router, req).await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "extra-fields body should be rejected; got {status}"
    );
}

// Class P — one-turn enforcement: provider call_count == 1 after success
#[tokio::test]
async fn class_p_one_turn_provider_call_count_is_exactly_one() {
    let prov = scripted_tool_call("fake_propose", json!({"mode":"ok"}));
    let ctx = build_ctx(Some(prov.clone())).await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"propose"}).to_string());
    let (status, _) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK);
    let scripted = ctx.scripted.expect("scripted provider stored on ctx");
    assert_eq!(
        scripted.call_count(),
        1,
        "chat handler must call the provider exactly once per HTTP request"
    );
    // Role separation: system prompt is never the user text.
    let nth = scripted.nth_call(0).expect("recorded call");
    assert_eq!(nth.system, SYS_PROMPT);
    assert!(nth.system != "propose");
}

// Class Q — auth required: missing Bearer → 401
#[tokio::test]
async fn class_q_missing_bearer_returns_401() {
    let ctx = build_ctx(Some(scripted_assistant("ok"))).await;
    let unauth = Request::builder()
        .method("POST")
        .uri(chat_uri(&ctx.sid))
        .header("content-type", "application/json")
        .body(Body::from(json!({"message":"x"}).to_string()))
        .unwrap();
    let resp = ctx.router.clone().oneshot(unauth).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── Agent C additions — Jupiter swap on the chat route ───────────────────
//
// Class R / S / T / U exercise the chat-route dispatch path with the real
// allowlisted tool names (`solend_deposit_usdc` and `submit_jupiter_swap`).
// They run against an in-memory registry built via
// `registry_with_solend_and_jupiter_stubs()` and use `ScriptedLlmProvider`,
// so no provider API call, no live network, no signing/submit code path is
// exercised.

// Class R — Jupiter swap dispatched (non-pending) → 200
#[tokio::test]
async fn class_r_jupiter_swap_dispatched_returns_200_with_output() {
    let prov = scripted_tool_call(
        "submit_jupiter_swap",
        json!({
            "input_mint":   "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "output_mint":  "So11111111111111111111111111111111111111112",
            "input_amount": 1_000_000,
            "slippage_bps": 50,
        }),
    );
    let ctx = build_ctx_with_registry(
        Some(prov.clone()),
        registry_with_solend_and_jupiter_stubs(),
    )
    .await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message": "swap 1 USDC to SOL"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "submit_jupiter_swap");
    assert_eq!(body["output"]["data"]["status"], "awaiting_approval");
}

// Class S — malformed Jupiter args → 200 with malformed variant
//
// Provider returns a non-object input for `submit_jupiter_swap` (a JSON
// array). The chat handler must fail closed with the malformed-arguments
// variant — same path Class E exercises for `fake_propose`.
#[tokio::test]
async fn class_s_malformed_jupiter_args_returns_200_with_malformed() {
    let prov = scripted_tool_call(
        "submit_jupiter_swap",
        json!(["not", "an", "object"]),
    );
    let ctx = build_ctx_with_registry(
        Some(prov.clone()),
        registry_with_solend_and_jupiter_stubs(),
    )
    .await;
    let req = authed_post(chat_uri(&ctx.sid), json!({"message":"x"}).to_string());
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "malformed_tool_arguments");
    assert_eq!(body["tool_name"], "submit_jupiter_swap");
}

// Class T — multi-tool with both real allowlisted names rejected whole-turn
//
// This is the load-bearing one: even if the LLM emits two perfectly
// valid allowlisted tool calls (Solend deposit + Jupiter swap) in a
// single turn, the ConversationHandler MUST reject the entire turn
// because of the one-tool-per-turn invariant — not because either tool
// is unknown.
#[tokio::test]
async fn class_t_solend_plus_jupiter_multi_tool_rejected_whole_turn() {
    let prov = Arc::new(ScriptedLlmProvider::tool_calls(vec![
        LlmToolCall {
            id: "a".into(),
            tool_name: "solend_deposit_usdc".into(),
            input: json!({"amount": 1000}),
        },
        LlmToolCall {
            id: "b".into(),
            tool_name: "submit_jupiter_swap".into(),
            input: json!({
                "input_mint":   "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "output_mint":  "So11111111111111111111111111111111111111112",
                "input_amount": 1_000_000,
                "slippage_bps": 50,
            }),
        },
    ]));
    let ctx = build_ctx_with_registry(
        Some(prov.clone()),
        registry_with_solend_and_jupiter_stubs(),
    )
    .await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"deposit and swap in one turn"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "multiple_tool_calls_rejected");
    assert_eq!(body["count"], 2);
}

// Class U — unknown / forbidden tool still denied with Jupiter registered
//
// Adding `submit_jupiter_swap` to the chat allowlist must not widen the
// surface to other tools. A scripted call to a name that is not in the
// allowlist (`send_raw_transaction`) must still resolve to the
// unknown-or-denied variant, even when the registry has both real
// chat tools registered.
#[tokio::test]
async fn class_u_unknown_tool_still_denied_with_jupiter_registered() {
    let prov = scripted_tool_call("send_raw_transaction", json!({"tx": "00"}));
    let ctx = build_ctx_with_registry(
        Some(prov.clone()),
        registry_with_solend_and_jupiter_stubs(),
    )
    .await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"submit raw"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "unknown_or_denied_tool");
    assert_eq!(body["tool_name"], "send_raw_transaction");
}

// ─── Phase 6C additions — read-only chat tools ────────────────────────────
//
// Class V/W/X/Y/Z/AA exercise the chat-route dispatch path with the two
// new read-only tools (`get_wallet_balances` and `get_jupiter_quote`).
// They use ScriptedLlmProvider so no live provider call, no RPC, no
// Jupiter API contact. The stubs below mirror the production tool names
// so the multi-tool rejection (class Z) provably triggers on the
// batching policy alone, not on a name-unknown path.

struct BalancesStub;

#[async_trait]
impl Tool for BalancesStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_wallet_balances".into(),
            description: "test-only Phase 6C balances stub".into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": [],
                "properties": {}
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8_000,
        }
    }

    async fn execute(&self, _: ToolInput) -> Result<ToolOutput, ToolError> {
        // Simulate a wallet with 0.05 USDC and 0.1 SOL — chosen so the
        // insufficient-balance demo (class AA) is unambiguous when the
        // user requests "deposit 0.1 USDC".
        Ok(ToolOutput {
            tool_name: "get_wallet_balances".into(),
            success: true,
            data: Some(json!({
                "status": "ok",
                "wallet_pubkey": "3xTfBYx7Y7iC5HgKXTpe9eKJD1FH3v4qDcSFv6oxrt7P",
                "sol_lamports": 100_000_000,
                "sol_ui": "0.100000000",
                "usdc_mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "usdc_ata": "BalancesStubATA1111111111111111111111111111",
                "usdc_raw": 50_000,
                "usdc_ui": "0.050000",
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

struct QuoteStub;

#[async_trait]
impl Tool for QuoteStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_jupiter_quote".into(),
            description: "test-only Phase 6C Jupiter quote stub".into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["input_mint", "output_mint", "input_amount", "slippage_bps"],
                "properties": {
                    "input_mint":   { "type": "string" },
                    "output_mint":  { "type": "string" },
                    "input_amount": { "type": "integer", "minimum": 1 },
                    "slippage_bps": { "type": "integer", "minimum": 0, "maximum": 100 }
                }
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let slippage_bps = input.parameters["slippage_bps"].as_u64().unwrap_or(0);
        if slippage_bps > 100 {
            return Ok(ToolOutput {
                tool_name: "get_jupiter_quote".into(),
                success: false,
                data: Some(json!({
                    "status": "policy_blocked",
                    "policy_rule_name": "slippage-exceeds-quote-cap",
                    "slippage_bps": slippage_bps,
                })),
                error: Some(format!("slippage_bps {slippage_bps} exceeds 100")),
                duration_ms: 0,
            });
        }
        Ok(ToolOutput {
            tool_name: "get_jupiter_quote".into(),
            success: true,
            data: Some(json!({
                "status": "ok",
                "input_mint": input.parameters["input_mint"],
                "output_mint": input.parameters["output_mint"],
                "input_amount": input.parameters["input_amount"],
                "out_amount": 150_000,
                "other_amount_threshold": 148_500,
                "price_impact_pct": "0.0123",
                "route_summary": ["Orca"],
                "slippage_bps": slippage_bps,
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

struct SolendPositionStub;

#[async_trait]
impl Tool for SolendPositionStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_solend_position".into(),
            description: "test-only Phase 6H Solend position scanner stub".into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {},
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8_000,
        }
    }

    async fn execute(&self, _input: ToolInput) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            tool_name: "get_solend_position".into(),
            success: true,
            data: Some(json!({
                "status":                       "ok",
                "wallet_pubkey":                "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW",
                "network":                      "mainnet",
                "protocol":                     "Solend/Save",
                "program_id":                   "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo",
                "lending_market":               "lendingMarketStubForP5D2_____________________",
                "usdc_main_pool_reserve":       "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw",
                "usdc_main_pool_mint":          "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "obligation_count":             1,
                "usdc_deposit_position_count":  1,
                "positions": [{
                    "kind":                            "deposit",
                    "obligation_pubkey":               "obligationStubP5D2_____________________________",
                    "owner_pubkey":                    "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW",
                    "lending_market":                  "lendingMarketStubForP5D2_____________________",
                    "reserve_pubkey":                  "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw",
                    "is_usdc_main_pool_reserve":       true,
                    "deposited_collateral_amount_raw": "5000000",
                    "supplied_usdc_estimate_raw":      null,
                    "estimate_unavailable_reason":     "stub",
                    "has_borrow":                      false,
                    "source":                          "obligation_scan",
                }],
                "decode_warnings":              [],
                "dashboard_visibility_note":    "stub note",
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

/// Phase 6I-B — preview tool stub. Returns a fully-populated OK preview
/// JSON whose shape matches the production
/// `PreviewSolendWithdrawAllTool::ok_preview_output`. The
/// `obligation_pubkey` and `collateral_amount_raw` fields are echoed
/// from the input so a single stub serves both the "happy path" and
/// "echo-the-input" assertions.
struct PreviewSolendWithdrawAllStub;

#[async_trait]
impl Tool for PreviewSolendWithdrawAllStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "preview_solend_withdraw_all".into(),
            description: "test-only Phase 6I-B Solend withdraw-all preview stub".into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["obligation_pubkey"],
                "properties": {
                    "obligation_pubkey": { "type": "string" }
                }
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let obligation_pubkey = input
            .parameters
            .get("obligation_pubkey")
            .and_then(|v| v.as_str())
            .unwrap_or("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV")
            .to_string();
        Ok(ToolOutput {
            tool_name: "preview_solend_withdraw_all".into(),
            success: true,
            data: Some(json!({
                "status":                       "ok",
                "mode":                         "withdraw_all_collateral",
                "protocol":                     "Solend/Save",
                "network":                      "mainnet",
                "program_id":                   "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo",
                "wallet_pubkey":                "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW",
                "obligation_pubkey":            obligation_pubkey,
                "lending_market":               "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY",
                "reserve_pubkey":               "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw",
                "reserve_mint":                 "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "collateral_amount_raw":        "3857506",
                "underlying_usdc_estimate_raw": null,
                "underlying_usdc_estimate_ui":  null,
                "estimate_unavailable_reason":  "stub: exchange-rate decode deferred",
                "requires_user_signature":      true,
                "required_signers":             ["wallet"],
                "requires_obligation_keypair":  false,
                "will_create_approval":         false,
                "will_sign":                    false,
                "will_broadcast":               false,
                "next_step":                    "stub: preview-only",
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

/// Phase 6I-D — execution-proposal tool stub. Mirrors the production
/// tool's awaiting_approval shape so the chat-route tests exercise
/// dispatch + invariant assertions without standing up a real RPC
/// reader / park store.
struct SolendWithdrawAllUsdcStub;

#[async_trait]
impl Tool for SolendWithdrawAllUsdcStub {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "solend_withdraw_all_usdc".into(),
            description: "test-only Phase 6I-D Solend withdraw-all execution stub".into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["obligation_pubkey"],
                "properties": {
                    "obligation_pubkey": { "type": "string" }
                }
            }),
            output_schema: json!({"type": "object"}),
            required_capabilities: vec!["propose_signing".to_string()],
            supports_streaming: false,
            timeout_ms: 15_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let obligation_pubkey = input
            .parameters
            .get("obligation_pubkey")
            .and_then(|v| v.as_str())
            .unwrap_or("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV")
            .to_string();
        let approval_request_id = Uuid::new_v4();
        let intent_id = Uuid::new_v4();
        Ok(ToolOutput {
            tool_name: "solend_withdraw_all_usdc".into(),
            success: true,
            data: Some(json!({
                "status":                              "awaiting_approval",
                "protocol":                            "Solend/Save",
                "network":                             "mainnet",
                "mode":                                "withdraw_all_collateral",
                "asset":                               "USDC",
                "wallet_pubkey":                       "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW",
                "obligation_pubkey":                   obligation_pubkey,
                "lending_market":                      "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY",
                "reserve_pubkey":                      "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw",
                "reserve_mint":                        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "collateral_amount_raw":               "3857506",
                "underlying_usdc_estimate_raw":        null,
                "estimate_unavailable_reason":         "stub",
                "requires_user_signature":             true,
                "required_signers":                    ["wallet"],
                "requires_obligation_keypair":         false,
                "intent_id":                           intent_id.to_string(),
                "approval_request_id":                 approval_request_id.to_string(),
                "approval_required":                   true,
                "will_build_transaction_on_sign_click": true,
                "will_sign":                           false,
                "will_broadcast":                      false,
                "policy_verdict":                      "Pass",
                "policy_rule_name":                    "solend-withdraw-all-explicit-obligation",
                "human_readable_reason":               "stub: awaiting_approval",
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

fn registry_with_all_chat_tools() -> ToolRegistry {
    ToolRegistry::from_tools(vec![
        Arc::new(SolendDepositStub),
        Arc::new(JupiterSwapStub),
        Arc::new(BalancesStub),
        Arc::new(QuoteStub),
        Arc::new(SolendPositionStub),
        Arc::new(PreviewSolendWithdrawAllStub),
        Arc::new(SolendWithdrawAllUsdcStub),
    ])
}

// Class V — get_wallet_balances dispatched → 200 with balance JSON
#[tokio::test]
async fn class_v_balance_query_dispatched_returns_200_with_output() {
    let prov = scripted_tool_call("get_wallet_balances", json!({}));
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"What are my balances?"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "get_wallet_balances");
    assert_eq!(body["output"]["data"]["status"], "ok");
    assert_eq!(body["output"]["data"]["usdc_raw"], 50_000);
    assert_eq!(body["output"]["data"]["usdc_ui"], "0.050000");
    assert_eq!(body["output"]["data"]["sol_lamports"], 100_000_000);
}

// Class W — get_jupiter_quote dispatched → 200 with quote JSON
#[tokio::test]
async fn class_w_quote_query_dispatched_returns_200_with_output() {
    let prov = scripted_tool_call(
        "get_jupiter_quote",
        json!({
            "input_mint":   "So11111111111111111111111111111111111111112",
            "output_mint":  "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "input_amount": 1_000_000,
            "slippage_bps": 100,
        }),
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"How much USDC for 0.001 SOL?"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "get_jupiter_quote");
    assert_eq!(body["output"]["data"]["status"], "ok");
    assert_eq!(body["output"]["data"]["out_amount"], 150_000);
    assert!(body["output"]["data"]["route_summary"].is_array());
}

// Class X — quote with slippage > 100 → 200 dispatched with policy_blocked
//
// The tool intentionally keeps this on the dispatched path (Ok ToolOutput
// with success=false) so the LLM/UI can render structured rejection info
// instead of a generic ToolError variant.
#[tokio::test]
async fn class_x_quote_over_slippage_returns_policy_blocked() {
    let prov = scripted_tool_call(
        "get_jupiter_quote",
        json!({
            "input_mint":   "So11111111111111111111111111111111111111112",
            "output_mint":  "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "input_amount": 1_000_000,
            "slippage_bps": 200,
        }),
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"Quote 0.001 SOL with 2% slippage"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "get_jupiter_quote");
    assert_eq!(body["output"]["data"]["status"], "policy_blocked");
    assert_eq!(
        body["output"]["data"]["policy_rule_name"],
        "slippage-exceeds-quote-cap"
    );
}

// Class Y — conditional prompt scripted to call get_wallet_balances FIRST
//
// When the LLM is given a conditional message ("deposit X if balance is
// above Y"), the alignment prompt instructs it to call the read-only
// tool first and stop the turn. We assert the wire shape: the chat
// route returns the balance dispatch — NOT a deposit proposal.
#[tokio::test]
async fn class_y_conditional_prompt_routes_to_balance_check_first() {
    let prov = scripted_tool_call("get_wallet_balances", json!({}));
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"Deposit 0.001 USDC into Solend if my USDC balance is above 0.3."})
            .to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "get_wallet_balances");
    // The deposit tool MUST NOT have been invoked in this turn.
    assert_ne!(body["tool_name"], "solend_deposit_usdc");
}

// Class Z — read-only + write tool batched in one turn → whole-turn rejected
//
// Even when the LLM emits a perfectly valid balance check AND a valid
// solend_deposit_usdc call in the same turn, the ConversationHandler
// MUST reject the entire turn for one-tool-per-turn — exactly as it
// rejected the Solend + Jupiter batch in class T.
#[tokio::test]
async fn class_z_balance_plus_deposit_multi_tool_rejected_whole_turn() {
    let prov = Arc::new(ScriptedLlmProvider::tool_calls(vec![
        LlmToolCall {
            id: "a".into(),
            tool_name: "get_wallet_balances".into(),
            input: json!({}),
        },
        LlmToolCall {
            id: "b".into(),
            tool_name: "solend_deposit_usdc".into(),
            input: json!({"amount": 1000}),
        },
    ]));
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"Check balance and deposit"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "multiple_tool_calls_rejected");
    assert_eq!(body["count"], 2);
}

// Class AA — insufficient-balance demo: LLM declines with text only
//
// Demo flow: user asks "deposit 0.1 USDC if I have it". On a follow-up
// turn (or in a single-turn fast path) where the LLM has already seen
// the balance and concluded it is insufficient, the alignment prompt
// instructs it to respond with plain text and NOT call any transaction
// tool. This test scripts that exact path: the provider returns an
// assistant_text response naming the observed balance and declining.
// The chat route surfaces this as ChatResponse::AssistantText (200).
#[tokio::test]
async fn class_aa_insufficient_balance_returns_assistant_text_no_proposal() {
    let prov = scripted_assistant(
        "Your USDC balance is 0.050000 (50000 raw), which is insufficient \
         for the requested 0.1 USDC deposit. I will not create a proposal.",
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"Deposit 0.1 USDC into Solend if I have it."}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "assistant_text");
    let text = body["assistant_text"].as_str().unwrap_or("");
    assert!(
        text.contains("insufficient") || text.contains("Insufficient"),
        "expected insufficient-balance phrasing; got {text}"
    );
    assert!(
        text.contains("not create") || text.contains("will not"),
        "expected explicit no-proposal phrasing; got {text}"
    );
    // Wire-shape: no tool_dispatched fields on this 200 path.
    assert!(body["tool_name"].is_null());
    assert!(body["output"].is_null());
}

// Class AB — Phase 6H position-scan: NL prompt routes to
// `get_solend_position` and the chat handler returns 200 dispatched
// with the structured position output. The tool advertises
// `required_capabilities: vec![]` so this turn cannot create an
// approval, signing handoff, or broadcast — those wire-shape fields
// are absent from the response.
#[tokio::test]
async fn class_ab_solend_position_query_dispatched_returns_200_with_output() {
    let prov = scripted_tool_call("get_solend_position", json!({}));
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message":"Where is my Solend deposit?"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "get_solend_position");
    let data = &body["output"]["data"];
    assert_eq!(data["status"], "ok");
    assert_eq!(
        data["usdc_main_pool_reserve"],
        "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw"
    );
    assert_eq!(data["usdc_deposit_position_count"], 1);
    let positions = data["positions"].as_array().unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0]["kind"], "deposit");
    assert_eq!(positions[0]["is_usdc_main_pool_reserve"], true);
    assert_eq!(positions[0]["source"], "obligation_scan");
    // Read-only invariant: this dispatched response must NOT contain
    // approval / signing / broadcast / tx_signature fields, and must
    // not surface a private key or signed-bytes payload.
    let body_s = serde_json::to_string(&body).unwrap();
    for forbidden in &[
        "approval_request_id",
        "signing_request_id",
        "tx_signature",
        "tx_bytes",
        "signed_bytes",
        "private_key",
    ] {
        assert!(
            !body_s.contains(forbidden),
            "read-only response must not contain `{forbidden}` field"
        );
    }
}

// ─── Phase 6I-B additions — withdraw-all preview tool ────────────────────────
//
// Class AC / AD / AE / AF lock the chat-route invariants for the new
// `preview_solend_withdraw_all` tool: NL dispatch reaches the preview;
// the dispatched response carries the structured preview JSON; no
// approval / signing / broadcast field is surfaced; the multi-tool
// rejection still applies if the LLM tries to batch the scanner with
// the preview in one turn; and the chat allowlist contains the preview
// while still EXCLUDING the (un-implemented) `solend_withdraw_usdc`
// execution tool.

const TARGET_OBLIGATION_BS58: &str = "HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV";

// Class AC — NL preview prompt routes to `preview_solend_withdraw_all`
// and the chat handler returns 200 dispatched with the preview JSON.
// Asserts every read-only invariant flag (`will_create_approval`,
// `will_sign`, `will_broadcast`, `requires_obligation_keypair`) is
// `false` and that no approval / signing / broadcast / private-key /
// tx-bytes field appears anywhere in the response body.
#[tokio::test]
async fn class_ac_preview_withdraw_all_dispatched_returns_200_with_output() {
    let prov = scripted_tool_call(
        "preview_solend_withdraw_all",
        json!({ "obligation_pubkey": TARGET_OBLIGATION_BS58 }),
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({
            "message": format!(
                "Preview withdraw-all for Solend obligation {TARGET_OBLIGATION_BS58}"
            )
        })
        .to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "preview_solend_withdraw_all");

    let data = &body["output"]["data"];
    assert_eq!(data["status"], "ok");
    assert_eq!(data["mode"], "withdraw_all_collateral");
    assert_eq!(data["obligation_pubkey"], TARGET_OBLIGATION_BS58);
    assert_eq!(data["collateral_amount_raw"], "3857506");
    assert_eq!(data["reserve_pubkey"], "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw");
    assert_eq!(data["reserve_mint"], "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

    // Wire-shape invariant: no execution / approval bookkeeping is
    // promoted to top-level chat fields. The dispatched envelope's
    // `approval_request_id` / `signing_request_id` fields are absent
    // (those would only appear for `awaiting_approval` flows).
    assert!(body["approval_request_id"].is_null());
    assert!(body["signing_request_id"].is_null());
    assert!(body["tx_signature"].is_null());

    // Output-shape invariant: every "future side effect" flag is false.
    assert_eq!(data["will_create_approval"], json!(false));
    assert_eq!(data["will_sign"], json!(false));
    assert_eq!(data["will_broadcast"], json!(false));
    assert_eq!(data["requires_obligation_keypair"], json!(false));
    assert_eq!(data["requires_user_signature"], json!(true));
    assert_eq!(data["required_signers"], json!(["wallet"]));

    // Body-wide forbidden field scan. Any leak of execution / signing /
    // private-key material would surface here regardless of nesting.
    let body_s = serde_json::to_string(&body).unwrap();
    for forbidden in &[
        "approval_request_id",
        "signing_request_id",
        "tx_signature",
        "tx_bytes",
        "signed_bytes",
        "private_key",
    ] {
        assert!(
            !body_s.contains(forbidden),
            "preview response must not contain `{forbidden}` anywhere in body"
        );
    }
}

// Class AD — multi-tool rejection still triggers if the LLM tries to
// batch `get_solend_position` and `preview_solend_withdraw_all` in the
// SAME turn. The one-tool-per-turn rule overrides any combination of
// allowlisted read-only tools.
#[tokio::test]
async fn class_ad_position_plus_preview_multi_tool_rejected_whole_turn() {
    let prov = Arc::new(ScriptedLlmProvider::tool_calls(vec![
        LlmToolCall {
            id: "a".into(),
            tool_name: "get_solend_position".into(),
            input: json!({}),
        },
        LlmToolCall {
            id: "b".into(),
            tool_name: "preview_solend_withdraw_all".into(),
            input: json!({ "obligation_pubkey": TARGET_OBLIGATION_BS58 }),
        },
    ]));
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message": "find my Solend obligation and preview withdraw in one go"})
            .to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "multiple_tool_calls_rejected");
    assert_eq!(body["count"], 2);
}

// Class AE — chat allowlist contains the preview tool AND continues to
// EXCLUDE the (still un-implemented) `solend_withdraw_usdc` execution
// tool. Locks the Phase 6I-B chat surface.
#[test]
fn class_ae_chat_allowlist_includes_preview_excludes_execution() {
    let allowlist = claw_gateway::runtime::chat_wiring::CHAT_TOOL_ALLOWLIST;
    assert!(
        allowlist.contains(&"preview_solend_withdraw_all"),
        "Phase 6I-B: preview tool must be in chat allowlist"
    );
    assert!(
        !allowlist.contains(&"solend_withdraw_usdc"),
        "withdraw EXECUTION tool must remain absent from chat allowlist"
    );
    // Phase 6H scanner still in (no regression).
    assert!(
        allowlist.contains(&"get_solend_position"),
        "Phase 6H scanner must remain in chat allowlist"
    );
}

// Class AF — execute-style prompts that try to skip preview and go
// straight to withdraw must NOT be answered with an `awaiting_approval`
// dispatch. The script returns assistant text (the alignment prompt
// directs the LLM to refuse-and-explain), and the chat envelope is the
// plain text outcome — no dispatched tool, no approval id.
#[tokio::test]
async fn class_af_execute_withdraw_prompt_does_not_create_approval_or_sign() {
    let prov = scripted_assistant(
        "Withdraw EXECUTION is not enabled yet. I can only PREVIEW the \
         withdraw — give me the obligation pubkey and I'll run \
         preview_solend_withdraw_all.",
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message": "Withdraw it now and sign it"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    // No tool was dispatched on this turn — assistant text only.
    assert!(body["tool_name"].is_null(), "execute-style prompt must not dispatch a tool");
    assert!(body["output"].is_null(), "execute-style prompt must not produce a tool output");
    // No approval / signing artifact anywhere in the body.
    let body_s = serde_json::to_string(&body).unwrap();
    for forbidden in &[
        "approval_request_id",
        "signing_request_id",
        "tx_signature",
        "tx_bytes",
        "signed_bytes",
        "private_key",
        "awaiting_approval",
    ] {
        assert!(
            !body_s.contains(forbidden),
            "execute-style refusal must not contain `{forbidden}` anywhere in body"
        );
    }
}

// ─── Phase 6I-D additions — withdraw-all execution-proposal tool ─────────────
//
// Class AG / AH / AI / AJ / AK lock the chat-route invariants for
// `solend_withdraw_all_usdc`: the explicit-obligation NL prompt
// dispatches the tool and returns awaiting_approval; partial-amount
// prompts do NOT reach the execution tool; preview+execute multi-tool
// is rejected; the chat allowlist contains both preview and execution
// while still excluding the legacy `solend_withdraw_usdc` execution
// name; no body field surfaces tx_bytes / signing handoff at chat time.

// Class AG — explicit-obligation withdraw-all NL prompt dispatches the
// execution tool and returns awaiting_approval with the structured
// proposal JSON. Asserts every invariant flag and the absence of any
// signing / broadcast field.
#[tokio::test]
async fn class_ag_withdraw_all_explicit_obligation_dispatches_to_awaiting_approval() {
    let prov = scripted_tool_call(
        "solend_withdraw_all_usdc",
        json!({ "obligation_pubkey": TARGET_OBLIGATION_BS58 }),
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({
            "message": format!(
                "Withdraw all USDC from Solend obligation {TARGET_OBLIGATION_BS58}. \
                 Do not approve, sign, or broadcast."
            )
        })
        .to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "tool_dispatched");
    assert_eq!(body["tool_name"], "solend_withdraw_all_usdc");

    let data = &body["output"]["data"];
    assert_eq!(data["status"], "awaiting_approval");
    assert_eq!(data["mode"], "withdraw_all_collateral");
    assert_eq!(data["obligation_pubkey"], TARGET_OBLIGATION_BS58);
    assert_eq!(
        data["lending_market"],
        "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY"
    );
    assert_eq!(data["reserve_pubkey"], "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw");
    assert_eq!(data["reserve_mint"], "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    assert_eq!(data["collateral_amount_raw"], "3857506");

    // Brief-required invariant flags:
    assert_eq!(data["requires_user_signature"], json!(true));
    assert_eq!(data["required_signers"], json!(["wallet"]));
    assert_eq!(data["requires_obligation_keypair"], json!(false));
    assert_eq!(data["will_build_transaction_on_sign_click"], json!(true));
    assert_eq!(data["will_sign"], json!(false));
    assert_eq!(data["will_broadcast"], json!(false));

    // approval_request_id is present and parses as a UUID.
    let arid = data["approval_request_id"].as_str().expect("approval_request_id");
    Uuid::parse_str(arid).expect("approval_request_id must parse as UUID");

    // Body-wide forbidden-field scan: at chat-tool time, no signing
    // handoff / tx-bytes / private-key surface should appear.
    let body_s = serde_json::to_string(&body).unwrap();
    for forbidden in &[
        "signing_request_id",
        "tx_signature",
        "tx_bytes",
        "signed_bytes",
        "private_key",
        "transaction_base64",
    ] {
        assert!(
            !body_s.contains(forbidden),
            "awaiting_approval response must not contain `{forbidden}` anywhere in body"
        );
    }
}

// Class AH — partial-amount prompt does NOT call the execution tool.
// Scripted assistant text response models the alignment-prompt-driven
// refuse-and-explain behavior. The wire shape is plain text — no
// dispatched tool, no awaiting_approval payload.
#[tokio::test]
async fn class_ah_partial_amount_prompt_does_not_dispatch_execution_tool() {
    let prov = scripted_assistant(
        "Partial withdraws are not supported. Only withdraw-all by explicit obligation \
         pubkey is available in this slice. If you'd like, give me the obligation \
         pubkey and I'll run preview_solend_withdraw_all first.",
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message": "Withdraw 2 USDC from Solend"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert!(body["tool_name"].is_null(), "partial-amount prompt must not dispatch a tool");
    assert!(body["output"].is_null(), "partial-amount prompt must not produce a tool output");
    let body_s = serde_json::to_string(&body).unwrap();
    for forbidden in &[
        "awaiting_approval",
        "approval_request_id",
        "signing_request_id",
        "tx_signature",
        "tx_bytes",
        "signed_bytes",
    ] {
        assert!(
            !body_s.contains(forbidden),
            "partial-amount refusal must not contain `{forbidden}`"
        );
    }
}

// Class AI — multi-tool rejection still triggers when the LLM tries
// to batch the preview + execution in one turn.
#[tokio::test]
async fn class_ai_preview_plus_execute_multi_tool_rejected_whole_turn() {
    let prov = Arc::new(ScriptedLlmProvider::tool_calls(vec![
        LlmToolCall {
            id: "a".into(),
            tool_name: "preview_solend_withdraw_all".into(),
            input: json!({ "obligation_pubkey": TARGET_OBLIGATION_BS58 }),
        },
        LlmToolCall {
            id: "b".into(),
            tool_name: "solend_withdraw_all_usdc".into(),
            input: json!({ "obligation_pubkey": TARGET_OBLIGATION_BS58 }),
        },
    ]));
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message": "preview and then withdraw in one turn"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "multiple_tool_calls_rejected");
    assert_eq!(body["count"], 2);
}

// Class AJ — chat allowlist contains both preview and execution; the
// legacy / un-implemented `solend_withdraw_usdc` (without `_all`)
// remains absent. Locks the Phase 6I-D chat surface.
#[test]
fn class_aj_chat_allowlist_includes_preview_and_execution() {
    let allowlist = claw_gateway::runtime::chat_wiring::CHAT_TOOL_ALLOWLIST;
    assert!(
        allowlist.contains(&"preview_solend_withdraw_all"),
        "preview tool must remain in allowlist"
    );
    assert!(
        allowlist.contains(&"solend_withdraw_all_usdc"),
        "Phase 6I-D: execution tool must be in allowlist"
    );
    assert!(
        !allowlist.contains(&"solend_withdraw_usdc"),
        "legacy / un-implemented `solend_withdraw_usdc` must remain absent"
    );
    assert!(
        allowlist.contains(&"get_solend_position"),
        "Phase 6H scanner must remain in allowlist"
    );
    // No borrow/repay tool added in this slice.
    for forbidden in &["solend_borrow_usdc", "solend_repay_usdc"] {
        assert!(
            !allowlist.contains(forbidden),
            "borrow/repay tool `{forbidden}` must not appear in allowlist"
        );
    }
}

// Class AK — malformed args (JSON array instead of object) targeting
// the execution tool resolves to the malformed-arguments envelope —
// same path Class E / S exercise for `fake_propose` / Jupiter swap.
// The production tool also enforces `deny_unknown_fields` at its
// `serde::Deserialize` boundary, exhaustively asserted by the unit
// test `extra_fields_in_input_rejected_by_deny_unknown_fields`.
#[tokio::test]
async fn class_ak_execution_tool_malformed_args_rejected_as_malformed() {
    let prov = scripted_tool_call(
        "solend_withdraw_all_usdc",
        json!(["not", "an", "object"]),
    );
    let ctx = build_ctx_with_registry(Some(prov.clone()), registry_with_all_chat_tools()).await;
    let req = authed_post(
        chat_uri(&ctx.sid),
        json!({"message": "withdraw all from obligation"}).to_string(),
    );
    let (status, body) = send(&ctx.router, req).await;
    assert_eq!(status, StatusCode::OK, "body={body:#}");
    assert_eq!(body["status"], "malformed_tool_arguments");
    assert_eq!(body["tool_name"], "solend_withdraw_all_usdc");
}

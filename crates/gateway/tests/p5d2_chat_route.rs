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

async fn build_ctx(scripted: Option<Arc<ScriptedLlmProvider>>) -> Ctx {
    let sid = SessionId::from(Uuid::new_v4());
    let (tx, _) = broadcast::channel::<GatewayEvent>(4);

    let chat_ref: Option<ChatHandlerRef> = scripted.clone().map(|p| {
        let llm: LlmClientRef = p;
        GatewayChatHandler::new(
            llm,
            registry_with_fake_tool(),
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

//! P5 — Wire-shape contract tests.
//!
//! These tests protect the JSON contract between the Rust gateway and the
//! Next.js frontend. Each test hits a real `axum::Router`, deserializes the
//! response as `serde_json::Value`, and asserts the exact field names and
//! shapes that the TypeScript types in `frontend/src/lib/types.ts` depend on.
//!
//! If a Rust serde rename, field addition, or enum variant change breaks one
//! of these tests, the frontend WILL break in live mode. Fix the test by
//! updating both sides — never silently adjust only the backend.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower::ServiceExt;
use uuid::Uuid;

use claw_api::{
    AppState, ApprovalHandler, ApprovalHandlerRef, AuthToken,
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
use claw_gateway::ApprovalStore;
use claw_observability::HealthRegistry;
use claw_state_store::{AuditRepository, Database, SpendRepository};
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{
        ApprovalChainStage, ApprovalRequest, ApprovalStage, ApprovalWorkflow,
        ApprovalWorkflowState,
    },
    events::GatewayEvent,
    policy::{PolicyAction, PolicyCondition, PolicyRule, PolicyVerdict},
    session::SessionId,
    transaction::SimulationResult,
};

// ── Stubs (minimal copies from p2 — kept local to avoid coupling) ──────────

const TOKEN: &str = "p5-wire";

fn fake_sim() -> SimulationResult {
    SimulationResult {
        success: true, error: None, compute_units_used: Some(500),
        logs: vec![], return_data: None, account_diffs: vec![],
        fee_lamports: Some(5000),
    }
}

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
    { Box::pin(async move { WalletSignatureOutcome { request_id: rid, accepted: false, signature: None, tx_signature: None, submitted: false, error: Some("stub".into()), rebuild_required: false } }) }
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

struct StoreBackedApproval(ApprovalStore);
impl ApprovalHandler for StoreBackedApproval {
    fn pending_for_session(&self, sid: &SessionId) -> Vec<ApprovalRequest> {
        self.0.pending_for_session(sid)
    }
    fn all_pending(&self) -> Vec<claw_api::state::PendingApprovalItem> {
        self.0.all_pending_with_workflow().into_iter()
            .map(|(r, w)| claw_api::state::PendingApprovalItem { request: r, workflow: w })
            .collect()
    }
    fn get_by_id(&self, rid: Uuid) -> Option<claw_api::state::PendingApprovalItem> {
        let r = self.0.get(rid)?;
        let w = self.0.get_workflow(rid)?;
        Some(claw_api::state::PendingApprovalItem { request: r, workflow: w })
    }
    fn session_for_request(&self, rid: Uuid) -> Option<SessionId> {
        self.0.session_for_request(rid)
    }
    fn decide(&self, d: claw_types::approval::ApprovalDecision)
        -> Pin<Box<dyn Future<Output = (claw_types::approval::ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>>
    { let (o, r) = self.0.decide(&d); Box::pin(async move { (o, r) }) }
}

struct VecPolicyReader(Vec<PolicyRule>);
impl PolicyReader for VecPolicyReader { fn rules(&self) -> Vec<PolicyRule> { self.0.clone() } }

struct Ctx {
    router: axum::Router,
    store:  ApprovalStore,
    sid:    SessionId,
}

async fn setup(rules: Vec<PolicyRule>) -> Ctx {
    let store = ApprovalStore::new();
    let sid   = SessionId::from(Uuid::new_v4());
    let (tx, _) = broadcast::channel::<GatewayEvent>(4);
    let state = AppState {
        session_mgr:       SessionManagerRef::new(Arc::new(StubSession(sid.clone()))),
        message_handler:   MessageHandlerRef::new(Arc::new(StubMsg)),
        approval:          ApprovalHandlerRef::new(Arc::new(StoreBackedApproval(store.clone()))),
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
        policy:            PolicyReaderRef::new(Arc::new(VecPolicyReader(rules))),
        audit:             AuditReaderRef::new(Arc::new(StubAudit)),
        wallets:           WalletDirectoryRef::new(Arc::new(StubWalletDir)),
        demo_seeder:       None,
        chat:              None,
    };
    let router = claw_api::create_router(state, HealthRegistry::new());
    Ctx { router, store, sid }
}

fn authed_get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty()).unwrap()
}

async fn json_get(router: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = router.clone().oneshot(authed_get(path)).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

// ── Wire-shape: PolicyCondition unit variants serialize as bare strings ──────

#[tokio::test]
async fn policy_condition_unit_variants_are_bare_strings() {
    let rules = vec![
        PolicyRule {
            name: "prog-check".into(),
            description: "".into(),
            condition: PolicyCondition::ProgramNotInAllowlist,
            action: PolicyAction::Reject { reason: "blocked".into() },
        },
        PolicyRule {
            name: "dest-check".into(),
            description: "".into(),
            condition: PolicyCondition::DestinationInDenylist,
            action: PolicyAction::Reject { reason: "denied".into() },
        },
        PolicyRule {
            name: "sim-check".into(),
            description: "".into(),
            condition: PolicyCondition::SimulationNotPassed,
            action: PolicyAction::Reject { reason: "no sim".into() },
        },
        PolicyRule {
            name: "legacy-check".into(),
            description: "".into(),
            condition: PolicyCondition::LegacyTokenTransferPresent,
            action: PolicyAction::Reject { reason: "legacy".into() },
        },
        PolicyRule {
            name: "catch-all".into(),
            description: "".into(),
            condition: PolicyCondition::Always,
            action: PolicyAction::Approve,
        },
    ];
    let ctx = setup(rules).await;
    let (s, b) = json_get(&ctx.router, "/policy/rules").await;
    assert_eq!(s, StatusCode::OK);

    let items = b["rules"].as_array().expect("rules is array");

    // Unit variants must be bare JSON strings, NOT { "type": "..." } objects.
    // The frontend normalizer (api.ts normalizeCondition) depends on this.
    assert_eq!(items[0]["condition"], json!("ProgramNotInAllowlist"));
    assert_eq!(items[1]["condition"], json!("DestinationInDenylist"));
    assert_eq!(items[2]["condition"], json!("SimulationNotPassed"));
    assert_eq!(items[3]["condition"], json!("LegacyTokenTransferPresent"));
    assert_eq!(items[4]["condition"], json!("Always"));
}

// ── Wire-shape: PolicyCondition tagged variants have correct fields ──────────

#[tokio::test]
async fn policy_condition_tagged_variants_have_correct_field_names() {
    let rules = vec![
        PolicyRule {
            name: "amount".into(), description: "".into(),
            condition: PolicyCondition::AmountExceedsLamports(1_000_000_000),
            action: PolicyAction::Approve,
        },
        PolicyRule {
            name: "cost".into(), description: "".into(),
            condition: PolicyCondition::CostExceedsSol(2.5),
            action: PolicyAction::Approve,
        },
        PolicyRule {
            name: "daily".into(), description: "".into(),
            condition: PolicyCondition::DailySpendExceedsSol(10.0),
            action: PolicyAction::Approve,
        },
        PolicyRule {
            name: "token".into(), description: "".into(),
            condition: PolicyCondition::TokenAmountExceeds {
                mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
                threshold: 1_000_000_000,
            },
            action: PolicyAction::Approve,
        },
        PolicyRule {
            name: "mint-list".into(), description: "".into(),
            condition: PolicyCondition::MintNotInAllowlist {
                allowed_mints: vec!["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into()],
            },
            action: PolicyAction::Approve,
        },
    ];
    let ctx = setup(rules).await;
    let (_, b) = json_get(&ctx.router, "/policy/rules").await;
    let items = b["rules"].as_array().unwrap();

    // AmountExceedsLamports: { type, threshold }
    let c0 = &items[0]["condition"];
    assert_eq!(c0["type"], "AmountExceedsLamports");
    assert_eq!(c0["threshold"], 1_000_000_000u64);

    // CostExceedsSol: { type, threshold } — NOT "sol"!
    let c1 = &items[1]["condition"];
    assert_eq!(c1["type"], "CostExceedsSol");
    assert!(c1["threshold"].is_f64(), "CostExceedsSol.threshold must be a number");
    assert!(c1.get("sol").is_none(), "field is 'threshold', not 'sol'");

    // DailySpendExceedsSol: { type, threshold }
    let c2 = &items[2]["condition"];
    assert_eq!(c2["type"], "DailySpendExceedsSol");
    assert!(c2["threshold"].is_f64());
    assert!(c2.get("sol").is_none());

    // TokenAmountExceeds: { type, mint, threshold }
    let c3 = &items[3]["condition"];
    assert_eq!(c3["type"], "TokenAmountExceeds");
    assert!(c3["mint"].is_string());
    assert!(c3["threshold"].is_number());

    // MintNotInAllowlist: { type, allowed_mints }
    let c4 = &items[4]["condition"];
    assert_eq!(c4["type"], "MintNotInAllowlist");
    assert!(c4["allowed_mints"].is_array());
}

// ── Wire-shape: PolicyAction unit "Approve" is a bare string ────────────────

#[tokio::test]
async fn policy_action_approve_is_bare_string() {
    let ctx = setup(vec![PolicyRule {
        name: "auto".into(), description: "".into(),
        condition: PolicyCondition::Always,
        action: PolicyAction::Approve,
    }]).await;
    let (_, b) = json_get(&ctx.router, "/policy/rules").await;
    assert_eq!(b["rules"][0]["action"], json!("Approve"));
}

// ── Wire-shape: PolicyAction tagged variants have correct fields ────────────

#[tokio::test]
async fn policy_action_tagged_variants_have_correct_fields() {
    let ctx = setup(vec![
        PolicyRule {
            name: "human".into(), description: "".into(),
            condition: PolicyCondition::Always,
            action: PolicyAction::RequireHumanApproval {
                reason: "big transfer".into(),
                required_approver_role: Some("treasury".into()),
            },
        },
        PolicyRule {
            name: "chain".into(), description: "".into(),
            condition: PolicyCondition::Always,
            action: PolicyAction::RequireApprovalChain {
                reason: "multi-stage".into(),
                stages: vec![
                    ApprovalChainStage { role: "risk".into(), description: "risk review".into(), min_approvals: 1 },
                    ApprovalChainStage { role: "treasury".into(), description: "treasury sign".into(), min_approvals: 2 },
                ],
            },
        },
        PolicyRule {
            name: "block".into(), description: "".into(),
            condition: PolicyCondition::Always,
            action: PolicyAction::Reject { reason: "forbidden".into() },
        },
    ]).await;
    let (_, b) = json_get(&ctx.router, "/policy/rules").await;
    let items = b["rules"].as_array().unwrap();

    // RequireHumanApproval: { type, reason, required_approver_role }
    let a0 = &items[0]["action"];
    assert_eq!(a0["type"], "RequireHumanApproval");
    assert_eq!(a0["reason"], "big transfer");
    assert_eq!(a0["required_approver_role"], "treasury");

    // RequireApprovalChain: { type, reason, stages: [{ role, description, min_approvals }] }
    let a1 = &items[1]["action"];
    assert_eq!(a1["type"], "RequireApprovalChain");
    let stages = a1["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 2);
    assert_eq!(stages[0]["role"], "risk");
    assert_eq!(stages[1]["min_approvals"], 2);

    // Reject: { type, reason }
    let a2 = &items[2]["action"];
    assert_eq!(a2["type"], "Reject");
    assert_eq!(a2["reason"], "forbidden");
}

// ── Wire-shape: GET /approvals/:id returns request.id and workflow ───────────

#[tokio::test]
async fn get_approval_by_id_returns_request_id_and_workflow() {
    let ctx = setup(vec![]).await;

    let request = ApprovalRequest::new(
        ctx.sid.clone(), Uuid::new_v4(), "test proposal",
        PolicyVerdict::RequiresHumanApproval {
            reason: "test".into(), rule_name: "test-rule".into(),
            required_approver_role: None, approval_chain: None,
        },
        fake_sim(),
    );
    let rid = request.id;
    let wf = ApprovalWorkflow::single_stage(rid, ctx.sid.clone(), None);
    ctx.store.register(request, wf);

    let (s, b) = json_get(&ctx.router, &format!("/approvals/{rid}")).await;
    assert_eq!(s, StatusCode::OK);

    // request.id must match the URL parameter (the frontend links by this).
    assert_eq!(b["request"]["id"], rid.to_string());
    // transaction_id is a different field — must not be confused with id.
    assert!(b["request"]["transaction_id"].is_string());
    assert_ne!(b["request"]["id"], b["request"]["transaction_id"]);
    // Workflow state and stages present.
    assert_eq!(b["workflow"]["state"], "pending");
    assert!(b["workflow"]["stages"].is_array());
}

// ── Wire-shape: GET /approvals/:id returns 404 for unknown UUID ─────────────

#[tokio::test]
async fn get_approval_returns_404_for_unknown_id() {
    let ctx = setup(vec![]).await;
    let (s, _) = json_get(&ctx.router, &format!("/approvals/{}", Uuid::new_v4())).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ── Wire-shape: GET /approvals/:id returns 400 for non-UUID ─────────────────

#[tokio::test]
async fn get_approval_returns_400_for_bad_id() {
    let ctx = setup(vec![]).await;
    let (s, _) = json_get(&ctx.router, "/approvals/not-a-uuid").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

//! N23 — Operator authorization runtime verification.
//!
//! Proves that when an operator's registry-assigned roles do NOT include
//! the role claimed in the POST body, the approve endpoint rejects with 403
//! and the `role_not_held` error code.
//!
//! Uses axum::Router + tower::ServiceExt::oneshot — no TCP listener needed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower::ServiceExt;
use uuid::Uuid;

use claw_api::{
    AppState, ApprovalHandler, ApprovalHandlerRef, AuthToken,
    EventSubscriber, EventSubscriberRef,
    MessageHandler, MessageHandlerRef, SessionManagerRef, SessionOps,
    WalletChallengeHandler, WalletChallengeHandlerRef, WalletChallengeInfo,
    WalletSignatureHandler, WalletSignatureHandlerRef, WalletSignatureOutcome,
    PendingWalletSignatureInfo,
    auth::OperatorRegistry,
    state::{AuditReaderRef, PolicyReaderRef, WalletDirectoryRef},
};
use claw_observability::HealthRegistry;
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::GatewayEvent,
    operator::{OperatorId, OperatorIdentity},
    policy::PolicyRule,
    session::SessionId,
};

// ── Stub implementations ─────────────────────────────────────────────────────

struct StubSessionOps {
    active: SessionId,
}
impl SessionOps for StubSessionOps {
    fn open(&self, _: AgentRole, _: String, _: Option<Vec<PolicyRule>>) -> SessionId {
        self.active.clone()
    }
    fn close(&self, _: &SessionId, _: &str) {}
    fn active_count(&self) -> usize { 1 }
    fn is_active(&self, id: &SessionId) -> bool { id == &self.active }
}

struct StubMsg;
impl MessageHandler for StubMsg {
    fn handle<'a>(&'a self, _: &'a SessionId, _: AgentCommand)
        -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>>
    {
        Box::pin(async { Err("stub".into()) })
    }
}

/// Stub approval handler that returns matching session for any request.
struct StubApproval {
    session_id: SessionId,
}
impl ApprovalHandler for StubApproval {
    fn pending_for_session(&self, _: &SessionId) -> Vec<ApprovalRequest> { vec![] }
    fn session_for_request(&self, _: Uuid) -> Option<SessionId> {
        Some(self.session_id.clone())
    }
    fn decide(&self, _: ApprovalDecision)
        -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>>
    {
        Box::pin(async { (ApprovalOutcome::NotFound, None) })
    }
}

struct StubEvents;
impl EventSubscriber for StubEvents {
    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        broadcast::channel(1).1
    }
}

struct StubWalletSig;
impl WalletSignatureHandler for StubWalletSig {
    fn submit_signed_tx(&self, _: &SessionId, _: Uuid, _: String)
        -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>>
    {
        Box::pin(async {
            WalletSignatureOutcome {
                request_id: Uuid::nil(), accepted: false, signature: None,
                tx_signature: None, submitted: false, error: Some("stub".into()),
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
    {
        Box::pin(async { Err("stub".into()) })
    }
    fn verify_and_bind(&self, _: &SessionId, _: &str, _: &str, _: &str)
        -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(async { Err("stub".into()) })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn self_claimed_higher_role_is_rejected() {
    let sid = SessionId::from(Uuid::new_v4());
    let request_id = Uuid::new_v4();

    // Operator registry: "risk-token" maps to an operator with roles=["risk"]
    let registry = OperatorRegistry::from_operators(vec![(
        "risk-token".to_string(),
        OperatorIdentity {
            id: OperatorId("risk-operator".to_string()),
            display_name: "Risk Operator".to_string(),
            roles: vec!["risk".to_string()],
        },
    )]);

    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(StubSessionOps { active: sid.clone() })),
        message_handler: MessageHandlerRef::new(Arc::new(StubMsg)),
        approval: ApprovalHandlerRef::new(Arc::new(StubApproval { session_id: sid.clone() })),
        events: EventSubscriberRef::new(Arc::new(StubEvents)),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token: AuthToken::new("fallback-token"),
        operator_registry: registry,
        metrics,
        propose: None,
        rate_limiter: None,
        policy: PolicyReaderRef::noop(),
        audit: AuditReaderRef::noop(),
        wallets: WalletDirectoryRef::noop(),
        demo_seeder: None,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());

    // POST /sessions/:id/approve with Bearer risk-token but claiming approver_role "treasury"
    let body = json!({
        "request_id": request_id,
        "approved": true,
        "approver_role": "treasury"
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/approve", sid))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer risk-token")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap();

    assert_eq!(status.as_u16(), 403, "expected 403, got {}: {:?}", status, json);
    assert_eq!(json["error"], "role_not_held", "response body: {:?}", json);
    assert_eq!(json["claimed_role"], "treasury");
    assert!(
        json["operator_roles"].as_array().unwrap().contains(&Value::String("risk".to_string())),
        "should list operator's actual roles"
    );
}

#[tokio::test]
async fn operator_with_correct_role_passes_role_check() {
    // Verify that when the operator holds the claimed role, the request
    // proceeds past the role check (may fail later at decide(), but
    // the point is it doesn't get 403 role_not_held).
    let sid = SessionId::from(Uuid::new_v4());
    let request_id = Uuid::new_v4();

    let registry = OperatorRegistry::from_operators(vec![(
        "treasury-token".to_string(),
        OperatorIdentity {
            id: OperatorId("treasury-op".to_string()),
            display_name: "Treasury Operator".to_string(),
            roles: vec!["treasury".to_string()],
        },
    )]);

    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(StubSessionOps { active: sid.clone() })),
        message_handler: MessageHandlerRef::new(Arc::new(StubMsg)),
        approval: ApprovalHandlerRef::new(Arc::new(StubApproval { session_id: sid.clone() })),
        events: EventSubscriberRef::new(Arc::new(StubEvents)),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token: AuthToken::new("fallback-token"),
        operator_registry: registry,
        metrics,
        propose: None,
        rate_limiter: None,
        policy: PolicyReaderRef::noop(),
        audit: AuditReaderRef::noop(),
        wallets: WalletDirectoryRef::noop(),
        demo_seeder: None,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());

    let body = json!({
        "request_id": request_id,
        "approved": true,
        "approver_role": "treasury"
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/approve", sid))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer treasury-token")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();

    // Should NOT be 403 role_not_held. It will be 404 not_found because
    // the stub ApprovalHandler.decide() returns NotFound, but that's fine —
    // the point is we passed the role check.
    assert_ne!(status.as_u16(), 403, "should not be 403 when role matches");
}

/// Capturing approval handler that records the last decision it receives.
struct CapturingApprovalHandler {
    last_decision: Arc<std::sync::Mutex<Option<claw_types::approval::ApprovalDecision>>>,
    session_id: SessionId,
}

impl ApprovalHandler for CapturingApprovalHandler {
    fn pending_for_session(&self, _: &SessionId) -> Vec<claw_types::approval::ApprovalRequest> {
        vec![]
    }
    fn session_for_request(&self, _: Uuid) -> Option<SessionId> {
        Some(self.session_id.clone())
    }
    fn decide(
        &self,
        decision: claw_types::approval::ApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<claw_types::approval::ApprovalRequest>)> + Send + '_>>
    {
        let captured = self.last_decision.clone();
        Box::pin(async move {
            *captured.lock().unwrap() = Some(decision);
            // Return Approved with a fake request so the route handler succeeds.
            let fake_req = claw_types::approval::ApprovalRequest::new(
                SessionId::from(Uuid::new_v4()),
                Uuid::new_v4(),
                "fake",
                claw_types::policy::PolicyVerdict::RequiresHumanApproval {
                    reason: "test".into(),
                    rule_name: "test".into(),
                    required_approver_role: Some("treasury".to_string()),
                    approval_chain: None,
                },
                claw_types::transaction::SimulationResult {
                    success: true,
                    error: None,
                    compute_units_used: Some(1000),
                    logs: vec![],
                    return_data: None,
                    account_diffs: vec![],
                    fee_lamports: Some(5000),
                },
            );
            (ApprovalOutcome::Approved, Some(fake_req))
        })
    }
}

#[tokio::test]
async fn token_mapped_role_can_approve_required_role_request() {
    // When no approver_role is in the body, the route handler should auto-fill
    // the operator's first registered role from the registry. Verify that the
    // CapturingApprovalHandler receives the auto-filled role.
    let sid = SessionId::from(Uuid::new_v4());
    let request_id = Uuid::new_v4();
    let captured = Arc::new(std::sync::Mutex::new(None));

    let registry = claw_api::auth::OperatorRegistry::from_operators(vec![(
        "treasury-token".to_string(),
        claw_types::operator::OperatorIdentity {
            id: claw_types::operator::OperatorId("treasury-op".to_string()),
            display_name: "Treasury Operator".to_string(),
            roles: vec!["treasury".to_string()],
        },
    )]);

    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());

    let handler = CapturingApprovalHandler {
        last_decision: captured.clone(),
        session_id: sid.clone(),
    };

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(StubSessionOps { active: sid.clone() })),
        message_handler: MessageHandlerRef::new(Arc::new(StubMsg)),
        approval: ApprovalHandlerRef::new(Arc::new(handler)),
        events: EventSubscriberRef::new(Arc::new(StubEvents)),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token: AuthToken::new("fallback-token"),
        operator_registry: registry,
        metrics,
        propose: None,
        rate_limiter: None,
        policy: PolicyReaderRef::noop(),
        audit: AuditReaderRef::noop(),
        wallets: WalletDirectoryRef::noop(),
        demo_seeder: None,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());

    // POST with Bearer treasury-token but NO approver_role in body
    let body = json!({
        "request_id": request_id,
        "approved": true
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/approve", sid))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer treasury-token")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();

    assert_eq!(status.as_u16(), 202, "expected 202 Accepted, got {}", status);

    // Verify the captured decision has the auto-filled role
    let decision = captured.lock().unwrap();
    let decision = decision.as_ref().expect("handler should have been called");
    assert_eq!(
        decision.approver_role,
        Some("treasury".to_string()),
        "route handler should auto-fill the operator's registered role when body omits it"
    );
}

#[tokio::test]
async fn multi_role_operator_uses_first_role_when_body_omits_role() {
    // Operator with roles=["risk", "treasury", "admin"] sends a POST with
    // no approver_role in the body. The route handler should auto-fill with
    // the FIRST role ("risk"). Verify via CapturingApprovalHandler.
    let sid = SessionId::from(Uuid::new_v4());
    let request_id = Uuid::new_v4();
    let captured = Arc::new(std::sync::Mutex::new(None));

    let registry = claw_api::auth::OperatorRegistry::from_operators(vec![(
        "multi-token".to_string(),
        claw_types::operator::OperatorIdentity {
            id: claw_types::operator::OperatorId("multi-op".to_string()),
            display_name: "Multi-Role Operator".to_string(),
            roles: vec![
                "risk".to_string(),
                "treasury".to_string(),
                "admin".to_string(),
            ],
        },
    )]);

    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());

    let handler = CapturingApprovalHandler {
        last_decision: captured.clone(),
        session_id: sid.clone(),
    };

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(StubSessionOps { active: sid.clone() })),
        message_handler: MessageHandlerRef::new(Arc::new(StubMsg)),
        approval: ApprovalHandlerRef::new(Arc::new(handler)),
        events: EventSubscriberRef::new(Arc::new(StubEvents)),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token: AuthToken::new("fallback-token"),
        operator_registry: registry,
        metrics,
        propose: None,
        rate_limiter: None,
        policy: PolicyReaderRef::noop(),
        audit: AuditReaderRef::noop(),
        wallets: WalletDirectoryRef::noop(),
        demo_seeder: None,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());

    // POST with Bearer multi-token but NO approver_role in body.
    let body = json!({
        "request_id": request_id,
        "approved": true
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/approve", sid))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer multi-token")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();

    assert_eq!(status.as_u16(), 202, "expected 202 Accepted, got {}", status);

    // Verify the captured decision has the FIRST role from the multi-role list.
    let decision = captured.lock().unwrap();
    let decision = decision.as_ref().expect("handler should have been called");
    assert_eq!(
        decision.approver_role,
        Some("risk".to_string()),
        "should auto-fill with the FIRST role ('risk') from the multi-role operator"
    );
    assert_eq!(
        decision.operator_id,
        Some("multi-op".to_string()),
        "operator_id should be set from the registry identity"
    );
}

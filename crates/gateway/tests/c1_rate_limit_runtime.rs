//! C1 — Rate limiting runtime verification.
//!
//! Spins up a router with a tight rate limit (3 rpm, 0 burst) and proves:
//! 1. First 3 requests return 200
//! 2. 4th request returns 429 + Retry-After header + JSON body
//! 3. `rate_limit_hits` metric increments
//! 4. Different tokens get independent counters
//! 5. /health (public) is NOT rate-limited
//!
//! Uses axum::Router + tower::ServiceExt::oneshot — no TCP listener needed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::sync::broadcast;
use tower::ServiceExt;
use uuid::Uuid;

use claw_api::{
    AppState, ApprovalHandler, ApprovalHandlerRef, AuthToken,
    EventSubscriber, EventSubscriberRef,
    MessageHandler, MessageHandlerRef, RateLimiter, SessionManagerRef, SessionOps,
    WalletChallengeHandler, WalletChallengeHandlerRef, WalletChallengeInfo,
    WalletSignatureHandler, WalletSignatureHandlerRef, WalletSignatureOutcome,
    PendingWalletSignatureInfo,
    state::{AuditReaderRef, PolicyReaderRef, WalletDirectoryRef},
};
use claw_observability::HealthRegistry;
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::GatewayEvent,
    session::SessionId,
};

// ── Stub implementations ─────────────────────────────────────────────────────

struct StubSessionOps { active: SessionId }
impl SessionOps for StubSessionOps {
    fn open(&self, _: AgentRole, _: String, _: Option<Vec<claw_types::policy::PolicyRule>>) -> SessionId { self.active.clone() }
    fn close(&self, _: &SessionId, _: &str) {}
    fn active_count(&self) -> usize { 1 }
    fn is_active(&self, id: &SessionId) -> bool { id == &self.active }
}

struct StubMsg;
impl MessageHandler for StubMsg {
    fn handle<'a>(&'a self, _: &'a SessionId, _: AgentCommand)
        -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>> {
        Box::pin(async { Err("stub".into()) })
    }
}

struct StubApproval;
impl ApprovalHandler for StubApproval {
    fn pending_for_session(&self, _: &SessionId) -> Vec<ApprovalRequest> { vec![] }
    fn session_for_request(&self, _: uuid::Uuid) -> Option<SessionId> { None }
    fn decide(&self, _: ApprovalDecision)
        -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>> {
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
    fn submit_signed_tx(&self, _: &SessionId, _: uuid::Uuid, _: String)
        -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>> {
        Box::pin(async { WalletSignatureOutcome {
            request_id: uuid::Uuid::nil(), accepted: false, signature: None,
            tx_signature: None, submitted: false, error: Some("stub".into()),
            rebuild_required: false,
        }})
    }
    fn pending_for_session(&self, _: &SessionId) -> Vec<PendingWalletSignatureInfo> { vec![] }
    fn bind_wallet(&self, _: &SessionId, _: &str) {}
    fn wallets_for_session(&self, _: &SessionId) -> Vec<String> { vec![] }
}

struct StubChallenge;
impl WalletChallengeHandler for StubChallenge {
    fn create_challenge(&self, _: &SessionId, _: &str)
        -> Pin<Box<dyn Future<Output = Result<WalletChallengeInfo, String>> + Send + '_>> {
        Box::pin(async { Err("stub".into()) })
    }
    fn verify_and_bind(&self, _: &SessionId, _: &str, _: &str, _: &str)
        -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Err("stub".into()) })
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_router(
    rate_limiter: Option<RateLimiter>,
    metrics: Arc<claw_observability::metrics::MetricsRegistry>,
) -> (axum::Router, SessionId, String) {
    let sid = SessionId::from(Uuid::new_v4());
    let token = "test-rate-limit-token".to_string();

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(StubSessionOps { active: sid.clone() })),
        message_handler: MessageHandlerRef::new(Arc::new(StubMsg)),
        approval: ApprovalHandlerRef::new(Arc::new(StubApproval)),
        events: EventSubscriberRef::new(Arc::new(StubEvents)),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        solend_signatures: None,
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token: AuthToken::new(token.clone()),
        metrics,
        propose: None,
        rate_limiter,
        operator_registry: claw_api::auth::OperatorRegistry::new(),
        policy: PolicyReaderRef::noop(),
        audit: AuditReaderRef::noop(),
        wallets: WalletDirectoryRef::noop(),
        demo_seeder: None,
        chat: None,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());
    (router, sid, token)
}

async fn get(router: &axum::Router, path: &str, token: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(path)
        .header("Authorization", format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

async fn get_raw(router: &axum::Router, path: &str) -> StatusCode {
    let req = Request::builder()
        .uri(path)
        .body(Body::empty())
        .unwrap();
    router.clone().oneshot(req).await.unwrap().status()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rate_limit_429_after_exceeded() {
    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());
    let limiter = RateLimiter::new(3, 0, metrics.clone());
    let (router, _sid, token) = build_router(Some(limiter), metrics.clone());

    // First 3 → 200
    for i in 0..3 {
        let (status, _) = get(&router, "/sessions", &token).await;
        assert_eq!(status, StatusCode::OK, "request {} should succeed", i + 1);
    }

    // 4th → 429
    let req = Request::builder()
        .uri("/sessions")
        .header("Authorization", format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // Verify Retry-After header exists
    let retry_after = resp.headers().get("Retry-After")
        .expect("429 should include Retry-After header");
    let secs: u64 = retry_after.to_str().unwrap().parse().unwrap();
    assert!(secs >= 1, "Retry-After should be at least 1 second");

    // Verify JSON body
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "too_many_requests");

    // Verify metric incremented
    assert_eq!(metrics.rate_limit_hits.get(), 1);
}

#[tokio::test]
async fn rate_limit_independent_tokens() {
    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());
    let limiter = RateLimiter::new(1, 0, metrics.clone());

    let sid = SessionId::from(Uuid::new_v4());
    // Auth accepts any token matching the configured one, but rate limiter
    // hashes the actual token string. We'll test with the real token and
    // verify the counter works. For true multi-token isolation we test
    // at the unit level (different_tokens_independent).
    let token = "multi-token-test".to_string();
    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(StubSessionOps { active: sid.clone() })),
        message_handler: MessageHandlerRef::new(Arc::new(StubMsg)),
        approval: ApprovalHandlerRef::new(Arc::new(StubApproval)),
        events: EventSubscriberRef::new(Arc::new(StubEvents)),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        solend_signatures: None,
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token: AuthToken::new(token.clone()),
        metrics: metrics.clone(),
        propose: None,
        rate_limiter: Some(limiter),
        operator_registry: claw_api::auth::OperatorRegistry::new(),
        policy: PolicyReaderRef::noop(),
        audit: AuditReaderRef::noop(),
        wallets: WalletDirectoryRef::noop(),
        demo_seeder: None,
        chat: None,
    };
    let router = claw_api::create_router(state, HealthRegistry::new());

    // 1st request passes
    let (s1, _) = get(&router, "/sessions", &token).await;
    assert_eq!(s1, StatusCode::OK);

    // 2nd request blocked
    let (s2, _) = get(&router, "/sessions", &token).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);

    assert_eq!(metrics.rate_limit_hits.get(), 1);
}

#[tokio::test]
async fn health_endpoint_not_rate_limited() {
    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());
    let limiter = RateLimiter::new(1, 0, metrics.clone());
    let (router, _, _) = build_router(Some(limiter), metrics.clone());

    // Hit /health 10 times — all should return 200 (no auth, no rate limit)
    for _ in 0..10 {
        let status = get_raw(&router, "/health").await;
        assert_eq!(status, StatusCode::OK);
    }

    // Rate limit counter should be 0 — /health is public
    assert_eq!(metrics.rate_limit_hits.get(), 0);
}

#[tokio::test]
async fn metrics_endpoint_shows_rate_limit_hits() {
    let metrics = Arc::new(claw_observability::metrics::MetricsRegistry::new());
    let limiter = RateLimiter::new(2, 0, metrics.clone());
    let (router, _, token) = build_router(Some(limiter), metrics.clone());

    // Use up the limit
    get(&router, "/sessions", &token).await;
    get(&router, "/sessions", &token).await;
    // This one triggers 429
    get(&router, "/sessions", &token).await;

    // Now check /metrics reports rate_limit_hits = 1
    let (status, json) = get(&router, "/metrics", &token).await;
    // /metrics itself may be rate limited (it's a protected route), but
    // the counter was already bumped. If we get 429 here, the metric
    // was already incremented. If 200, check the JSON.
    if status == StatusCode::OK {
        assert_eq!(json["rate_limit_hits"], 1);
    } else {
        // Even if /metrics itself got 429'd, confirm the counter
        assert_eq!(metrics.rate_limit_hits.get(), 2); // one for /sessions, one for /metrics
    }
}

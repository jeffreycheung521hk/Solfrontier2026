//! Phase 2 — Operator / API black-box suite.
//!
//! Drives the real `axum::Router` produced by `claw_api::create_router` and
//! asserts response codes + body shape for every dashboard read surface,
//! plus the operator decision flow (role_mismatch / role_not_held /
//! duplicate / expired) and the demo-seed / metrics / health gates.
//!
//! Nothing here touches a real daemon binary or external RPC — all state
//! lives in in-memory SQLite + in-memory stores constructed per test.

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
        DemoSeeder, DemoSeederRef, DemoSeedReport,
        PolicyReader, PolicyReaderRef,
        WalletDirectory, WalletDirectoryRef, WalletSummaryDto,
    },
};
use claw_gateway::ApprovalStore;
use claw_observability::HealthRegistry;
use claw_state_store::{audit::AuditSeverity, AuditRepository, Database, SpendRepository};
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

// ── Shared helpers ──────────────────────────────────────────────────────────

const TOKEN: &str = "p2-token";

fn fake_simulation() -> SimulationResult {
    SimulationResult {
        success: true,
        error: None,
        compute_units_used: Some(1000),
        logs: vec![],
        return_data: None,
        account_diffs: vec![],
        fee_lamports: Some(5000),
    }
}

// ── Test-local adapter impls ────────────────────────────────────────────────

struct StubSession {
    sid: SessionId,
}
impl SessionOps for StubSession {
    fn open(&self, _: AgentRole, _: String, _: Option<Vec<PolicyRule>>) -> SessionId { self.sid.clone() }
    fn close(&self, _: &SessionId, _: &str) {}
    fn active_count(&self) -> usize { 1 }
    fn is_active(&self, id: &SessionId) -> bool { id == &self.sid }
}

struct StubMsg;
impl MessageHandler for StubMsg {
    fn handle<'a>(
        &'a self, _s: &'a SessionId, _c: AgentCommand,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>> {
        Box::pin(async { Err("stub".into()) })
    }
}

/// Approval handler backed by a live `ApprovalStore` so tests can register
/// workflows up-front and inspect what `decide()` did after HTTP calls.
struct StoreBackedApproval {
    store: ApprovalStore,
}
impl ApprovalHandler for StoreBackedApproval {
    fn pending_for_session(&self, sid: &SessionId) -> Vec<ApprovalRequest> {
        self.store.pending_for_session(sid)
    }
    fn all_pending(&self) -> Vec<claw_api::state::PendingApprovalItem> {
        self.store
            .all_pending_with_workflow()
            .into_iter()
            .map(|(request, workflow)| claw_api::state::PendingApprovalItem { request, workflow })
            .collect()
    }
    fn session_for_request(&self, rid: Uuid) -> Option<SessionId> {
        self.store.session_for_request(rid)
    }
    fn decide(
        &self,
        d: claw_types::approval::ApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = (claw_types::approval::ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>> {
        let (o, r) = self.store.decide(&d);
        Box::pin(async move { (o, r) })
    }
}

struct StubEvents {
    tx: broadcast::Sender<GatewayEvent>,
}
impl EventSubscriber for StubEvents {
    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> { self.tx.subscribe() }
}

struct StubWalletSig;
impl WalletSignatureHandler for StubWalletSig {
    fn submit_signed_tx(
        &self, _s: &SessionId, rid: Uuid, _b: String,
    ) -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>> {
        Box::pin(async move {
            WalletSignatureOutcome { request_id: rid, accepted: false, signature: None, tx_signature: None, submitted: false, error: Some("stub".into()) }
        })
    }
    fn pending_for_session(&self, _s: &SessionId) -> Vec<PendingWalletSignatureInfo> { vec![] }
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

/// Vector-backed policy reader whose rule order exactly reflects what the
/// test provided — used to assert `/policy/rules` preserves config order.
struct VecPolicyReader {
    rules: Vec<PolicyRule>,
}
impl PolicyReader for VecPolicyReader {
    fn rules(&self) -> Vec<PolicyRule> { self.rules.clone() }
}

/// Audit reader backed by a real SQLite `AuditRepository` — exercises
/// pagination (limit + offset) through the route layer.
struct RepoAuditReader {
    audit: AuditRepository,
}
impl AuditReader for RepoAuditReader {
    fn list(
        &self, limit: i64, offset: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<AuditRowDto>, String>> + Send + '_>> {
        Box::pin(async move {
            let rows = self.audit.list_all(limit, offset).await.map_err(|e| e.to_string())?;
            Ok(rows.into_iter().map(|r| AuditRowDto {
                id:             r.id,
                session_id:     r.session_id,
                correlation_id: r.correlation_id,
                occurred_at:    r.occurred_at,
                event_type:     r.event_type,
                actor:          r.actor,
                payload:        r.payload,
                severity:       r.severity,
            }).collect())
        })
    }
}

/// Wallet directory backed by the real `SpendRepository` so daily spend
/// gets queried out of SQLite, not hardcoded in the fixture.
struct RepoWalletDirectory {
    spend_repo: SpendRepository,
    entries:    Vec<(String, String)>, // (label, pubkey)
}
impl WalletDirectory for RepoWalletDirectory {
    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<WalletSummaryDto>> + Send + '_>> {
        Box::pin(async move {
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let mut out = Vec::new();
            for (label, pubkey) in &self.entries {
                let spend = self.spend_repo.wallet_daily_spend(pubkey, &today).await.unwrap_or(0);
                out.push(WalletSummaryDto {
                    pubkey:               pubkey.clone(),
                    label:                label.clone(),
                    signer_type:          claw_types::wallet::SignerType::LocalKeypair,
                    daily_spend_lamports: spend,
                    policy:               None,
                });
            }
            out
        })
    }
}

/// Minimal seeder that registers a single-stage ApprovalRequest per call
/// and appends one audit row + one spend row. Matches the production gate
/// behavior: each call appends, i.e. not strict idempotent.
struct AccumulativeSeeder {
    approval_store: ApprovalStore,
    audit:          AuditRepository,
    spend_repo:     SpendRepository,
    session_id:     SessionId,
    wallet_pubkey:  String,
}
impl DemoSeeder for AccumulativeSeeder {
    fn seed(&self) -> Pin<Box<dyn Future<Output = Result<DemoSeedReport, String>> + Send + '_>> {
        Box::pin(async move {
            let request = ApprovalRequest::new(
                self.session_id.clone(),
                Uuid::new_v4(),
                "seed-demo test entry",
                PolicyVerdict::RequiresHumanApproval {
                    reason: "seed".into(),
                    rule_name: "large-transfer-requires-human".into(),
                    required_approver_role: None,
                    approval_chain: None,
                },
                fake_simulation(),
            );
            let rid = request.id;
            let workflow = ApprovalWorkflow::single_stage(rid, self.session_id.clone(), None);
            self.approval_store.register(request, workflow);

            self.audit.append(
                Some(&self.session_id.to_string()),
                &rid.to_string(),
                "human_approved",
                "op_demo",
                &json!({"rule": "seed-demo"}),
                AuditSeverity::Info,
            ).await.map_err(|e| e.to_string())?;

            let sid_s = self.session_id.to_string();
            self.spend_repo.record_spend(&self.wallet_pubkey, &sid_s, &Uuid::new_v4().to_string(), 1_000_000_000)
                .await.map_err(|e| e.to_string())?;

            Ok(DemoSeedReport { approvals_created: 1, audit_rows_written: 1, wallets_spend_bumped: 1 })
        })
    }
}

// ── Harness builder ─────────────────────────────────────────────────────────

struct Harness {
    router:         axum::Router,
    approval_store: ApprovalStore,
    audit:          AuditRepository,
    spend_repo:     SpendRepository,
    session_id:     SessionId,
    _db:            Database, // keeps SQLite alive for the test lifetime
}

#[derive(Default)]
struct HarnessOptions {
    policy_rules:    Vec<PolicyRule>,
    wallets:         Vec<(String, String)>,
    seeder_enabled:  bool,
    wallet_pubkey:   Option<String>,
    operator_roles:  Vec<(String, Vec<String>)>, // (token, roles)
}

async fn build_harness(opts: HarnessOptions) -> Harness {
    let db         = Database::open_in_memory().await.expect("in-memory DB");
    let audit      = AuditRepository::new(db.pool().clone());
    let spend_repo = SpendRepository::new(db.pool().clone());
    let store      = ApprovalStore::new();
    let sid        = SessionId::from(Uuid::new_v4());

    let policy = Arc::new(VecPolicyReader { rules: opts.policy_rules });
    let audit_r = Arc::new(RepoAuditReader { audit: audit.clone() });
    let wallet_d = Arc::new(RepoWalletDirectory {
        spend_repo: spend_repo.clone(),
        entries:    opts.wallets,
    });

    let demo_seeder: Option<DemoSeederRef> = if opts.seeder_enabled {
        let wallet_pk = opts.wallet_pubkey.clone().unwrap_or_else(|| "seed-wallet".into());
        Some(DemoSeederRef::new(Arc::new(AccumulativeSeeder {
            approval_store: store.clone(),
            audit:          audit.clone(),
            spend_repo:     spend_repo.clone(),
            session_id:     sid.clone(),
            wallet_pubkey:  wallet_pk,
        })))
    } else { None };

    let operator_registry = if opts.operator_roles.is_empty() {
        OperatorRegistry::new()
    } else {
        let ops: Vec<(String, claw_types::operator::OperatorIdentity)> = opts
            .operator_roles
            .iter()
            .map(|(token, roles)| {
                (
                    token.clone(),
                    claw_types::operator::OperatorIdentity {
                        id:           claw_types::operator::OperatorId(token.clone()),
                        display_name: token.clone(),
                        roles:        roles.clone(),
                    },
                )
            })
            .collect();
        OperatorRegistry::from_operators(ops)
    };

    let (tx, _rx) = broadcast::channel::<GatewayEvent>(16);

    let state = AppState {
        session_mgr:       SessionManagerRef::new(Arc::new(StubSession { sid: sid.clone() })),
        message_handler:   MessageHandlerRef::new(Arc::new(StubMsg)),
        approval:          ApprovalHandlerRef::new(Arc::new(StoreBackedApproval { store: store.clone() })),
        events:            EventSubscriberRef::new(Arc::new(StubEvents { tx })),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token:        AuthToken::new(TOKEN),
        operator_registry,
        metrics:           Arc::new(claw_observability::metrics::MetricsRegistry::new()),
        propose:           None::<TransactionProposerRef>,
        rate_limiter:      None,
        policy:            PolicyReaderRef::new(policy),
        audit:             AuditReaderRef::new(audit_r),
        wallets:           WalletDirectoryRef::new(wallet_d),
        demo_seeder,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());

    Harness { router, approval_store: store, audit, spend_repo, session_id: sid, _db: db }
}

async fn call_json(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp   = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body   = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn get(path: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

fn post_json(path: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn sample_rule(name: &str, threshold: u64) -> PolicyRule {
    PolicyRule {
        name: name.into(),
        description: format!("rule {name}"),
        condition: PolicyCondition::AmountExceedsLamports(threshold),
        action: PolicyAction::RequireHumanApproval {
            reason: format!("{name} fired"),
            required_approver_role: None,
        },
    }
}

// ── Case 1: GET /policy/rules preserves config order ────────────────────────

#[tokio::test]
async fn case1_policy_rules_preserve_config_order() {
    let rules = vec![
        sample_rule("a-first", 10_000_000),
        sample_rule("b-second", 50_000_000),
        sample_rule("c-third", 100_000_000),
    ];
    let h = build_harness(HarnessOptions { policy_rules: rules.clone(), ..Default::default() }).await;

    let (s, b) = call_json(&h.router, get("/policy/rules", Some(TOKEN))).await;
    assert_eq!(s, StatusCode::OK);
    let names: Vec<&str> = b["rules"].as_array().unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a-first", "b-second", "c-third"], "rule order must match input");
}

// ── Case 2: GET /pending-approvals surfaces cross-session workflow ───────────

#[tokio::test]
async fn case2_pending_approvals_are_cross_session_and_include_workflow() {
    let h = build_harness(HarnessOptions::default()).await;

    // Two approvals from two different sessions — neither equal to harness sid.
    let session_a = SessionId::from(Uuid::new_v4());
    let session_b = SessionId::from(Uuid::new_v4());
    let chain = vec![
        ApprovalChainStage { role: "risk".into(), description: "".into(), min_approvals: 1 },
        ApprovalChainStage { role: "treasury".into(), description: "".into(), min_approvals: 1 },
    ];
    let req_a = ApprovalRequest::new(
        session_a.clone(), Uuid::new_v4(), "session A",
        PolicyVerdict::RequiresHumanApproval {
            reason: "chain".into(), rule_name: "chain-rule".into(),
            required_approver_role: Some("risk".into()),
            approval_chain: Some(chain.clone()),
        },
        fake_simulation(),
    );
    let req_b = ApprovalRequest::new(
        session_b.clone(), Uuid::new_v4(), "session B",
        PolicyVerdict::RequiresHumanApproval {
            reason: "single".into(), rule_name: "single-rule".into(),
            required_approver_role: None,
            approval_chain: None,
        },
        fake_simulation(),
    );
    let rid_a = req_a.id;
    let rid_b = req_b.id;
    let wf_a = ApprovalWorkflow {
        request_id: rid_a, session_id: session_a,
        state: ApprovalWorkflowState::Pending,
        stages: chain.iter().enumerate().map(|(i, s)| ApprovalStage {
            index: i, allowed_roles: vec![s.role.clone()], min_approvals: s.min_approvals, decisions: vec![],
        }).collect(),
        created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
        expires_at: None,
    };
    let wf_b = ApprovalWorkflow::single_stage(rid_b, session_b, None);
    h.approval_store.register(req_a, wf_a);
    h.approval_store.register(req_b, wf_b);

    let (s, b) = call_json(&h.router, get("/pending-approvals", Some(TOKEN))).await;
    assert_eq!(s, StatusCode::OK);
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "must surface pending across both sessions");

    let ids: Vec<&str> = items.iter().map(|i| i["request"]["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&rid_a.to_string().as_str()));
    assert!(ids.contains(&rid_b.to_string().as_str()));

    // Workflow must be present and carry stages.
    for it in items {
        assert!(it["workflow"]["stages"].is_array());
        assert_eq!(it["workflow"]["state"].as_str(), Some("pending"));
    }
}

// ── Case 3: GET /wallets with daily_spend from SpendRepository ──────────────

#[tokio::test]
async fn case3_wallets_include_daily_spend() {
    let pk = "wallet-aaa";
    let h = build_harness(HarnessOptions {
        wallets: vec![("hot".into(), pk.into())],
        ..Default::default()
    }).await;

    // Pre-seed spend.
    let sid_s = h.session_id.to_string();
    h.spend_repo.record_spend(pk, &sid_s, &Uuid::new_v4().to_string(), 2_500_000_000).await.unwrap();

    let (s, b) = call_json(&h.router, get("/wallets", Some(TOKEN))).await;
    assert_eq!(s, StatusCode::OK);
    let wallets = b["wallets"].as_array().unwrap();
    assert_eq!(wallets.len(), 1);
    assert_eq!(wallets[0]["pubkey"].as_str(), Some(pk));
    assert_eq!(wallets[0]["label"].as_str(), Some("hot"));
    assert_eq!(wallets[0]["daily_spend_lamports"].as_u64(), Some(2_500_000_000));
}

// ── Case 4: GET /audit pagination ───────────────────────────────────────────

#[tokio::test]
async fn case4_audit_pagination_limit_and_offset() {
    let h = build_harness(HarnessOptions::default()).await;

    // Insert 5 rows with distinct event_types.
    for i in 0..5u32 {
        h.audit.append(
            None, &format!("corr-{i}"), &format!("event-{i}"),
            "system", &json!({"i": i}), AuditSeverity::Info,
        ).await.unwrap();
        // Stagger timestamps so ordering is deterministic (list_all DESC by occurred_at)
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let (s, b) = call_json(&h.router, get("/audit?limit=2&offset=0", Some(TOKEN))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["limit"].as_i64(), Some(2));
    assert_eq!(b["offset"].as_i64(), Some(0));
    let page1 = b["rows"].as_array().unwrap();
    assert_eq!(page1.len(), 2);

    let (s2, b2) = call_json(&h.router, get("/audit?limit=2&offset=2", Some(TOKEN))).await;
    assert_eq!(s2, StatusCode::OK);
    let page2 = b2["rows"].as_array().unwrap();
    assert_eq!(page2.len(), 2);

    // Pages must not overlap.
    let ids1: Vec<&str> = page1.iter().map(|r| r["id"].as_str().unwrap()).collect();
    let ids2: Vec<&str> = page2.iter().map(|r| r["id"].as_str().unwrap()).collect();
    for id in &ids2 { assert!(!ids1.contains(id), "page 2 row leaked into page 1"); }

    // DESC ordering: page 1 is newer than page 2.
    let newest: i64 = page1[0]["occurred_at"].as_i64().unwrap();
    let older:  i64 = page2[0]["occurred_at"].as_i64().unwrap();
    assert!(newest >= older, "page1 must be newest-first");
}

// ── Case 5: /health public; /metrics requires auth ──────────────────────────

#[tokio::test]
async fn case5_health_public_metrics_auth_gated() {
    let h = build_harness(HarnessOptions::default()).await;

    let (s_health, _)   = call_json(&h.router, get("/health", None)).await;
    assert_eq!(s_health, StatusCode::OK, "/health must be reachable without auth");

    let (s_metrics_no, _) = call_json(&h.router, get("/metrics", None)).await;
    assert_eq!(s_metrics_no, StatusCode::UNAUTHORIZED, "/metrics must reject unauthenticated calls");

    let (s_metrics_ok, _) = call_json(&h.router, get("/metrics", Some(TOKEN))).await;
    assert_eq!(s_metrics_ok, StatusCode::OK, "/metrics must succeed with valid bearer");
}

// ── Case 6: /debug/seed-demo accumulates + disabled returns 503 ─────────────

#[tokio::test]
async fn case6a_seed_demo_disabled_returns_503() {
    let h = build_harness(HarnessOptions { seeder_enabled: false, ..Default::default() }).await;

    let (s, b) = call_json(&h.router, post_json("/debug/seed-demo", TOKEN, json!({}))).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"].as_str(), Some("demo_seed_disabled"));
}

#[tokio::test]
async fn case6b_seed_demo_accumulates_each_call() {
    let pk = "seed-wallet-bbb";
    let h = build_harness(HarnessOptions {
        seeder_enabled: true,
        wallet_pubkey:  Some(pk.into()),
        wallets:        vec![("seed-wallet".into(), pk.into())],
        ..Default::default()
    }).await;

    let before_pending = h.approval_store.pending_count();
    let before_rows    = h.audit.list_all(100, 0).await.unwrap().len();

    let (s1, b1) = call_json(&h.router, post_json("/debug/seed-demo", TOKEN, json!({}))).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["approvals_created"].as_u64(), Some(1));

    let mid_pending = h.approval_store.pending_count();
    let mid_rows    = h.audit.list_all(100, 0).await.unwrap().len();

    let (s2, b2) = call_json(&h.router, post_json("/debug/seed-demo", TOKEN, json!({}))).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2["approvals_created"].as_u64(), Some(1));

    let after_pending = h.approval_store.pending_count();
    let after_rows    = h.audit.list_all(100, 0).await.unwrap().len();

    assert_eq!(mid_pending - before_pending, 1, "first call adds one pending approval");
    assert_eq!(after_pending - mid_pending,  1, "second call adds another (accumulative)");
    assert_eq!(mid_rows - before_rows, 1);
    assert_eq!(after_rows - mid_rows,  1);

    // Daily spend accumulated for the seed wallet.
    let (_, wb) = call_json(&h.router, get("/wallets", Some(TOKEN))).await;
    let ws = wb["wallets"].as_array().unwrap();
    let w  = ws.iter().find(|w| w["pubkey"] == pk).unwrap();
    let spent = w["daily_spend_lamports"].as_u64().unwrap_or(0);
    assert!(spent >= 2_000_000_000, "two seed calls should bump spend by >= 2 SOL worth, got {spent}");
}

// ── Case 7: approval response codes + body shapes ───────────────────────────

fn register_chain_workflow(h: &Harness, allowed_roles_s0: Vec<String>) -> Uuid {
    let chain = vec![
        ApprovalChainStage { role: allowed_roles_s0.first().cloned().unwrap_or_default(), description: "".into(), min_approvals: 1 },
        ApprovalChainStage { role: "treasury".into(), description: "".into(), min_approvals: 1 },
    ];
    let req = ApprovalRequest::new(
        h.session_id.clone(), Uuid::new_v4(), "case7",
        PolicyVerdict::RequiresHumanApproval {
            reason: "chain".into(), rule_name: "chain-rule".into(),
            required_approver_role: Some(chain[0].role.clone()),
            approval_chain: Some(chain.clone()),
        },
        fake_simulation(),
    );
    let rid = req.id;
    let wf = ApprovalWorkflow {
        request_id: rid, session_id: h.session_id.clone(),
        state: ApprovalWorkflowState::Pending,
        stages: chain.iter().enumerate().map(|(i, s)| ApprovalStage {
            index: i, allowed_roles: if i == 0 { allowed_roles_s0.clone() } else { vec![s.role.clone()] },
            min_approvals: s.min_approvals, decisions: vec![],
        }).collect(),
        created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
        expires_at: None,
    };
    h.approval_store.register(req, wf);
    rid
}

#[tokio::test]
async fn case7a_role_mismatch_returns_403() {
    // No operator registry → use anonymous path so approver_role flows straight
    // to the store. Then claim "treasury" against a stage requiring "risk".
    let h = build_harness(HarnessOptions::default()).await;
    let rid = register_chain_workflow(&h, vec!["risk".into()]);

    let body = json!({ "request_id": rid, "approved": true, "approver_role": "treasury" });
    let (s, b) = call_json(&h.router, post_json(
        &format!("/sessions/{}/approve", h.session_id), TOKEN, body,
    )).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"].as_str(), Some("role_mismatch"));
    assert_eq!(b["required_role"].as_str(), Some("risk"));
}

#[tokio::test]
async fn case7b_role_not_held_returns_403() {
    // Operator token is registered with only ["risk"], but body claims "cfo".
    let h = build_harness(HarnessOptions {
        operator_roles: vec![(TOKEN.into(), vec!["risk".into()])],
        ..Default::default()
    }).await;
    let rid = register_chain_workflow(&h, vec!["risk".into()]);

    let body = json!({ "request_id": rid, "approved": true, "approver_role": "cfo" });
    let (s, b) = call_json(&h.router, post_json(
        &format!("/sessions/{}/approve", h.session_id), TOKEN, body,
    )).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"].as_str(), Some("role_not_held"));
    assert_eq!(b["claimed_role"].as_str(), Some("cfo"));
}

#[tokio::test]
async fn case7c_duplicate_operator_returns_409() {
    // Two-operator quorum where the same token tries to vote twice.
    let h = build_harness(HarnessOptions {
        operator_roles: vec![(TOKEN.into(), vec!["risk".into()])],
        ..Default::default()
    }).await;

    // Register a single-stage workflow needing 2 risk approvals (quorum).
    let chain = vec![
        ApprovalChainStage { role: "risk".into(), description: "".into(), min_approvals: 2 },
    ];
    let req = ApprovalRequest::new(
        h.session_id.clone(), Uuid::new_v4(), "case7c",
        PolicyVerdict::RequiresHumanApproval {
            reason: "q".into(), rule_name: "q-rule".into(),
            required_approver_role: Some("risk".into()),
            approval_chain: Some(chain.clone()),
        },
        fake_simulation(),
    );
    let rid = req.id;
    let wf = ApprovalWorkflow {
        request_id: rid, session_id: h.session_id.clone(),
        state: ApprovalWorkflowState::Pending,
        stages: vec![ApprovalStage {
            index: 0, allowed_roles: vec!["risk".into()], min_approvals: 2, decisions: vec![],
        }],
        created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
        expires_at: None,
    };
    h.approval_store.register(req, wf);

    let body = json!({ "request_id": rid, "approved": true, "approver_role": "risk" });

    // First vote — 200 with quorum_progress.
    let (s1, b1) = call_json(&h.router, post_json(
        &format!("/sessions/{}/approve", h.session_id), TOKEN, body.clone(),
    )).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["status"].as_str(), Some("quorum_progress"));
    assert_eq!(b1["approvals_so_far"].as_u64(), Some(1));

    // Second vote from same operator — 409 duplicate_operator.
    let (s2, b2) = call_json(&h.router, post_json(
        &format!("/sessions/{}/approve", h.session_id), TOKEN, body,
    )).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(b2["error"].as_str(), Some("duplicate_operator"));
}

#[tokio::test]
async fn case7d_expired_returns_410_gone() {
    let h = build_harness(HarnessOptions::default()).await;
    // Register a workflow whose lease is already in the past.
    let req = ApprovalRequest::new(
        h.session_id.clone(), Uuid::new_v4(), "case7d",
        PolicyVerdict::RequiresHumanApproval {
            reason: "".into(), rule_name: "".into(),
            required_approver_role: None, approval_chain: None,
        },
        fake_simulation(),
    );
    let rid = req.id;
    let mut wf = ApprovalWorkflow::single_stage(rid, h.session_id.clone(), None);
    wf.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(30));
    h.approval_store.register(req, wf);

    let (s, b) = call_json(&h.router, post_json(
        &format!("/sessions/{}/approve", h.session_id), TOKEN,
        json!({ "request_id": rid, "approved": true }),
    )).await;
    assert_eq!(s, StatusCode::GONE);
    assert_eq!(b["error"].as_str(), Some("lease_expired"));
}

// ── Case 8: Dashboard composition — the 3 endpoints line up ─────────────────

#[tokio::test]
async fn case8_dashboard_composite_matches_across_endpoints() {
    let pk = "dash-wallet";
    let h = build_harness(HarnessOptions {
        seeder_enabled: true,
        wallet_pubkey:  Some(pk.into()),
        wallets:        vec![("dash".into(), pk.into())],
        policy_rules:   vec![sample_rule("x", 1)],
        ..Default::default()
    }).await;

    // Seed once so all 3 endpoints have at least one row.
    let (s, _) = call_json(&h.router, post_json("/debug/seed-demo", TOKEN, json!({}))).await;
    assert_eq!(s, StatusCode::OK);

    let (_, pend) = call_json(&h.router, get("/pending-approvals", Some(TOKEN))).await;
    let (_, aud)  = call_json(&h.router, get("/audit?limit=50", Some(TOKEN))).await;
    let (_, wal)  = call_json(&h.router, get("/wallets", Some(TOKEN))).await;

    let items = pend["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "dashboard should see exactly 1 pending approval");
    let rid = items[0]["request"]["id"].as_str().unwrap();

    // Audit trail must include a row correlating to that request_id.
    let hit = aud["rows"].as_array().unwrap()
        .iter()
        .any(|r| r["correlation_id"].as_str() == Some(rid));
    assert!(hit, "audit must surface the seeded correlation_id {rid}");

    // Wallet's daily_spend_lamports > 0 after seed.
    let w = wal["wallets"].as_array().unwrap().iter()
        .find(|w| w["pubkey"] == pk).unwrap();
    assert!(
        w["daily_spend_lamports"].as_u64().unwrap() > 0,
        "seed should have bumped daily spend"
    );
}

// ── Case 10: Audit "restart" — fresh AuditRepository over same pool ─────────

#[tokio::test]
async fn case10_audit_rows_survive_restart_via_fresh_repository() {
    let h = build_harness(HarnessOptions::default()).await;

    // Write before "restart".
    h.audit.append(
        Some(&h.session_id.to_string()), "corr-restart",
        "human_approved", "op_x", &json!({"pre_restart": true}),
        AuditSeverity::Info,
    ).await.unwrap();

    // Drop the old handle and construct a fresh one sharing the same pool —
    // this simulates a process restart re-opening the SQLite file.
    let fresh = AuditRepository::new(h._db.pool().clone());
    let rows  = fresh.list_all(100, 0).await.unwrap();
    assert!(
        rows.iter().any(|r| r.correlation_id == "corr-restart"),
        "audit row must be readable after a fresh repository instance"
    );

    // ApprovalStore (in-memory DashMap) does NOT survive: this asserts the
    // invariant documented in PITCH.md — seed must be re-applied on restart.
    let reset_store = ApprovalStore::new();
    assert_eq!(reset_store.pending_count(), 0);
}

// ── C3: auth middleware rejects /approve with missing or wrong token ───────
//
// Closes a matrix hole flagged in docs/TEST_RESULTS.md (bucket C, gap C3).
// `/metrics` without a token is already covered by case5; this pair proves
// the same middleware gate applies to the approve route specifically.

#[tokio::test]
async fn approve_route_without_token_returns_401() {
    let h = build_harness(HarnessOptions::default()).await;

    let req = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/approve", h.session_id))
        .header("content-type", "application/json")
        // No Authorization header intentionally.
        .body(Body::from(json!({
            "request_id": Uuid::new_v4(),
            "approved":   true,
        }).to_string()))
        .unwrap();

    let (status, _) = call_json(&h.router, req).await;
    assert_eq!(
        status, StatusCode::UNAUTHORIZED,
        "/approve must reject requests without a bearer token at the middleware layer",
    );
}

#[tokio::test]
async fn approve_route_with_wrong_token_returns_401() {
    let h = build_harness(HarnessOptions::default()).await;

    let req = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/approve", h.session_id))
        .header("authorization", "Bearer definitely-not-the-configured-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({
            "request_id": Uuid::new_v4(),
            "approved":   true,
        }).to_string()))
        .unwrap();

    let (status, _) = call_json(&h.router, req).await;
    assert_eq!(
        status, StatusCode::UNAUTHORIZED,
        "/approve must reject requests whose bearer token does not match auth_token",
    );
}

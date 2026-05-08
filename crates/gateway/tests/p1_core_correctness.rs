//! Phase 1 — core correctness coverage for four spec cases that were
//! previously only partially covered:
//!
//! - Case 2:  USDC >= 10K triggers a full risk → treasury chain end-to-end
//! - Case 10: Expired decision writes `approval_lease_expired` audit and
//!            never a fake `human_approved` / `human_rejected`
//! - Case 12: `POST /sessions/:id/propose-transfer` failure returns 422
//!            and leaves `/pending-approvals` empty
//! - Case 13: `POST /sessions/:id/propose-transfer` success + policy hit
//!            surfaces an approval request in `/pending-approvals`
//!
//! Helpers are intentionally local so this file reads as one review unit.

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
    MessageHandler, MessageHandlerRef, ProposeTransferResult,
    SessionManagerRef, SessionOps,
    TransactionProposer, TransactionProposerRef,
    WalletChallengeHandler, WalletChallengeHandlerRef, WalletChallengeInfo,
    WalletSignatureHandler, WalletSignatureHandlerRef, WalletSignatureOutcome,
    PendingWalletSignatureInfo,
    state::{AuditReaderRef, PolicyReaderRef, WalletDirectoryRef},
};
use claw_gateway::{approval_audit::emit_decide_audit, ApprovalStore};
use claw_observability::HealthRegistry;
use claw_risk_engine::{PolicyEvaluationContext, PolicySet};
use claw_state_store::{AuditRepository, Database};
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{
        ApprovalChainStage, ApprovalDecision, ApprovalOutcome, ApprovalRequest,
        ApprovalStage, ApprovalWorkflow, ApprovalWorkflowState,
    },
    events::GatewayEvent,
    policy::{PolicyAction, PolicyCondition, PolicyRule, PolicyVerdict},
    session::SessionId,
    solana::SolanaNetwork,
    transaction::{
        AccountRole, InstructionSummary, SimulationResult, TokenTransfer, TransactionProposal,
    },
};

// ── Shared helpers ───────────────────────────────────────────────────────────

const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

fn fake_simulation() -> SimulationResult {
    SimulationResult {
        success:            true,
        error:              None,
        compute_units_used: Some(1_000),
        logs:               vec![],
        return_data:        None,
        account_diffs:      vec![],
        fee_lamports:       Some(5_000),
    }
}

// ── Case 2: USDC >= 10K chain end-to-end ─────────────────────────────────────

fn usdc_transfer_proposal(raw_amount: u64) -> TransactionProposal {
    TransactionProposal {
        id:          Uuid::new_v4(),
        session_id:  SessionId::from(Uuid::new_v4()),
        wallet_pubkey: "mm-wallet-pubkey".into(),
        network:     SolanaNetwork::Devnet,
        description: "USDC transfer".into(),
        transaction_b64: String::new(),
        instructions_summary: vec![InstructionSummary {
            program_id:   TOKEN_PROGRAM.into(),
            program_name: Some("spl-token".into()),
            description:  "TransferChecked".into(),
            transfer_lamports:        None,
            is_legacy_token_transfer: false,
            token_transfer: Some(TokenTransfer {
                mint:        USDC_MINT.into(),
                source:      "mm-wallet-pubkey".into(),
                destination: "counterparty".into(),
                amount:      raw_amount,
                decimals:    Some(6),
            }),
            accounts: vec![
                AccountRole {
                    pubkey:      "mm-wallet-pubkey".into(),
                    label:       Some("source".into()),
                    is_signer:   true,
                    is_writable: true,
                },
                AccountRole {
                    pubkey:      "counterparty".into(),
                    label:       Some("destination".into()),
                    is_signer:   false,
                    is_writable: true,
                },
                AccountRole {
                    pubkey:      USDC_MINT.into(),
                    label:       Some("mint".into()),
                    is_signer:   false,
                    is_writable: false,
                },
            ],
        }],
        created_at: chrono::Utc::now(),
    }
}

fn workflow_from_chain(
    request_id: Uuid,
    session_id: SessionId,
    chain:      &[ApprovalChainStage],
) -> ApprovalWorkflow {
    let now = chrono::Utc::now();
    ApprovalWorkflow {
        request_id,
        session_id,
        state: ApprovalWorkflowState::Pending,
        stages: chain
            .iter()
            .enumerate()
            .map(|(i, s)| ApprovalStage {
                index:         i,
                allowed_roles: vec![s.role.clone()],
                min_approvals: s.min_approvals,
                decisions:     vec![],
            })
            .collect(),
        created_at: now,
        updated_at: now,
        expires_at: None,
    }
}

#[test]
fn usdc_10k_triggers_chain_and_completes_through_risk_and_treasury() {
    // Build a policy set that mirrors the production USDC rules:
    // a high-value chain rule (>= 10_000 USDC = 10_000_000_000 raw, 6 decimals)
    // followed by a medium-value single-approver rule.
    let chain_stages = vec![
        ApprovalChainStage {
            role:          "risk".into(),
            description:   "Risk officer review for high-value USDC".into(),
            min_approvals: 1,
        },
        ApprovalChainStage {
            role:          "treasury".into(),
            description:   "Treasury sign-off for high-value USDC".into(),
            min_approvals: 1,
        },
    ];
    let policy = PolicySet::new(
        vec![
            PolicyRule {
                name: "usdc-high-value-chain".into(),
                description: "USDC >= 10,000 requires risk + treasury".into(),
                condition: PolicyCondition::TokenAmountExceeds {
                    mint:      USDC_MINT.into(),
                    threshold: 10_000_000_000, // 10,000 USDC, 6 decimals
                },
                action: PolicyAction::RequireApprovalChain {
                    reason: "USDC >= 10K requires multi-stage approval".into(),
                    stages: chain_stages.clone(),
                },
            },
            PolicyRule {
                name: "usdc-medium-value-requires-human".into(),
                description: "USDC >= 100 requires human approval".into(),
                condition: PolicyCondition::TokenAmountExceeds {
                    mint:      USDC_MINT.into(),
                    threshold: 100_000_000, // 100 USDC
                },
                action: PolicyAction::RequireHumanApproval {
                    reason:                 "USDC >= 100".into(),
                    required_approver_role: Some("treasury".into()),
                },
            },
        ],
        vec![],
        vec![],
    );

    let proposal = usdc_transfer_proposal(10_000_000_000); // exactly 10k (>= semantics)
    let ctx = PolicyEvaluationContext {
        proposal:                    &proposal,
        simulation_result:           None,
        network:                     SolanaNetwork::Devnet,
        session_id:                  &proposal.session_id,
        session_spend_lamports:      0,
        wallet_daily_spend_lamports: 0,
    };

    let result = policy.evaluate(&ctx);
    let (approval_chain, rule_name) = match &result.verdict {
        PolicyVerdict::RequiresHumanApproval { approval_chain, rule_name, .. } => (
            approval_chain.clone().expect("chain rule must carry stages"),
            rule_name.clone(),
        ),
        other => panic!("expected chain verdict for USDC 10k, got {:?}", other),
    };
    assert_eq!(rule_name, "usdc-high-value-chain", "must hit chain rule, not medium");
    assert_eq!(approval_chain.len(), 2);
    assert_eq!(approval_chain[0].role, "risk");
    assert_eq!(approval_chain[1].role, "treasury");
    assert_eq!(
        result.matched_rule_index, Some(0),
        "chain rule must be first-match-wins"
    );

    // Register in the store and walk the workflow terminal.
    let session_id = proposal.session_id.clone();
    let request = ApprovalRequest::new(
        session_id.clone(),
        proposal.id,
        "USDC 10k transfer",
        result.verdict,
        fake_simulation(),
    );
    let request_id = request.id;
    let workflow = workflow_from_chain(request_id, session_id, &approval_chain);

    let store = ApprovalStore::new();
    store.register(request, workflow);

    let risk_decision = ApprovalDecision::approve_as(
        request_id,
        Some("risk-approved".into()),
        "risk".into(),
    );
    let (out1, _) = store.decide(&risk_decision);
    assert_eq!(
        out1,
        ApprovalOutcome::StageAdvanced {
            completed_stage:    0,
            next_stage:         1,
            next_required_role: Some("treasury".into()),
        },
    );

    let treasury_decision = ApprovalDecision::approve_as(
        request_id,
        Some("treasury-ok".into()),
        "treasury".into(),
    );
    let (out2, maybe_req) = store.decide(&treasury_decision);
    assert_eq!(out2, ApprovalOutcome::Approved);
    assert!(maybe_req.is_some(), "terminal Approved must return the request");
    assert_eq!(store.pending_count(), 0);
}

// ── Case 10: Expired decision audit emission ────────────────────────────────

async fn setup_audit() -> (Database, AuditRepository) {
    let db    = Database::open_in_memory().await.expect("in-memory DB");
    let audit = AuditRepository::new(db.pool().clone());
    (db, audit)
}

#[tokio::test]
async fn expired_decide_writes_lease_expired_audit_not_human_decision() {
    let (_db, audit) = setup_audit().await;

    // Build an already-expired workflow + register.
    let session_id = SessionId::from(Uuid::new_v4());
    let request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "lease expiry audit test",
        PolicyVerdict::RequiresHumanApproval {
            reason:                 "lease test".into(),
            rule_name:              "lease-rule".into(),
            required_approver_role: None,
            approval_chain:         None,
        },
        fake_simulation(),
    );
    let request_id = request.id;

    let mut workflow = ApprovalWorkflow::single_stage(request_id, session_id.clone(), None);
    workflow.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(60));

    let store = ApprovalStore::new();
    store.register(request, workflow);

    // Decide → ApprovalStore produces Expired + no request.
    let decision = ApprovalDecision::approve(request_id, Some("too late".into()));
    let (outcome, maybe_req) = store.decide(&decision);
    assert_eq!(outcome, ApprovalOutcome::Expired);
    assert!(
        maybe_req.is_none(),
        "expired must not return a request — an audit row would misidentify this as a human decision"
    );

    // Emit audit using the extracted function — this is what daemon does.
    emit_decide_audit(&audit, &decision, &outcome, maybe_req.as_ref()).await;

    // Inspect audit. We expect exactly one row tagged `approval_lease_expired`
    // and zero rows tagged `human_approved` or `human_rejected`.
    let rows = audit.list_all(100, 0).await.expect("list audit");
    let mut lease_expired = 0usize;
    let mut human_approved = 0usize;
    let mut human_rejected = 0usize;
    for r in &rows {
        match r.event_type.as_str() {
            "approval_lease_expired" => lease_expired += 1,
            "human_approved"         => human_approved += 1,
            "human_rejected"         => human_rejected += 1,
            _ => {}
        }
    }
    assert_eq!(lease_expired, 1, "expiry must write exactly one approval_lease_expired row");
    assert_eq!(
        human_approved, 0,
        "expiry must NEVER produce a human_approved row — would be a false audit"
    );
    assert_eq!(
        human_rejected, 0,
        "expiry must NEVER produce a human_rejected row — would be a false audit"
    );
}

// ── Cases 12 & 13: propose-transfer HTTP-layer invariants ────────────────────

/// Session manager stub that always reports the configured session as active.
struct StubSession {
    sid: SessionId,
}
impl SessionOps for StubSession {
    fn open(&self, _: AgentRole, _: String, _: Option<Vec<PolicyRule>>) -> SessionId {
        self.sid.clone()
    }
    fn close(&self, _: &SessionId, _: &str) {}
    fn active_count(&self) -> usize { 1 }
    fn is_active(&self, id: &SessionId) -> bool { id == &self.sid }
}

struct StubMsg;
impl MessageHandler for StubMsg {
    fn handle<'a>(
        &'a self,
        _s: &'a SessionId,
        _c: AgentCommand,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>> {
        Box::pin(async { Err("stub".into()) })
    }
}

/// Approval handler backed by a shared ApprovalStore so the test can assert
/// what the pipeline actually registered.
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
        d: ApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>> {
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
        &self, _s: &SessionId, rid: Uuid, _b64: String,
    ) -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>> {
        Box::pin(async move {
            WalletSignatureOutcome {
                request_id: rid,
                accepted:   false,
                signature:  None,
                tx_signature: None,
                submitted:  false,
                error:      Some("stub".into()),
                rebuild_required: false,
            }
        })
    }
    fn pending_for_session(&self, _s: &SessionId) -> Vec<PendingWalletSignatureInfo> { vec![] }
    fn bind_wallet(&self, _: &SessionId, _: &str) {}
    fn wallets_for_session(&self, _: &SessionId) -> Vec<String> { vec![] }
}

struct StubChallenge;
impl WalletChallengeHandler for StubChallenge {
    fn create_challenge(
        &self, _s: &SessionId, _pk: &str,
    ) -> Pin<Box<dyn Future<Output = Result<WalletChallengeInfo, String>> + Send + '_>> {
        Box::pin(async { Err("stub".into()) })
    }
    fn verify_and_bind(
        &self, _s: &SessionId, _cid: &str, _pk: &str, _sig: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Err("stub".into()) })
    }
}

/// Proposer that always returns an error — models a simulation/pipeline
/// failure that must NOT leak a pending approval.
struct FailingProposer;
impl TransactionProposer for FailingProposer {
    fn propose_transfer(
        &self, _s: &SessionId, _f: &str, _t: &str, _l: u64,
    ) -> Pin<Box<dyn Future<Output = Result<ProposeTransferResult, String>> + Send + '_>> {
        Box::pin(async { Err("simulation rejected".into()) })
    }
}

/// Proposer that emulates the real pipeline: policy decided human approval
/// is required, so an approval request is registered BEFORE we return
/// success to the caller.
struct ApprovalRegisteringProposer {
    store:      ApprovalStore,
    session_id: SessionId,
}
impl TransactionProposer for ApprovalRegisteringProposer {
    fn propose_transfer(
        &self, _s: &SessionId, _f: &str, _t: &str, lamports: u64,
    ) -> Pin<Box<dyn Future<Output = Result<ProposeTransferResult, String>> + Send + '_>> {
        let store      = self.store.clone();
        let session_id = self.session_id.clone();
        Box::pin(async move {
            let request = ApprovalRequest::new(
                session_id.clone(),
                Uuid::new_v4(),
                format!("{lamports} lamport transfer"),
                PolicyVerdict::RequiresHumanApproval {
                    reason:                 "transfer exceeds threshold".into(),
                    rule_name:              "large-transfer-requires-human".into(),
                    required_approver_role: None,
                    approval_chain:         None,
                },
                fake_simulation(),
            );
            let request_id = request.id;
            let workflow   = ApprovalWorkflow::single_stage(request_id, session_id, None);
            store.register(request, workflow);

            Ok(ProposeTransferResult {
                status: "awaiting_wallet_signature".into(),
                wallet_signature_request_id: Some(request_id),
                unsigned_tx_b64: Some("".into()),
                signature: None,
                error: None,
            })
        })
    }
}

fn build_state(
    sid:            SessionId,
    approval_store: ApprovalStore,
    proposer:       Option<Arc<dyn TransactionProposer>>,
) -> AppState {
    let (tx, _rx) = broadcast::channel::<GatewayEvent>(16);
    AppState {
        session_mgr:       SessionManagerRef::new(Arc::new(StubSession { sid: sid.clone() })),
        message_handler:   MessageHandlerRef::new(Arc::new(StubMsg)),
        approval:          ApprovalHandlerRef::new(Arc::new(StoreBackedApproval {
            store: approval_store,
        })),
        events:            EventSubscriberRef::new(Arc::new(StubEvents { tx })),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(StubWalletSig)),
        solend_signatures: None,
        solend_jit_prepare: None,
        solend_withdraw_jit_prepare: None,
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubChallenge)),
        auth_token:        AuthToken::new("p1-token"),
        operator_registry: claw_api::auth::OperatorRegistry::new(),
        metrics:           Arc::new(claw_observability::metrics::MetricsRegistry::new()),
        propose:           proposer.map(TransactionProposerRef::new),
        rate_limiter:      None,
        policy:            PolicyReaderRef::noop(),
        audit:             AuditReaderRef::noop(),
        wallets:           WalletDirectoryRef::noop(),
        demo_seeder:       None,
        chat:              None,
    }
}

async fn call_json(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp    = router.clone().oneshot(req).await.unwrap();
    let status  = resp.status();
    let body    = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn propose_transfer_failure_returns_422_with_no_pending_approval() {
    let sid            = SessionId::from(Uuid::new_v4());
    let approval_store = ApprovalStore::new();

    let state = build_state(
        sid.clone(),
        approval_store.clone(),
        Some(Arc::new(FailingProposer)),
    );
    let router = claw_api::create_router(state, HealthRegistry::new());

    let req = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{sid}/propose-transfer"))
        .header("authorization", "Bearer p1-token")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "from":     "11111111111111111111111111111111",
                "to":       "22222222222222222222222222222222",
                "lamports": 50_000_000u64,
            }).to_string()
        ))
        .unwrap();

    let (status, body) = call_json(&router, req).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "proposer error must surface as 422");
    assert!(body.get("error").is_some(), "error body expected, got {body}");

    // No approval request leaked into the store.
    assert_eq!(approval_store.pending_count(), 0);
    assert_eq!(approval_store.all_pending_with_workflow().len(), 0);

    // And /pending-approvals reflects it.
    let list_req = Request::builder()
        .uri("/pending-approvals")
        .header("authorization", "Bearer p1-token")
        .body(Body::empty())
        .unwrap();
    let (ls, lb) = call_json(&router, list_req).await;
    assert_eq!(ls, StatusCode::OK);
    let items = lb.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(items.is_empty(), "failure path must not create a pending approval: {lb}");
}

#[tokio::test]
async fn propose_transfer_success_surfaces_approval_in_pending_list() {
    let sid            = SessionId::from(Uuid::new_v4());
    let approval_store = ApprovalStore::new();

    let proposer = Arc::new(ApprovalRegisteringProposer {
        store:      approval_store.clone(),
        session_id: sid.clone(),
    });
    let state  = build_state(sid.clone(), approval_store.clone(), Some(proposer));
    let router = claw_api::create_router(state, HealthRegistry::new());

    let req = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{sid}/propose-transfer"))
        .header("authorization", "Bearer p1-token")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "from":     "11111111111111111111111111111111",
                "to":       "22222222222222222222222222222222",
                "lamports": 50_000_000u64,
            }).to_string()
        ))
        .unwrap();

    let (status, body) = call_json(&router, req).await;
    assert_eq!(status, StatusCode::OK, "success path expected 200, got body {body}");
    assert_eq!(body.get("status").and_then(|v| v.as_str()), Some("awaiting_wallet_signature"));

    // Pipeline registered exactly one approval.
    assert_eq!(approval_store.pending_count(), 1);

    // /pending-approvals surfaces it with rule name intact.
    let list_req = Request::builder()
        .uri("/pending-approvals")
        .header("authorization", "Bearer p1-token")
        .body(Body::empty())
        .unwrap();
    let (ls, lb) = call_json(&router, list_req).await;
    assert_eq!(ls, StatusCode::OK);
    let items = lb.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "success + policy hit must expose one pending approval: {lb}");

    let rule = items[0]
        .get("request")
        .and_then(|r| r.get("policy_verdict"))
        .and_then(|v| v.get("rule_name"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(rule, "large-transfer-requires-human");
}

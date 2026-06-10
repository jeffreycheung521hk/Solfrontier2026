//! N6 Integration Smoke Test
//!
//! Proves the full approval → signing → audit → SSE round-trip works:
//!
//! 1. Park a transaction (simulates SubmitForSigningTool's AwaitingApproval path)
//! 2. Register an ApprovalRequest in ApprovalStore
//! 3. Approve via ApprovalStore::decide() + PendingSigningStore::signal()
//! 4. resume_after_approval completes signing via orchestrator with mock signer
//! 5. Audit records persisted (not just logged)
//! 6. SSE events emitted (ApprovalRequested, ApprovalReceived, TransactionSigned)
//! 7. Rejection path is safe
//! 8. Duplicate decision rejected
//! 9. Typestate integrity preserved (only successful sim → approved → signed)
//!
//! Uses: mock signer, in-memory SQLite, no HTTP listener, no real RPC.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signature::Signature,
    system_instruction,
    transaction::Transaction,
};
use uuid::Uuid;

use claw_gateway::{
    approval_store::ApprovalStore,
    completion_metadata::CompletionMetadataStore,
    event_bus::EventBus,
    pending_signing::PendingSigningStore,
    tools::resume_after_approval,
    orchestrator::SignatureOrchestrator,
    orchestrator::local_adapter::LocalWalletAdapter,
};
use claw_state_store::{
    audit::{AuditRepository, AuditSeverity},
    db::Database,
    spend::SpendRepository,
};
use claw_types::{
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::GatewayEvent,
    policy::PolicyVerdict,
    session::SessionId,
    solana::SolanaNetwork,
    transaction::{SimulationResult, TransactionProposal, TransactionStatus},
};
use claw_wallet_engine::{
    errors::WalletError,
    keystore::SecretKeystore,
    signer::{Signer, SignerRef},
};

// ── Mock Signer ──────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct MockSigner {
    pubkey: Pubkey,
}

#[allow(dead_code)]
impl MockSigner {
    fn new() -> Self {
        Self { pubkey: Pubkey::new_unique() }
    }
}

#[async_trait]
impl Signer for MockSigner {
    fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    async fn sign_transaction(&self, _tx: &mut Transaction) -> Result<Signature, WalletError> {
        // Return a deterministic fake signature.
        Ok(Signature::new_unique())
    }

    fn description(&self) -> String {
        "mock-signer-for-tests".to_string()
    }

    fn is_automatic(&self) -> bool {
        true
    }
}

// ── Test Helpers ─────────────────────────────────────────────────────────────

fn make_test_tx() -> Transaction {
    make_test_tx_from(&Pubkey::new_unique())
}

fn make_test_tx_from(from: &Pubkey) -> Transaction {
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(from, &to, 1_000_000);
    let msg = solana_sdk::message::Message::new(&[ix], Some(from));
    let mut tx = Transaction::new_unsigned(msg);
    tx.message.recent_blockhash = Hash::new_unique();
    tx
}

fn make_successful_sim() -> SimulationResult {
    SimulationResult {
        success:            true,
        error:              None,
        compute_units_used: Some(2000),
        logs:               vec!["Program log: transfer ok".to_string()],
        return_data:        None,
        account_diffs:      vec![],
        fee_lamports:       Some(5000),
    }
}

fn make_proposal(session_id: SessionId) -> TransactionProposal {
    TransactionProposal {
        id:                   Uuid::new_v4(),
        session_id,
        wallet_pubkey:        Pubkey::new_unique().to_string(),
        network:              SolanaNetwork::Devnet,
        description:          "test transfer 1 SOL".to_string(),
        transaction_b64:      String::new(), // not needed for resume path
        instructions_summary: vec![],
        created_at:           chrono::Utc::now(),
    }
}

/// Build an orchestrator with a local adapter backed by the given signer + keystore.
fn make_test_orchestrator(signer: SignerRef, keystore: SecretKeystore) -> SignatureOrchestrator {
    let local_adapter = Arc::new(LocalWalletAdapter::new(signer, keystore));
    SignatureOrchestrator::new(vec![local_adapter])
}

async fn setup_db() -> Database {
    Database::open_in_memory().await.expect("in-memory DB")
}

async fn setup_audit() -> AuditRepository {
    let db = setup_db().await;
    AuditRepository::new(db.pool().clone())
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Full happy path: park → approve → resume_after_approval → signed via orchestrator.
/// Verifies audit persistence + SSE events + typestate.
#[tokio::test]
async fn approve_signs_and_emits_events() {
    let session_id = SessionId::from(Uuid::new_v4());
    let approval_store = ApprovalStore::new();
    let pending_signing = PendingSigningStore::new();
    let event_bus = EventBus::new();
    let db = setup_db().await;
    let audit = AuditRepository::new(db.pool().clone());
    let spend = SpendRepository::new(db.pool().clone());
    // Build a real keypair, load it into keystore, and create signer + orchestrator.
    let keystore = SecretKeystore::new();
    let real_keypair = solana_sdk::signature::Keypair::new();
    let keypair_bytes: Vec<u8> = real_keypair.to_bytes().to_vec();
    let json_bytes = serde_json::to_string(&keypair_bytes).unwrap();
    let real_pubkey = keystore.load_from_json_bytes(&json_bytes).unwrap();
    let real_signer: SignerRef = Arc::new(
        claw_wallet_engine::local::LocalKeypairSigner::new(real_pubkey, keystore.clone())
    );
    let orchestrator = make_test_orchestrator(real_signer.clone(), keystore.clone());

    // Use the real signer's pubkey as the tx fee payer so signing succeeds.
    let tx = make_test_tx_from(&real_pubkey);
    let sim = make_successful_sim();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason:    "mainnet policy requires human approval".into(),
        rule_name: "mainnet-guard".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    let mut proposal = make_proposal(session_id.clone());
    proposal.wallet_pubkey = real_pubkey.to_string();
    let request_id = Uuid::new_v4();

    // Subscribe to events BEFORE any publishing.
    let mut rx = event_bus.subscribe();

    // ── Step 1: Park the transaction (simulates SubmitForSigningTool's AwaitingApproval) ──
    let decision_rx = pending_signing
        .park(request_id, proposal.clone(), &tx, sim.clone(), verdict.clone(), 1000)
        .expect("park should succeed");

    assert_eq!(pending_signing.parked_count(), 1);

    // ── Step 2: Register approval request ──
    let approval_request = ApprovalRequest::new(
        session_id.clone(),
        proposal.id,
        "test transfer 1 SOL",
        verdict.clone(),
        sim.clone(),
    );
    let approval_request = ApprovalRequest {
        id: request_id,
        ..approval_request
    };
    let wf = claw_types::approval::ApprovalWorkflow::single_stage(approval_request.id, approval_request.session_id.clone(), None);
    approval_store.register(approval_request, wf);
    assert_eq!(approval_store.pending_count(), 1);

    // Emit ApprovalRequested event (as SubmitForSigningTool would).
    event_bus.publish(GatewayEvent::ApprovalRequested(
        claw_types::events::ApprovalLifecycleEvent {
            header:         claw_types::events::EventHeader::new(Some(session_id.clone())),
            session_id:     session_id.clone(),
            request_id,
            transaction_id: proposal.id,
            approved:       None,
        },
    ));

    // ── Step 3: Spawn the resume task (as SubmitForSigningTool would) ──
    let pending_for_resume = pending_signing.clone();
    let audit_for_resume = audit.clone();
    let spend_for_resume = spend.clone();
    let event_bus_for_resume = event_bus.clone();
    let session_for_resume = session_id.clone();
    let proposal_for_resume = proposal.clone();
    let wallet_pubkey_for_resume = real_pubkey.to_string();
    let completion_meta = CompletionMetadataStore::new();

    let resume_handle = tokio::spawn(async move {
        resume_after_approval(
            decision_rx,
            request_id,
            proposal_for_resume,
            orchestrator,
            pending_for_resume,
            audit_for_resume,
            spend_for_resume,
            event_bus_for_resume,
            session_for_resume,
            wallet_pubkey_for_resume,
            completion_meta,
            None,
            300,
        ).await;
    });

    // ── Step 4: Approve ──
    let decision = ApprovalDecision::approve(request_id, Some("looks good".into()));
    let (outcome, maybe_req) = approval_store.decide(&decision);
    assert_eq!(outcome, ApprovalOutcome::Approved);
    assert!(maybe_req.is_some());
    assert_eq!(approval_store.pending_count(), 0, "request consumed after decision");

    // Signal the parked signing task.
    let signaled = pending_signing.signal(request_id, claw_types::approval::ApprovalWorkflowState::Approved);
    assert!(signaled, "signal must reach the parked task");

    // Emit ApprovalReceived event (as GatewayApprovalHandler would).
    let _ = audit.append(
        Some(&session_id.to_string()),
        &request_id.to_string(),
        "human_approved",
        "operator",
        &serde_json::json!({ "request_id": request_id, "approved": true }),
        AuditSeverity::Info,
    ).await;

    event_bus.publish(GatewayEvent::ApprovalReceived(
        claw_types::events::ApprovalLifecycleEvent {
            header:         claw_types::events::EventHeader::new(Some(session_id.clone())),
            session_id:     session_id.clone(),
            request_id,
            transaction_id: proposal.id,
            approved:       Some(true),
        },
    ));

    // ── Step 5: Wait for resume task to complete ──
    tokio::time::timeout(Duration::from_secs(5), resume_handle)
        .await
        .expect("resume task should complete within 5s")
        .expect("resume task should not panic");

    // ── Step 6: Verify audit persistence ──
    let session_str = session_id.to_string();
    let audit_rows = audit
        .list_for_session(&session_str, 50)
        .await
        .expect("audit query should succeed");

    // Should have: human_approved + transaction_signed_after_approval
    let event_types: Vec<&str> = audit_rows.iter().map(|r| r.event_type.as_str()).collect();
    assert!(
        event_types.contains(&"transaction_signed_after_approval"),
        "audit must contain transaction_signed_after_approval, got: {event_types:?}"
    );
    assert!(
        event_types.contains(&"human_approved"),
        "audit must contain human_approved, got: {event_types:?}"
    );

    // ── Step 7: Verify SSE events ──
    let mut saw_approval_requested = false;
    let mut saw_approval_received = false;
    let mut saw_transaction_signed = false;

    loop {
        match rx.try_recv() {
            Ok(event) => match event {
                GatewayEvent::ApprovalRequested(_) => saw_approval_requested = true,
                GatewayEvent::ApprovalReceived(_) => saw_approval_received = true,
                GatewayEvent::TransactionSigned(ref e) => {
                    saw_transaction_signed = true;
                    assert_eq!(e.status, TransactionStatus::Signed);
                    assert!(e.signature.is_some(), "signed event must contain signature");
                }
                _ => {}
            },
            Err(_) => break,
        }
    }

    assert!(saw_approval_requested, "must emit ApprovalRequested event");
    assert!(saw_approval_received,  "must emit ApprovalReceived event");
    assert!(saw_transaction_signed,  "must emit TransactionSigned event");

    // ── Step 8: Parked entry consumed ──
    assert_eq!(pending_signing.parked_count(), 0, "parked entry should be consumed after signing");
}

/// Rejection path: park → reject → no signing, TransactionFailed event emitted.
#[tokio::test]
async fn reject_emits_failure_event_no_signing() {
    let session_id = SessionId::from(Uuid::new_v4());
    let approval_store = ApprovalStore::new();
    let pending_signing = PendingSigningStore::new();
    let event_bus = EventBus::new();
    let audit = setup_audit().await;
    let db = setup_db().await;
    let spend = SpendRepository::new(db.pool().clone());

    // Build a real signer + orchestrator for the resume task
    let keystore = SecretKeystore::new();
    let real_keypair = solana_sdk::signature::Keypair::new();
    let keypair_bytes: Vec<u8> = real_keypair.to_bytes().to_vec();
    let json_bytes = serde_json::to_string(&keypair_bytes).unwrap();
    let real_pubkey = keystore.load_from_json_bytes(&json_bytes).unwrap();
    let real_signer: SignerRef = Arc::new(
        claw_wallet_engine::local::LocalKeypairSigner::new(real_pubkey, keystore.clone())
    );
    let orchestrator = make_test_orchestrator(real_signer, keystore);

    let tx = make_test_tx();
    let sim = make_successful_sim();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason:    "mainnet policy".into(),
        rule_name: "mainnet-guard".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    let mut proposal = make_proposal(session_id.clone());
    proposal.wallet_pubkey = real_pubkey.to_string();
    let request_id = Uuid::new_v4();

    let mut rx = event_bus.subscribe();

    let decision_rx = pending_signing
        .park(request_id, proposal.clone(), &tx, sim.clone(), verdict.clone(), 1000)
        .expect("park should succeed");

    let approval_request = ApprovalRequest {
        id: request_id,
        ..ApprovalRequest::new(session_id.clone(), proposal.id, "test reject", verdict, sim)
    };
    let wf = claw_types::approval::ApprovalWorkflow::single_stage(approval_request.id, approval_request.session_id.clone(), None);
    approval_store.register(approval_request, wf);

    // Spawn resume task.
    let pending_for_resume = pending_signing.clone();
    let audit_for_resume = audit.clone();
    let spend_for_resume = spend.clone();
    let event_bus_for_resume = event_bus.clone();
    let session_for_resume = session_id.clone();
    let proposal_for_resume = proposal.clone();
    let wallet_pubkey_for_resume = real_pubkey.to_string();
    let completion_meta = CompletionMetadataStore::new();

    let resume_handle = tokio::spawn(async move {
        resume_after_approval(
            decision_rx,
            request_id,
            proposal_for_resume,
            orchestrator,
            pending_for_resume,
            audit_for_resume,
            spend_for_resume,
            event_bus_for_resume,
            session_for_resume,
            wallet_pubkey_for_resume,
            completion_meta,
            None,
            300,
        ).await;
    });

    // Reject.
    let decision = ApprovalDecision::reject(request_id, Some("too risky".into()));
    let (outcome, _) = approval_store.decide(&decision);
    assert_eq!(outcome, ApprovalOutcome::Rejected);

    let signaled = pending_signing.signal(request_id, claw_types::approval::ApprovalWorkflowState::Rejected);
    assert!(signaled);

    tokio::time::timeout(Duration::from_secs(5), resume_handle)
        .await
        .expect("resume task should complete within 5s")
        .expect("resume task should not panic");

    // Verify TransactionFailed event emitted, NOT TransactionSigned.
    let mut saw_failed = false;
    let mut saw_signed = false;
    loop {
        match rx.try_recv() {
            Ok(event) => match event {
                GatewayEvent::TransactionFailed(ref e) => {
                    saw_failed = true;
                    assert_eq!(e.at_stage, TransactionStatus::AwaitingApproval);
                    assert!(e.error.contains("rejected"), "error should mention rejection");
                }
                GatewayEvent::TransactionSigned(_) => saw_signed = true,
                _ => {}
            },
            Err(_) => break,
        }
    }

    assert!(saw_failed,  "must emit TransactionFailed event on rejection");
    assert!(!saw_signed, "must NOT emit TransactionSigned event on rejection");

    // Parked entry removed.
    assert_eq!(pending_signing.parked_count(), 0);
}

/// Duplicate decision on the same request_id is rejected.
#[tokio::test]
async fn duplicate_decision_rejected() {
    let session_id = SessionId::from(Uuid::new_v4());
    let approval_store = ApprovalStore::new();
    let sim = make_successful_sim();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "test".into(),
        rule_name: "test-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    let request_id = Uuid::new_v4();

    let approval_request = ApprovalRequest {
        id: request_id,
        ..ApprovalRequest::new(session_id, Uuid::new_v4(), "dup test", verdict, sim)
    };
    let wf = claw_types::approval::ApprovalWorkflow::single_stage(approval_request.id, approval_request.session_id.clone(), None);
    approval_store.register(approval_request, wf);

    // First decision succeeds.
    let d1 = ApprovalDecision::approve(request_id, None);
    let (outcome1, _) = approval_store.decide(&d1);
    assert_eq!(outcome1, ApprovalOutcome::Approved);

    // Second decision on the same request_id fails.
    let d2 = ApprovalDecision::approve(request_id, None);
    let (outcome2, _) = approval_store.decide(&d2);
    assert_eq!(outcome2, ApprovalOutcome::AlreadyDecided,
        "duplicate decision must return AlreadyDecided (workflow is terminal)");
}

/// Typestate integrity: the semantic preconditions that gate construction.
///
/// `SimulatedTransaction` may only wrap a successful simulation and
/// `ApprovedTransaction` may only wrap a non-blocked verdict. Both
/// `new_unchecked` constructors are now `pub(crate)` and all fields are private,
/// so this downstream crate can no longer build the typestates directly — the
/// only path is `TransactionReviewPipeline::{simulate, evaluate_policy}`. That
/// external construction is a privacy compile error is verified by the
/// `compile_fail` doctest in `claw_wallet_engine::pipeline`; here we assert the
/// verdict/simulation semantics those constructors depend on.
#[tokio::test]
async fn typestate_integrity_preserved() {
    let sim = make_successful_sim();
    assert!(sim.success, "precondition: a SimulatedTransaction may only wrap a successful sim");

    // A non-blocked verdict is the precondition for ApprovedTransaction.
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason:    "test".into(),
        rule_name: "test-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    assert!(!verdict.is_blocked(), "RequiresHumanApproval must not be blocked");

    // A blocked verdict can never reach ApprovedTransaction.
    let blocked = PolicyVerdict::Rejected {
        reason:    "blocked".into(),
        rule_name: "deny-all".into(),
    };
    assert!(blocked.is_blocked(), "Rejected verdict must report is_blocked()");
}

/// Capability model: Execution role has ProposeSigning but not SignTransaction.
#[tokio::test]
async fn capability_model_execution_propose_not_sign() {
    use claw_tool_system::permissions::{Capability, CapabilitySet};
    use claw_types::agent::AgentRole;

    let caps = CapabilitySet::for_role(AgentRole::Execution);

    assert!(caps.has(&Capability::ProposeSigning));
    assert!(!caps.has(&Capability::SignTransaction));
    assert!(!caps.has(&Capability::SendTransaction));

    let result = caps.check_required(&["propose_signing".to_string()]);
    assert!(result.is_ok(), "Execution role must pass propose_signing check");

    let result = caps.check_required(&["sign_transaction".to_string()]);
    assert!(result.is_err(), "Execution role must NOT pass sign_transaction check");
}

/// Event bus delivers to multiple subscribers.
#[tokio::test]
async fn event_bus_multiple_subscribers() {
    let bus = EventBus::new();
    let mut rx1 = bus.subscribe();
    let mut rx2 = bus.subscribe();

    let session_id = SessionId::from(Uuid::new_v4());
    let event = GatewayEvent::ApprovalRequested(claw_types::events::ApprovalLifecycleEvent {
        header:         claw_types::events::EventHeader::new(Some(session_id.clone())),
        session_id,
        request_id:     Uuid::new_v4(),
        transaction_id: Uuid::new_v4(),
        approved:       None,
    });

    let count = bus.publish(event);
    assert_eq!(count, 2, "two subscribers should receive the event");

    assert!(rx1.try_recv().is_ok());
    assert!(rx2.try_recv().is_ok());
}

/// Audit repository persists and is queryable (not just logged).
#[tokio::test]
async fn audit_persistence_queryable() {
    let audit = setup_audit().await;
    let session_id = Uuid::new_v4().to_string();

    audit.append(
        Some(&session_id),
        "corr-1",
        "test_event_a",
        "test",
        &serde_json::json!({ "key": "value" }),
        AuditSeverity::Info,
    ).await.expect("append should succeed");

    audit.append(
        Some(&session_id),
        "corr-2",
        "test_event_b",
        "test",
        &serde_json::json!({ "other": 42 }),
        AuditSeverity::Warning,
    ).await.expect("append should succeed");

    let rows = audit.list_for_session(&session_id, 10).await.expect("query");
    assert_eq!(rows.len(), 2);

    let types: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
    assert!(types.contains(&"test_event_a"));
    assert!(types.contains(&"test_event_b"));
}

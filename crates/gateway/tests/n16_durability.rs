//! P1-1 — Durability integration tests.
//!
//! Proves that pending workflow state survives simulated daemon restart.
//!
//! # Coverage
//!
//! 1. pending_approval_survives_restart
//! 2. pending_external_signature_survives_restart
//! 3. completion_metadata_survives_restart
//! 4. stale_recovered_request_fails_closed
//! 5. consumed_request_not_reloaded
//! 6. duplicate_completion_after_restart_fails_closed
//! 7. recovery_does_not_create_orphans
//! 8. startup_with_corrupt_record_fails_closed

use std::sync::Arc;
use std::time::Duration;

use solana_sdk::{
    hash::Hash,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer as SdkSigner},
    system_instruction,
    transaction::Transaction,
};
use uuid::Uuid;

use claw_gateway::{
    approval_store::ApprovalStore,
    completion_metadata::CompletionMetadataStore,
    durable_pending::DurablePendingState,
    external_wallet::ExternalWalletStore,
    orchestrator::SignatureOrchestrator,
    orchestrator::external_adapter::ExternalWalletAdapter,
    orchestrator::types::{SignatureError, SignatureOutcome, SignatureRequestId},
    pending_signing::PendingSigningStore,
};
use claw_state_store::{
    db::Database,
    pending_state::PendingStateRepository,
};
use claw_types::{
    approval::ApprovalRequest,
    policy::PolicyVerdict,
    session::SessionId,
    transaction::{SimulationResult, TransactionProposal},
};

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn setup_db() -> Database {
    Database::open_in_memory().await.expect("in-memory DB")
}

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

fn fake_proposal(session_id: SessionId, wallet_pubkey: &str) -> TransactionProposal {
    TransactionProposal {
        id: Uuid::new_v4(),
        session_id,
        wallet_pubkey: wallet_pubkey.to_string(),
        network: claw_types::solana::SolanaNetwork::Devnet,
        description: "durability test".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![],
        created_at: chrono::Utc::now(),
    }
}

fn make_test_tx(from: &Pubkey, to: &Pubkey) -> Transaction {
    let ix = system_instruction::transfer(from, to, 1_000_000);
    let msg = Message::new(&[ix], Some(from));
    let mut tx = Transaction::new_unsigned(msg);
    tx.message.recent_blockhash = Hash::new_unique();
    tx
}

fn make_external_orchestrator(session_id: &SessionId, pubkey: &Pubkey) -> (ExternalWalletStore, SignatureOrchestrator) {
    let external_wallet = ExternalWalletStore::new();
    external_wallet.bind_wallet(session_id, &pubkey.to_string());
    let adapter = Arc::new(ExternalWalletAdapter::new(Arc::new(external_wallet.clone())));
    let orchestrator = SignatureOrchestrator::new(vec![adapter]);
    (external_wallet, orchestrator)
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 1: Pending approval survives restart
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn pending_approval_survives_restart() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&pubkey, &to);
    let proposal = fake_proposal(session_id.clone(), &pubkey.to_string());
    let request_id = Uuid::new_v4();
    let simulation = fake_simulation();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "test".into(),
        rule_name: "test-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };

    let approval_request = ApprovalRequest {
        id: request_id,
        session_id: session_id.clone(),
        transaction_id: proposal.id,
        description: "test transfer".into(),
        policy_verdict: verdict.clone(),
        simulation: simulation.clone(),
        requested_at: chrono::Utc::now(),
        decided:         false,
        required_approver_role: None,
    };

    let tx_bytes = bincode::serialize(&tx).unwrap();

    // Persist the approval.
    durable.persist_approval(
        request_id, &approval_request, &proposal,
        &tx_bytes, &simulation, &verdict,
    ).await.unwrap();

    // Simulate restart: create fresh in-memory stores.
    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let orchestrator2 = SignatureOrchestrator::new(vec![]);
    let completion_meta2 = CompletionMetadataStore::new();

    // Recover.
    let report = durable.recover(
        Duration::from_secs(300), // generous TTL
        &approval_store2,
        &pending_signing2,
        &orchestrator2,
        &completion_meta2,
    ).await;

    assert_eq!(report.recovered_approvals, 1);
    assert_eq!(report.corrupt_approvals, 0);

    // ApprovalStore should have the recovered entry.
    assert_eq!(approval_store2.pending_count(), 1);
    let recovered = approval_store2.get(request_id);
    assert!(recovered.is_some());
    assert_eq!(recovered.unwrap().transaction_id, proposal.id);

    // PendingSigningStore should have the recovered entry.
    assert_eq!(pending_signing2.parked_count(), 1);
    let parked_proposal = pending_signing2.get_proposal(&request_id);
    assert!(parked_proposal.is_some());

    // The recovered entry should be signalable (fresh oneshot).
    let signaled = pending_signing2.signal(request_id, claw_types::approval::ApprovalWorkflowState::Approved);
    assert!(signaled, "recovered entry should be signalable");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 2: Pending external signature survives restart
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn pending_external_signature_survives_restart() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&pubkey, &to);
    let proposal_id = Uuid::new_v4();
    let finalized_tx_bytes = bincode::serialize(&tx).unwrap();

    let request_id = SignatureRequestId::new();

    // Persist the signature request.
    durable.persist_signature(
        &request_id, &session_id, proposal_id,
        &pubkey.to_string(), "test", &finalized_tx_bytes, "external",
    ).await.unwrap();

    // Simulate restart: fresh orchestrator with external adapter.
    let (_ext_wallet, orchestrator2) = make_external_orchestrator(&session_id, &pubkey);

    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let completion_meta2 = CompletionMetadataStore::new();

    let report = durable.recover(
        Duration::from_secs(300),
        &approval_store2,
        &pending_signing2,
        &orchestrator2,
        &completion_meta2,
    ).await;

    assert_eq!(report.recovered_signatures, 1);
    assert_eq!(report.corrupt_signatures, 0);

    // Orchestrator should have the recovered pending entry.
    assert_eq!(orchestrator2.pending_count(), 1);

    // Complete the recovered entry.
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&signed_tx).unwrap();

    let outcome = orchestrator2.complete(&request_id, &signed_bytes).await;
    assert!(outcome.is_ok());
    match outcome.unwrap() {
        SignatureOutcome::Signed { transaction_id, .. } => {
            assert_eq!(transaction_id, proposal_id);
        }
        _ => panic!("expected Signed"),
    }

    assert_eq!(orchestrator2.pending_count(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 3: Completion metadata survives restart
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn completion_metadata_survives_restart() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let request_id = Uuid::new_v4();

    // Persist metadata.
    durable.persist_completion_meta(request_id, Some(7777)).await.unwrap();

    // Simulate restart.
    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let orchestrator2 = SignatureOrchestrator::new(vec![]);
    let completion_meta2 = CompletionMetadataStore::new();

    let report = durable.recover(
        Duration::from_secs(300),
        &approval_store2,
        &pending_signing2,
        &orchestrator2,
        &completion_meta2,
    ).await;

    assert_eq!(report.recovered_meta, 1);

    // CompletionMetadataStore should have the recovered entry.
    let meta = completion_meta2.take(&request_id);
    assert!(meta.is_some());
    assert_eq!(meta.unwrap().fee_lamports, Some(7777));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 4: Stale recovered request fails closed
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn stale_recovered_request_fails_closed() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let request_id = Uuid::new_v4();
    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&pubkey, &to);
    let simulation = fake_simulation();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "test".into(),
        rule_name: "test-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    let proposal = fake_proposal(session_id.clone(), &pubkey.to_string());

    let approval = ApprovalRequest {
        id: request_id,
        session_id: session_id.clone(),
        transaction_id: proposal.id,
        description: "stale test".into(),
        policy_verdict: verdict.clone(),
        simulation: simulation.clone(),
        requested_at: chrono::Utc::now(),
        decided:         false,
        required_approver_role: None,
    };

    let tx_bytes = bincode::serialize(&tx).unwrap();
    durable.persist_approval(
        request_id, &approval, &proposal, &tx_bytes, &simulation, &verdict,
    ).await.unwrap();

    // Also persist a signature and meta.
    let sig_id = SignatureRequestId::new();
    durable.persist_signature(
        &sig_id, &session_id, proposal.id,
        &pubkey.to_string(), "test", &tx_bytes, "external",
    ).await.unwrap();
    durable.persist_completion_meta(sig_id.0, Some(5000)).await.unwrap();

    // Recover with TTL=0 — everything is stale.
    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let orchestrator2 = SignatureOrchestrator::new(vec![]);
    let completion_meta2 = CompletionMetadataStore::new();

    let report = durable.recover(
        Duration::ZERO, // everything expired
        &approval_store2,
        &pending_signing2,
        &orchestrator2,
        &completion_meta2,
    ).await;

    // All rows should be expired, not recovered.
    assert_eq!(report.expired_approvals, 1);
    assert_eq!(report.expired_signatures, 1);
    assert_eq!(report.expired_meta, 1);
    assert_eq!(report.recovered_approvals, 0);
    assert_eq!(report.recovered_signatures, 0);
    assert_eq!(report.recovered_meta, 0);

    // In-memory stores should be empty.
    assert_eq!(approval_store2.pending_count(), 0);
    assert_eq!(pending_signing2.parked_count(), 0);
    assert_eq!(orchestrator2.pending_count(), 0);

    // DB should be clean too.
    let remaining = repo.load_pending_approvals().await.unwrap();
    assert_eq!(remaining.len(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 5: Consumed request not reloaded
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn consumed_request_not_reloaded() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let request_id = Uuid::new_v4();
    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let simulation = fake_simulation();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "test".into(), rule_name: "test-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    let proposal = fake_proposal(session_id.clone(), &pubkey.to_string());
    let approval = ApprovalRequest {
        id: request_id, session_id: session_id.clone(),
        transaction_id: proposal.id, description: "consumed test".into(),
        policy_verdict: verdict.clone(), simulation: simulation.clone(),
        requested_at: chrono::Utc::now(), decided:         false,
        required_approver_role: None,
    };
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let tx_bytes = bincode::serialize(&tx).unwrap();

    durable.persist_approval(
        request_id, &approval, &proposal, &tx_bytes, &simulation, &verdict,
    ).await.unwrap();

    // Simulate consumption (decision made) — mark terminal.
    durable.mark_approval_terminal(request_id, "consumed", Some("test")).await;

    // Verify it's no longer loaded (state != 'pending').
    let rows = repo.load_pending_approvals().await.unwrap();
    assert_eq!(rows.len(), 0);

    // Recovery should find nothing.
    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let orchestrator2 = SignatureOrchestrator::new(vec![]);
    let completion_meta2 = CompletionMetadataStore::new();

    let report = durable.recover(
        Duration::from_secs(300),
        &approval_store2,
        &pending_signing2,
        &orchestrator2,
        &completion_meta2,
    ).await;

    assert_eq!(report.recovered_approvals, 0);
    assert_eq!(approval_store2.pending_count(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 6: Duplicate completion after restart fails closed
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn duplicate_completion_after_restart_fails_closed() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&pubkey, &to);
    let proposal_id = Uuid::new_v4();
    let finalized_tx_bytes = bincode::serialize(&tx).unwrap();
    let request_id = SignatureRequestId::new();

    durable.persist_signature(
        &request_id, &session_id, proposal_id,
        &pubkey.to_string(), "test", &finalized_tx_bytes, "external",
    ).await.unwrap();

    // Simulate restart.
    let (_, orchestrator2) = make_external_orchestrator(&session_id, &pubkey);
    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let completion_meta2 = CompletionMetadataStore::new();

    durable.recover(
        Duration::from_secs(300),
        &approval_store2, &pending_signing2, &orchestrator2, &completion_meta2,
    ).await;

    assert_eq!(orchestrator2.pending_count(), 1);

    // Complete the recovered request.
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&signed_tx).unwrap();

    let outcome1 = orchestrator2.complete(&request_id, &signed_bytes).await;
    assert!(outcome1.is_ok());

    // Mark durable entry terminal (as handler would).
    durable.mark_signature_terminal(&request_id, "consumed", Some("test")).await;

    // Duplicate completion should fail.
    let outcome2 = orchestrator2.complete(&request_id, &signed_bytes).await;
    assert!(matches!(outcome2, Err(SignatureError::RequestNotFound)));

    // Second restart should not reload the consumed request.
    let (_, orchestrator3) = make_external_orchestrator(&session_id, &pubkey);
    let report = durable.recover(
        Duration::from_secs(300),
        &ApprovalStore::new(), &PendingSigningStore::new(),
        &orchestrator3, &CompletionMetadataStore::new(),
    ).await;
    assert_eq!(report.recovered_signatures, 0);
    assert_eq!(orchestrator3.pending_count(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 7: Recovery does not create orphans
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn recovery_does_not_create_orphans() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();

    // Create matching pairs: signature + completion meta.
    let sig_id = SignatureRequestId::new();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();

    durable.persist_signature(
        &sig_id, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "test", &finalized, "external",
    ).await.unwrap();
    durable.persist_completion_meta(sig_id.0, Some(5000)).await.unwrap();

    // Expire everything.
    let report = durable.recover(
        Duration::ZERO,
        &ApprovalStore::new(), &PendingSigningStore::new(),
        &SignatureOrchestrator::new(vec![]), &CompletionMetadataStore::new(),
    ).await;

    assert_eq!(report.expired_signatures, 1);
    assert_eq!(report.expired_meta, 1);

    // Verify DB is completely clean.
    let sigs = repo.load_pending_signatures().await.unwrap();
    let metas = repo.load_completion_meta().await.unwrap();
    assert_eq!(sigs.len(), 0, "no orphan signature rows");
    assert_eq!(metas.len(), 0, "no orphan completion meta rows");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 8: Startup with corrupt record fails closed
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn startup_with_corrupt_record_fails_closed() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    // Insert a corrupt approval row (invalid JSON).
    repo.insert_pending_approval(
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        "corrupt test",
        "wallet123",
        "NOT_VALID_JSON",  // corrupt approval_json
        "deadbeef",        // valid hex but irrelevant since JSON parse fails first
        "{}",
        "{}",
    ).await.unwrap();

    // Insert a corrupt signature row (invalid request_id UUID).
    // We can't easily insert a truly corrupt row since the repo validates types,
    // but we can insert a row with an invalid expected_signer that will fail
    // pubkey parsing during recovery.
    repo.insert_pending_signature(
        &Uuid::new_v4().to_string(),
        "not-a-valid-uuid-session",  // invalid session UUID
        &Uuid::new_v4().to_string(),
        "NOT_A_PUBKEY",              // invalid pubkey
        "corrupt test",
        &[1, 2, 3],                 // some bytes
        "external",
    ).await.unwrap();

    // Recover.
    let approval_store = ApprovalStore::new();
    let pending_signing = PendingSigningStore::new();
    let orchestrator = SignatureOrchestrator::new(vec![]);
    let completion_meta = CompletionMetadataStore::new();

    let report = durable.recover(
        Duration::from_secs(300),
        &approval_store,
        &pending_signing,
        &orchestrator,
        &completion_meta,
    ).await;

    // Both should be counted as corrupt, not recovered.
    assert_eq!(report.corrupt_approvals, 1);
    assert_eq!(report.corrupt_signatures, 1);
    assert_eq!(report.recovered_approvals, 0);
    assert_eq!(report.recovered_signatures, 0);

    // In-memory stores should remain empty.
    assert_eq!(approval_store.pending_count(), 0);
    assert_eq!(pending_signing.parked_count(), 0);
    assert_eq!(orchestrator.pending_count(), 0);

    // Corrupt rows should be cleaned from DB (fail-closed).
    let remaining_approvals = repo.load_pending_approvals().await.unwrap();
    let remaining_sigs = repo.load_pending_signatures().await.unwrap();
    assert_eq!(remaining_approvals.len(), 0, "corrupt row should be removed");
    assert_eq!(remaining_sigs.len(), 0, "corrupt row should be removed");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 9: Multi-stage approval chain survives restart via production recovery
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_stage_chain_recovered_from_durable_state() {
    use claw_types::approval::{ApprovalChainStage, ApprovalDecision, ApprovalOutcome};

    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&pubkey, &to);
    let proposal = fake_proposal(session_id.clone(), &pubkey.to_string());
    let request_id = Uuid::new_v4();
    let simulation = fake_simulation();

    // Verdict with a multi-stage approval chain (risk → treasury).
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "high-value chain".into(),
        rule_name: "chain-rule".into(),
        required_approver_role: None,
        approval_chain: Some(vec![
            ApprovalChainStage { role: "risk".into(), description: "Risk review".into(), min_approvals: 1 },
            ApprovalChainStage { role: "treasury".into(), description: "Treasury sign-off".into(), min_approvals: 1 },
        ]),
    };

    let approval_request = ApprovalRequest {
        id: request_id,
        session_id: session_id.clone(),
        transaction_id: proposal.id,
        description: "multi-stage test".into(),
        policy_verdict: verdict.clone(),
        simulation: simulation.clone(),
        requested_at: chrono::Utc::now(),
        decided: false,
        required_approver_role: None,
    };

    let tx_bytes = bincode::serialize(&tx).unwrap();

    // Persist the approval (this stores the verdict JSON which contains the chain).
    durable.persist_approval(
        request_id, &approval_request, &proposal,
        &tx_bytes, &simulation, &verdict,
    ).await.unwrap();

    // ── Simulate daemon restart ─────────────────────────────────────────────
    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let orchestrator2 = SignatureOrchestrator::new(vec![]);
    let completion_meta2 = CompletionMetadataStore::new();

    let report = durable.recover(
        Duration::from_secs(300),
        &approval_store2,
        &pending_signing2,
        &orchestrator2,
        &completion_meta2,
    ).await;

    assert_eq!(report.recovered_approvals, 1, "should recover the multi-stage approval");
    assert_eq!(report.corrupt_approvals, 0);

    // ── Verify the recovered workflow is MULTI-STAGE ────────────────────────
    let recovered_workflow = approval_store2.get_workflow(request_id);
    assert!(recovered_workflow.is_some(), "workflow should be recovered");
    let wf = recovered_workflow.unwrap();
    assert_eq!(wf.stages.len(), 2, "should have 2 stages (risk → treasury)");
    assert_eq!(wf.stages[0].allowed_roles, vec!["risk"]);
    assert_eq!(wf.stages[1].allowed_roles, vec!["treasury"]);
    assert_eq!(wf.stages[0].min_approvals, 1);
    assert_eq!(wf.stages[1].min_approvals, 1);
    assert!(wf.stages[0].decisions.is_empty(), "stage 0 should be undecided");
    assert!(wf.stages[1].decisions.is_empty(), "stage 1 should be undecided");

    // ── Verify the recovered workflow can be decided through stages ──────────
    // Stage 0: risk approves → StageAdvanced
    let d1 = ApprovalDecision::approve_as(request_id, None, "risk".into());
    let (o1, _) = approval_store2.decide(&d1);
    assert!(matches!(o1, ApprovalOutcome::StageAdvanced { .. }),
        "stage 0 should advance: {:?}", o1);

    // Stage 1: treasury approves → Approved (terminal)
    let d2 = ApprovalDecision::approve_as(request_id, None, "treasury".into());
    let (o2, req) = approval_store2.decide(&d2);
    assert_eq!(o2, ApprovalOutcome::Approved, "should complete after both stages");
    assert!(req.is_some());
    assert_eq!(approval_store2.pending_count(), 0);
}

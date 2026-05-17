//! P1-1 hardening tests — crash-safety semantics.
//!
//! These tests verify that the lifecycle-state approach prevents:
//! - fire-and-forget persistence (persist must succeed before live)
//! - stale resurrection (consumed/expired/rejected rows not reloaded)
//! - ambiguous terminal states

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
    orchestrator::types::{SignatureError, SignatureRequestId},
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
        success: true, error: None, compute_units_used: Some(1000),
        logs: vec![], return_data: None, account_diffs: vec![],
        fee_lamports: Some(5000),
    }
}

fn fake_proposal(session_id: SessionId, wallet_pubkey: &str) -> TransactionProposal {
    TransactionProposal {
        id: Uuid::new_v4(), session_id,
        wallet_pubkey: wallet_pubkey.to_string(),
        network: claw_types::solana::SolanaNetwork::Devnet,
        description: "hardening test".into(),
        transaction_b64: String::new(), instructions_summary: vec![],
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
    let ew = ExternalWalletStore::new();
    ew.bind_wallet(session_id, &pubkey.to_string());
    let adapter = Arc::new(ExternalWalletAdapter::new(Arc::new(ew.clone())));
    (ew, SignatureOrchestrator::new(vec![adapter]))
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 1: persist_approval returns Result — caller can fail closed
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn persist_approval_returns_result() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo);

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let proposal = fake_proposal(session_id.clone(), &pubkey.to_string());
    let simulation = fake_simulation();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "test".into(), rule_name: "test-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    let approval = ApprovalRequest {
        id: Uuid::new_v4(), session_id: session_id.clone(),
        transaction_id: proposal.id, description: "test".into(),
        policy_verdict: verdict.clone(), simulation: simulation.clone(),
        requested_at: chrono::Utc::now(), decided:         false,
        required_approver_role: None,
    };
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let tx_bytes = bincode::serialize(&tx).unwrap();

    // persist_approval returns Ok
    let result = durable.persist_approval(
        approval.id, &approval, &proposal, &tx_bytes, &simulation, &verdict,
    ).await;
    assert!(result.is_ok(), "persist_approval must return Result::Ok on success");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 2: consumed approval row not recovered after restart
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn consumed_approval_not_recovered() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let proposal = fake_proposal(session_id.clone(), &pubkey.to_string());
    let simulation = fake_simulation();
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "test".into(), rule_name: "r".into(),
        required_approver_role: None,
        approval_chain: None,
    };
    let request_id = Uuid::new_v4();
    let approval = ApprovalRequest {
        id: request_id, session_id: session_id.clone(),
        transaction_id: proposal.id, description: "test".into(),
        policy_verdict: verdict.clone(), simulation: simulation.clone(),
        requested_at: chrono::Utc::now(), decided:         false,
        required_approver_role: None,
    };
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let tx_bytes = bincode::serialize(&tx).unwrap();

    durable.persist_approval(
        request_id, &approval, &proposal, &tx_bytes, &simulation, &verdict,
    ).await.unwrap();

    // Mark as consumed (simulating what the handler does on approval)
    durable.mark_approval_terminal(request_id, "consumed", Some("approved")).await;

    // Simulate restart
    let approval_store2 = ApprovalStore::new();
    let pending_signing2 = PendingSigningStore::new();
    let orchestrator2 = SignatureOrchestrator::new(vec![]);
    let completion_meta2 = CompletionMetadataStore::new();

    let report = durable.recover(
        Duration::from_secs(300),
        &approval_store2, &pending_signing2, &orchestrator2, &completion_meta2,
    ).await;

    assert_eq!(report.recovered_approvals, 0, "consumed row must NOT be recovered");
    assert_eq!(approval_store2.pending_count(), 0);
    assert_eq!(pending_signing2.parked_count(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 3: expired signature row not recovered after restart
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn expired_signature_not_recovered() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized_tx_bytes = bincode::serialize(&tx).unwrap();
    let request_id = SignatureRequestId::new();

    durable.persist_signature(
        &request_id, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "test", &finalized_tx_bytes, "external",
    ).await.unwrap();

    // Mark as expired (simulating what the reaper does)
    durable.mark_signature_terminal(&request_id, "expired", Some("TTL")).await;

    // Simulate restart
    let (_, orchestrator2) = make_external_orchestrator(&session_id, &pubkey);

    let report = durable.recover(
        Duration::from_secs(300),
        &ApprovalStore::new(), &PendingSigningStore::new(),
        &orchestrator2, &CompletionMetadataStore::new(),
    ).await;

    assert_eq!(report.recovered_signatures, 0, "expired row must NOT be recovered");
    assert_eq!(orchestrator2.pending_count(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 4: rejected signature row not recovered after restart
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn rejected_signature_not_recovered() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();
    let request_id = SignatureRequestId::new();

    durable.persist_signature(
        &request_id, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "test", &finalized, "external",
    ).await.unwrap();

    // Mark as rejected (simulating verification failure)
    durable.mark_signature_terminal(&request_id, "rejected", Some("tampered")).await;

    // Simulate restart
    let (_, orchestrator2) = make_external_orchestrator(&session_id, &pubkey);

    let report = durable.recover(
        Duration::from_secs(300),
        &ApprovalStore::new(), &PendingSigningStore::new(),
        &orchestrator2, &CompletionMetadataStore::new(),
    ).await;

    assert_eq!(report.recovered_signatures, 0, "rejected row must NOT be recovered");
    assert_eq!(orchestrator2.pending_count(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 5: mark_terminal is idempotent (double-mark does not fail)
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mark_terminal_is_idempotent() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();
    let request_id = SignatureRequestId::new();

    durable.persist_signature(
        &request_id, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "test", &finalized, "external",
    ).await.unwrap();

    // First mark
    durable.mark_signature_terminal(&request_id, "consumed", Some("first")).await;

    // Second mark (idempotent — should not panic or error)
    // The WHERE clause only matches state='pending', so this is a no-op.
    durable.mark_signature_terminal(&request_id, "consumed", Some("second")).await;

    // Still not recoverable
    let pending = repo.load_pending_signatures().await.unwrap();
    assert_eq!(pending.len(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 6: duplicate completion after restart with lifecycle state
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn consumed_signature_not_resurrected_on_second_restart() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();
    let proposal_id = Uuid::new_v4();
    let request_id = SignatureRequestId::new();

    durable.persist_signature(
        &request_id, &session_id, proposal_id,
        &pubkey.to_string(), "test", &finalized, "external",
    ).await.unwrap();

    // First restart: recover
    let (_, orch1) = make_external_orchestrator(&session_id, &pubkey);
    durable.recover(
        Duration::from_secs(300),
        &ApprovalStore::new(), &PendingSigningStore::new(),
        &orch1, &CompletionMetadataStore::new(),
    ).await;
    assert_eq!(orch1.pending_count(), 1);

    // Complete the recovered request
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&signed_tx).unwrap();
    let outcome = orch1.complete(&request_id, &signed_bytes).await;
    assert!(outcome.is_ok());

    // Mark terminal (as handler would)
    durable.mark_signature_terminal(&request_id, "consumed", Some("completed")).await;

    // Second restart: consumed row must NOT be recovered
    let (_, orch2) = make_external_orchestrator(&session_id, &pubkey);
    let report = durable.recover(
        Duration::from_secs(300),
        &ApprovalStore::new(), &PendingSigningStore::new(),
        &orch2, &CompletionMetadataStore::new(),
    ).await;
    assert_eq!(report.recovered_signatures, 0, "consumed row must NOT be resurrected");
    assert_eq!(orch2.pending_count(), 0);

    // Attempting completion on second orchestrator fails
    let outcome2 = orch2.complete(&request_id, &signed_bytes).await;
    assert!(matches!(outcome2, Err(SignatureError::RequestNotFound)));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 7: completion metadata lifecycle state prevents resurrection
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn consumed_completion_meta_not_recovered() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let request_id = Uuid::new_v4();
    durable.persist_completion_meta(request_id, Some(5000)).await.unwrap();

    // Mark consumed
    durable.mark_completion_meta_terminal(request_id, "consumed").await;

    // Recover
    let cm = CompletionMetadataStore::new();
    let report = durable.recover(
        Duration::from_secs(300),
        &ApprovalStore::new(), &PendingSigningStore::new(),
        &SignatureOrchestrator::new(vec![]), &cm,
    ).await;

    assert_eq!(report.recovered_meta, 0);
    assert!(cm.take(&request_id).is_none(), "consumed meta must not be recovered");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 8: purge_terminal cleans up old terminal rows
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn purge_terminal_cleans_old_rows() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();

    // Create and mark terminal
    let rid = SignatureRequestId::new();
    durable.persist_signature(
        &rid, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "test", &finalized, "external",
    ).await.unwrap();
    durable.mark_signature_terminal(&rid, "consumed", None).await;

    durable.persist_completion_meta(rid.0, Some(5000)).await.unwrap();
    durable.mark_completion_meta_terminal(rid.0, "consumed").await;

    // Purge terminal rows
    let purged = repo.purge_terminal(0).await.unwrap();
    assert!(purged >= 2, "should purge at least 2 terminal rows, got {purged}");

    // No pending rows exist
    assert_eq!(repo.load_pending_signatures().await.unwrap().len(), 0);
    assert_eq!(repo.load_completion_meta().await.unwrap().len(), 0);
}

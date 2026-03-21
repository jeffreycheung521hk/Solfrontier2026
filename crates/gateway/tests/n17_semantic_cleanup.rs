//! Semantic cleanup tests — abort_pending vs expire, periodic purge.
//!
//! # Coverage
//!
//! 1. abort_pending removes in-memory pending entry
//! 2. abort_pending on unknown request returns RequestNotFound
//! 3. expire still works for natural TTL path (no regression)
//! 4. abort_pending and expire are semantically distinct at orchestrator level
//! 5. purge_terminal with retention preserves recent terminal rows
//! 6. purge_terminal with retention removes old terminal rows
//! 7. purge_terminal never removes pending rows

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
    orchestrator::types::{SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId},
    pending_signing::PendingSigningStore,
};
use claw_state_store::{
    db::Database,
    pending_state::PendingStateRepository,
};
use claw_types::session::SessionId;

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn setup_db() -> Database {
    Database::open_in_memory().await.expect("in-memory DB")
}

fn make_test_tx(from: &Pubkey, to: &Pubkey) -> Transaction {
    let ix = system_instruction::transfer(from, to, 1_000_000);
    let msg = Message::new(&[ix], Some(from));
    let mut tx = Transaction::new_unsigned(msg);
    tx.message.recent_blockhash = Hash::new_unique();
    tx
}

fn make_external_orchestrator(session_id: &SessionId, pubkey: &Pubkey) -> SignatureOrchestrator {
    let ew = ExternalWalletStore::new();
    ew.bind_wallet(session_id, &pubkey.to_string());
    let adapter = Arc::new(ExternalWalletAdapter::new(Arc::new(ew)));
    SignatureOrchestrator::new(vec![adapter])
}

fn make_pending_request(session_id: &SessionId, pubkey: &Pubkey, tx: &Transaction) -> SignatureRequest {
    SignatureRequest {
        id: SignatureRequestId::new(),
        session_id: session_id.clone(),
        transaction_id: Uuid::new_v4(),
        finalized_tx_bytes: bincode::serialize(tx).unwrap(),
        expected_signer: *pubkey,
        description: "semantic test".into(),
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 1: abort_pending removes in-memory pending entry
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn abort_pending_removes_in_memory_entry() {
    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let orchestrator = make_external_orchestrator(&session_id, &pubkey);

    let request = make_pending_request(&session_id, &pubkey, &tx);
    let request_id = request.id;

    let outcome = orchestrator.submit(request).await.unwrap();
    assert!(matches!(outcome, SignatureOutcome::Pending { .. }));
    assert_eq!(orchestrator.pending_count(), 1);

    let aborted = orchestrator.abort_pending(&request_id);
    assert!(aborted.is_ok());
    assert_eq!(orchestrator.pending_count(), 0);

    // Subsequent abort returns RequestNotFound.
    let again = orchestrator.abort_pending(&request_id);
    assert!(matches!(again, Err(SignatureError::RequestNotFound)));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 2: abort_pending on unknown request returns RequestNotFound
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn abort_pending_unknown_returns_not_found() {
    let orchestrator = SignatureOrchestrator::new(vec![]);
    let unknown_id = SignatureRequestId::new();
    let result = orchestrator.abort_pending(&unknown_id);
    assert!(matches!(result, Err(SignatureError::RequestNotFound)));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 3: expire still works for natural TTL (no regression)
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn expire_still_works_for_natural_ttl() {
    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let orchestrator = make_external_orchestrator(&session_id, &pubkey);

    let request = make_pending_request(&session_id, &pubkey, &tx);
    let request_id = request.id;
    let proposal_id = request.transaction_id;

    orchestrator.submit(request).await.unwrap();
    assert_eq!(orchestrator.pending_count(), 1);

    // Natural TTL expiry via expired_candidates + expire.
    let candidates = orchestrator.expired_candidates(Duration::ZERO);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0], request_id);

    let expired_req = orchestrator.expire(&request_id).unwrap();
    assert_eq!(expired_req.transaction_id, proposal_id);
    assert_eq!(orchestrator.pending_count(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 4: abort_pending and expire are semantically distinct
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn abort_and_expire_are_distinct_paths() {
    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let orchestrator = make_external_orchestrator(&session_id, &pubkey);

    // Submit two requests.
    let req1 = make_pending_request(&session_id, &pubkey, &tx);
    let req2 = make_pending_request(&session_id, &pubkey, &tx);
    let id1 = req1.id;
    let id2 = req2.id;

    orchestrator.submit(req1).await.unwrap();
    orchestrator.submit(req2).await.unwrap();
    assert_eq!(orchestrator.pending_count(), 2);

    // Abort first (persistence rollback semantics).
    let aborted = orchestrator.abort_pending(&id1).unwrap();
    assert_eq!(aborted.id, id1);

    // Expire second (natural TTL semantics).
    let expired = orchestrator.expire(&id2).unwrap();
    assert_eq!(expired.id, id2);

    // Both removed; both return the original request.
    assert_eq!(orchestrator.pending_count(), 0);

    // Neither is re-abortable or re-expirable.
    assert!(matches!(orchestrator.abort_pending(&id1), Err(SignatureError::RequestNotFound)));
    assert!(matches!(orchestrator.expire(&id2), Err(SignatureError::RequestNotFound)));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 5: purge_terminal with retention preserves recent terminal rows
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn purge_with_retention_preserves_recent_terminal() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();

    // Create and mark terminal (just created → very recent).
    let rid = SignatureRequestId::new();
    durable.persist_signature_and_meta(
        &rid, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "test", &finalized, "external", Some(5000),
    ).await.unwrap();
    durable.mark_signature_terminal(&rid, "consumed", None).await;
    durable.mark_completion_meta_terminal(rid.0, "consumed").await;

    // Purge with 14-day retention — row was just created, should be preserved.
    let retention_14d = 14 * 24 * 60 * 60 * 1000i64;
    let purged = repo.purge_terminal(retention_14d).await.unwrap();
    assert_eq!(purged, 0, "recent terminal rows should NOT be purged");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 6: purge_terminal with zero retention removes all terminal rows
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn purge_with_zero_retention_removes_all_terminal() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();

    let rid = SignatureRequestId::new();
    durable.persist_signature_and_meta(
        &rid, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "test", &finalized, "external", Some(5000),
    ).await.unwrap();
    durable.mark_signature_terminal(&rid, "consumed", None).await;
    durable.mark_completion_meta_terminal(rid.0, "consumed").await;

    // Purge with zero retention — everything terminal should go.
    let purged = repo.purge_terminal(0).await.unwrap();
    assert!(purged >= 2, "terminal rows should be purged, got {purged}");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 7: purge_terminal never removes pending rows regardless of retention
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn purge_never_removes_pending_rows() {
    let db = setup_db().await;
    let repo = PendingStateRepository::new(db.pool().clone());
    let durable = DurablePendingState::new(repo.clone());

    let session_id = SessionId::from(Uuid::new_v4());
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let tx = make_test_tx(&pubkey, &Pubkey::new_unique());
    let finalized = bincode::serialize(&tx).unwrap();

    // Create a pending (non-terminal) entry.
    let rid = SignatureRequestId::new();
    durable.persist_signature_and_meta(
        &rid, &session_id, Uuid::new_v4(),
        &pubkey.to_string(), "pending-test", &finalized, "external", Some(1000),
    ).await.unwrap();

    // Purge with zero retention — most aggressive possible.
    let purged = repo.purge_terminal(0).await.unwrap();
    assert_eq!(purged, 0, "pending rows must NEVER be purged");

    // Verify pending row still exists.
    let sigs = repo.load_pending_signatures().await.unwrap();
    assert_eq!(sigs.len(), 1);
    assert_eq!(sigs[0].request_id, rid.0.to_string());

    let metas = repo.load_completion_meta().await.unwrap();
    assert_eq!(metas.len(), 1);
}

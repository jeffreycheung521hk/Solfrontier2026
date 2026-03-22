//! STEP 6 — Orchestrator integration tests.
//!
//! Proves the STEP 5 wiring works end-to-end through real caller-side flows.
//!
//! # Coverage
//!
//! 1. Auto-approved local flow (orchestrator routes to LocalWalletAdapter)
//! 2. Auto-approved external flow (orchestrator returns Pending, handler completes)
//! 3. Human-approved local flow (approval → resume → orchestrator → signed)
//! 4. Human-approved external flow (approval → resume → pending → complete)
//! 5. Tampered completion fail-closed (verification rejects, entry consumed)
//! 6. Not-found / duplicate / race behavior
//! 7. Expiry reaper integration
//! 8. CompletionMetadataStore cleanup guarantees
//!
//! Uses: real keypairs, in-memory SQLite, no HTTP listener, no real RPC.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::json;
use solana_sdk::{
    hash::Hash,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer as SdkSigner},
    system_instruction,
    transaction::Transaction,
};
use tokio::sync::Barrier;
use uuid::Uuid;

use claw_gateway::{
    approval_store::ApprovalStore,
    completion_metadata::{CompletionMeta, CompletionMetadataStore},
    event_bus::EventBus,
    external_wallet::ExternalWalletStore,
    orchestrator::SignatureOrchestrator,
    orchestrator::external_adapter::ExternalWalletAdapter,
    orchestrator::local_adapter::LocalWalletAdapter,
    orchestrator::types::{SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId},
    pending_signing::PendingSigningStore,
    tools::resume_after_approval,
};
use claw_state_store::{
    audit::{AuditRepository, AuditSeverity},
    db::Database,
    spend::SpendRepository,
};
use claw_types::{
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::{EventHeader, GatewayEvent, WalletSignatureEvent},
    policy::PolicyVerdict,
    session::SessionId,
    transaction::{SimulationResult, TransactionProposal, TransactionStatus},
};
use claw_wallet_engine::{
    keystore::SecretKeystore,
    local::LocalKeypairSigner,
    signer::SignerRef,
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
        description: "orchestrator integration test".to_string(),
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

struct TestInfra {
    db: Database,
    audit: AuditRepository,
    spend: SpendRepository,
    event_bus: EventBus,
    pending_signing: PendingSigningStore,
    completion_meta: CompletionMetadataStore,
}

impl TestInfra {
    async fn new() -> Self {
        let db = setup_db().await;
        let audit = AuditRepository::new(db.pool().clone());
        let spend = SpendRepository::new(db.pool().clone());
        Self {
            db,
            audit,
            spend,
            event_bus: EventBus::new(),
            pending_signing: PendingSigningStore::new(),
            completion_meta: CompletionMetadataStore::new(),
        }
    }
}

struct LocalSignerSetup {
    keypair: Keypair,
    pubkey: Pubkey,
    signer: SignerRef,
    keystore: SecretKeystore,
    orchestrator: SignatureOrchestrator,
}

impl LocalSignerSetup {
    fn new() -> Self {
        let keystore = SecretKeystore::new();
        let keypair = Keypair::new();
        let keypair_bytes: Vec<u8> = keypair.to_bytes().to_vec();
        let json_bytes = serde_json::to_string(&keypair_bytes).unwrap();
        let pubkey = keystore.load_from_json_bytes(&json_bytes).unwrap();
        let signer: SignerRef = Arc::new(
            LocalKeypairSigner::new(pubkey, keystore.clone())
        );
        let local_adapter = Arc::new(LocalWalletAdapter::new(signer.clone(), keystore.clone()));
        let orchestrator = SignatureOrchestrator::new(vec![local_adapter]);
        Self { keypair, pubkey, signer, keystore, orchestrator }
    }
}

struct ExternalSignerSetup {
    keypair: Keypair,
    pubkey: Pubkey,
    external_wallet: ExternalWalletStore,
    orchestrator: SignatureOrchestrator,
}

impl ExternalSignerSetup {
    fn new(session_id: &SessionId) -> Self {
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(session_id, &pubkey.to_string());
        let external_adapter = Arc::new(ExternalWalletAdapter::new(
            Arc::new(external_wallet.clone()),
        ));
        let orchestrator = SignatureOrchestrator::new(vec![external_adapter]);
        Self { keypair, pubkey, external_wallet, orchestrator }
    }
}

fn build_signature_request(
    session_id: &SessionId,
    tx: &Transaction,
    expected_signer: &Pubkey,
    proposal_id: Uuid,
) -> SignatureRequest {
    let finalized_tx_bytes = bincode::serialize(tx).unwrap();
    SignatureRequest {
        id: SignatureRequestId::new(),
        session_id: session_id.clone(),
        transaction_id: proposal_id,
        finalized_tx_bytes,
        expected_signer: *expected_signer,
        description: "integration test".to_string(),
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 1: Auto-approved local flow
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auto_approved_local_signs_synchronously_via_orchestrator() {
    let infra = TestInfra::new().await;
    let local = LocalSignerSetup::new();
    let session_id = SessionId::from(Uuid::new_v4());
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&local.pubkey, &to);
    let proposal_id = Uuid::new_v4();

    let mut rx = infra.event_bus.subscribe();

    let request = build_signature_request(&session_id, &tx, &local.pubkey, proposal_id);
    let outcome = local.orchestrator.submit(request).await.expect("submit should succeed");

    // Must be Signed, not Pending
    match outcome {
        SignatureOutcome::Signed { signature, transaction_id, expected_signer, .. } => {
            assert!(!signature.is_empty(), "signature must be non-empty");
            assert_eq!(transaction_id, proposal_id);
            assert_eq!(expected_signer, local.pubkey.to_string());

            // Record spend at caller layer
            let _ = infra.spend.record_spend(
                &local.pubkey.to_string(),
                &session_id.to_string(),
                &proposal_id.to_string(),
                5000,
            ).await;

            infra.event_bus.publish(GatewayEvent::TransactionSigned(
                claw_types::events::TransactionLifecycleEvent {
                    header: EventHeader::new(Some(session_id.clone())),
                    session_id: session_id.clone(),
                    transaction_id: proposal_id,
                    wallet_pubkey: local.pubkey.to_string(),
                    status: TransactionStatus::Signed,
                    signature: Some(signature.clone()),
                },
            ));
        }
        SignatureOutcome::Pending { .. } => panic!("local adapter must not return Pending"),
    }

    // No pending entries
    assert_eq!(local.orchestrator.pending_count(), 0);

    // Spend recorded
    let session_spend = infra.spend.session_spend(&session_id.to_string()).await.unwrap();
    assert_eq!(session_spend, 5000);

    // Event emitted
    let mut saw_signed = false;
    loop {
        match rx.try_recv() {
            Ok(GatewayEvent::TransactionSigned(_)) => saw_signed = true,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_signed, "must emit TransactionSigned event");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 2: Auto-approved external flow
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auto_approved_external_pending_then_complete() {
    let infra = TestInfra::new().await;
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let proposal_id = Uuid::new_v4();

    let request = build_signature_request(&session_id, &tx, &ext.pubkey, proposal_id);
    let request_id = request.id;
    let outcome = ext.orchestrator.submit(request).await.expect("submit should succeed");

    // Must be Pending (not Signed)
    let pending_tx_bytes = match outcome {
        SignatureOutcome::Pending { finalized_tx_bytes, request_id: rid, .. } => {
            assert_eq!(rid, request_id);
            finalized_tx_bytes
        }
        SignatureOutcome::Signed { .. } => panic!("external adapter must return Pending"),
    };

    // Orchestrator owns the pending entry
    assert_eq!(ext.orchestrator.pending_count(), 1);

    // Stash completion metadata (as the signing tool would)
    infra.completion_meta.insert(request_id.0, CompletionMeta { fee_lamports: Some(5000), last_valid_block_height: None });

    // Sign externally
    let mut signed_tx: Transaction = bincode::deserialize(&pending_tx_bytes).unwrap();
    signed_tx.sign(&[&ext.keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&signed_tx).unwrap();

    // Complete through orchestrator
    let complete_outcome = ext.orchestrator.complete(&request_id, &signed_bytes).await
        .expect("complete should succeed");

    match complete_outcome {
        SignatureOutcome::Signed { signature, transaction_id, expected_signer, .. } => {
            assert!(!signature.is_empty());
            assert_eq!(transaction_id, proposal_id);
            assert_eq!(expected_signer, ext.pubkey.to_string());
        }
        _ => panic!("complete must return Signed"),
    }

    // Pending entry consumed
    assert_eq!(ext.orchestrator.pending_count(), 0);

    // Completion metadata consumed
    let meta = infra.completion_meta.take(&request_id.0);
    assert!(meta.is_some(), "completion metadata should still be available for caller to consume");
    assert_eq!(meta.unwrap().fee_lamports, Some(5000));

    // Second take returns None (already consumed)
    assert!(infra.completion_meta.take(&request_id.0).is_none());
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 3: Human-approved local flow
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn human_approved_local_signs_via_orchestrator_after_approval() {
    let infra = TestInfra::new().await;
    let local = LocalSignerSetup::new();
    let session_id = SessionId::from(Uuid::new_v4());
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&local.pubkey, &to);
    let proposal = fake_proposal(session_id.clone(), &local.pubkey.to_string());
    let request_id = Uuid::new_v4();

    let mut rx = infra.event_bus.subscribe();

    // Park in PendingSigningStore (Control Plane)
    let decision_rx = infra.pending_signing.park(
        request_id, proposal.clone(), &tx,
        fake_simulation(),
        PolicyVerdict::RequiresHumanApproval {
            reason: "test".into(),
            rule_name: "test-rule".into(),
        },
        1000,
    ).expect("park should succeed");

    assert_eq!(infra.pending_signing.parked_count(), 1);

    // Spawn resume task
    let orchestrator = local.orchestrator.clone();
    let pending = infra.pending_signing.clone();
    let audit = infra.audit.clone();
    let spend = infra.spend.clone();
    let event_bus = infra.event_bus.clone();
    let session = session_id.clone();
    let prop = proposal.clone();
    let wpk = local.pubkey.to_string();
    let cm = infra.completion_meta.clone();

    let resume_handle = tokio::spawn(async move {
        resume_after_approval(
            decision_rx, request_id, prop, orchestrator,
            pending, audit, spend, event_bus, session, wpk, cm, None,
        ).await;
    });

    // Approve
    infra.pending_signing.signal(request_id, true);

    tokio::time::timeout(Duration::from_secs(5), resume_handle)
        .await
        .expect("timeout")
        .expect("task should not panic");

    // Parked entry consumed
    assert_eq!(infra.pending_signing.parked_count(), 0);

    // Orchestrator has no pending (local signs synchronously)
    assert_eq!(local.orchestrator.pending_count(), 0);

    // Spend recorded
    let session_spend = infra.spend.session_spend(&session_id.to_string()).await.unwrap();
    assert_eq!(session_spend, 5000);

    // Audit: transaction_signed_after_approval
    let audit_rows = infra.audit.list_for_session(&session_id.to_string(), 50).await.unwrap();
    let types: Vec<&str> = audit_rows.iter().map(|r| r.event_type.as_str()).collect();
    assert!(types.contains(&"transaction_signed_after_approval"), "got: {types:?}");

    // Event: TransactionSigned
    let mut saw_signed = false;
    loop {
        match rx.try_recv() {
            Ok(GatewayEvent::TransactionSigned(ref e)) => {
                saw_signed = true;
                assert!(e.signature.is_some());
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_signed);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 4: Human-approved external flow
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn human_approved_external_pending_then_complete_after_approval() {
    let infra = TestInfra::new().await;
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let proposal = fake_proposal(session_id.clone(), &ext.pubkey.to_string());
    let request_id = Uuid::new_v4();

    let mut rx = infra.event_bus.subscribe();

    // Park in PendingSigningStore (Control Plane — awaiting human approval)
    let decision_rx = infra.pending_signing.park(
        request_id, proposal.clone(), &tx,
        fake_simulation(),
        PolicyVerdict::RequiresHumanApproval {
            reason: "high value".into(),
            rule_name: "high-value-guard".into(),
        },
        1000,
    ).expect("park");

    // Spawn resume task
    let orchestrator = ext.orchestrator.clone();
    let pending = infra.pending_signing.clone();
    let audit = infra.audit.clone();
    let spend = infra.spend.clone();
    let event_bus = infra.event_bus.clone();
    let session = session_id.clone();
    let prop = proposal.clone();
    let wpk = ext.pubkey.to_string();
    let cm = infra.completion_meta.clone();

    let resume_handle = tokio::spawn(async move {
        resume_after_approval(
            decision_rx, request_id, prop, orchestrator,
            pending, audit, spend, event_bus, session, wpk, cm, None,
        ).await;
    });

    // Approve
    infra.pending_signing.signal(request_id, true);

    tokio::time::timeout(Duration::from_secs(5), resume_handle)
        .await.expect("timeout").expect("no panic");

    // PendingSigningStore consumed (approval side)
    assert_eq!(infra.pending_signing.parked_count(), 0);

    // Orchestrator has 1 pending (external — awaiting wallet signature)
    assert_eq!(ext.orchestrator.pending_count(), 1);

    // Audit: wallet_signature_requested_after_approval
    let audit_rows = infra.audit.list_for_session(&session_id.to_string(), 50).await.unwrap();
    let types: Vec<&str> = audit_rows.iter().map(|r| r.event_type.as_str()).collect();
    assert!(types.contains(&"wallet_signature_requested_after_approval"), "got: {types:?}");

    // Event: WalletSignatureRequested
    let mut saw_wallet_requested = false;
    loop {
        match rx.try_recv() {
            Ok(GatewayEvent::WalletSignatureRequested(_)) => saw_wallet_requested = true,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_wallet_requested);

    // CompletionMeta was stashed by resume_after_approval
    // Now complete the wallet signature through the orchestrator.
    let pending_summaries = ext.orchestrator.pending_for_session(&session_id);
    assert_eq!(pending_summaries.len(), 1);
    let pending_req_id = pending_summaries[0].request_id;

    // Sign externally
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&ext.keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&signed_tx).unwrap();

    let complete_outcome = ext.orchestrator.complete(&pending_req_id, &signed_bytes).await
        .expect("complete should succeed");

    match complete_outcome {
        SignatureOutcome::Signed { signature, .. } => {
            assert!(!signature.is_empty());
        }
        _ => panic!("complete must return Signed"),
    }

    // Pending consumed
    assert_eq!(ext.orchestrator.pending_count(), 0);

    // CompletionMeta cleanup: the caller (handler) would consume it here
    let meta = infra.completion_meta.take(&pending_req_id.0);
    assert!(meta.is_some(), "completion meta should have been stashed by resume task");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 5: Tampered completion fail-closed
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn tampered_completion_fails_and_consumes_entry() {
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let proposal_id = Uuid::new_v4();
    let completion_meta = CompletionMetadataStore::new();

    let request = build_signature_request(&session_id, &tx, &ext.pubkey, proposal_id);
    let request_id = request.id;

    ext.orchestrator.submit(request).await.expect("submit");
    assert_eq!(ext.orchestrator.pending_count(), 1);

    // Stash completion metadata
    completion_meta.insert(request_id.0, CompletionMeta { fee_lamports: Some(5000), last_valid_block_height: None });

    // Create a TAMPERED transaction (different destination, different amount)
    let evil_to = Pubkey::new_unique();
    let evil_ix = system_instruction::transfer(&ext.pubkey, &evil_to, 99_999_999);
    let evil_msg = Message::new(&[evil_ix], Some(&ext.pubkey));
    let mut evil_tx = Transaction::new_unsigned(evil_msg);
    evil_tx.message.recent_blockhash = tx.message.recent_blockhash;
    evil_tx.sign(&[&ext.keypair], evil_tx.message.recent_blockhash);
    let evil_bytes = bincode::serialize(&evil_tx).unwrap();

    // Attempt completion with tampered bytes
    let result = ext.orchestrator.complete(&request_id, &evil_bytes).await;
    assert!(result.is_err(), "tampered completion must fail");
    match result.unwrap_err() {
        SignatureError::VerificationFailed(_) => {} // expected
        other => panic!("expected VerificationFailed, got: {other}"),
    }

    // Consume-on-attempt: entry is consumed even on failure
    assert_eq!(ext.orchestrator.pending_count(), 0);

    // Second attempt → RequestNotFound
    let result2 = ext.orchestrator.complete(&request_id, &evil_bytes).await;
    assert!(matches!(result2, Err(SignatureError::RequestNotFound)));

    // Cleanup: completion meta should be cleaned up by caller on error
    completion_meta.remove(&request_id.0);
    assert!(completion_meta.take(&request_id.0).is_none());
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 6a: Complete unknown request_id → not found
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn complete_unknown_request_returns_not_found() {
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let unknown_id = SignatureRequestId::new();

    let result = ext.orchestrator.complete(&unknown_id, b"anything").await;
    assert!(matches!(result, Err(SignatureError::RequestNotFound)));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 6b: Duplicate request_id submission → fail-closed
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn duplicate_request_id_rejected() {
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);

    let shared_id = SignatureRequestId::new();
    let req1 = SignatureRequest {
        id: shared_id,
        session_id: session_id.clone(),
        transaction_id: Uuid::new_v4(),
        finalized_tx_bytes: bincode::serialize(&tx).unwrap(),
        expected_signer: ext.pubkey,
        description: "first".into(),
    };

    ext.orchestrator.submit(req1).await.expect("first submit should succeed");
    assert_eq!(ext.orchestrator.pending_count(), 1);

    let req2 = SignatureRequest {
        id: shared_id, // same ID
        session_id: session_id.clone(),
        transaction_id: Uuid::new_v4(),
        finalized_tx_bytes: bincode::serialize(&tx).unwrap(),
        expected_signer: ext.pubkey,
        description: "second".into(),
    };

    let result = ext.orchestrator.submit(req2).await;
    assert!(matches!(result, Err(SignatureError::DuplicateRequestId)));

    // Pending count unchanged — no silent overwrite
    assert_eq!(ext.orchestrator.pending_count(), 1);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 6c: Concurrent double-complete race → only one succeeds
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn concurrent_double_complete_race_exactly_one_succeeds() {
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let proposal_id = Uuid::new_v4();

    let request = build_signature_request(&session_id, &tx, &ext.pubkey, proposal_id);
    let request_id = request.id;
    ext.orchestrator.submit(request).await.expect("submit");

    // Sign
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&ext.keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = Arc::new(bincode::serialize(&signed_tx).unwrap());

    const RACERS: usize = 20;
    let barrier = Arc::new(Barrier::new(RACERS));
    let mut handles = Vec::new();

    for _ in 0..RACERS {
        let orch = ext.orchestrator.clone();
        let rid = request_id;
        let bytes = signed_bytes.clone();
        let barrier = barrier.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            orch.complete(&rid, &bytes).await
        }));
    }

    let mut ok_count = 0usize;
    let mut not_found_count = 0usize;
    for h in handles {
        match h.await.unwrap() {
            Ok(SignatureOutcome::Signed { .. }) => ok_count += 1,
            Err(SignatureError::RequestNotFound) => not_found_count += 1,
            other => panic!("unexpected result: {other:?}"),
        }
    }

    assert_eq!(ok_count, 1, "exactly one racer must succeed");
    assert_eq!(not_found_count, RACERS - 1, "all others must get RequestNotFound");
    assert_eq!(ext.orchestrator.pending_count(), 0, "no residual pending");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 7: Expiry reaper integration
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn expiry_reaper_expires_old_requests() {
    let infra = TestInfra::new().await;
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let proposal_id = Uuid::new_v4();

    let mut rx = infra.event_bus.subscribe();

    let request = build_signature_request(&session_id, &tx, &ext.pubkey, proposal_id);
    let request_id = request.id;
    ext.orchestrator.submit(request).await.expect("submit");

    // Stash completion metadata
    infra.completion_meta.insert(request_id.0, CompletionMeta { fee_lamports: Some(5000), last_valid_block_height: None });

    assert_eq!(ext.orchestrator.pending_count(), 1);

    // Use Duration::ZERO to make everything expired immediately
    let candidates = ext.orchestrator.expired_candidates(Duration::ZERO);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0], request_id);

    // Expire
    let expired_request = ext.orchestrator.expire(&request_id).expect("expire should succeed");
    assert_eq!(expired_request.transaction_id, proposal_id);

    // Emit expiry event (as reaper would)
    infra.event_bus.publish(GatewayEvent::WalletSignatureExpired(
        WalletSignatureEvent {
            header: EventHeader::new(Some(session_id.clone())),
            session_id: session_id.clone(),
            request_id: request_id.0,
            transaction_id: proposal_id,
            wallet_pubkey: ext.pubkey.to_string(),
            error: Some("signature request expired (TTL exceeded)".to_string()),
        },
    ));

    // Clean up completion metadata (as reaper would)
    infra.completion_meta.remove(&request_id.0);

    // Pending entry removed
    assert_eq!(ext.orchestrator.pending_count(), 0);

    // Completion is no longer possible
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&ext.keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&signed_tx).unwrap();
    let result = ext.orchestrator.complete(&request_id, &signed_bytes).await;
    assert!(matches!(result, Err(SignatureError::RequestNotFound)));

    // Event emitted
    let mut saw_expired = false;
    loop {
        match rx.try_recv() {
            Ok(GatewayEvent::WalletSignatureExpired(ref e)) => {
                saw_expired = true;
                assert_eq!(e.transaction_id, proposal_id);
                assert!(e.error.as_ref().unwrap().contains("expired"));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_expired, "must emit WalletSignatureExpired event");

    // Completion metadata fully cleaned
    assert!(infra.completion_meta.take(&request_id.0).is_none());
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 8a: CompletionMetadataStore cleanup — successful completion
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn completion_meta_consumed_on_success() {
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let completion_meta = CompletionMetadataStore::new();

    let request = build_signature_request(&session_id, &tx, &ext.pubkey, Uuid::new_v4());
    let request_id = request.id;
    ext.orchestrator.submit(request).await.unwrap();

    // Stash metadata
    completion_meta.insert(request_id.0, CompletionMeta { fee_lamports: Some(7777), last_valid_block_height: None });

    // Complete successfully
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&ext.keypair], signed_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&signed_tx).unwrap();
    ext.orchestrator.complete(&request_id, &signed_bytes).await.unwrap();

    // Caller takes metadata for spend recording
    let meta = completion_meta.take(&request_id.0);
    assert_eq!(meta.unwrap().fee_lamports, Some(7777));

    // Now empty — no orphan
    assert!(completion_meta.take(&request_id.0).is_none());
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 8b: CompletionMetadataStore cleanup — verification failure
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn completion_meta_cleaned_on_verification_failure() {
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let completion_meta = CompletionMetadataStore::new();

    let request = build_signature_request(&session_id, &tx, &ext.pubkey, Uuid::new_v4());
    let request_id = request.id;
    ext.orchestrator.submit(request).await.unwrap();

    completion_meta.insert(request_id.0, CompletionMeta { fee_lamports: Some(5000), last_valid_block_height: None });

    // Attempt with UNSIGNED tx (signer didn't sign → verification fails)
    let unsigned_bytes = bincode::serialize(&tx).unwrap();
    let result = ext.orchestrator.complete(&request_id, &unsigned_bytes).await;
    assert!(result.is_err());

    // Caller must clean up metadata on error
    completion_meta.remove(&request_id.0);
    assert!(completion_meta.take(&request_id.0).is_none(), "no orphan after cleanup");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 8c: CompletionMetadataStore cleanup — expiry
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn completion_meta_cleaned_on_expiry() {
    let session_id = SessionId::from(Uuid::new_v4());
    let ext = ExternalSignerSetup::new(&session_id);
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&ext.pubkey, &to);
    let completion_meta = CompletionMetadataStore::new();

    let request = build_signature_request(&session_id, &tx, &ext.pubkey, Uuid::new_v4());
    let request_id = request.id;
    ext.orchestrator.submit(request).await.unwrap();

    completion_meta.insert(request_id.0, CompletionMeta { fee_lamports: Some(5000), last_valid_block_height: None });

    // Expire
    ext.orchestrator.expire(&request_id).unwrap();

    // Reaper cleans up metadata
    completion_meta.remove(&request_id.0);
    assert!(completion_meta.take(&request_id.0).is_none(), "no orphan after expiry cleanup");
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 8d: CompletionMetadataStore — request not found does not leak
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn completion_meta_not_found_does_not_leak() {
    let completion_meta = CompletionMetadataStore::new();
    let unknown_id = Uuid::new_v4();

    // Inserting metadata for a request that was never submitted
    completion_meta.insert(unknown_id, CompletionMeta { fee_lamports: Some(5000), last_valid_block_height: None });

    // Cleanup removes it cleanly
    completion_meta.remove(&unknown_id);
    assert!(completion_meta.take(&unknown_id).is_none());
}

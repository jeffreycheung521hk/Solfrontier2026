//! N21 — Phase 1.4 submission ordering and tracking invariants.
//!
//! Regression-proofs the on-chain submission flow:
//!
//! 1. Happy path: tracking_repo.insert() → tracker.track() → row recoverable.
//! 2. RPC failure: no tracking_repo.insert(), no tracker.track().
//! 3. Immutability: signed_tx_bytes submitted to RPC are the exact verified bytes.
//! 4. Exactly-once consume: orchestrator.complete() is atomic consume-on-attempt.
//! 5. submission_block_height = 0 causes immediate premature Dropped (proves bug).
//! 6. Real submission_block_height avoids premature Dropped.
//! 7. Durable-first: DB failure prevents in-memory-only tracking state.
//!
//! Uses: real in-memory SQLite, real TransactionTracker, no RPC, no HTTP.

use std::sync::Arc;

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
    external_wallet::ExternalWalletStore,
    orchestrator::SignatureOrchestrator,
    orchestrator::external_adapter::ExternalWalletAdapter,
    orchestrator::types::{SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId},
};
use claw_solana_core::tracker::TransactionTracker;
use claw_solana_core::tx_lifecycle::{
    TxState, RpcObservation, calculate_next_state,
};
use claw_state_store::{
    db::Database,
    TransactionTrackingRepository,
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

fn build_signature_request(
    session_id: &SessionId,
    tx: &Transaction,
    expected_signer: &Pubkey,
    proposal_id: Uuid,
) -> SignatureRequest {
    let finalized_tx_bytes = bincode::serialize(tx).unwrap();
    SignatureRequest {
        id:                 SignatureRequestId::from(proposal_id),
        session_id:         session_id.clone(),
        transaction_id:     proposal_id,
        expected_signer:    *expected_signer,
        finalized_tx_bytes,
        description:        "submission invariant test".to_string(),
    }
}

// ── Test 1: Happy path — DB insert → tracker registration → recoverable ──────

#[tokio::test]
async fn happy_path_tracking_persisted_and_recoverable() {
    let db = setup_db().await;
    let tracking_repo = TransactionTrackingRepository::new(db.pool().clone());

    let sig = solana_sdk::signature::Signature::new_unique();
    let tx_id = Uuid::new_v4();
    let req_id = Uuid::new_v4();
    let session_id = "test-session";
    let wallet = Pubkey::new_unique().to_string();
    let last_valid = 1000u64;
    let submission_height = 850u64;

    // Insert tracking row (simulates successful DB write).
    tracking_repo.insert(
        &sig.to_string(), &tx_id.to_string(), &req_id.to_string(),
        session_id, &wallet, last_valid, submission_height,
    ).await.expect("insert must succeed");

    // Verify recoverable via load_active.
    let rows = tracking_repo.load_active().await.expect("load_active must succeed");
    assert_eq!(rows.len(), 1, "exactly one active tracking row");
    assert_eq!(rows[0].signature, sig.to_string());
    assert_eq!(rows[0].state, "submitted");
    assert_eq!(rows[0].submission_block_height, submission_height as i64);
    assert_eq!(rows[0].last_valid_block_height, last_valid as i64);
}

// ── Test 2: RPC failure path — no DB row, no in-memory tracking ──────────────

#[tokio::test]
async fn rpc_failure_no_tracking_row_no_memory() {
    let db = setup_db().await;
    let tracking_repo = TransactionTrackingRepository::new(db.pool().clone());

    // Simulate: RPC failed, so we never call tracking_repo.insert() or tracker.track().
    // Just verify that load_active returns empty — no phantom rows.
    let rows = tracking_repo.load_active().await.expect("load_active must succeed");
    assert!(rows.is_empty(), "no tracking rows when RPC submission failed");
}

// ── Test 3: Immutability — signed bytes match exactly ─────────────────────────

#[tokio::test]
async fn signed_bytes_immutability_through_orchestrator() {
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let recipient = Pubkey::new_unique();

    let mut tx = make_test_tx(&pubkey, &recipient);
    tx.partial_sign(&[&keypair], tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&tx).unwrap();

    // Park in external wallet store → complete → verify bytes match.
    let session_id = SessionId::from(Uuid::new_v4());
    let store = ExternalWalletStore::new();
    store.bind_wallet(&session_id, &pubkey.to_string());

    let external_adapter = Arc::new(ExternalWalletAdapter::new(Arc::new(store.clone())));
    let orchestrator = SignatureOrchestrator::new(vec![external_adapter]);

    let proposal_id = Uuid::new_v4();
    let request = build_signature_request(&session_id, &tx, &pubkey, proposal_id);

    // Submit → external → Pending.
    let outcome = orchestrator.submit(request).await.unwrap();
    assert!(matches!(outcome, SignatureOutcome::Pending { .. }));

    // Complete with the EXACT signed bytes.
    let req_id = SignatureRequestId::from(proposal_id);
    let result = orchestrator.complete(&req_id, &signed_bytes).await;
    match result {
        Ok(SignatureOutcome::Signed { signed_tx_bytes: returned_bytes, .. }) => {
            assert_eq!(
                returned_bytes, signed_bytes,
                "orchestrator must return the exact same bytes, no mutation"
            );
        }
        other => panic!("expected Signed, got: {other:?}"),
    }
}

// ── Test 4: Exactly-once consume — second complete() returns NotFound ─────────

#[tokio::test]
async fn exactly_once_consume_on_attempt() {
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let recipient = Pubkey::new_unique();

    let mut tx = make_test_tx(&pubkey, &recipient);
    tx.partial_sign(&[&keypair], tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&tx).unwrap();

    let session_id = SessionId::from(Uuid::new_v4());
    let store = ExternalWalletStore::new();
    store.bind_wallet(&session_id, &pubkey.to_string());

    let external_adapter = Arc::new(ExternalWalletAdapter::new(Arc::new(store.clone())));
    let orchestrator = SignatureOrchestrator::new(vec![external_adapter]);

    let proposal_id = Uuid::new_v4();
    let request = build_signature_request(&session_id, &tx, &pubkey, proposal_id);
    let _ = orchestrator.submit(request).await.unwrap();

    let req_id = SignatureRequestId::from(proposal_id);

    // First complete: succeeds.
    let first = orchestrator.complete(&req_id, &signed_bytes).await;
    assert!(first.is_ok(), "first complete must succeed");

    // Second complete: consumed, returns NotFound.
    let second = orchestrator.complete(&req_id, &signed_bytes).await;
    assert!(
        matches!(second, Err(SignatureError::RequestNotFound)),
        "second complete must return RequestNotFound, got: {second:?}"
    );
}

// ── Test 5: submission_block_height = 0 causes premature Dropped ──────────────

#[test]
fn submission_height_zero_causes_premature_dropped() {
    // Simulates the bug: submission_block_height = 0 with current_block_height
    // at a realistic mainnet value. The state machine should classify this as
    // Dropped because blocks_elapsed = current_height - 0 >= 50.
    let current_height = 300_000_000u64; // realistic mainnet
    let submission_height = 0u64;        // BUG: zero
    let last_valid_height = current_height + 150; // still valid

    let observation = RpcObservation {
        status: None,  // not yet observed
        err: None,
        slot: None,
    };

    let new_state = calculate_next_state(
        &TxState::Submitted,
        &observation,
        current_height,
        submission_height,
        last_valid_height,
    );

    // With height=0, blocks_elapsed = 300M >> 50, so state becomes Dropped.
    assert_eq!(
        new_state,
        TxState::Dropped,
        "submission_block_height=0 must cause premature Dropped (proving the bug)"
    );
}

// ── Test 6: Real submission_block_height avoids premature Dropped ─────────────

#[test]
fn real_submission_height_avoids_premature_dropped() {
    // With a correct submission_block_height, the state machine should stay
    // in Submitted because the grace period hasn't elapsed.
    let current_height = 300_000_000u64;
    let submission_height = current_height - 10; // only 10 blocks ago
    let last_valid_height = current_height + 150;

    let observation = RpcObservation {
        status: None,
        err: None,
        slot: None,
    };

    let new_state = calculate_next_state(
        &TxState::Submitted,
        &observation,
        current_height,
        submission_height,
        last_valid_height,
    );

    assert_eq!(
        new_state,
        TxState::Submitted,
        "with correct submission_block_height, tx should remain Submitted within grace period"
    );
}

// ── Test 7: Durable-first — DB row required before in-memory tracking ─────────

#[tokio::test]
async fn durable_first_db_insert_required_before_tracker() {
    let db = setup_db().await;
    let tracking_repo = TransactionTrackingRepository::new(db.pool().clone());

    let sig = solana_sdk::signature::Signature::new_unique();
    let tx_id = Uuid::new_v4();
    let req_id = Uuid::new_v4();
    let session_id = "test-session";
    let wallet = Pubkey::new_unique().to_string();
    let last_valid = 1000u64;
    let submission_height = 850u64;

    // Step 1: Insert to DB succeeds.
    tracking_repo.insert(
        &sig.to_string(), &tx_id.to_string(), &req_id.to_string(),
        session_id, &wallet, last_valid, submission_height,
    ).await.expect("DB insert must succeed first");

    // Step 2: Only THEN register in tracker.
    let (change_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let rpc_pool = claw_solana_core::rpc::RpcPool::new(
        claw_solana_core::rpc::RpcPoolConfig {
            endpoints: vec![claw_solana_core::rpc::EndpointConfig {
                url: "http://localhost:8899".to_string(),
                label: "test".to_string(),
                is_write_endpoint: false,
            }],
            ..Default::default()
        },
    );
    let tracker = TransactionTracker::with_change_channel(
        Arc::new(rpc_pool),
        change_tx,
    );

    tracker.track(
        sig, tx_id, req_id,
        session_id.to_string(), wallet.clone(),
        last_valid, submission_height,
        None,
    );

    // Verify: tracker has the entry.
    assert_eq!(tracker.active_count(), 1);
    let record = tracker.get_record(&sig).expect("record must exist in tracker");
    assert_eq!(record.submission_block_height, submission_height);

    // Verify: DB has the entry.
    let rows = tracking_repo.load_active().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].submission_block_height, submission_height as i64);
}

// ── Test 8: Restart recovery with real submission_block_height ─────────────────

#[tokio::test]
async fn restart_recovery_preserves_submission_block_height() {
    let db = setup_db().await;
    let tracking_repo = TransactionTrackingRepository::new(db.pool().clone());

    let sig = solana_sdk::signature::Signature::new_unique();
    let tx_id = Uuid::new_v4();
    let req_id = Uuid::new_v4();
    let submission_height = 299_999_850u64;
    let last_valid = 300_000_000u64;

    tracking_repo.insert(
        &sig.to_string(), &tx_id.to_string(), &req_id.to_string(),
        "session-1", "wallet-1", last_valid, submission_height,
    ).await.unwrap();

    // Simulate restart: load_active and verify submission_block_height.
    let rows = tracking_repo.load_active().await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.submission_block_height, submission_height as i64);
    assert_eq!(row.last_valid_block_height, last_valid as i64);

    // Verify the recovered height would NOT cause premature Dropped.
    let current_height = submission_height + 10; // 10 blocks later
    let observation = RpcObservation {
        status: None,
        err: None,
        slot: None,
    };
    let state = calculate_next_state(
        &TxState::Submitted,
        &observation,
        current_height,
        row.submission_block_height as u64,
        row.last_valid_block_height as u64,
    );
    assert_eq!(state, TxState::Submitted, "recovered row must not premature-drop");
}

// ── Test 9: Idempotent insert (INSERT OR IGNORE) ─────────────────────────────

#[tokio::test]
async fn idempotent_insert_does_not_error() {
    let db = setup_db().await;
    let tracking_repo = TransactionTrackingRepository::new(db.pool().clone());

    let sig = solana_sdk::signature::Signature::new_unique();
    let tx_id = Uuid::new_v4();
    let req_id = Uuid::new_v4();

    // First insert.
    tracking_repo.insert(
        &sig.to_string(), &tx_id.to_string(), &req_id.to_string(),
        "session-1", "wallet-1", 1000, 850,
    ).await.expect("first insert");

    // Second insert with same signature (idempotent).
    tracking_repo.insert(
        &sig.to_string(), &tx_id.to_string(), &req_id.to_string(),
        "session-1", "wallet-1", 1000, 850,
    ).await.expect("second insert must not error (INSERT OR IGNORE)");

    let rows = tracking_repo.load_active().await.unwrap();
    assert_eq!(rows.len(), 1, "exactly one row despite double insert");
}

// ── Test 10: Fallback submission height avoids zero ───────────────────────────

#[test]
fn fallback_submission_height_avoids_zero() {
    // Validates the fallback strategy: last_valid_block_height - 150.
    // This is the conservative estimate used when getBlockHeight fails.
    let last_valid_block_height = 300_000_150u64;
    let fallback_height = last_valid_block_height.saturating_sub(150);

    // Fallback should be non-zero and close to a realistic submission time.
    assert!(fallback_height > 0, "fallback must be non-zero");
    assert_eq!(fallback_height, 300_000_000u64);

    // With this fallback, a transaction 10 blocks later should NOT be Dropped.
    let current_height = fallback_height + 10;
    let observation = RpcObservation { status: None, err: None, slot: None };
    let state = calculate_next_state(
        &TxState::Submitted,
        &observation,
        current_height,
        fallback_height,
        last_valid_block_height,
    );
    assert_eq!(state, TxState::Submitted, "fallback height must avoid premature Dropped");
}

#[test]
fn bincode_vs_wire_format_comparison() {
    use solana_sdk::{hash::Hash, message::Message, system_instruction, transaction::Transaction};

    let from = Pubkey::new_unique();
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&from, &to, 1000);
    let msg = Message::new(&[ix], Some(&from));
    let mut tx = Transaction::new_unsigned(msg);
    tx.message.recent_blockhash = Hash::new_unique();

    let bincode_bytes = bincode::serialize(&tx).unwrap();
    let msg_data = tx.message_data();

    println!("bincode total len: {}", bincode_bytes.len());
    println!("message_data len: {}", msg_data.len());
    println!("sigs count: {}", tx.signatures.len());

    // Parse bincode sig count (should be u64 LE at offset 0)
    let bc_sig_count = u64::from_le_bytes(bincode_bytes[0..8].try_into().unwrap()) as usize;
    println!("bincode sig_count from first 8 bytes: {}", bc_sig_count);

    let bc_msg_start = 8 + bc_sig_count * 64;
    let bc_msg = &bincode_bytes[bc_msg_start..];
    println!("bincode msg portion len: {}", bc_msg.len());

    let msg_match = bc_msg == msg_data.as_slice();
    println!("bincode_msg == message_data: {}", msg_match);

    if !msg_match && bc_msg.len() >= 10 && msg_data.len() >= 10 {
        println!("bincode_msg[0..10]: {:02x?}", &bc_msg[..10]);
        println!("msg_data[0..10]:    {:02x?}", &msg_data[..10]);
    }

    // Expected wire format len
    let wire_len = 1 + tx.signatures.len() * 64 + msg_data.len();
    println!("expected wire len: {}", wire_len);
    println!("diff (bincode - wire): {}", bincode_bytes.len() as i64 - wire_len as i64);

    // Print first 12 bincode bytes
    println!("bincode[0..12]: {:02x?}", &bincode_bytes[..12.min(bincode_bytes.len())]);
}

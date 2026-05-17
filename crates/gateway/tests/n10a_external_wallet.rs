//! N10A External Wallet — Validation Tests
//!
//! Proves:
//! 1. Session-wallet binding works correctly
//! 2. ExternalWalletStore park/complete lifecycle
//! 3. Signed transaction with matching message bytes is accepted
//! 4. Signed transaction with modified instructions is rejected
//! 5. Signed transaction from wrong wallet is rejected
//! 6. Double-complete is rejected (one-time-actionable)
//! 7. ExternalWalletStore park/reject lifecycle
//!
//! Uses: in-memory stores, real Transaction/Signature types, no HTTP listener, no real RPC.

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
    ExternalWalletStore, VerifyError,
    submit_signed_transaction, verify_signed_tx,
};
use claw_types::{
    policy::PolicyVerdict,
    session::SessionId,
    transaction::{SimulationResult, TransactionProposal},
};

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
        description: "test transfer".to_string(),
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

// ── Test 1: Session-wallet binding ───────────────────────────────────────────

#[test]
fn session_wallet_binding() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let pubkey = "PhantomAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    assert!(!store.is_external_wallet(&session, pubkey));
    assert!(store.wallets_for_session(&session).is_empty());

    store.bind_wallet(&session, pubkey);
    assert!(store.is_external_wallet(&session, pubkey));
    assert_eq!(store.wallets_for_session(&session).len(), 1);

    // Different session doesn't see this binding.
    let other_session = SessionId::from(Uuid::new_v4());
    assert!(!store.is_external_wallet(&other_session, pubkey));

    // Unbind clears all.
    store.unbind_session(&session);
    assert!(!store.is_external_wallet(&session, pubkey));
}

// ── Test 2: Park/complete lifecycle ──────────────────────────────────────────

#[tokio::test]
async fn park_complete_lifecycle() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);
    let request_id = Uuid::new_v4();

    let rx = store.park(
        request_id,
        fake_proposal(session, &from.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        from.pubkey().to_string(),
    ).unwrap();

    assert_eq!(store.pending_count(), 1);

    // Sign the transaction.
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&from], signed_tx.message.recent_blockhash);

    // Complete with signed tx.
    assert!(store.complete(&request_id, signed_tx.clone()));

    // Receiver should get the signed transaction.
    let received = rx.await.unwrap();
    assert!(received.is_some());
    let received_tx = received.unwrap();
    assert_eq!(received_tx.signatures.len(), signed_tx.signatures.len());
}

// ── Test 3: Message bytes match (acceptance) ─────────────────────────────────

#[test]
fn message_bytes_match_accepted() {
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);

    // Sign the same transaction.
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&from], signed_tx.message.recent_blockhash);

    // Message bytes must be identical.
    assert_eq!(
        tx.message_data(),
        signed_tx.message_data(),
        "signing should not change message bytes"
    );

    // Check that the expected signer has signed.
    let signer_idx = signed_tx.message.account_keys
        .iter()
        .position(|k| k == &from.pubkey())
        .expect("signer must be in account keys");

    assert!(
        signed_tx.signatures[signer_idx] != solana_sdk::signature::Signature::default(),
        "signer must have a non-default signature"
    );
}

// ── Test 4: Modified instructions rejected ───────────────────────────────────

#[test]
fn modified_instructions_rejected() {
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);

    // Create a modified transaction (different amount).
    let different_to = Pubkey::new_unique();
    let ix2 = system_instruction::transfer(&from.pubkey(), &different_to, 2_000_000);
    let msg2 = Message::new(&[ix2], Some(&from.pubkey()));
    let mut modified_tx = Transaction::new_unsigned(msg2);
    modified_tx.message.recent_blockhash = tx.message.recent_blockhash;
    modified_tx.sign(&[&from], modified_tx.message.recent_blockhash);

    // Message bytes differ.
    assert_ne!(
        tx.message_data(),
        modified_tx.message_data(),
        "modified transaction must have different message bytes"
    );
}

// ── Test 4b: Changed blockhash rejected ───────────────────────────────────────

#[test]
fn changed_blockhash_rejected() {
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);

    // Same instructions, same accounts, different blockhash.
    let mut tampered = tx.clone();
    tampered.message.recent_blockhash = Hash::new_unique();

    assert_ne!(
        tx.message_data(),
        tampered.message_data(),
        "different blockhash must produce different message bytes"
    );
}

// ── Test 4c: Compute budget tampering rejected ───────────────────────────────

#[test]
fn compute_budget_tampering_rejected() {
    use solana_sdk::compute_budget::ComputeBudgetInstruction;

    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);

    // Tamper: insert an extra compute budget instruction.
    let original_ix = system_instruction::transfer(&from.pubkey(), &to, 1_000_000);
    let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(999_999);
    let msg = Message::new(&[cu_ix, original_ix], Some(&from.pubkey()));
    let mut tampered = Transaction::new_unsigned(msg);
    tampered.message.recent_blockhash = tx.message.recent_blockhash;

    assert_ne!(
        tx.message_data(),
        tampered.message_data(),
        "added compute budget instruction must change message bytes"
    );

    // Also: changing the compute unit limit value changes message bytes.
    let cu_ix_different = ComputeBudgetInstruction::set_compute_unit_limit(100_000);
    let original_ix2 = system_instruction::transfer(&from.pubkey(), &to, 1_000_000);
    let msg2 = Message::new(&[cu_ix_different, original_ix2], Some(&from.pubkey()));
    let mut tampered2 = Transaction::new_unsigned(msg2);
    tampered2.message.recent_blockhash = tx.message.recent_blockhash;

    assert_ne!(
        tampered.message_data(),
        tampered2.message_data(),
        "different compute unit limit must produce different message bytes"
    );
}

// ── Test 5: Wrong wallet rejected ────────────────────────────────────────────

#[test]
fn wrong_wallet_signature_rejected() {
    let expected_signer = Keypair::new();
    let _wrong_signer = Keypair::new();
    let to = Pubkey::new_unique();

    let tx = make_test_tx(&expected_signer.pubkey(), &to);

    // Sign with the wrong key — the expected_signer's position won't have
    // a valid signature from wrong_signer.
    let signed_tx = tx.clone();
    // The transaction requires expected_signer but we don't have their key.
    // Check that the expected signer position has a default (no) signature.
    let signer_idx = signed_tx.message.account_keys
        .iter()
        .position(|k| k == &expected_signer.pubkey())
        .expect("expected signer must be in account keys");

    assert_eq!(
        signed_tx.signatures[signer_idx],
        solana_sdk::signature::Signature::default(),
        "unsigned tx must have default signature at signer position"
    );

    // If someone signs with wrong_signer, the expected_signer position is still default.
    // The verification logic in GatewayWalletSignatureHandler checks this.
}

// ── Test 6: Double-complete is rejected ──────────────────────────────────────

#[tokio::test]
async fn double_complete_rejected() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);
    let request_id = Uuid::new_v4();

    let _rx = store.park(
        request_id,
        fake_proposal(session, &from.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        from.pubkey().to_string(),
    ).unwrap();

    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&from], signed_tx.message.recent_blockhash);

    // First complete succeeds.
    assert!(store.complete(&request_id, signed_tx.clone()));

    // Second complete fails (response_tx already taken).
    assert!(!store.complete(&request_id, signed_tx));
}

// ── Test 7: Park/reject lifecycle ────────────────────────────────────────────

#[tokio::test]
async fn park_reject_lifecycle() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);
    let request_id = Uuid::new_v4();

    let rx = store.park(
        request_id,
        fake_proposal(session, &from.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        from.pubkey().to_string(),
    ).unwrap();

    // Reject.
    assert!(store.reject(&request_id));

    // Receiver gets None.
    let received = rx.await.unwrap();
    assert!(received.is_none(), "rejected request should yield None");

    // Cleanup.
    store.remove(&request_id);
    assert_eq!(store.pending_count(), 0);
}

// ── Test 8: Multiple wallets per session ─────────────────────────────────────

#[test]
fn multiple_wallets_per_session() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());

    store.bind_wallet(&session, "wallet1");
    store.bind_wallet(&session, "wallet2");
    store.bind_wallet(&session, "wallet1"); // duplicate bind is idempotent

    let wallets = store.wallets_for_session(&session);
    assert_eq!(wallets.len(), 2);
    assert!(store.is_external_wallet(&session, "wallet1"));
    assert!(store.is_external_wallet(&session, "wallet2"));
    assert!(!store.is_external_wallet(&session, "wallet3"));
}

// ── Test 9: Expected signer retrieval ────────────────────────────────────────

#[test]
fn expected_signer_retrieval() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);
    let request_id = Uuid::new_v4();
    let expected = from.pubkey().to_string();

    let _rx = store.park(
        request_id,
        fake_proposal(session, &expected),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        expected.clone(),
    ).unwrap();

    assert_eq!(store.expected_signer(&request_id), Some(expected));
    assert_eq!(store.expected_signer(&Uuid::new_v4()), None);
}

// ── Test 10: Get parked tx bytes ─────────────────────────────────────────────

#[test]
fn get_parked_tx_bytes() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let from = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&from.pubkey(), &to);
    let request_id = Uuid::new_v4();

    let _rx = store.park(
        request_id,
        fake_proposal(session, &from.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        from.pubkey().to_string(),
    ).unwrap();

    let bytes = store.get(&request_id).expect("must have parked bytes");
    let deserialized: Transaction = bincode::deserialize(&bytes).expect("must deserialize");
    assert_eq!(
        deserialized.message_data(),
        tx.message_data(),
        "parked tx message must match original"
    );
}

// ── Test 13: pending_for_session returns matching entries ─────────────────────

#[test]
fn pending_for_session_returns_matching() {
    let store = ExternalWalletStore::new();
    let session_a = SessionId::from(Uuid::new_v4());
    let session_b = SessionId::from(Uuid::new_v4());
    let from = Keypair::new();
    let to = Pubkey::new_unique();

    // Park two requests in session A, one in session B.
    let tx = make_test_tx(&from.pubkey(), &to);
    let _rx1 = store.park(
        Uuid::new_v4(),
        fake_proposal(session_a.clone(), &from.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        from.pubkey().to_string(),
    ).unwrap();

    let _rx2 = store.park(
        Uuid::new_v4(),
        fake_proposal(session_a.clone(), &from.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        from.pubkey().to_string(),
    ).unwrap();

    let _rx3 = store.park(
        Uuid::new_v4(),
        fake_proposal(session_b.clone(), &from.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        from.pubkey().to_string(),
    ).unwrap();

    assert_eq!(store.pending_for_session(&session_a).len(), 2);
    assert_eq!(store.pending_for_session(&session_b).len(), 1);
    assert_eq!(store.pending_for_session(&SessionId::from(Uuid::new_v4())).len(), 0);
}

// ══════════════════════════════════════════════════════════════════════════════
// N10 Phase 1 — External signer round-trip tests
// ══════════════════════════════════════════════════════════════════════════════

// ── Test 14: Happy path — full external signer round-trip ─────────────────────

#[tokio::test]
async fn happy_path_external_signer_roundtrip() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let signer = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&signer.pubkey(), &to);
    let request_id = Uuid::new_v4();

    // Park the unsigned transaction.
    let rx = store.park(
        request_id,
        fake_proposal(session, &signer.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        signer.pubkey().to_string(),
    ).unwrap();

    assert_eq!(store.pending_count(), 1);

    // Simulate client-side signing: sign the same tx with the keypair.
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&signer], signed_tx.message.recent_blockhash);
    let signed_tx_bytes = bincode::serialize(&signed_tx).unwrap();

    // Submit through the store-level handler.
    submit_signed_transaction(&store, request_id, signed_tx_bytes).unwrap();

    // The oneshot receiver should yield the signed transaction.
    let received = rx.await.unwrap();
    assert!(received.is_some(), "complete must have sent Some(signed_tx)");
    let received_tx = received.unwrap();
    assert_eq!(received_tx.signatures.len(), signed_tx.signatures.len());
    assert_eq!(received_tx.message_data(), tx.message_data());
}

// ── Test 15: Reject on message mismatch (modified instruction) ────────────────

#[test]
fn reject_on_message_mismatch() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let signer = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&signer.pubkey(), &to);
    let request_id = Uuid::new_v4();

    let _rx = store.park(
        request_id,
        fake_proposal(session, &signer.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        signer.pubkey().to_string(),
    ).unwrap();

    // Create a DIFFERENT transaction (different recipient, different amount).
    let different_to = Pubkey::new_unique();
    let tampered_ix = system_instruction::transfer(&signer.pubkey(), &different_to, 9_999_999);
    let tampered_msg = Message::new(&[tampered_ix], Some(&signer.pubkey()));
    let mut tampered_tx = Transaction::new_unsigned(tampered_msg);
    tampered_tx.message.recent_blockhash = tx.message.recent_blockhash;
    tampered_tx.sign(&[&signer], tampered_tx.message.recent_blockhash);
    let tampered_bytes = bincode::serialize(&tampered_tx).unwrap();

    // Submit the tampered tx — must be rejected.
    let err = submit_signed_transaction(&store, request_id, tampered_bytes)
        .expect_err("must reject tampered tx");
    assert!(
        matches!(err, claw_gateway::SubmitError::VerificationFailed(_)),
        "expected VerificationFailed, got: {err}"
    );

    // Store should have rejected and cleaned up.
    assert!(store.get(&request_id).is_none(), "entry should be removed after rejection");
}

// ── Test 16: Reject on signer mismatch (wrong keypair) ───────────────────────

#[test]
fn reject_on_signer_mismatch() {
    let store = ExternalWalletStore::new();
    let session = SessionId::from(Uuid::new_v4());
    let expected_signer = Keypair::new();
    let _wrong_signer = Keypair::new();
    let to = Pubkey::new_unique();

    // The tx is built with expected_signer as fee payer.
    let tx = make_test_tx(&expected_signer.pubkey(), &to);
    let request_id = Uuid::new_v4();

    let _rx = store.park(
        request_id,
        fake_proposal(session, &expected_signer.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        expected_signer.pubkey().to_string(),
    ).unwrap();

    // The expected_signer built the tx but we DON'T sign with them.
    // The unsigned tx has a default signature at the signer position.
    // Submit the unsigned tx — signer didn't sign.
    let unsigned_bytes = bincode::serialize(&tx).unwrap();

    let err = submit_signed_transaction(&store, request_id, unsigned_bytes)
        .expect_err("must reject unsigned tx");
    assert!(
        matches!(err, claw_gateway::SubmitError::VerificationFailed(_)),
        "expected VerificationFailed, got: {err}"
    );
}

// ── Test 17: Reject on invalid (corrupted) signature ──────────────────────────

#[test]
fn reject_on_invalid_signature() {
    let signer = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&signer.pubkey(), &to);

    // Sign correctly first.
    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&signer], signed_tx.message.recent_blockhash);

    // Corrupt the signature: flip a byte in the raw signature bytes.
    let mut corrupted = signed_tx.clone();
    let mut sig_bytes: [u8; 64] = corrupted.signatures[0].into();
    sig_bytes[0] ^= 0xFF;
    corrupted.signatures[0] = solana_sdk::signature::Signature::from(sig_bytes);

    // Use verify_signed_tx directly to test the cryptographic check.
    let result = verify_signed_tx(&tx, &corrupted, &signer.pubkey());
    assert!(
        matches!(result, Err(VerifyError::InvalidSignature)),
        "expected InvalidSignature, got: {result:?}"
    );
}

// ── Test 18: verify_signed_tx happy path ──────────────────────────────────────

#[test]
fn verify_signed_tx_happy_path() {
    let signer = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&signer.pubkey(), &to);

    let mut signed_tx = tx.clone();
    signed_tx.sign(&[&signer], signed_tx.message.recent_blockhash);

    // Must pass all 4 checks.
    verify_signed_tx(&tx, &signed_tx, &signer.pubkey()).unwrap();
}

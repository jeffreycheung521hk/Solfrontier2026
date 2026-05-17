//! Unit tests for signature orchestration.
//!
//! Tests only Execution Plane concerns:
//! - Correct routing to local vs external adapter
//! - Request state transitions (submit → pending → complete)
//! - Duplicate request ID rejection
//! - Duplicate completion rejection (exactly-once)
//! - Verification failure results in consumed entry
//! - Expiry removes pending entry
//! - No policy/intent logic in the wallet layer
//!
//! Does NOT test:
//! - DeFi execution
//! - Phantom frontend
//! - Policy evaluation
//! - Approval lifecycle

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, Message, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer as SolanaSigner,
    system_instruction,
    transaction::{Transaction, VersionedTransaction},
};
use uuid::Uuid;

use claw_types::session::SessionId;

use super::adapter::{ExternalWalletBindings, WalletAdapter};
use super::external_adapter::ExternalWalletAdapter;
use super::types::*;
use super::SignatureOrchestrator;

// ── Test helpers ────────────────────────────────────────────────────────────

/// A mock external wallet bindings that always returns true for any query.
struct AlwaysBoundBindings;
impl ExternalWalletBindings for AlwaysBoundBindings {
    fn is_bound(&self, _session_id: &SessionId, _pubkey: &str) -> bool {
        true
    }
}

/// A mock external wallet bindings that always returns false.
struct NeverBoundBindings;
impl ExternalWalletBindings for NeverBoundBindings {
    fn is_bound(&self, _session_id: &SessionId, _pubkey: &str) -> bool {
        false
    }
}

/// A mock local adapter that signs by setting a known signature.
/// Does NOT use a real keystore — this is a unit test for orchestrator routing.
struct MockLocalAdapter {
    supported_pubkey: Pubkey,
}

#[async_trait]
impl WalletAdapter for MockLocalAdapter {
    fn adapter_type(&self) -> &str {
        "local"
    }

    fn supports(&self, request: &SignatureRequest) -> bool {
        request.expected_signer == self.supported_pubkey
    }

    async fn sign(
        &self,
        request: &SignatureRequest,
    ) -> Result<SignatureOutcome, SignatureError> {
        // Mock: just return Signed with a fake signature.
        // Real LocalWalletAdapter would deserialize and sign.
        Ok(SignatureOutcome::Signed {
            request_id: request.id,
            signature: "mock_local_signature_abc123".to_string(),
            signed_tx_bytes: request.finalized_tx_bytes.clone(),
            adapter_type: self.adapter_type().to_string(),
            transaction_id: request.transaction_id,
            expected_signer: request.expected_signer.to_string(),
        })
    }

    async fn complete(
        &self,
        _request: &SignatureRequest,
        _signed_tx_bytes: &[u8],
    ) -> Result<SignatureOutcome, SignatureError> {
        Err(SignatureError::SigningFailed(
            "local adapter does not support async completion".to_string(),
        ))
    }
}

/// Creates a minimal test transaction and serializes it.
fn make_test_tx() -> (Keypair, Transaction, Vec<u8>) {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    let tx = Transaction::new_unsigned(msg);
    let tx_bytes = bincode::serialize(&tx).expect("serialize tx");
    (payer, tx, tx_bytes)
}

/// Creates a `SignatureRequest` from test data.
fn make_request(signer_pubkey: Pubkey, tx_bytes: Vec<u8>) -> SignatureRequest {
    SignatureRequest {
        id: SignatureRequestId::new(),
        session_id: SessionId::new(),
        transaction_id: Uuid::new_v4(),
        finalized_tx_bytes: tx_bytes,
        expected_signer: signer_pubkey,
        description: "test transaction".to_string(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn local_adapter_routes_and_signs_synchronously() {
    let (payer, _tx, tx_bytes) = make_test_tx();
    let local_pubkey = payer.pubkey();

    let local = Arc::new(MockLocalAdapter {
        supported_pubkey: local_pubkey,
    }) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![local]);

    let request = make_request(local_pubkey, tx_bytes);
    let outcome = orch.submit(request).await.expect("submit should succeed");

    match outcome {
        SignatureOutcome::Signed { signature, adapter_type, .. } => {
            assert_eq!(adapter_type, "local");
            assert_eq!(signature, "mock_local_signature_abc123");
        }
        SignatureOutcome::Pending { .. } => panic!("local adapter should not return Pending"),
    }

    // Local signing is synchronous — nothing in pending map.
    assert_eq!(orch.pending_count(), 0);
}

#[tokio::test]
async fn external_adapter_routes_to_pending() {
    let (payer, _tx, tx_bytes) = make_test_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes.clone());
    let _request_id = request.id;

    let outcome = orch.submit(request).await.expect("submit should succeed");

    match outcome {
        SignatureOutcome::Pending { adapter_type, finalized_tx_bytes, .. } => {
            assert_eq!(adapter_type, "external");
            assert_eq!(finalized_tx_bytes, tx_bytes);
        }
        SignatureOutcome::Signed { .. } => panic!("external adapter should not return Signed"),
    }

    assert_eq!(orch.pending_count(), 1);

    // Verify pending_for_session returns the request.
    let session_id = SessionId::new(); // Different session — should be empty.
    assert!(orch.pending_for_session(&session_id).is_empty());
}

#[tokio::test]
async fn no_adapter_returns_error() {
    let (payer, _tx, tx_bytes) = make_test_tx();

    // External adapter bound to nothing, local adapter for a different key.
    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(NeverBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let other_pubkey = Pubkey::new_unique();
    let local = Arc::new(MockLocalAdapter {
        supported_pubkey: other_pubkey,
    }) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external, local]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let result = orch.submit(request).await;

    assert!(matches!(result, Err(SignatureError::NoAdapter)));
}

#[tokio::test]
async fn duplicate_request_id_rejected() {
    let (payer, _tx, tx_bytes) = make_test_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let shared_id = SignatureRequestId::new();

    let request1 = SignatureRequest {
        id: shared_id,
        session_id: SessionId::new(),
        transaction_id: Uuid::new_v4(),
        finalized_tx_bytes: tx_bytes.clone(),
        expected_signer: payer.pubkey(),
        description: "first".to_string(),
    };

    // First submit succeeds.
    orch.submit(request1).await.expect("first submit should succeed");
    assert_eq!(orch.pending_count(), 1);

    let request2 = SignatureRequest {
        id: shared_id,
        session_id: SessionId::new(),
        transaction_id: Uuid::new_v4(),
        finalized_tx_bytes: tx_bytes,
        expected_signer: payer.pubkey(),
        description: "duplicate".to_string(),
    };

    // Second submit with same ID is rejected.
    let result = orch.submit(request2).await;
    assert!(matches!(result, Err(SignatureError::DuplicateRequestId)));
    assert_eq!(orch.pending_count(), 1); // Still just the first.
}

#[tokio::test]
async fn complete_with_valid_signature_succeeds() {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    let blockhash = Hash::new_unique();
    tx.message.recent_blockhash = blockhash;

    let tx_bytes = bincode::serialize(&tx).expect("serialize");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;

    // Submit → Pending.
    orch.submit(request).await.expect("submit should succeed");
    assert_eq!(orch.pending_count(), 1);

    // Sign the transaction externally (simulating Phantom).
    tx.partial_sign(&[&payer], blockhash);
    let signed_bytes = bincode::serialize(&tx).expect("serialize signed");

    // Complete with valid signature.
    let outcome = orch.complete(&request_id, &signed_bytes).await
        .expect("complete should succeed");

    match outcome {
        SignatureOutcome::Signed { adapter_type, signature, .. } => {
            assert_eq!(adapter_type, "external");
            assert!(!signature.is_empty());
        }
        _ => panic!("expected Signed outcome"),
    }

    // Pending entry consumed.
    assert_eq!(orch.pending_count(), 0);
}

#[tokio::test]
async fn complete_with_tampered_message_fails_and_consumes() {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    let blockhash = Hash::new_unique();
    tx.message.recent_blockhash = blockhash;

    let tx_bytes = bincode::serialize(&tx).expect("serialize");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;

    orch.submit(request).await.expect("submit");
    assert_eq!(orch.pending_count(), 1);

    // Create a DIFFERENT transaction (tampered) and sign it.
    let ix2 = system_instruction::transfer(&payer.pubkey(), &to, 999_999);
    let msg2 = Message::new(&[ix2], Some(&payer.pubkey()));
    let mut tampered_tx = Transaction::new_unsigned(msg2);
    tampered_tx.message.recent_blockhash = blockhash;
    tampered_tx.partial_sign(&[&payer], blockhash);
    let tampered_bytes = bincode::serialize(&tampered_tx).expect("serialize tampered");

    // Complete with tampered transaction → verification failure.
    let result = orch.complete(&request_id, &tampered_bytes).await;
    assert!(matches!(result, Err(SignatureError::VerificationFailed(_))));

    // Entry consumed even on failure — not re-inserted.
    assert_eq!(orch.pending_count(), 0);

    // Second complete attempt → RequestNotFound.
    let result2 = orch.complete(&request_id, &tampered_bytes).await;
    assert!(matches!(result2, Err(SignatureError::RequestNotFound)));
}

#[tokio::test]
async fn complete_on_nonexistent_request_returns_not_found() {
    let orch = SignatureOrchestrator::new(vec![]);
    let fake_id = SignatureRequestId::new();

    let result = orch.complete(&fake_id, &[]).await;
    assert!(matches!(result, Err(SignatureError::RequestNotFound)));
}

#[tokio::test]
async fn expire_removes_pending_entry() {
    let (payer, _tx, tx_bytes) = make_test_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    let transaction_id = request.transaction_id;

    orch.submit(request).await.expect("submit");
    assert_eq!(orch.pending_count(), 1);

    // Expire the request.
    let consumed = orch.expire(&request_id).expect("expire should succeed");
    assert_eq!(consumed.id, request_id);
    assert_eq!(consumed.transaction_id, transaction_id);

    // Pending entry removed.
    assert_eq!(orch.pending_count(), 0);

    // Second expire → not found.
    let result = orch.expire(&request_id);
    assert!(matches!(result, Err(SignatureError::RequestNotFound)));
}

#[tokio::test]
async fn expire_after_complete_returns_not_found() {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    let blockhash = Hash::new_unique();
    tx.message.recent_blockhash = blockhash;
    let tx_bytes = bincode::serialize(&tx).expect("serialize");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;

    orch.submit(request).await.expect("submit");

    // Complete first.
    tx.partial_sign(&[&payer], blockhash);
    let signed_bytes = bincode::serialize(&tx).expect("serialize signed");
    orch.complete(&request_id, &signed_bytes).await.expect("complete");

    // Expire after complete → not found (already consumed).
    let result = orch.expire(&request_id);
    assert!(matches!(result, Err(SignatureError::RequestNotFound)));
}

#[tokio::test]
async fn expired_candidates_returns_old_entries() {
    let (payer, _tx, tx_bytes) = make_test_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;

    orch.submit(request).await.expect("submit");

    // With max_age = 0, everything is expired immediately.
    let expired = orch.expired_candidates(Duration::from_secs(0));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0], request_id);

    // With max_age = 1 hour, nothing is expired yet.
    let expired = orch.expired_candidates(Duration::from_secs(3600));
    assert!(expired.is_empty());
}

#[tokio::test]
async fn adapter_ordering_prefers_external_over_local() {
    // Both adapters support the same key. External is first → should win.
    let payer = Keypair::new();
    let pubkey = payer.pubkey();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let local = Arc::new(MockLocalAdapter {
        supported_pubkey: pubkey,
    }) as Arc<dyn WalletAdapter>;

    // External first in adapter list.
    let orch = SignatureOrchestrator::new(vec![external, local]);

    let (_payer, _tx, tx_bytes) = make_test_tx();
    let request = make_request(pubkey, tx_bytes);

    let outcome = orch.submit(request).await.expect("submit");

    match outcome {
        SignatureOutcome::Pending { adapter_type, .. } => {
            assert_eq!(adapter_type, "external");
        }
        _ => panic!("expected Pending from external adapter"),
    }
}

#[tokio::test]
async fn pending_for_session_filters_correctly() {
    let (payer, _tx, tx_bytes) = make_test_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let session_a = SessionId::new();
    let session_b = SessionId::new();

    // Submit two requests for session A, one for session B.
    for session in [&session_a, &session_a, &session_b] {
        let request = SignatureRequest {
            id: SignatureRequestId::new(),
            session_id: session.clone(),
            transaction_id: Uuid::new_v4(),
            finalized_tx_bytes: tx_bytes.clone(),
            expected_signer: payer.pubkey(),
            description: "test".to_string(),
        };
        orch.submit(request).await.expect("submit");
    }

    assert_eq!(orch.pending_count(), 3);
    assert_eq!(orch.pending_for_session(&session_a).len(), 2);
    assert_eq!(orch.pending_for_session(&session_b).len(), 1);

    let unknown_session = SessionId::new();
    assert_eq!(orch.pending_for_session(&unknown_session).len(), 0);
}

// ── V0 (Jupiter JIT) tests ───────────────────────────────────────────────────

/// Builds an unsigned V0 VersionedTransaction with one transfer instruction.
fn make_test_v0_tx() -> (Keypair, VersionedTransaction, Vec<u8>) {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let blockhash = Hash::new_unique();

    let msg = v0::Message::try_compile(&payer.pubkey(), &[ix], &[], blockhash)
        .expect("compile v0 message");
    let num_sigs = msg.header.num_required_signatures as usize;
    let vtx = VersionedTransaction {
        signatures: vec![Signature::default(); num_sigs],
        message: VersionedMessage::V0(msg),
    };
    let tx_bytes = bincode::serialize(&vtx).expect("serialize v0");
    (payer, vtx, tx_bytes)
}

/// Signs a V0 VersionedTransaction in-place using the given keypair.
fn sign_v0_tx(vtx: &mut VersionedTransaction, keypair: &Keypair) {
    let msg_bytes = vtx.message.serialize();
    let sig = keypair.sign_message(&msg_bytes);
    // Payer is always at index 0 in a single-signer V0 tx.
    vtx.signatures[0] = sig;
}

#[tokio::test]
async fn v0_complete_with_valid_signature_succeeds() {
    let (payer, mut vtx, tx_bytes) = make_test_v0_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;

    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;

    orch.submit(request).await.expect("submit V0");
    assert_eq!(orch.pending_count(), 1);

    // Sign externally (simulating Phantom signing a V0 tx).
    sign_v0_tx(&mut vtx, &payer);
    let signed_bytes = bincode::serialize(&vtx).expect("serialize signed V0");

    let outcome = orch.complete(&request_id, &signed_bytes).await
        .expect("V0 complete should succeed");

    match outcome {
        SignatureOutcome::Signed { adapter_type, signature, .. } => {
            assert_eq!(adapter_type, "external");
            assert!(!signature.is_empty());
        }
        _ => panic!("expected Signed outcome"),
    }

    assert_eq!(orch.pending_count(), 0);
}

#[tokio::test]
async fn v0_complete_with_tampered_instruction_data_fails() {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let blockhash = Hash::new_unique();

    // Original: transfer 1000 lamports.
    let orig_ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let orig_msg = v0::Message::try_compile(&payer.pubkey(), &[orig_ix], &[], blockhash)
        .expect("compile original");
    let orig_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); orig_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(orig_msg),
    };
    let orig_bytes = bincode::serialize(&orig_vtx).expect("serialize original");

    // Tampered: transfer 999_999 lamports (different data bytes).
    let tampered_ix = system_instruction::transfer(&payer.pubkey(), &to, 999_999);
    let tampered_msg = v0::Message::try_compile(&payer.pubkey(), &[tampered_ix], &[], blockhash)
        .expect("compile tampered");
    let mut tampered_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); tampered_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(tampered_msg),
    };
    sign_v0_tx(&mut tampered_vtx, &payer);
    let tampered_bytes = bincode::serialize(&tampered_vtx).expect("serialize tampered");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), orig_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    // Tampered tx has different instruction data → must fail.
    let result = orch.complete(&request_id, &tampered_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed, got {:?}", result
    );
    assert_eq!(orch.pending_count(), 0);
}

#[tokio::test]
async fn v0_complete_rejects_legacy_signed_bytes() {
    let (payer, _vtx, v0_bytes) = make_test_v0_tx();

    // Build a legacy tx signed by the same key.
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    let mut legacy_tx = Transaction::new_unsigned(msg);
    let blockhash = Hash::new_unique();
    legacy_tx.partial_sign(&[&payer], blockhash);
    let legacy_bytes = bincode::serialize(&legacy_tx).expect("serialize legacy");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    // Original stored as V0.
    let request = make_request(payer.pubkey(), v0_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    // Attempting to complete with legacy bytes must fail.
    let result = orch.complete(&request_id, &legacy_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed when submitting legacy bytes for V0 original, got {:?}", result
    );
}

#[tokio::test]
async fn v0_complete_rejects_wrong_signer() {
    let (payer, mut vtx, tx_bytes) = make_test_v0_tx();

    // Sign with a DIFFERENT keypair — signer won't match expected_signer (payer.pubkey()).
    let wrong_key = Keypair::new();
    sign_v0_tx(&mut vtx, &wrong_key);
    let signed_bytes = bincode::serialize(&vtx).expect("serialize");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    // wrong_key pubkey is not in the V0 static keys → signer not found.
    let result = orch.complete(&request_id, &signed_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for wrong signer, got {:?}", result
    );
}

// ── Layer 1: Pure V0 verification tests ─────────────────────────────────────

/// Builds a V0 VersionedTransaction that actually has address_table_lookups.
fn make_test_v0_tx_with_alt() -> (Keypair, VersionedTransaction, Vec<u8>) {
    let payer = Keypair::new();
    let alt_key = Pubkey::new_unique();
    let program_id = Pubkey::new_unique();
    let alt_account = Pubkey::new_unique();

    let alt = AddressLookupTableAccount {
        key: alt_key,
        addresses: vec![alt_account],
    };

    let blockhash = Hash::new_unique();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(alt_account, false),
        ],
        data: vec![1, 2, 3, 4],
    };

    let msg = v0::Message::try_compile(&payer.pubkey(), &[ix], &[alt], blockhash)
        .expect("compile v0 with ALT");

    let vtx = VersionedTransaction {
        signatures: vec![Signature::default(); msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(msg),
    };
    let tx_bytes = bincode::serialize(&vtx).unwrap();
    (payer, vtx, tx_bytes)
}

// 1a. V0 + ALT happy path
#[tokio::test]
async fn v0_alt_happy_path_is_accepted() {
    let (payer, mut vtx, tx_bytes) = make_test_v0_tx_with_alt();

    // Verify this tx actually has ALT lookups.
    match &vtx.message {
        VersionedMessage::V0(m) => assert!(!m.address_table_lookups.is_empty(), "must have ALT"),
        _ => panic!("expected V0"),
    }

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit V0 with ALT");

    sign_v0_tx(&mut vtx, &payer);
    let signed_bytes = bincode::serialize(&vtx).expect("serialize signed V0+ALT");

    let outcome = orch.complete(&request_id, &signed_bytes).await
        .expect("V0+ALT complete should succeed");

    match outcome {
        SignatureOutcome::Signed { adapter_type, signature, .. } => {
            assert_eq!(adapter_type, "external");
            assert!(!signature.is_empty());
        }
        _ => panic!("expected Signed outcome"),
    }
    assert_eq!(orch.pending_count(), 0);
}

// 1b. V0 + ALT account index tamper — the red flag test
#[tokio::test]
async fn v0_alt_account_index_tamper_is_rejected() {
    // Original tx with ALT.
    let (payer, _original_vtx, original_bytes) = make_test_v0_tx_with_alt();

    // Tampered: build a DIFFERENT V0+ALT tx where a different account is used.
    // Uses a completely different alt_account, meaning account indices differ.
    let alt_key2 = Pubkey::new_unique();
    let program_id2 = Pubkey::new_unique();
    let different_account = Pubkey::new_unique(); // NOT the same alt_account as original

    let alt2 = AddressLookupTableAccount {
        key: alt_key2,
        addresses: vec![different_account],
    };

    // Must use the SAME blockhash as original to pass the blockhash lock check.
    // Re-parse original to get its blockhash.
    let original_vtx: VersionedTransaction = bincode::deserialize(&original_bytes).unwrap();
    let blockhash = *original_vtx.message.recent_blockhash();

    let ix_tampered = Instruction {
        program_id: program_id2,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(different_account, false),
        ],
        data: vec![1, 2, 3, 4], // same data as original
    };

    let msg_tampered = v0::Message::try_compile(&payer.pubkey(), &[ix_tampered], &[alt2], blockhash)
        .expect("compile tampered v0");
    let mut tampered_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); msg_tampered.header.num_required_signatures as usize],
        message: VersionedMessage::V0(msg_tampered),
    };
    sign_v0_tx(&mut tampered_vtx, &payer);
    let tampered_bytes = bincode::serialize(&tampered_vtx).expect("serialize tampered");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), original_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    // Tampered version has different program_id and/or account indices → must fail.
    let result = orch.complete(&request_id, &tampered_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for ALT account tamper, got {:?}", result
    );
    assert_eq!(orch.pending_count(), 0);
}

// 1c. Phantom prepend tolerance — original 1 ix, signed [CB, SWAP], same payer/blockhash
#[tokio::test]
async fn v0_prepend_compute_budget_is_accepted() {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let blockhash = Hash::new_unique();

    // Original: just the transfer instruction.
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let orig_msg = v0::Message::try_compile(&payer.pubkey(), &[transfer_ix.clone()], &[], blockhash)
        .expect("compile original");
    let orig_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); orig_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(orig_msg),
    };
    let orig_bytes = bincode::serialize(&orig_vtx).expect("serialize original");

    // Signed: [ComputeBudget ix, transfer ix] — same blockhash, same payer.
    let cb_ix = Instruction {
        program_id: solana_sdk::compute_budget::id(),
        accounts: vec![],
        data: vec![3u8, 128, 150, 152, 0], // SetComputeUnitLimit(10_000_000) encoding
    };
    let signed_msg = v0::Message::try_compile(
        &payer.pubkey(),
        &[cb_ix, transfer_ix],
        &[],
        blockhash,
    ).expect("compile signed with prepend");
    let mut signed_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); signed_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(signed_msg),
    };
    sign_v0_tx(&mut signed_vtx, &payer);
    let signed_bytes = bincode::serialize(&signed_vtx).expect("serialize signed");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), orig_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let outcome = orch.complete(&request_id, &signed_bytes).await
        .expect("prepend CB should be accepted");
    assert!(matches!(outcome, SignatureOutcome::Signed { .. }), "expected Signed");
    assert_eq!(orch.pending_count(), 0);
}

// 1d. Append instruction rejected
#[tokio::test]
async fn v0_append_malicious_instruction_is_rejected() {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let blockhash = Hash::new_unique();

    // Original: just transfer.
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let orig_msg = v0::Message::try_compile(&payer.pubkey(), &[transfer_ix.clone()], &[], blockhash)
        .expect("compile original");
    let orig_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); orig_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(orig_msg),
    };
    let orig_bytes = bincode::serialize(&orig_vtx).expect("serialize original");

    // Signed: [transfer, MALICIOUS] — malicious appended.
    let malicious_ix = Instruction {
        program_id: Pubkey::new_unique(),
        accounts: vec![AccountMeta::new(payer.pubkey(), true)],
        data: vec![0xDE, 0xAD, 0xBE, 0xEF],
    };
    let signed_msg = v0::Message::try_compile(
        &payer.pubkey(),
        &[transfer_ix, malicious_ix],
        &[],
        blockhash,
    ).expect("compile signed with append");
    let mut signed_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); signed_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(signed_msg),
    };
    sign_v0_tx(&mut signed_vtx, &payer);
    let signed_bytes = bincode::serialize(&signed_vtx).expect("serialize signed");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), orig_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    // Append: offset = 2 - 1 = 1, so signed_ixs[1] is compared to orig_ixs[0].
    // The appended ix lives at index 1, but original ix is at index 0.
    // After offset, signed_ixs[offset + 0] = signed_ixs[1] = MALICIOUS.
    // Program IDs differ → VerificationFailed.
    let result = orch.complete(&request_id, &signed_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for appended malicious ix, got {:?}", result
    );
    assert_eq!(orch.pending_count(), 0);
}

// 1e. Middle insert rejected
#[tokio::test]
async fn v0_middle_insert_is_rejected() {
    let payer = Keypair::new();
    let to = Pubkey::new_unique();
    let blockhash = Hash::new_unique();

    // Build two distinct instructions for original: [IX_A, IX_B].
    let ix_a = system_instruction::transfer(&payer.pubkey(), &to, 1000);
    let ix_b = system_instruction::transfer(&payer.pubkey(), &to, 2000);

    let orig_msg = v0::Message::try_compile(
        &payer.pubkey(),
        &[ix_a.clone(), ix_b.clone()],
        &[],
        blockhash,
    ).expect("compile original");
    let orig_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); orig_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(orig_msg),
    };
    let orig_bytes = bincode::serialize(&orig_vtx).expect("serialize original");

    // Signed: [IX_A, MALICIOUS, IX_B] — insert in the middle.
    let malicious_ix = Instruction {
        program_id: Pubkey::new_unique(),
        accounts: vec![AccountMeta::new(payer.pubkey(), true)],
        data: vec![0xFF, 0xFF],
    };
    let signed_msg = v0::Message::try_compile(
        &payer.pubkey(),
        &[ix_a, malicious_ix, ix_b],
        &[],
        blockhash,
    ).expect("compile signed with middle insert");
    let mut signed_vtx = VersionedTransaction {
        signatures: vec![Signature::default(); signed_msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(signed_msg),
    };
    sign_v0_tx(&mut signed_vtx, &payer);
    let signed_bytes = bincode::serialize(&signed_vtx).expect("serialize signed");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), orig_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    // offset = 3 - 2 = 1. signed_ixs[1+0] = MALICIOUS vs orig_ixs[0] = IX_A → mismatch.
    let result = orch.complete(&request_id, &signed_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for middle insert, got {:?}", result
    );
    assert_eq!(orch.pending_count(), 0);
}

// 1f. Blockhash mutation rejected
#[tokio::test]
async fn v0_blockhash_mutation_is_rejected() {
    let (payer, original_vtx, tx_bytes) = make_test_v0_tx();

    // Clone and mutate the blockhash.
    let mut tampered = original_vtx.clone();
    match &mut tampered.message {
        VersionedMessage::V0(m) => { m.recent_blockhash = Hash::new_unique(); }
        _ => unreachable!(),
    }
    // Re-sign with correct key (so ed25519 itself would pass if blockhash wasn't locked).
    sign_v0_tx(&mut tampered, &payer);
    let tampered_bytes = bincode::serialize(&tampered).expect("serialize tampered");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let result = orch.complete(&request_id, &tampered_bytes).await;
    // Must return BlockhashModified (the retry signal), not generic VerificationFailed.
    assert!(
        matches!(result, Err(SignatureError::BlockhashModified)),
        "expected BlockhashModified for blockhash mutation, got {:?}", result
    );
    assert_eq!(orch.pending_count(), 0);
}

// Verify the error Display string contains the actionable rebuild hint.
#[tokio::test]
async fn v0_blockhash_modified_error_contains_rebuild_hint() {
    let err = SignatureError::BlockhashModified;
    let msg = err.to_string();
    assert!(msg.contains("rebuild required"), "Display must mention rebuild: {msg}");
    assert!(msg.contains("blockhash"), "Display must mention blockhash: {msg}");
}

// 1g. Correct signer, malicious data
#[tokio::test]
async fn v0_malicious_data_with_correct_signer_is_rejected() {
    let (payer, original_vtx, tx_bytes) = make_test_v0_tx();

    // Clone and change instruction data.
    let mut malicious = original_vtx.clone();
    match &mut malicious.message {
        VersionedMessage::V0(m) => { m.instructions[0].data = vec![0xDE, 0xAD, 0xBE, 0xEF]; }
        _ => unreachable!(),
    }
    sign_v0_tx(&mut malicious, &payer);
    let malicious_bytes = bincode::serialize(&malicious).expect("serialize malicious");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let result = orch.complete(&request_id, &malicious_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for malicious data, got {:?}", result
    );
    assert_eq!(orch.pending_count(), 0);
}

// ── Layer 2: Format confusion ────────────────────────────────────────────────

// 2a. Legacy original, V0 signed bytes rejected
#[tokio::test]
async fn legacy_original_v0_signed_bytes_rejected() {
    // Original is legacy.
    let (payer, _tx, legacy_bytes) = make_test_tx();

    // Signed bytes are V0.
    let (payer2, mut vtx, _) = make_test_v0_tx();
    sign_v0_tx(&mut vtx, &payer2);
    let v0_signed_bytes = bincode::serialize(&vtx).expect("serialize v0");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    // Original is legacy, so complete() dispatches to complete_legacy().
    // complete_legacy() tries bincode::deserialize::<Transaction>(v0_bytes) → fails.
    let request = make_request(payer.pubkey(), legacy_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let result = orch.complete(&request_id, &v0_signed_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for legacy-original+V0-signed mismatch, got {:?}", result
    );
}

// 2b. Corrupted signed bytes rejected
#[tokio::test]
async fn v0_corrupted_signed_bytes_rejected() {
    let (payer, _vtx, tx_bytes) = make_test_v0_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let result = orch.complete(&request_id, b"garbage_bytes").await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for garbage bytes, got {:?}", result
    );
}

// 2c. Unsigned V0 (default signature) rejected
#[tokio::test]
async fn v0_unsigned_default_signature_rejected() {
    let (payer, vtx, tx_bytes) = make_test_v0_tx();

    // vtx is NOT signed (all signatures are Signature::default()).
    let unsigned_bytes = bincode::serialize(&vtx).expect("serialize unsigned");

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let result = orch.complete(&request_id, &unsigned_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed for unsigned V0 tx, got {:?}", result
    );
}

// ── Layer 3: Exactly-once semantics ──────────────────────────────────────────

// 3a. V0 complete twice: second attempt is NotFound
#[tokio::test]
async fn v0_complete_twice_second_is_not_found() {
    let (payer, mut vtx, tx_bytes) = make_test_v0_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    sign_v0_tx(&mut vtx, &payer);
    let signed_bytes = bincode::serialize(&vtx).expect("serialize signed");

    // First complete succeeds.
    let outcome = orch.complete(&request_id, &signed_bytes).await
        .expect("first complete should succeed");
    assert!(matches!(outcome, SignatureOutcome::Signed { .. }));
    assert_eq!(orch.pending_count(), 0);

    // Second complete → RequestNotFound (entry already consumed).
    let result2 = orch.complete(&request_id, &signed_bytes).await;
    assert!(
        matches!(result2, Err(SignatureError::RequestNotFound)),
        "expected RequestNotFound on second complete, got {:?}", result2
    );
}

// 3b. Failed complete leaves no pending hole; second attempt is NotFound
#[tokio::test]
async fn v0_failed_complete_consumes_entry_second_is_not_found() {
    let (payer, original_vtx, tx_bytes) = make_test_v0_tx();

    let external = Arc::new(ExternalWalletAdapter::new(
        Arc::new(AlwaysBoundBindings),
    )) as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), tx_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");
    assert_eq!(orch.pending_count(), 1);

    // Complete with wrong data → VerificationFailed, entry consumed.
    let mut malicious = original_vtx.clone();
    match &mut malicious.message {
        VersionedMessage::V0(m) => { m.instructions[0].data = vec![0xBA, 0xD0]; }
        _ => unreachable!(),
    }
    sign_v0_tx(&mut malicious, &payer);
    let bad_bytes = bincode::serialize(&malicious).expect("serialize bad");

    let result = orch.complete(&request_id, &bad_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "expected VerificationFailed, got {:?}", result
    );
    // Entry consumed even on failure.
    assert_eq!(orch.pending_count(), 0);

    // Second attempt → RequestNotFound (not VerificationFailed again).
    let result2 = orch.complete(&request_id, &bad_bytes).await;
    assert!(
        matches!(result2, Err(SignatureError::RequestNotFound)),
        "expected RequestNotFound after failed complete, got {:?}", result2
    );
}

// ── A1-Execute regression tests ─────────────────────────────────────────────
//
// Tests 3 and 4 of the A1-Execute test plan. They lock down the Phantom-
// canonicalization tolerance added to `complete_v0()` during the first live
// daemon-completion attempt (error `"V0 instruction 1 accounts mismatch"`):
//
//   - `v0_phantom_canonicalization_is_accepted` (test 3): a V0 message whose
//     `static_account_keys` have been reordered and whose instruction
//     account indices have been updated accordingly must verify, because
//     the semantic resolution to (pubkey, ...) is identical.
//
//   - `v0_resolved_identity_swap_is_rejected` (test 4): same index layout,
//     but one of the static slots holds a DIFFERENT pubkey (an attacker
//     swapping destination). The resolved identity differs; complete_v0
//     must reject.
//
// Together they encode the safety boundary for the resolved-identity check:
// reordering is fine, substitution is not.

/// Build two V0 messages that MUST be semantically equivalent but bytewise
/// different: `original` uses key order [payer, program, extra_acct];
/// `canonical` uses key order [payer, extra_acct, program]. Both target the
/// same instruction (program called with [payer, extra_acct] and identical
/// data + blockhash).
fn build_reordered_v0_pair(
    payer: &Keypair,
    program: Pubkey,
    extra_acct: Pubkey,
    blockhash: Hash,
    data: &[u8],
) -> (VersionedTransaction, VersionedTransaction) {
    use solana_sdk::instruction::CompiledInstruction;
    use solana_sdk::message::MessageHeader;

    // Both messages: one required signature (the payer). The V0 runtime
    // requires static_account_keys[0] to be the fee-payer and the single
    // signer; we preserve that invariant in BOTH messages.
    let header = MessageHeader {
        num_required_signatures: 1,
        num_readonly_signed_accounts: 0,
        num_readonly_unsigned_accounts: 1, // program is readonly-unsigned
    };

    // Original key order: [payer, program, extra_acct]
    let orig_keys = vec![payer.pubkey(), program, extra_acct];
    let orig_ix = CompiledInstruction {
        program_id_index: 1,      // program at index 1
        accounts: vec![0, 2],     // payer (0), extra_acct (2)
        data: data.to_vec(),
    };
    let orig_msg = v0::Message {
        header,
        account_keys: orig_keys,
        recent_blockhash: blockhash,
        instructions: vec![orig_ix],
        address_table_lookups: vec![],
    };

    // Canonicalized key order: [payer, extra_acct, program]
    let canon_keys = vec![payer.pubkey(), extra_acct, program];
    let canon_ix = CompiledInstruction {
        program_id_index: 2,      // program moved to index 2
        accounts: vec![0, 1],     // payer (0), extra_acct now at 1
        data: data.to_vec(),
    };
    let canon_msg = v0::Message {
        header,
        account_keys: canon_keys,
        recent_blockhash: blockhash,
        instructions: vec![canon_ix],
        address_table_lookups: vec![],
    };

    let orig_vtx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(orig_msg),
    };
    let canon_vtx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(canon_msg),
    };
    (orig_vtx, canon_vtx)
}

#[tokio::test]
async fn v0_phantom_canonicalization_is_accepted() {
    let payer = Keypair::new();
    let program = Pubkey::new_unique();
    let extra_acct = Pubkey::new_unique();
    let blockhash = Hash::new_unique();
    let data = vec![0xAA, 0xBB, 0xCC];

    let (orig_vtx, mut canon_vtx) =
        build_reordered_v0_pair(&payer, program, extra_acct, blockhash, &data);

    // Sanity: the two messages really are bytewise different (otherwise the
    // test isn't exercising the new resolved-identity path — it would pass
    // even under the old strict byte-compare).
    let orig_msg_bytes = orig_vtx.message.serialize();
    let canon_msg_bytes = canon_vtx.message.serialize();
    assert_ne!(
        orig_msg_bytes, canon_msg_bytes,
        "canonicalized message must differ bytewise from original — otherwise the test \
         is trivially equivalent to the byte-identical case and does not cover Phantom's \
         re-canonicalization behavior"
    );

    // Serialize the original (the "parked" tx) and sign the canonicalized
    // one (the "Phantom-signed" tx).
    let orig_bytes = bincode::serialize(&orig_vtx).expect("serialize orig");
    sign_v0_tx(&mut canon_vtx, &payer);
    let canon_signed_bytes = bincode::serialize(&canon_vtx).expect("serialize canon signed");

    let external =
        Arc::new(ExternalWalletAdapter::new(Arc::new(AlwaysBoundBindings)))
            as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);

    let request = make_request(payer.pubkey(), orig_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let outcome = orch
        .complete(&request_id, &canon_signed_bytes)
        .await
        .expect("canonicalized-but-semantically-equivalent V0 must verify");

    match outcome {
        SignatureOutcome::Signed {
            adapter_type,
            signature,
            signed_tx_bytes,
            ..
        } => {
            assert_eq!(adapter_type, "external");
            assert!(!signature.is_empty());
            // The returned bytes must still serialize as V0 so the daemon's
            // dispatcher (see `orchestrator::classify_signed_tx_kind`) will
            // route them to `send_raw_v0_transaction`, not legacy.
            let re: VersionedTransaction =
                bincode::deserialize(&signed_tx_bytes).expect("V0 shape preserved");
            assert!(
                matches!(re.message, VersionedMessage::V0(_)),
                "completed signed bytes must remain V0 for correct RPC routing"
            );
        }
        _ => panic!("expected Signed outcome"),
    }
    assert_eq!(orch.pending_count(), 0);
}

#[tokio::test]
async fn v0_resolved_identity_swap_is_rejected() {
    // A1-Execute test 4: prove the canonicalization tolerance did NOT
    // weaken the tamper boundary. Tamper case: signed tx keeps the same
    // index layout but swaps one static-key slot to a different pubkey
    // (an attacker trying to re-route a transfer at the same index).
    // Data, blockhash, program, index positions all unchanged — the only
    // difference is the pubkey at one static slot.
    use solana_sdk::instruction::CompiledInstruction;
    use solana_sdk::message::MessageHeader;

    let payer = Keypair::new();
    let program = Pubkey::new_unique();
    let legit_account = Pubkey::new_unique();
    let attacker_account = Pubkey::new_unique();
    let blockhash = Hash::new_unique();
    let data = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let header = MessageHeader {
        num_required_signatures: 1,
        num_readonly_signed_accounts: 0,
        num_readonly_unsigned_accounts: 1,
    };
    let ix = CompiledInstruction {
        program_id_index: 1,
        accounts: vec![0, 2],
        data: data.clone(),
    };

    // Original: legit account at static[2].
    let orig_msg = v0::Message {
        header,
        account_keys: vec![payer.pubkey(), program, legit_account],
        recent_blockhash: blockhash,
        instructions: vec![ix.clone()],
        address_table_lookups: vec![],
    };
    let orig_vtx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(orig_msg),
    };
    let orig_bytes = bincode::serialize(&orig_vtx).expect("serialize orig");

    // Tampered: same header, same index layout, same data, same blockhash,
    // but static[2] = attacker_account instead of legit_account. The signer
    // signs over THIS tampered message (attacker has the signer's key in
    // this threat model; the resolved-identity check is the remaining
    // defense against semantic substitution).
    let tampered_msg = v0::Message {
        header,
        account_keys: vec![payer.pubkey(), program, attacker_account],
        recent_blockhash: blockhash,
        instructions: vec![ix],
        address_table_lookups: vec![],
    };
    let mut tampered_vtx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(tampered_msg),
    };
    sign_v0_tx(&mut tampered_vtx, &payer);
    let tampered_bytes = bincode::serialize(&tampered_vtx).expect("serialize tampered");

    let external =
        Arc::new(ExternalWalletAdapter::new(Arc::new(AlwaysBoundBindings)))
            as Arc<dyn WalletAdapter>;
    let orch = SignatureOrchestrator::new(vec![external]);
    let request = make_request(payer.pubkey(), orig_bytes);
    let request_id = request.id;
    orch.submit(request).await.expect("submit");

    let result = orch.complete(&request_id, &tampered_bytes).await;
    assert!(
        matches!(result, Err(SignatureError::VerificationFailed(_))),
        "resolved-identity swap must fail verification (even though ed25519 \
         would pass since the attacker re-signed) — got {:?}",
        result
    );
    // Entry consumed on failure (same semantics as every other verifier rejection).
    assert_eq!(orch.pending_count(), 0);
}

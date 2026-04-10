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
    hash::Hash,
    message::Message,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer as SolanaSigner,
    system_instruction,
    transaction::Transaction,
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

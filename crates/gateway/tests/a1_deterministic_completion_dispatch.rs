//! Deterministic tests for tests 5 and 6 of the A1-Execute test plan.
//!
//! These tests lock contracts at the **daemon completion path** level that
//! are adjacent to — but not covered by — the per-layer unit tests:
//!
//! - **Test 5 — Blockhash Modified outcome**: when `complete_v0` returns
//!   `SignatureError::BlockhashModified`, the caller-facing
//!   `WalletSignatureOutcome` must carry `rebuild_required = true` AND
//!   `accepted = false` AND never reach the `send_v0` / `send_raw_transaction`
//!   path. Locks `strict blockhash mode` end-to-end.
//!
//! - **Test 6 — V0 daemon dispatch contract**: the daemon's wallet-signature
//!   completion flow classifies the client-submitted signed bytes via
//!   `orchestrator::classify_signed_tx_kind` and routes to
//!   `send_raw_v0_transaction` for V0, `send_raw_transaction` for legacy.
//!   Here we exercise the end-to-end chain:
//!
//!     `orchestrator.complete(...)` → `signed_tx_bytes` →
//!     `classify_signed_tx_kind` → expected `SignedTxKind`
//!
//!   locking the invariant that `complete_v0` never silently produces
//!   legacy-shaped signed bytes that would fall through to the legacy RPC
//!   path.

use std::sync::Arc;

use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, Message as LegacyMessage, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::{Transaction, VersionedTransaction},
};
use uuid::Uuid;

use claw_api::{PendingWalletSignatureInfo, WalletSignatureHandler, WalletSignatureOutcome};
use claw_gateway::orchestrator::{
    adapter::{ExternalWalletBindings, WalletAdapter},
    classify_signed_tx_kind,
    external_adapter::ExternalWalletAdapter,
    types::{SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId},
    SignedTxKind, SignatureOrchestrator,
};
use claw_types::session::SessionId;

// ── Shared fixtures ─────────────────────────────────────────────────────────

struct AlwaysBound;
impl ExternalWalletBindings for AlwaysBound {
    fn is_bound(&self, _: &SessionId, _: &str) -> bool {
        true
    }
}

fn make_v0_tx(payer: &Keypair) -> (VersionedTransaction, Vec<u8>) {
    let program = Pubkey::new_unique();
    let extra = Pubkey::new_unique();
    let ix = Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(extra, false),
        ],
        data: vec![1, 2, 3, 4],
    };
    let msg = v0::Message::try_compile(&payer.pubkey(), &[ix], &[], Hash::new_unique())
        .expect("compile v0");
    let vtx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(msg),
    };
    let bytes = bincode::serialize(&vtx).expect("serialize");
    (vtx, bytes)
}

fn sign_v0_in_place(vtx: &mut VersionedTransaction, payer: &Keypair) {
    let msg_bytes = vtx.message.serialize();
    let sig = payer.sign_message(&msg_bytes);
    vtx.signatures[0] = sig;
}

fn make_request(session_id: SessionId, signer: Pubkey, tx_bytes: Vec<u8>) -> SignatureRequest {
    SignatureRequest {
        id: SignatureRequestId::new(),
        session_id,
        transaction_id: Uuid::new_v4(),
        finalized_tx_bytes: tx_bytes,
        expected_signer: signer,
        description: "A1 deterministic dispatch test".into(),
    }
}

// ── Recording broadcaster ───────────────────────────────────────────────────
//
// Purpose: count calls to each RPC method as the handler dispatches. The
// handler's `submit_signed_tx` uses the SAME `classify_signed_tx_kind`
// function the daemon uses; the recorder simply observes which branch ran.

#[derive(Default)]
struct BroadcastRecorder {
    v0_calls: std::sync::atomic::AtomicUsize,
    legacy_calls: std::sync::atomic::AtomicUsize,
}

impl BroadcastRecorder {
    fn v0(&self) -> usize {
        self.v0_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn legacy(&self) -> usize {
        self.legacy_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Handler that mirrors `GatewayWalletSignatureHandler::submit_signed_tx_inner`'s
/// classify → dispatch shape exactly, but records calls to a `BroadcastRecorder`
/// instead of hitting real RPC. Uses the SAME `classify_signed_tx_kind` the
/// daemon uses — so any regression in classifier behavior is directly
/// observable here.
struct RecordingHandler {
    orchestrator: SignatureOrchestrator,
    recorder: Arc<BroadcastRecorder>,
}

impl WalletSignatureHandler for RecordingHandler {
    fn submit_signed_tx(
        &self,
        _session_id: &SessionId,
        request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = WalletSignatureOutcome> + Send + '_>,
    > {
        let recorder = self.recorder.clone();
        let orch = self.orchestrator.clone();
        Box::pin(async move {
            use base64::Engine;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(&signed_tx_b64)
                .unwrap();
            let sig_req_id = SignatureRequestId::from(request_id);

            match orch.complete(&sig_req_id, &raw).await {
                Ok(SignatureOutcome::Signed { signed_tx_bytes, signature, .. }) => {
                    // Classify + record, exactly as daemon.rs does.
                    match classify_signed_tx_kind(&signed_tx_bytes) {
                        SignedTxKind::V0 => {
                            recorder
                                .v0_calls
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        SignedTxKind::Legacy => {
                            recorder
                                .legacy_calls
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    WalletSignatureOutcome {
                        request_id,
                        accepted: true,
                        signature: Some(signature),
                        tx_signature: Some("RECORDED".into()),
                        submitted: true,
                        error: None,
                        rebuild_required: false,
                    }
                }
                Ok(SignatureOutcome::Pending { .. }) => unreachable!("not returned by complete"),
                Err(e) => {
                    let rebuild_required = matches!(e, SignatureError::BlockhashModified);
                    WalletSignatureOutcome {
                        request_id,
                        accepted: false,
                        signature: None,
                        tx_signature: None,
                        submitted: false,
                        error: Some(e.to_string()),
                        rebuild_required,
                    }
                }
            }
        })
    }

    fn pending_for_session(&self, _: &SessionId) -> Vec<PendingWalletSignatureInfo> {
        vec![]
    }
    fn bind_wallet(&self, _: &SessionId, _: &str) {}
    fn wallets_for_session(&self, _: &SessionId) -> Vec<String> {
        vec![]
    }
}

// ── Test 5 — Blockhash modified outcome ─────────────────────────────────────

#[tokio::test]
async fn test5_blockhash_modified_surfaces_rebuild_required_and_never_sends() {
    let payer = Keypair::new();
    let (original_vtx, original_bytes) = make_v0_tx(&payer);

    // Build the "signed" version with a MODIFIED blockhash, then re-sign so
    // ed25519 itself would pass. This is the exact shape `BlockhashModified`
    // protects against: an external signer changed the V0 blockhash.
    let mut tampered = original_vtx.clone();
    match &mut tampered.message {
        VersionedMessage::V0(m) => {
            m.recent_blockhash = Hash::new_unique();
        }
        _ => unreachable!(),
    }
    sign_v0_in_place(&mut tampered, &payer);
    let tampered_bytes = bincode::serialize(&tampered).expect("serialize");

    let external: Arc<dyn WalletAdapter> =
        Arc::new(ExternalWalletAdapter::new(Arc::new(AlwaysBound)));
    let orchestrator = SignatureOrchestrator::new(vec![external]);

    let session_id = SessionId::from(Uuid::new_v4());
    let request = make_request(session_id.clone(), payer.pubkey(), original_bytes);
    let request_id = request.id.0;
    orchestrator.submit(request).await.expect("submit");

    let recorder = Arc::new(BroadcastRecorder::default());
    let handler = RecordingHandler {
        orchestrator: orchestrator.clone(),
        recorder: recorder.clone(),
    };

    use base64::Engine;
    let tampered_b64 =
        base64::engine::general_purpose::STANDARD.encode(&tampered_bytes);

    let outcome = handler
        .submit_signed_tx(&session_id, request_id, tampered_b64)
        .await;

    // ── The A1-Execute test 5 assertions ──────────────────────────────────
    assert!(!outcome.accepted, "accepted must be false on blockhash mutation");
    assert!(
        outcome.rebuild_required,
        "rebuild_required must be true — the caller is told to JIT-rebuild"
    );
    assert!(!outcome.submitted, "submitted must be false");
    assert!(outcome.tx_signature.is_none(), "no on-chain signature must be produced");
    assert!(outcome.signature.is_none(), "no wallet signature must be accepted");
    assert!(
        outcome
            .error
            .as_ref()
            .map(|e| e.contains("blockhash") || e.contains("rebuild"))
            .unwrap_or(false),
        "error message must mention blockhash/rebuild for operator UX: {:?}",
        outcome.error
    );

    // Structural safety: the broadcast recorder MUST NOT have seen any call.
    assert_eq!(
        recorder.v0(),
        0,
        "send_v0 must never be reached when verification rejects with BlockhashModified"
    );
    assert_eq!(
        recorder.legacy(),
        0,
        "send_raw_transaction (legacy) must also not be reached"
    );
}

// ── Test 6 — V0 daemon dispatch contract ────────────────────────────────────

#[tokio::test]
async fn test6_v0_bytes_from_complete_classify_as_v0_not_legacy() {
    // End-to-end: submit an original V0 tx, sign it, call complete(), take
    // the resulting `signed_tx_bytes` and verify the CLASSIFIER (the same
    // one `daemon.rs` routes on) decides V0. This locks the chain from
    // `complete_v0`'s output all the way to the daemon's RPC dispatch.
    let payer = Keypair::new();
    let (mut vtx, tx_bytes) = make_v0_tx(&payer);

    let external: Arc<dyn WalletAdapter> =
        Arc::new(ExternalWalletAdapter::new(Arc::new(AlwaysBound)));
    let orchestrator = SignatureOrchestrator::new(vec![external]);

    let session_id = SessionId::from(Uuid::new_v4());
    let request = make_request(session_id.clone(), payer.pubkey(), tx_bytes);
    let request_id = request.id.0;
    orchestrator.submit(request).await.expect("submit");

    sign_v0_in_place(&mut vtx, &payer);
    let signed_bytes = bincode::serialize(&vtx).expect("serialize");

    let recorder = Arc::new(BroadcastRecorder::default());
    let handler = RecordingHandler {
        orchestrator: orchestrator.clone(),
        recorder: recorder.clone(),
    };

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

    let outcome = handler
        .submit_signed_tx(&session_id, request_id, b64)
        .await;

    assert!(outcome.accepted, "happy V0 path must accept: {:?}", outcome.error);
    assert!(outcome.submitted, "happy V0 path must record submitted=true");

    // ── The A1-Execute test 6 assertion ───────────────────────────────────
    // EXACTLY one V0 dispatch. ZERO legacy dispatches.
    assert_eq!(
        recorder.v0(),
        1,
        "V0 signed bytes MUST route through send_raw_v0_transaction"
    );
    assert_eq!(
        recorder.legacy(),
        0,
        "V0 signed bytes MUST NOT fall through to legacy send_raw_transaction"
    );
}

#[tokio::test]
async fn test6_legacy_bytes_from_complete_classify_as_legacy() {
    // Symmetric negative: a legacy (non-V0) tx must route to legacy send.
    // Stand-up: skip orchestrator complete() entirely and just exercise the
    // classifier's daemon-facing contract: legacy bytes → Legacy variant →
    // `send_raw_transaction` branch.
    let payer = Keypair::new();
    let program = Pubkey::new_unique();
    let ix = Instruction {
        program_id: program,
        accounts: vec![AccountMeta::new(payer.pubkey(), true)],
        data: vec![9, 9, 9],
    };
    let legacy_tx = Transaction::new_with_payer(&[ix], Some(&payer.pubkey()));
    let legacy_bytes = bincode::serialize(&legacy_tx).expect("serialize legacy");

    assert_eq!(
        classify_signed_tx_kind(&legacy_bytes),
        SignedTxKind::Legacy,
        "legacy transaction bytes MUST classify as Legacy so daemon routes \
         them to send_raw_transaction, not send_raw_v0_transaction"
    );

    // Also verify explicit VersionedMessage::Legacy still routes legacy.
    let legacy_msg = LegacyMessage::new(&[Instruction {
        program_id: Pubkey::new_unique(),
        accounts: vec![],
        data: vec![],
    }], Some(&payer.pubkey()));
    let legacy_versioned = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(legacy_msg),
    };
    let lv_bytes = bincode::serialize(&legacy_versioned).expect("serialize");
    assert_eq!(
        classify_signed_tx_kind(&lv_bytes),
        SignedTxKind::Legacy,
        "VersionedMessage::Legacy must NOT route to V0 send"
    );
}

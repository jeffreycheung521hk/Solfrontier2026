//! N10B — External signer bridge simulation tests.
//!
//! Tests the full HTTP round-trip for the external wallet signature flow:
//!
//! 1. Bind wallet to session via POST /sessions/:id/bind-wallet
//! 2. Park a transaction (simulating what the signing tool does)
//! 3. List pending via GET /sessions/:id/wallet-signatures
//! 4. Retrieve unsigned_tx_b64, sign locally, submit via POST
//! 5. Assert acceptance, signature present, pending entry removed
//!
//! Also tests rejection paths: tampered message, wrong signer.
//!
//! Uses axum::Router + tower::ServiceExt::oneshot — no TCP listener needed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use solana_sdk::{
    hash::Hash,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer as SdkSigner},
    system_instruction,
    transaction::Transaction,
};
use tokio::sync::broadcast;
use tower::ServiceExt;
use uuid::Uuid;

use claw_api::{
    AppState, ApprovalHandler, ApprovalHandlerRef, AuthToken,
    EventSubscriber, EventSubscriberRef,
    MessageHandler, MessageHandlerRef, SessionManagerRef, SessionOps,
    WalletChallengeHandler, WalletChallengeHandlerRef, WalletChallengeInfo,
    WalletSignatureHandler, WalletSignatureHandlerRef, WalletSignatureOutcome,
    PendingWalletSignatureInfo,
};
use claw_gateway::ExternalWalletStore;
use claw_observability::HealthRegistry;
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::GatewayEvent,
    policy::PolicyVerdict,
    session::SessionId,
    transaction::{SimulationResult, TransactionProposal},
};

// ── Stub implementations for AppState ─────────────────────────────────────────

struct StubSessionOps {
    active_session: SessionId,
}

impl SessionOps for StubSessionOps {
    fn open(&self, _role: AgentRole, _channel: String) -> SessionId {
        self.active_session.clone()
    }
    fn close(&self, _id: &SessionId, _reason: &str) {}
    fn active_count(&self) -> usize { 1 }
    fn is_active(&self, id: &SessionId) -> bool {
        id == &self.active_session
    }
}

struct StubMessageHandler;

impl MessageHandler for StubMessageHandler {
    fn handle<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _command: AgentCommand,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>> {
        Box::pin(async { Err("stub".into()) })
    }
}

struct StubApprovalHandler;

impl ApprovalHandler for StubApprovalHandler {
    fn pending_for_session(&self, _session_id: &SessionId) -> Vec<ApprovalRequest> {
        vec![]
    }
    fn session_for_request(&self, _request_id: uuid::Uuid) -> Option<SessionId> {
        None
    }
    fn decide(
        &self,
        _decision: ApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>> {
        Box::pin(async {
            (ApprovalOutcome::NotFound, None)
        })
    }
}

struct StubEventSubscriber;

impl EventSubscriber for StubEventSubscriber {
    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        let (tx, rx) = broadcast::channel(1);
        drop(tx);
        rx
    }
}

struct StubWalletChallengeHandler;

impl WalletChallengeHandler for StubWalletChallengeHandler {
    fn create_challenge(
        &self,
        _session_id: &SessionId,
        _wallet_pubkey: &str,
    ) -> Pin<Box<dyn Future<Output = Result<WalletChallengeInfo, String>> + Send + '_>> {
        Box::pin(async { Err("stub: not implemented".to_string()) })
    }
    fn verify_and_bind(
        &self,
        _session_id: &SessionId,
        _challenge_id: &str,
        _wallet_pubkey: &str,
        _signature_b64: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Err("stub: not implemented".to_string()) })
    }
}

// ── Real WalletSignatureHandler backed by ExternalWalletStore ─────────────────
//
// Uses the same verification logic as the production daemon, but without
// audit, events, or spend tracking. This tests the HTTP layer + verification.

struct TestWalletSignatureHandler {
    store: ExternalWalletStore,
}

impl WalletSignatureHandler for TestWalletSignatureHandler {
    fn submit_signed_tx(
        &self,
        _session_id: &SessionId,
        request_id: Uuid,
        signed_tx_b64: String,
    ) -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>> {
        Box::pin(async move {
            // Decode base64
            let signed_tx_bytes = match base64::engine::general_purpose::STANDARD.decode(&signed_tx_b64) {
                Ok(b) => b,
                Err(e) => {
                    return WalletSignatureOutcome {
                        request_id,
                        accepted: false,
                        signature: None,
                        tx_signature: None,
                        submitted: false,
                        error: Some(format!("invalid base64: {e}")),
                    };
                }
            };

            // Use the store-level submit function
            match claw_gateway::submit_signed_transaction(&self.store, request_id, signed_tx_bytes) {
                Ok(()) => {
                    WalletSignatureOutcome {
                        request_id,
                        accepted: true,
                        signature: Some("test-signature".into()),
                        tx_signature: None,
                        submitted: false,
                        error: None,
                    }
                }
                Err(e) => {
                    WalletSignatureOutcome {
                        request_id,
                        accepted: false,
                        signature: None,
                        tx_signature: None,
                        submitted: false,
                        error: Some(e.to_string()),
                    }
                }
            }
        })
    }

    fn pending_for_session(&self, session_id: &SessionId) -> Vec<PendingWalletSignatureInfo> {
        self.store
            .pending_for_session(session_id)
            .into_iter()
            .map(|(request_id, transaction_id, description, expected_signer, tx_bytes)| {
                let unsigned_tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
                PendingWalletSignatureInfo {
                    request_id,
                    transaction_id,
                    description,
                    expected_signer,
                    unsigned_tx_b64,
                }
            })
            .collect()
    }

    fn bind_wallet(&self, session_id: &SessionId, pubkey: &str) {
        self.store.bind_wallet(session_id, pubkey);
    }

    fn wallets_for_session(&self, session_id: &SessionId) -> Vec<String> {
        self.store.wallets_for_session(session_id)
    }
}

// ── Test harness ──────────────────────────────────────────────────────────────

fn make_test_tx(from: &Pubkey, to: &Pubkey) -> Transaction {
    let ix = system_instruction::transfer(from, to, 1_000_000);
    let msg = Message::new(&[ix], Some(from));
    let mut tx = Transaction::new_unsigned(msg);
    tx.message.recent_blockhash = Hash::new_unique();
    tx
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
        description: "bridge sim test".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![],
        created_at: chrono::Utc::now(),
    }
}

struct TestHarness {
    router: axum::Router,
    store: ExternalWalletStore,
    session_id: SessionId,
    token: String,
}

fn setup() -> TestHarness {
    let session_id = SessionId::from(Uuid::new_v4());
    let store = ExternalWalletStore::new();
    let token = "test-token-12345".to_string();

    let handler = TestWalletSignatureHandler {
        store: store.clone(),
    };

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(StubSessionOps {
            active_session: session_id.clone(),
        })),
        message_handler: MessageHandlerRef::new(Arc::new(StubMessageHandler)),
        approval: ApprovalHandlerRef::new(Arc::new(StubApprovalHandler)),
        events: EventSubscriberRef::new(Arc::new(StubEventSubscriber)),
        wallet_signatures: WalletSignatureHandlerRef::new(Arc::new(handler)),
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(StubWalletChallengeHandler)),
        auth_token: AuthToken::new(token.clone()),
    };

    let health = HealthRegistry::new();
    let router = claw_api::create_router(state, health);

    TestHarness { router, store, session_id, token }
}

async fn send_json(
    router: &mut axum::Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .uri(path)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json");

    let req = match method {
        "GET" => builder.method("GET").body(Body::empty()).unwrap(),
        "POST" => {
            let body_str = body.map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string());
            builder.method("POST").body(Body::from(body_str)).unwrap()
        }
        _ => panic!("unsupported method"),
    };

    let response = router.as_service().oneshot(req).await.unwrap();
    let status = response.status();
    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body_bytes).unwrap_or(json!(null));

    (status, json)
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════════

/// Full round-trip: bind wallet → park tx → list pending → sign → submit → accepted.
#[tokio::test]
async fn bridge_roundtrip_happy_path() {
    let harness = setup();
    let mut router = harness.router;
    let session_str = harness.session_id.to_string();
    let signer = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&signer.pubkey(), &to);
    let request_id = Uuid::new_v4();

    // 1. Bind wallet
    let (status, body) = send_json(
        &mut router,
        "POST",
        &format!("/sessions/{}/bind-wallet", session_str),
        &harness.token,
        Some(json!({ "pubkey": signer.pubkey().to_string() })),
    ).await;
    assert_eq!(status, StatusCode::OK, "bind-wallet: {body}");
    assert_eq!(body["bound"], true);

    // 2. Park a transaction (simulating what the signing tool does internally)
    let _rx = harness.store.park(
        request_id,
        fake_proposal(harness.session_id.clone(), &signer.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        signer.pubkey().to_string(),
    ).unwrap();

    // 3. List pending — should see the parked entry
    let (status, body) = send_json(
        &mut router,
        "GET",
        &format!("/sessions/{}/wallet-signatures", session_str),
        &harness.token,
        None,
    ).await;
    assert_eq!(status, StatusCode::OK, "list: {body}");
    assert_eq!(body["pending_count"], 1);

    let unsigned_tx_b64 = body["requests"][0]["unsigned_tx_b64"]
        .as_str()
        .expect("must have unsigned_tx_b64");

    // 4. Decode, sign, re-encode (simulating Phantom)
    let unsigned_bytes = base64::engine::general_purpose::STANDARD
        .decode(unsigned_tx_b64)
        .expect("valid base64");
    let mut decoded_tx: Transaction = bincode::deserialize(&unsigned_bytes)
        .expect("valid transaction");

    // Verify the decoded tx matches what we parked
    assert_eq!(decoded_tx.message_data(), tx.message_data());

    // Sign it
    decoded_tx.sign(&[&signer], decoded_tx.message.recent_blockhash);
    let signed_bytes = bincode::serialize(&decoded_tx).unwrap();
    let signed_tx_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

    // 5. Submit the signed tx
    let (status, body) = send_json(
        &mut router,
        "POST",
        &format!("/sessions/{}/wallet-signatures", session_str),
        &harness.token,
        Some(json!({
            "request_id": request_id.to_string(),
            "signed_tx_b64": signed_tx_b64,
        })),
    ).await;
    assert_eq!(status, StatusCode::ACCEPTED, "submit: {body}");
    assert_eq!(body["accepted"], true);
    assert!(body["signature"].is_string(), "must have signature");
    assert!(body["error"].is_null(), "no error on success");

    // 6. Pending should now be empty
    let (status, body) = send_json(
        &mut router,
        "GET",
        &format!("/sessions/{}/wallet-signatures", session_str),
        &harness.token,
        None,
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pending_count"], 0);
}

/// Tampered message (modified instructions) must be rejected via HTTP.
#[tokio::test]
async fn bridge_reject_tampered_message() {
    let harness = setup();
    let mut router = harness.router;
    let session_str = harness.session_id.to_string();
    let signer = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&signer.pubkey(), &to);
    let request_id = Uuid::new_v4();

    // Park the original tx
    let _rx = harness.store.park(
        request_id,
        fake_proposal(harness.session_id.clone(), &signer.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        signer.pubkey().to_string(),
    ).unwrap();

    // Create a DIFFERENT tx (different destination, different amount)
    let evil_to = Pubkey::new_unique();
    let evil_ix = system_instruction::transfer(&signer.pubkey(), &evil_to, 99_999_999);
    let evil_msg = Message::new(&[evil_ix], Some(&signer.pubkey()));
    let mut evil_tx = Transaction::new_unsigned(evil_msg);
    evil_tx.message.recent_blockhash = tx.message.recent_blockhash;
    evil_tx.sign(&[&signer], evil_tx.message.recent_blockhash);

    let evil_bytes = bincode::serialize(&evil_tx).unwrap();
    let evil_b64 = base64::engine::general_purpose::STANDARD.encode(&evil_bytes);

    // Submit the tampered tx
    let (status, body) = send_json(
        &mut router,
        "POST",
        &format!("/sessions/{}/wallet-signatures", session_str),
        &harness.token,
        Some(json!({
            "request_id": request_id.to_string(),
            "signed_tx_b64": evil_b64,
        })),
    ).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "tampered must be rejected: {body}");
    assert_eq!(body["accepted"], false);
    assert!(body["error"].as_str().unwrap().contains("message"), "error should mention message: {body}");
}

/// Wrong signer (unsigned tx submitted) must be rejected via HTTP.
#[tokio::test]
async fn bridge_reject_wrong_signer() {
    let harness = setup();
    let mut router = harness.router;
    let session_str = harness.session_id.to_string();
    let expected_signer = Keypair::new();
    let to = Pubkey::new_unique();
    let tx = make_test_tx(&expected_signer.pubkey(), &to);
    let request_id = Uuid::new_v4();

    // Park the tx expecting expected_signer
    let _rx = harness.store.park(
        request_id,
        fake_proposal(harness.session_id.clone(), &expected_signer.pubkey().to_string()),
        &tx,
        fake_simulation(),
        PolicyVerdict::Approved { rule_name: "test".into() },
        expected_signer.pubkey().to_string(),
    ).unwrap();

    // Submit the UNSIGNED tx — expected_signer position has default signature
    let unsigned_bytes = bincode::serialize(&tx).unwrap();
    let unsigned_b64 = base64::engine::general_purpose::STANDARD.encode(&unsigned_bytes);

    let (status, body) = send_json(
        &mut router,
        "POST",
        &format!("/sessions/{}/wallet-signatures", session_str),
        &harness.token,
        Some(json!({
            "request_id": request_id.to_string(),
            "signed_tx_b64": unsigned_b64,
        })),
    ).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "unsigned must be rejected: {body}");
    assert_eq!(body["accepted"], false);
    assert!(body["error"].as_str().unwrap().contains("sign"), "error should mention signing: {body}");
}

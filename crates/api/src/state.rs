//! Shared application state injected into route handlers.
//!
//! `AppState` is `Clone` and cheap to clone — all fields are `Arc`-wrapped
//! internally. It is injected via axum's `State` extractor.
//!
//! # Dependency discipline
//!
//! `claw-api` must NOT depend on `claw-gateway`. The `SessionOps`,
//! `MessageHandler`, `ApprovalHandler`, and `EventSubscriber` traits defined
//! here are minimal interfaces that `claw-gateway` implements. Adapters live
//! in `claw-gateway/daemon.rs`.

use std::{future::Future, pin::Pin, sync::Arc};

use tokio::sync::broadcast;

use serde::{Deserialize, Serialize};

use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::GatewayEvent,
    session::SessionId,
};

use crate::auth::AuthToken;

/// Minimal session management interface needed by the API layer.
pub trait SessionOps: Send + Sync + 'static {
    /// Opens a new session and returns its ID.
    fn open(&self, role: AgentRole, channel: String) -> SessionId;
    /// Closes a session by ID.
    fn close(&self, id: &SessionId, reason: &str);
    /// Returns the number of active sessions.
    fn active_count(&self) -> usize;
    /// Returns `true` if the given session is currently active.
    fn is_active(&self, id: &SessionId) -> bool;
}

/// Message handling interface — executes an agent command for a session.
///
/// Uses `BoxFuture` for object safety (avoiding `async_trait` macro complexity
/// across crate boundaries).
pub trait MessageHandler: Send + Sync + 'static {
    /// Handles an incoming message for the given session.
    fn handle<'a>(
        &'a self,
        session_id: &'a SessionId,
        command: AgentCommand,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>>;
}

/// Approval handling interface — registers pending requests and processes decisions.
///
/// Lives in `claw-api` as a trait so that route handlers can drive approvals
/// without a direct dependency on `claw-gateway`.
pub trait ApprovalHandler: Send + Sync + 'static {
    /// Returns all pending approval requests for the given session.
    fn pending_for_session(&self, session_id: &SessionId) -> Vec<ApprovalRequest>;

    /// Processes an operator decision.
    ///
    /// Returns the outcome and, on success, the original request so callers
    /// can resume the appropriate pipeline path. This call is non-async because
    /// the store is in-memory; audit persistence and event emission happen in the
    /// gateway implementation layer.
    fn decide(
        &self,
        decision: ApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>>;
}

/// Event subscription interface — hands the caller a broadcast receiver for
/// `GatewayEvent`s. The caller owns the receiver and is responsible for consuming
/// it promptly (slow consumers will lag and may receive `RecvError::Lagged`).
pub trait EventSubscriber: Send + Sync + 'static {
    /// Returns a new receiver subscribed to future gateway events.
    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent>;
}

/// The outcome of submitting a signed transaction from an external wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletSignatureOutcome {
    /// The request ID that was resolved.
    pub request_id: uuid::Uuid,
    /// `true` if the signed transaction was accepted (verification passed).
    pub accepted: bool,
    /// The wallet's ed25519 signature (present if verification passed).
    pub signature: Option<String>,
    /// On-chain transaction signature from RPC sendTransaction (present if submitted).
    pub tx_signature: Option<String>,
    /// Whether the transaction was successfully submitted to the Solana network.
    pub submitted: bool,
    /// Error message if verification or submission failed.
    pub error: Option<String>,
}

/// Handles signed transactions submitted by external wallets.
///
/// The gateway implementation verifies that the signed transaction
/// matches the parked unsigned transaction (message bytes must be
/// identical), then signs/submits it.
pub trait WalletSignatureHandler: Send + Sync + 'static {
    /// Submits a signed transaction from an external wallet.
    ///
    /// `request_id`: the wallet signature request ID (from the `awaiting_wallet_signature` response)
    /// `signed_tx_b64`: base64-encoded signed transaction bytes
    fn submit_signed_tx(
        &self,
        session_id: &SessionId,
        request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>>;

    /// Lists pending wallet signature requests for a session.
    fn pending_for_session(&self, session_id: &SessionId) -> Vec<PendingWalletSignatureInfo>;

    /// Binds an external wallet pubkey to a session.
    fn bind_wallet(&self, session_id: &SessionId, pubkey: &str);

    /// Returns the list of external wallets bound to a session.
    fn wallets_for_session(&self, session_id: &SessionId) -> Vec<String>;
}

/// Summary info for a pending wallet signature request (API-visible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingWalletSignatureInfo {
    /// The request ID.
    pub request_id: uuid::Uuid,
    /// The transaction ID.
    pub transaction_id: uuid::Uuid,
    /// Description of the transaction.
    pub description: String,
    /// The expected signer pubkey.
    pub expected_signer: String,
    /// Base64-encoded unsigned transaction for the external wallet to sign.
    pub unsigned_tx_b64: String,
}

/// A cloneable reference to a `WalletSignatureHandler` implementation.
#[derive(Clone)]
pub struct WalletSignatureHandlerRef(pub Arc<dyn WalletSignatureHandler>);

impl WalletSignatureHandlerRef {
    /// Wraps a `WalletSignatureHandler` implementation.
    pub fn new(inner: Arc<dyn WalletSignatureHandler>) -> Self {
        Self(inner)
    }

    /// Submits a signed transaction.
    pub async fn submit_signed_tx(
        &self,
        session_id: &SessionId,
        request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> WalletSignatureOutcome {
        self.0.submit_signed_tx(session_id, request_id, signed_tx_b64).await
    }

    /// Lists pending wallet signature requests.
    pub fn pending_for_session(&self, session_id: &SessionId) -> Vec<PendingWalletSignatureInfo> {
        self.0.pending_for_session(session_id)
    }

    /// Binds an external wallet.
    pub fn bind_wallet(&self, session_id: &SessionId, pubkey: &str) {
        self.0.bind_wallet(session_id, pubkey)
    }

    /// Returns bound external wallets.
    pub fn wallets_for_session(&self, session_id: &SessionId) -> Vec<String> {
        self.0.wallets_for_session(session_id)
    }
}

/// A cloneable reference to a `SessionOps` implementation.
#[derive(Clone)]
pub struct SessionManagerRef(pub Arc<dyn SessionOps>);

impl SessionManagerRef {
    /// Wraps a `SessionOps` implementation.
    pub fn new(inner: Arc<dyn SessionOps>) -> Self {
        Self(inner)
    }

    /// Opens a new session.
    pub fn open(&self, role: AgentRole, channel: impl Into<String>) -> SessionId {
        self.0.open(role, channel.into())
    }

    /// Closes a session.
    pub fn close(&self, id: &SessionId, reason: &str) {
        self.0.close(id, reason)
    }

    /// Returns the number of active sessions.
    pub fn active_count(&self) -> usize {
        self.0.active_count()
    }

    /// Returns `true` if the session is active.
    pub fn is_active(&self, id: &SessionId) -> bool {
        self.0.is_active(id)
    }
}

/// A cloneable reference to a `MessageHandler` implementation.
#[derive(Clone)]
pub struct MessageHandlerRef(pub Arc<dyn MessageHandler>);

impl MessageHandlerRef {
    /// Wraps a `MessageHandler` implementation.
    pub fn new(inner: Arc<dyn MessageHandler>) -> Self {
        Self(inner)
    }

    /// Dispatches a command to the agent.
    pub async fn handle(
        &self,
        session_id: &SessionId,
        command: AgentCommand,
    ) -> Result<AgentResponse, String> {
        self.0.handle(session_id, command).await
    }
}

/// A cloneable reference to an `ApprovalHandler` implementation.
#[derive(Clone)]
pub struct ApprovalHandlerRef(pub Arc<dyn ApprovalHandler>);

impl ApprovalHandlerRef {
    /// Wraps an `ApprovalHandler` implementation.
    pub fn new(inner: Arc<dyn ApprovalHandler>) -> Self {
        Self(inner)
    }

    /// Returns pending approval requests for a session.
    pub fn pending_for_session(&self, session_id: &SessionId) -> Vec<ApprovalRequest> {
        self.0.pending_for_session(session_id)
    }

    /// Processes an operator decision.
    pub async fn decide(
        &self,
        decision: ApprovalDecision,
    ) -> (ApprovalOutcome, Option<ApprovalRequest>) {
        self.0.decide(decision).await
    }
}

/// A cloneable reference to an `EventSubscriber` implementation.
#[derive(Clone)]
pub struct EventSubscriberRef(pub Arc<dyn EventSubscriber>);

impl EventSubscriberRef {
    /// Wraps an `EventSubscriber` implementation.
    pub fn new(inner: Arc<dyn EventSubscriber>) -> Self {
        Self(inner)
    }

    /// Subscribes to the event bus.
    pub fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.0.subscribe()
    }
}

/// Handles wallet ownership challenge-response flow.
///
/// The challenge-response proves that a client controls the private key
/// of the wallet pubkey before `bind_wallet()` is called.
pub trait WalletChallengeHandler: Send + Sync + 'static {
    /// Create a challenge for the given session and wallet pubkey.
    /// Returns challenge_id, message to sign, and expiry.
    fn create_challenge(
        &self,
        session_id: &SessionId,
        wallet_pubkey: &str,
    ) -> Pin<Box<dyn Future<Output = Result<WalletChallengeInfo, String>> + Send + '_>>;

    /// Verify a challenge response and bind the wallet on success.
    /// Returns the verified wallet pubkey.
    fn verify_and_bind(
        &self,
        session_id: &SessionId,
        challenge_id: &str,
        wallet_pubkey: &str,
        signature_b64: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;
}

/// Info returned when a challenge is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletChallengeInfo {
    /// The challenge ID (for the confirm step).
    pub challenge_id: String,
    /// The canonical message the client must sign.
    pub message: String,
    /// When this challenge expires (Unix ms).
    pub expires_at: i64,
}

/// A cloneable reference to a `WalletChallengeHandler` implementation.
#[derive(Clone)]
pub struct WalletChallengeHandlerRef(pub Arc<dyn WalletChallengeHandler>);

impl WalletChallengeHandlerRef {
    /// Wraps a `WalletChallengeHandler` implementation.
    pub fn new(inner: Arc<dyn WalletChallengeHandler>) -> Self {
        Self(inner)
    }

    /// Create a challenge.
    pub async fn create_challenge(
        &self,
        session_id: &SessionId,
        wallet_pubkey: &str,
    ) -> Result<WalletChallengeInfo, String> {
        self.0.create_challenge(session_id, wallet_pubkey).await
    }

    /// Verify and bind.
    pub async fn verify_and_bind(
        &self,
        session_id: &SessionId,
        challenge_id: &str,
        wallet_pubkey: &str,
        signature_b64: &str,
    ) -> Result<String, String> {
        self.0.verify_and_bind(session_id, challenge_id, wallet_pubkey, signature_b64).await
    }
}

/// The shared state injected into every route handler.
#[derive(Clone)]
pub struct AppState {
    /// Session lifecycle management.
    pub session_mgr:         SessionManagerRef,
    /// Agent command dispatch.
    pub message_handler:     MessageHandlerRef,
    /// Approval request handling (operator-in-the-loop).
    pub approval:            ApprovalHandlerRef,
    /// Event bus subscription factory (for SSE streams).
    pub events:              EventSubscriberRef,
    /// External wallet signature handling.
    pub wallet_signatures:   WalletSignatureHandlerRef,
    /// Wallet ownership challenge-response.
    pub wallet_challenges:   WalletChallengeHandlerRef,
    /// Bearer token for API authentication.
    pub auth_token:          AuthToken,
}

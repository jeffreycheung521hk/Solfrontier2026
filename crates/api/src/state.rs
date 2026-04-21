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
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest, ApprovalWorkflow},
    events::GatewayEvent,
    policy::PolicyRule,
    session::SessionId,
    wallet::SignerType,
};

use crate::auth::AuthToken;

/// Minimal session management interface needed by the API layer.
pub trait SessionOps: Send + Sync + 'static {
    /// Opens a new session and returns its ID.
    /// `policy_overrides`: optional per-session policy rules evaluated before global rules.
    fn open(&self, role: AgentRole, channel: String, policy_overrides: Option<Vec<PolicyRule>>) -> SessionId;
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

    /// Returns every pending approval across every session, with its workflow.
    /// Ordered by `requested_at` ascending.
    ///
    /// Default returns empty — suitable for tests using stub handlers that
    /// don't need to surface cross-session listings.
    fn all_pending(&self) -> Vec<PendingApprovalItem> {
        Vec::new()
    }

    /// Returns a single approval request with its workflow, or `None` if the ID
    /// is unknown. Covers both pending and already-decided requests.
    fn get_by_id(&self, request_id: uuid::Uuid) -> Option<PendingApprovalItem> {
        None
    }

    /// Peek at which session owns a pending request (P0-3: session-request binding).
    fn session_for_request(&self, request_id: uuid::Uuid) -> Option<SessionId>;

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

/// A pending approval plus its workflow, as returned by `/pending-approvals`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalItem {
    pub request:  ApprovalRequest,
    pub workflow: ApprovalWorkflow,
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
    /// `true` when the wallet modified the transaction's `recent_blockhash`.
    ///
    /// This is a retry signal: the caller should request a fresh JIT build from the
    /// original approved `SwapIntent` and re-send the new transaction for signing.
    /// All other verification failures leave this `false`.
    #[serde(default)]
    pub rebuild_required: bool,
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
    pub fn open(&self, role: AgentRole, channel: impl Into<String>, policy_overrides: Option<Vec<PolicyRule>>) -> SessionId {
        self.0.open(role, channel.into(), policy_overrides)
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

    /// Returns every pending approval across every session.
    pub fn all_pending(&self) -> Vec<PendingApprovalItem> {
        self.0.all_pending()
    }

    /// Returns a single approval by ID, with its workflow.
    pub fn get_by_id(&self, request_id: uuid::Uuid) -> Option<PendingApprovalItem> {
        self.0.get_by_id(request_id)
    }

    /// Peek at which session owns a pending request (P0-3).
    pub fn session_for_request(&self, request_id: uuid::Uuid) -> Option<SessionId> {
        self.0.session_for_request(request_id)
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

/// Result of proposing a transaction through the signing pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposeTransferResult {
    /// "awaiting_wallet_signature" or "signed"
    pub status: String,
    /// The wallet signature request ID (if awaiting external wallet).
    pub wallet_signature_request_id: Option<uuid::Uuid>,
    /// Base64-encoded unsigned transaction (if awaiting external wallet).
    pub unsigned_tx_b64: Option<String>,
    /// The on-chain signature (if auto-signed locally).
    pub signature: Option<String>,
    /// Error message (if failed).
    pub error: Option<String>,
}

/// Direct transaction proposal interface (bypasses LLM agent).
///
/// Provides deterministic transaction building for E2E testing and
/// operator-driven workflows.
pub trait TransactionProposer: Send + Sync + 'static {
    /// Builds a SOL transfer and routes it through the full signing pipeline.
    fn propose_transfer(
        &self,
        session_id: &SessionId,
        from: &str,
        to: &str,
        lamports: u64,
    ) -> Pin<Box<dyn Future<Output = Result<ProposeTransferResult, String>> + Send + '_>>;
}

/// A cloneable reference to a `TransactionProposer` implementation.
#[derive(Clone)]
pub struct TransactionProposerRef(pub Arc<dyn TransactionProposer>);

impl TransactionProposerRef {
    pub fn new(inner: Arc<dyn TransactionProposer>) -> Self {
        Self(inner)
    }

    pub async fn propose_transfer(
        &self,
        session_id: &SessionId,
        from: &str,
        to: &str,
        lamports: u64,
    ) -> Result<ProposeTransferResult, String> {
        self.0.propose_transfer(session_id, from, to, lamports).await
    }
}

// ── Read-only surfaces for the showcase / dashboard routes ──────────────────
//
// These traits expose a flat, UI-friendly snapshot of data that lives in
// claw-gateway / claw-state-store / claw-risk-engine. claw-api must not
// depend on those crates directly; adapters in `claw-gateway/daemon.rs`
// implement the traits.

/// Read the currently-loaded global policy rules.
pub trait PolicyReader: Send + Sync + 'static {
    fn rules(&self) -> Vec<PolicyRule>;
}

/// Cloneable reference to a `PolicyReader` implementation.
#[derive(Clone)]
pub struct PolicyReaderRef(pub Arc<dyn PolicyReader>);

impl PolicyReaderRef {
    pub fn new(inner: Arc<dyn PolicyReader>) -> Self { Self(inner) }
    pub fn rules(&self) -> Vec<PolicyRule> { self.0.rules() }
    /// No-op reader returning an empty rule list — useful for tests and
    /// bring-up paths where no policy is loaded.
    pub fn noop() -> Self {
        struct Noop;
        impl PolicyReader for Noop {
            fn rules(&self) -> Vec<PolicyRule> { Vec::new() }
        }
        Self(Arc::new(Noop))
    }
}

/// Read audit rows (paged, most-recent first).
pub trait AuditReader: Send + Sync + 'static {
    fn list(
        &self,
        limit:  i64,
        offset: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<AuditRowDto>, String>> + Send + '_>>;
}

/// Cloneable reference to an `AuditReader` implementation.
#[derive(Clone)]
pub struct AuditReaderRef(pub Arc<dyn AuditReader>);

impl AuditReaderRef {
    pub fn new(inner: Arc<dyn AuditReader>) -> Self { Self(inner) }
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<AuditRowDto>, String> {
        self.0.list(limit, offset).await
    }
    /// No-op reader that always returns an empty list — useful for tests.
    pub fn noop() -> Self {
        struct Noop;
        impl AuditReader for Noop {
            fn list(
                &self,
                _limit:  i64,
                _offset: i64,
            ) -> Pin<Box<dyn Future<Output = Result<Vec<AuditRowDto>, String>> + Send + '_>> {
                Box::pin(async { Ok(Vec::new()) })
            }
        }
        Self(Arc::new(Noop))
    }
}

/// Wire shape for a single audit row — matches the `audit_events` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRowDto {
    pub id:             String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id:     Option<String>,
    pub correlation_id: String,
    /// Unix milliseconds.
    pub occurred_at:    i64,
    pub event_type:     String,
    pub actor:          String,
    /// JSON-encoded event-specific payload.
    pub payload:        String,
    pub severity:       String,
}

/// List configured wallets with their current-day spend.
pub trait WalletDirectory: Send + Sync + 'static {
    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<WalletSummaryDto>> + Send + '_>>;
}

/// Cloneable reference to a `WalletDirectory` implementation.
#[derive(Clone)]
pub struct WalletDirectoryRef(pub Arc<dyn WalletDirectory>);

impl WalletDirectoryRef {
    pub fn new(inner: Arc<dyn WalletDirectory>) -> Self { Self(inner) }
    pub async fn list(&self) -> Vec<WalletSummaryDto> { self.0.list().await }
    /// No-op directory that lists no wallets — useful for tests.
    pub fn noop() -> Self {
        struct Noop;
        impl WalletDirectory for Noop {
            fn list(&self) -> Pin<Box<dyn Future<Output = Vec<WalletSummaryDto>> + Send + '_>> {
                Box::pin(async { Vec::new() })
            }
        }
        Self(Arc::new(Noop))
    }
}

/// Wire shape for a configured wallet plus its policy snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletSummaryDto {
    pub pubkey:               String,
    pub label:                String,
    pub signer_type:          SignerType,
    pub daily_spend_lamports: u64,
    /// Per-wallet policy overrides, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy:               Option<WalletPolicySummaryDto>,
}

/// Wire shape for per-wallet policy overrides (subset of `WalletPolicyConfig`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletPolicySummaryDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_amount_lamports:     Option<u64>,
    #[serde(default)]
    pub program_allowlist:       Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_approver_role:  Option<String>,
}

// ── Demo seed (development-only) ────────────────────────────────────────────
//
// Used by the showcase frontend to populate the dashboard with a realistic set
// of pending approvals, audit events, and wallet spend without having to run a
// real agent against devnet. The daemon only wires an implementation when
// `CLAW_ENABLE_DEMO_SEED=1` is set at startup; otherwise the route returns
// 503 and the trait ref stays `None`.

/// Seed a synthetic-but-realistic snapshot into the running daemon.
pub trait DemoSeeder: Send + Sync + 'static {
    fn seed(&self) -> Pin<Box<dyn Future<Output = Result<DemoSeedReport, String>> + Send + '_>>;
}

/// Summary of what was created by `POST /debug/seed-demo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoSeedReport {
    pub approvals_created: usize,
    pub audit_rows_written: usize,
    pub wallets_spend_bumped: usize,
}

/// Cloneable reference to a `DemoSeeder` implementation.
#[derive(Clone)]
pub struct DemoSeederRef(pub Arc<dyn DemoSeeder>);

impl DemoSeederRef {
    pub fn new(inner: Arc<dyn DemoSeeder>) -> Self { Self(inner) }
    pub async fn seed(&self) -> Result<DemoSeedReport, String> { self.0.seed().await }
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
    /// Bearer token for API authentication (legacy single-token mode).
    pub auth_token:          AuthToken,
    /// Token-to-operator identity mapping. Empty = legacy single-token mode.
    pub operator_registry:   crate::auth::OperatorRegistry,
    /// Metrics counters for observability.
    pub metrics:             std::sync::Arc<claw_observability::metrics::MetricsRegistry>,
    /// Direct transaction proposal (bypasses LLM agent).
    pub propose:             Option<TransactionProposerRef>,
    /// Per-token sliding-window rate limiter (None = disabled).
    pub rate_limiter:        Option<crate::rate_limit::RateLimiter>,
    /// Read-only view of the currently loaded global policy.
    pub policy:              PolicyReaderRef,
    /// Paged view of the audit_events table.
    pub audit:               AuditReaderRef,
    /// Wallet directory with today's per-wallet spend.
    pub wallets:             WalletDirectoryRef,
    /// Development-only demo seeder. `None` unless the daemon was started with
    /// `CLAW_ENABLE_DEMO_SEED=1`. Gates `POST /debug/seed-demo`.
    pub demo_seeder:         Option<DemoSeederRef>,
}

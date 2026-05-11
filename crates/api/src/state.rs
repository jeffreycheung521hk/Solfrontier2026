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

// ── Solend signature handling (Phase 4C-6) ─────────────────────────────────
//
// Separate from `WalletSignatureHandler` because the Solend deposit flow
// has its own parked-artifact shape (`SolendSigningStore`) and its own
// terminal outcome cache (`SolendSubmissionLifecycleStore`). Conflating
// the two would either force Solend-specific fields into the generic
// `WalletSignatureOutcome` or force the Solend lifecycle / signing
// stores to impersonate the generic `external_wallet::ExternalWalletStore`.
// Keeping them as parallel trait surfaces matches the broader repo
// convention of per-protocol integrations.

/// Wire shape for `GET /sessions/:id/solend-signatures/:request_id`.
///
/// Phase 4C-7 — the GET endpoint now returns the LATEST lifecycle
/// state for the signing_request_id. A frontend can poll the same URL
/// and progress through AwaitingSignature → Submitted → Confirming →
/// Finalized / Failed / ConfirmationTimeout without changing route.
///
/// Variants:
///
/// | Variant              | Chain state                  | Terminal | tx_signature |
/// |----------------------|------------------------------|----------|--------------|
/// | `Found`              | Awaiting user signature      | No       | — (bytes)    |
/// | `Submitted`          | Broadcast, no observation    | No       | yes          |
/// | `Confirming`         | Confirmed supermajority      | No       | yes          |
/// | `Finalized`          | Block rooted                 | Yes ✓    | yes          |
/// | `Failed`             | Landed with exec error       | Yes ✗    | yes          |
/// | `ConfirmationTimeout`| Past last_valid_block_height | Yes ✗    | yes          |
/// | `Rejected`           | Pre-broadcast verification   | Yes ✗    | —            |
/// | `BroadcastFailed`    | Verified, RPC send failed    | Yes ✗    | —            |
/// | `Expired`            | Signing TTL elapsed          | Yes ✗    | —            |
/// | `NotFound`           | No record / wrong session    | —        | —            |
///
/// `Found` is the only variant that includes `unsigned_tx_b64`. After
/// the user has signed + POSTed the signature back, GET responses no
/// longer include raw tx bytes — only metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SolendRetrievalResult {
    /// Parked handoff found, session matches, TTL alive. Frontend can
    /// now present `unsigned_tx_b64` to the user's wallet for signing.
    Found {
        signing_request_id: uuid::Uuid,
        intent_id: uuid::Uuid,
        session_wallet: String,
        /// Base64-encoded bincode-serialized legacy `Transaction`. If
        /// `obligation_signer_backend_partial` is `true`, this tx is
        /// already partially signed by the obligation Keypair.
        unsigned_tx_b64: String,
        obligation_signer_backend_partial: bool,
        last_valid_block_height: u64,
        /// Unix milliseconds.
        expires_at_unix_ms: i64,
        verified_slot: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        simulation_slot: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        units_consumed: Option<u64>,
    },
    /// Phase 4C-7 — user signed, backend verified and broadcast, but
    /// no on-chain observation yet. **Non-terminal.**
    Submitted {
        signing_request_id: uuid::Uuid,
        intent_id: uuid::Uuid,
        tx_signature: String,
        last_valid_block_height: u64,
    },
    /// Phase 4C-7 — `confirmation_status = "confirmed"` observed.
    /// **Non-terminal**; continue polling for `Finalized`.
    Confirming {
        signing_request_id: uuid::Uuid,
        intent_id: uuid::Uuid,
        tx_signature: String,
        slot: u64,
        last_valid_block_height: u64,
    },
    /// Phase 4C-7 — `confirmation_status = "finalized"`. **Terminal
    /// success.**
    Finalized {
        signing_request_id: uuid::Uuid,
        intent_id: uuid::Uuid,
        tx_signature: String,
        slot: u64,
    },
    /// Phase 4C-7 — landed on-chain with non-null `err`. **Terminal
    /// failure.**
    Failed {
        signing_request_id: uuid::Uuid,
        intent_id: uuid::Uuid,
        tx_signature: String,
        err: String,
    },
    /// Phase 4C-7 — current block height exceeded
    /// `last_valid_block_height` while unobserved. **Terminal
    /// failure.** The UI should surface "sign a new transaction" —
    /// `requires_reproposal` is always `true` for this variant.
    ConfirmationTimeout {
        signing_request_id: uuid::Uuid,
        tx_signature: String,
        last_valid_block_height: u64,
        current_block_height: u64,
        requires_reproposal: bool,
        reason: String,
    },
    /// Pre-confirmation terminal: verification pipeline rejected the
    /// signed transaction. A new signing handoff is required.
    Rejected {
        signing_request_id: uuid::Uuid,
        error_type: String,
        message: String,
    },
    /// Pre-confirmation terminal: verified but broadcast failed.
    BroadcastFailed {
        signing_request_id: uuid::Uuid,
        error_type: String,
        message: String,
    },
    /// Pre-confirmation terminal: signing TTL elapsed before submit.
    PreSubmitExpired { reason: String },
    /// No parked entry for this id, OR the requesting session does not
    /// own the entry. The two cases are intentionally indistinguishable
    /// to avoid leaking existence to cross-session probes.
    NotFound,
    /// Was parked but the signing TTL elapsed without a submit (4C-6
    /// original meaning — kept for back-compat when the GET arrives
    /// purely after TTL sweep and the lifecycle store has no record).
    Expired,
}

/// Wire shape for `POST /sessions/:id/solend-signatures/:request_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SolendSubmitResult {
    /// Verification passed; broadcast accepted. `tx_signature` is the
    /// base58-encoded on-chain signature string returned by the RPC.
    Submitted {
        signing_request_id: uuid::Uuid,
        intent_id: uuid::Uuid,
        session_wallet: String,
        tx_signature: String,
        verified_slot: u64,
        last_valid_block_height: u64,
    },
    /// The submit path observed an already-terminal record in the
    /// lifecycle cache (idempotent replay from UX recovery — the user
    /// lost the previous response and is asking again). Returns the
    /// ORIGINAL `tx_signature` and `recorded_at_unix_ms` so the UI can
    /// resume its confirmation-polling timeline without re-broadcasting.
    Recovered {
        signing_request_id: uuid::Uuid,
        tx_signature: String,
        recorded_at_unix_ms: i64,
    },
    /// Parked entry not present AND no lifecycle cache record. Either
    /// never existed, or TTL swept + cache TTL also expired.
    NotFound,
    /// Blockhash / TTL expired before submit.
    Expired { reason: String },
    /// Verification steps A–F failed. Typed `error_type` is wire-stable.
    Rejected { error_type: String, message: String },
    /// Verification passed; broadcast itself failed (RPC / network).
    /// Request is already consumed — a new handoff is required to retry.
    BroadcastFailed {
        signing_request_id: uuid::Uuid,
        error_type: String,
        message: String,
    },
}

/// Handler for Solend signature retrieval + submit routes (Phase 4C-6).
///
/// The gateway implementation bridges this trait to
/// `SolendSigningStore::tx_bytes_for_session` (retrieval),
/// `submit_signed_solend_transaction` (submit), and
/// `SolendSubmissionLifecycleStore` (idempotent recovery + terminal
/// caching). Routes call through the trait so `claw-api` remains
/// gateway-agnostic.
pub trait SolendSignatureHandler: Send + Sync + 'static {
    /// Retrieve the parked Solend signing handoff for this session +
    /// request id. Session-ownership mismatches return `NotFound`.
    fn retrieve(
        &self,
        session_id: &SessionId,
        signing_request_id: uuid::Uuid,
    ) -> Pin<Box<dyn Future<Output = SolendRetrievalResult> + Send + '_>>;

    /// Accept the user-signed transaction, verify, and submit.
    fn submit(
        &self,
        session_id: &SessionId,
        signing_request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> Pin<Box<dyn Future<Output = SolendSubmitResult> + Send + '_>>;
}

/// Cloneable reference to a `SolendSignatureHandler` implementation.
#[derive(Clone)]
pub struct SolendSignatureHandlerRef(pub Arc<dyn SolendSignatureHandler>);

impl SolendSignatureHandlerRef {
    pub fn new(inner: Arc<dyn SolendSignatureHandler>) -> Self {
        Self(inner)
    }

    pub async fn retrieve(
        &self,
        session_id: &SessionId,
        signing_request_id: uuid::Uuid,
    ) -> SolendRetrievalResult {
        self.0.retrieve(session_id, signing_request_id).await
    }

    pub async fn submit(
        &self,
        session_id: &SessionId,
        signing_request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> SolendSubmitResult {
        self.0.submit(session_id, signing_request_id, signed_tx_b64).await
    }
}

// ── Phase 6B Window 2 — JIT signing-handoff prepare ─────────────────────────
//
// The prepare route turns an `Approved + JIT-ready` Solend deposit
// into a fresh signing handoff. It is the new Sign-click backend
// seam: the frontend calls it from the user's "Sign with Phantom"
// click handler, the daemon fetches a fresh blockhash + assembles +
// partial-signs the obligation slot, and returns a `signing_request_id`
// that the existing GET / POST `/sessions/:id/solend-signatures/:id`
// pair then consumes.
//
// This boundary is the structural fix for the live-mainnet timing race
// observed twice on 2026-05-04 where the blockhash expired between
// approval-time tx assembly and the user reading + clicking Approve in
// Phantom. By deferring assembly to Sign-click time, the blockhash
// budget starts at the click instead of at approval time.

/// Wire-shape for the Solend JIT prepare endpoint response.
///
/// Tagged-union: `{"status": "ready", ...}` on success, `{"status":
/// "<failure_variant>", ...}` on every typed failure. Cross-session
/// probes always collapse to `not_found` so an attacker cannot
/// distinguish "this id doesn't exist" from "you don't own it".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SolendJitPrepareResult {
    /// Fresh signing handoff created. The frontend now uses
    /// `signing_request_id` with the existing GET retrieve / POST
    /// submit endpoints to drive Phantom + finalize.
    Ready {
        approval_request_id: uuid::Uuid,
        signing_request_id: uuid::Uuid,
        session_id: SessionId,
        wallet: String,
        last_valid_block_height: u64,
        verified_slot: u64,
        expires_at_unix_ms: i64,
    },
    /// The approval workflow exists and belongs to this session, but
    /// is not in `Approved` state (still pending, rejected, or
    /// lease-expired). `state` mirrors `ApprovalWorkflowState`.
    NotApproved { state: String },
    /// No JIT-ready entry exists for this approval_request_id. Either
    /// the resume task never persisted one (preflight didn't pass) or
    /// the entry's TTL elapsed AND the lazy sweep has already removed
    /// it. Distinct from `JitReadyExpired` only by timing of access.
    JitReadyMissing,
    /// The currently-bound session wallet differs from the wallet that
    /// was bound when the resume task captured the JIT-ready entry.
    /// The frontend must rebind the original wallet (or the operator
    /// must re-propose) before the handoff can be created — using the
    /// new wallet would break message-hash + obligation-signature
    /// invariants downstream.
    WalletMismatch {
        expected: String,
        bound: Option<String>,
    },
    /// `create_signing_handoff` returned a typed error. Mirrors the
    /// `SigningHandoffError` variants without leaking raw Debug of
    /// private-key-adjacent types.
    HandoffCreateFailed { error_type: String, message: String },
    /// Indistinguishable variant for: approval workflow not found OR
    /// workflow's session_id does not match the path session. Cross-
    /// session probes cannot distinguish these.
    NotFound,
}

/// Backend seam for the prepare route. Concrete implementation lives
/// in `claw-gateway` (`runtime::solend_jit_prepare_wiring`); the route
/// handler calls through this trait so `claw-api` stays gateway-
/// agnostic.
pub trait SolendJitPrepareHandler: Send + Sync + 'static {
    fn prepare(
        &self,
        session_id: &SessionId,
        approval_request_id: uuid::Uuid,
    ) -> Pin<Box<dyn Future<Output = SolendJitPrepareResult> + Send + '_>>;
}

/// Cloneable reference to a `SolendJitPrepareHandler`.
#[derive(Clone)]
pub struct SolendJitPrepareHandlerRef(pub Arc<dyn SolendJitPrepareHandler>);

impl SolendJitPrepareHandlerRef {
    pub fn new(inner: Arc<dyn SolendJitPrepareHandler>) -> Self {
        Self(inner)
    }

    pub async fn prepare(
        &self,
        session_id: &SessionId,
        approval_request_id: uuid::Uuid,
    ) -> SolendJitPrepareResult {
        self.0.prepare(session_id, approval_request_id).await
    }
}

// ── Phase 6I-F — Solend WITHDRAW JIT-prepare ───────────────────────────────
//
// Mirrors the deposit JIT-prepare types above but scoped to withdraw-all.
// The withdraw flow has narrower preconditions (no obligation Keypair,
// no preflight outcome, no transient pubkey) so the result enum has
// dedicated typed-rejection arms for the withdraw re-check.

/// Wire shape returned by the withdraw JIT-prepare route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SolendWithdrawJitPrepareResult {
    /// Fresh withdraw signing handoff created. The frontend now uses
    /// `signing_request_id` with the existing GET retrieve / POST
    /// submit endpoints (the same endpoints deposit uses) to drive
    /// Phantom + finalize.
    Ready {
        approval_request_id: uuid::Uuid,
        signing_request_id: uuid::Uuid,
        session_id: SessionId,
        wallet: String,
        obligation_pubkey: String,
        reserve_pubkey: String,
        last_valid_block_height: u64,
        verified_slot: u64,
        expires_at_unix_ms: i64,
    },
    /// Workflow exists for this session but is not in `Approved` state.
    NotApproved { state: String },
    /// No parked withdraw intent found under this `approval_request_id`
    /// (resume task never persisted, lazy-swept, or this was a deposit
    /// approval).
    WithdrawIntentMissing,
    /// Currently-bound session wallet differs from the wallet captured
    /// in the parked withdraw intent.
    WalletMismatch {
        expected: String,
        bound: Option<String>,
    },
    /// Fresh re-check at prepare time blocked the withdraw. `reason` is
    /// one of the stable constants in
    /// `crate::integrations::solend_withdraw_park` (e.g.
    /// `owner_mismatch`, `borrow_appeared`, `no_usdc_deposit`,
    /// `obligation_not_found`).
    RecheckBlocked {
        reason: String,
        detail: Option<String>,
    },
    /// Plan assembly rejected the inputs (typically a snapshot drift
    /// between resume re-check and prepare).
    PlanAssemblyFailed {
        error_type: String,
        message: String,
    },
    /// `create_withdraw_signing_handoff` returned a typed error.
    HandoffCreateFailed { error_type: String, message: String },
    /// Fresh snapshot assembly failed (RPC error, etc.).
    SnapshotAssembleFailed {
        error_type: String,
        message: String,
    },
    /// Indistinguishable variant for: workflow not found OR workflow
    /// session_id does not match the path session.
    NotFound,
}

/// Backend seam for the withdraw prepare route. Concrete implementation
/// lives in `claw-gateway`; the route handler calls through this trait
/// so `claw-api` stays gateway-agnostic.
pub trait SolendWithdrawJitPrepareHandler: Send + Sync + 'static {
    fn prepare(
        &self,
        session_id: &SessionId,
        approval_request_id: uuid::Uuid,
    ) -> Pin<Box<dyn Future<Output = SolendWithdrawJitPrepareResult> + Send + '_>>;
}

/// Cloneable reference to a `SolendWithdrawJitPrepareHandler`.
#[derive(Clone)]
pub struct SolendWithdrawJitPrepareHandlerRef(pub Arc<dyn SolendWithdrawJitPrepareHandler>);

impl SolendWithdrawJitPrepareHandlerRef {
    pub fn new(inner: Arc<dyn SolendWithdrawJitPrepareHandler>) -> Self {
        Self(inner)
    }

    pub async fn prepare(
        &self,
        session_id: &SessionId,
        approval_request_id: uuid::Uuid,
    ) -> SolendWithdrawJitPrepareResult {
        self.0.prepare(session_id, approval_request_id).await
    }
}

// ── Phase 5D.2 — User-facing chat route ─────────────────────────────────────
//
// The chat route invokes a strict one-turn conversational handler. The HTTP
// layer is a "dumb pipe": it parses the path/body, calls `handle_chat`, and
// maps the typed `ChatRouteOutcome` to an HTTP status + JSON DTO. All
// LLM provider lookup, capability narrowing, tool dispatch, and sanitation
// happens behind the trait inside `claw-gateway`.
//
// This crate must not depend on `claw-agent-runtime`; the trait is defined
// here so that route handlers can drive the chat path without importing the
// runtime crate's `ConversationOutcome` directly.

/// Wire shape of the chat route response body (Phase 5D.2).
///
/// The DTO is intentionally minimal. It is derived from
/// `agent-runtime::ConversationOutcome` by the gateway-side adapter and
/// stripped of any data the handler does not want exposed to clients —
/// notably, no raw provider text, no `tx_bytes` / `transaction_base64`,
/// no signing handoff payload. Only the typed status + the minimized
/// `ToolOutput` JSON for `tool_dispatched`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChatResponse {
    /// The provider returned no tool calls. `assistant_text` is the
    /// sanitized text the model emitted (if any). `None` is rendered
    /// to clients as `null`.
    AssistantText { assistant_text: Option<String> },
    /// The provider asked for exactly one tool call and the dispatcher
    /// produced a `ToolOutput`. `output` is the Phase 5A minimized
    /// `ToolOutput` JSON (no raw bytes, no key material).
    ToolDispatched {
        tool_name: String,
        output: serde_json::Value,
    },
    /// 2+ tool calls — entire turn rejected. No tool ran.
    MultipleToolCallsRejected { count: usize },
    /// Tool name not in the narrowed registry, or session lacks the
    /// capability. Two cases collapsed into one wire variant so cross-
    /// session probes cannot distinguish them.
    UnknownOrDeniedTool { tool_name: String, reason: String },
    /// Tool input did not deserialize into the tool's strict-schema
    /// struct. Dispatch was not attempted.
    MalformedToolArguments { tool_name: String, reason: String },
    /// Provider response shape was outside contract. Dispatch was not
    /// attempted.
    MalformedProviderOutput { reason: String },
    /// Tool itself rejected the input (validation, permission, ...).
    /// Dispatch happened but the tool refused to act.
    ToolError { tool_name: String, message: String },
    /// Phase 5A guard: a previous propose-stage tool call for this
    /// session is already pending operator approval. The chat handler
    /// refuses to dispatch a second one until the first resolves.
    PendingActionExists { reason: String },
    /// W5d frontend chat wiring — the chat handler recognised the
    /// deterministic demo grammar
    ///
    /// > "If Solend Main Pool USDC deposit APR is above X%, deposit
    /// >  0.25 USDC from my bounded executor wallet into Solend."
    ///
    /// and routed it to the W5d bridge instead of the LLM. The bridge
    /// fetched the current Solend Main Pool USDC reserve via Helius,
    /// computed the supply APR through the B-O1 evaluator wrappers,
    /// applied the strict `current_apr_bps > threshold_bps` decision,
    /// and produced this typed result.
    ///
    /// The `status` field is always one of:
    ///   - `"condition_not_met"`  when the strict-greater check failed,
    ///   - `"ready_to_execute"`   when the check passed but the live-send
    ///     gates are not set (the chat route never broadcasts in this
    ///     slice — execution is reserved for the env-gated W5c harness).
    ///
    /// Parser / evaluator failures (wrong pool, malformed percent, RPC
    /// down) are surfaced via the existing
    /// [`ChatResponse::ToolError`] variant with
    /// `tool_name == "w5d_conditional_deposit"` so the frontend reuses
    /// its current error-card path.
    W5dConditionalDeposit { result: W5dConditionalDepositResultDto },
}

/// Wire DTO mirroring `claw_gateway::stage2_demo_apr_bridge::W5dEvaluationResult`.
///
/// The gateway-side adapter constructs this from the rich
/// `W5dEvaluationResult` at the chat-route boundary so the api crate
/// does not need a build-time dependency on the gateway crate.
///
/// W5e — extended with budget visibility, liveness anchor
/// (`last_checked_slot`), and persisted-rule identity. The status
/// field is now `watching` | `ready_to_execute` | `needs_funding`,
/// replacing W5d's `condition_not_met` (which incorrectly treated
/// a conditional order as a terminal quote check).
///
/// W5f — adds the Save/Solend UI display APY as the **primary
/// decision metric** alongside the native on-chain APR as an audit
/// field. `current_apr_bps` is kept as a wire-compat alias that
/// equals `save_display_apy_bps` so older frontends keep working.
///
/// **No-overclaim:** this DTO proves a chat-side deterministic
/// detection + Save REST API APY fetch + on-chain APR evaluation +
/// controlled-wallet budget read + (when wired) real
/// `Stage2WatchRuleRepository` persistence, with a typed lifecycle
/// status the frontend renders. It does NOT prove a
/// `clawsol-authority` `ExecuteAction`, an `AuthorizationRecord`
/// PDA live execution, a Jupiter conditional execution path, a
/// first-class production `SolendDepositControlledWallet`
/// `ActionSpec`, or a running watcher tick loop. `status="watching"`
/// describes the rule's durable state in the state-store, not an
/// active polling loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct W5dConditionalDepositResultDto {
    pub input_text: String,
    /// Legacy label kept for wire compat. After W5f, equal to
    /// `"save_display_apy"`. Prefer `decision_source` in new UI.
    pub source: String,
    pub reserve_pubkey: String,
    /// W5f: alias = `save_display_apy_bps`. Kept for wire compat
    /// with older frontends that read this field.
    pub current_apr_bps: u32,
    pub threshold_bps: u32,
    pub threshold_pct_label: String,
    /// W5f: decided by `save_display_apy_bps > threshold_bps`.
    pub condition_met: bool,
    /// Always `false` from the chat route in the present slice.
    pub execution_attempted: bool,
    /// W5e: `"watching"` | `"ready_to_execute"` | `"needs_funding"`.
    pub status: String,
    /// Reserved for future slices; always `None` here.
    pub tx_signature: Option<String>,
    /// W5e fields.
    pub controlled_wallet: String,
    pub source_usdc_ata: String,
    pub required_budget_raw: u64,
    pub current_budget_raw: u64,
    /// `"reserved"` | `"needs_funding"`.
    pub budget_status: String,
    pub last_checked_slot: u64,
    pub expires_at_slot: Option<u64>,
    pub rule_id_hex: Option<String>,
    pub canonical_rule_hash_hex: Option<String>,
    pub rule_persisted: bool,

    // ── W5f decision metric + audit ──────────────────────────────────────

    /// W5f: which metric drove `condition_met`. Always
    /// `"save_display_apy"` on the happy path.
    pub decision_source: String,
    /// W5f: Save/Solend UI display APY in basis points, fetched from
    /// the official Solend REST API at
    /// `https://api.solend.fi/v1/reserves?scope=solend&ids=<reserve>`
    /// (field `results[0].rates.supplyInterest`, percentage string
    /// converted to bps).
    pub save_display_apy_bps: u32,
    /// W5f: native on-chain supply APR in basis points, decoded via
    /// B-O1 reserve math. Audit-only — does NOT drive the chat-time
    /// decision.
    pub native_onchain_apr_bps: u32,
    /// W5f: provenance for the native APR. Always
    /// `"b_o1_reserve_math"` in this slice.
    pub native_onchain_apr_source: String,
}

/// Domain-level outcome returned by `ChatHandler::handle_chat`.
///
/// The HTTP route maps these to status codes:
/// - `Ok(ChatResponse)` → 200 (the inner variant is reflected in the JSON `status`)
/// - `PendingActionExists` → 409 Conflict (carries a `ChatResponse::PendingActionExists`)
/// - `BadRequest(reason)` → 400 (e.g., empty message after trim)
/// - `Disabled` → 503 (no `ChatHandler` was wired, or provider is `Disabled`)
#[derive(Debug, Clone)]
pub enum ChatRouteOutcome {
    Ok(ChatResponse),
    Conflict(ChatResponse),
    BadRequest(String),
    Disabled(String),
}

/// Handler for the user-facing chat route (Phase 5D.2).
///
/// This trait is the only seam the HTTP layer uses to drive a one-turn
/// conversational LLM call. The gateway's adapter (`GatewayChatHandler`
/// in `crates/gateway/src/runtime/chat_wiring.rs`) wraps a
/// `ConversationHandler` and produces the wire DTO.
pub trait ChatHandler: Send + Sync + 'static {
    fn handle_chat(
        &self,
        session_id: &SessionId,
        message: String,
    ) -> Pin<Box<dyn Future<Output = ChatRouteOutcome> + Send + '_>>;
}

/// Cloneable reference to a `ChatHandler` implementation.
#[derive(Clone)]
pub struct ChatHandlerRef(pub Arc<dyn ChatHandler>);

impl ChatHandlerRef {
    pub fn new(inner: Arc<dyn ChatHandler>) -> Self {
        Self(inner)
    }

    pub async fn handle_chat(
        &self,
        session_id: &SessionId,
        message: String,
    ) -> ChatRouteOutcome {
        self.0.handle_chat(session_id, message).await
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
    /// Solend-specific signing retrieval + submit handler. `None` when
    /// Solend integration isn't wired (e.g., minimal test harnesses).
    pub solend_signatures:   Option<SolendSignatureHandlerRef>,
    /// Phase 6B Window 2: backend seam for the JIT-prepare route that
    /// turns an `Approved + JIT-ready` Solend deposit into a fresh
    /// signing handoff at user-click time. `None` when Solend
    /// integration isn't wired.
    pub solend_jit_prepare:  Option<SolendJitPrepareHandlerRef>,
    /// Phase 6I-F: backend seam for the WITHDRAW JIT-prepare route. Same
    /// click-time-blockhash design as deposit, but scoped to the
    /// withdraw-all flow: looks up a parked
    /// `ParkedSolendWithdrawAllIntent`, re-fetches the obligation,
    /// re-runs the four structural invariants, assembles a
    /// `SolendWithdrawTxPlan`, and parks an unsigned tx in the shared
    /// `SolendSigningStore`. `None` when Solend integration isn't wired.
    pub solend_withdraw_jit_prepare: Option<SolendWithdrawJitPrepareHandlerRef>,
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
    /// Phase 5D.2 — strict one-turn chat handler. `None` unless an LLM
    /// provider is wired (`Disabled`/`Scripted`/`OpenAi`/`Anthropic` is
    /// chosen by the daemon's provider config). When `None`, the
    /// `POST /sessions/:id/chat` route returns 503.
    pub chat:                Option<ChatHandlerRef>,
}

//! Store for external wallet signature requests.
//!
//! When a transaction targets an external signer (e.g., Phantom Wallet),
//! the pipeline cannot sign locally. Instead, the finalized transaction
//! is parked here and the client is expected to:
//!
//! 1. Receive the unsigned transaction bytes (base64)
//! 2. Sign them externally (e.g., via Phantom's `signTransaction`)
//! 3. POST the signed transaction back to the gateway for verification
//!
//! # Verification
//!
//! On receipt, the gateway verifies:
//! - The signed transaction's message bytes are identical to the parked
//!   unsigned transaction's message bytes (no instruction tampering)
//! - The expected wallet pubkey is among the signers
//! - All required signatures are present and valid
//!
//! # Session-wallet binding
//!
//! Each session can bind one or more external wallets via
//! `bind_wallet(session_id, pubkey)`. The signing tool checks this
//! binding to decide whether to use the local or external path.
//!
//! # Durability
//!
//! Session-wallet bindings are persisted to SQLite (C4) and recovered
//! on daemon restart. Pending signature requests remain in-memory only.

use std::fmt;
use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use tokio::sync::oneshot;
use tracing::warn;
use uuid::Uuid;

use claw_state_store::WalletBindingRepository;
use claw_types::{
    policy::PolicyVerdict,
    session::SessionId,
    transaction::{SimulationResult, TransactionProposal},
};

// ── Verification types ────────────────────────────────────────────────────────

/// Error type for signed transaction verification failures.
#[derive(Debug)]
pub enum VerifyError {
    /// The signed transaction's message bytes differ from the original.
    MessageMismatch,
    /// The expected signer is not in the transaction's account keys.
    SignerNotInAccountKeys,
    /// The expected signer's signature slot is empty (default signature).
    SignerDidNotSign,
    /// The signature at the signer's position fails ed25519 verification
    /// against the transaction message.
    InvalidSignature,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MessageMismatch => write!(f, "transaction message bytes modified"),
            Self::SignerNotInAccountKeys => write!(f, "expected signer not in account keys"),
            Self::SignerDidNotSign => write!(f, "expected signer did not sign"),
            Self::InvalidSignature => write!(f, "signature verification failed"),
        }
    }
}

/// Error type for the submit_signed_transaction flow.
#[derive(Debug)]
pub enum SubmitError {
    /// The signed transaction bytes could not be deserialized.
    DeserializeFailed(String),
    /// No pending request with this ID.
    NotFound,
    /// The parked unsigned transaction could not be deserialized (internal error).
    InternalDeserializeFailed(String),
    /// Verification of the signed transaction failed.
    VerificationFailed(VerifyError),
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeserializeFailed(e) => write!(f, "invalid signed transaction bytes: {e}"),
            Self::NotFound => write!(f, "no pending wallet signature request with this ID"),
            Self::InternalDeserializeFailed(e) => write!(f, "internal error deserializing parked tx: {e}"),
            Self::VerificationFailed(e) => write!(f, "verification failed: {e}"),
        }
    }
}

// ── Pure verification helper ──────────────────────────────────────────────────

/// Verifies that a signed transaction is a valid signature over the exact
/// original (unsigned) transaction message, by the expected signer.
///
/// Checks (in order):
/// 1. Message bytes are identical (no instruction / blockhash / account tampering)
/// 2. Expected signer is present in account_keys
/// 3. The signature slot at the signer's index is non-default
/// 4. The signature is cryptographically valid (ed25519) against the message
pub fn verify_signed_tx(
    original_tx: &Transaction,
    signed_tx: &Transaction,
    expected_signer: &Pubkey,
) -> Result<(), VerifyError> {
    // 1. Message bytes must be identical.
    if original_tx.message_data() != signed_tx.message_data() {
        return Err(VerifyError::MessageMismatch);
    }

    // 2. Expected signer must be in account_keys.
    let signer_index = signed_tx.message.account_keys
        .iter()
        .position(|k| k == expected_signer)
        .ok_or(VerifyError::SignerNotInAccountKeys)?;

    // 3. Signature slot must be non-default (i.e. the signer actually signed).
    if signer_index >= signed_tx.signatures.len()
        || signed_tx.signatures[signer_index] == Signature::default()
    {
        return Err(VerifyError::SignerDidNotSign);
    }

    // 4. Cryptographic verification: ed25519 signature over the message bytes.
    let sig = &signed_tx.signatures[signer_index];
    let msg_bytes = signed_tx.message_data();
    if !sig.verify(expected_signer.as_ref(), &msg_bytes) {
        return Err(VerifyError::InvalidSignature);
    }

    Ok(())
}

// ── Store-level submit handler ────────────────────────────────────────────────

/// Verifies and submits a signed transaction to the external wallet store.
///
/// This is the pure store-level handler — no audit, no events, no spend tracking.
/// The gateway daemon wraps this with audit + events.
///
/// On success: calls `store.complete()` with the verified signed transaction.
/// On failure: calls `store.reject()` + `store.remove()` and returns the error.
pub fn submit_signed_transaction(
    store: &ExternalWalletStore,
    request_id: Uuid,
    signed_tx_bytes: Vec<u8>,
) -> Result<(), SubmitError> {
    // 1. Deserialize the signed transaction.
    let signed_tx: Transaction = bincode::deserialize(&signed_tx_bytes)
        .map_err(|e| SubmitError::DeserializeFailed(e.to_string()))?;

    // 2. Retrieve the parked unsigned transaction bytes.
    let original_bytes = store.get(&request_id)
        .ok_or(SubmitError::NotFound)?;

    let original_tx: Transaction = bincode::deserialize(&original_bytes)
        .map_err(|e| SubmitError::InternalDeserializeFailed(e.to_string()))?;

    // 3. Retrieve expected signer.
    let expected_signer_str = store.expected_signer(&request_id)
        .ok_or(SubmitError::NotFound)?;

    let expected_signer = expected_signer_str
        .parse::<Pubkey>()
        .map_err(|e| SubmitError::InternalDeserializeFailed(format!("invalid signer pubkey: {e}")))?;

    // 4. Verify.
    verify_signed_tx(&original_tx, &signed_tx, &expected_signer)
        .map_err(|e| {
            store.reject(&request_id);
            store.remove(&request_id);
            SubmitError::VerificationFailed(e)
        })?;

    // 5. All checks pass — complete and remove the entry.
    store.complete(&request_id, signed_tx);
    store.take(&request_id);
    Ok(())
}

/// A transaction parked awaiting an external wallet signature.
pub struct PendingWalletSignature {
    /// The original proposal metadata.
    pub proposal: TransactionProposal,

    /// The finalized unsigned transaction (serialized via bincode).
    /// This is the exact message the external wallet must sign.
    pub tx_bytes: Vec<u8>,

    /// The simulation result from the pipeline.
    pub simulation: SimulationResult,

    /// The policy verdict that was applied.
    pub policy_verdict: PolicyVerdict,

    /// The pubkey of the external wallet expected to sign.
    pub expected_signer: String,

    /// Oneshot sender to unblock the waiting task once the signed tx arrives.
    /// Sends `Some(Transaction)` on success, `None` on rejection/timeout.
    pub response_tx: Option<oneshot::Sender<Option<Transaction>>>,
}

/// Store for external wallet signature requests and session-wallet bindings.
///
/// Bindings are kept in-memory (`DashMap`) for fast reads and optionally
/// persisted to SQLite (`WalletBindingRepository`) for cross-restart
/// continuity (C4). When the repo is set, `bind_wallet()` and
/// `unbind_session()` write through to the database.
#[derive(Clone)]
pub struct ExternalWalletStore {
    /// Pending signature requests keyed by request_id.
    pending: Arc<DashMap<Uuid, PendingWalletSignature>>,

    /// Session → set of bound external wallet pubkeys (runtime-bound via bind_wallet).
    bindings: Arc<DashMap<SessionId, DashSet<String>>>,

    /// Pubkeys declared as external in config (A3).
    /// These are recognized across ALL sessions without needing bind_wallet.
    config_bound_pubkeys: Arc<DashSet<String>>,

    /// Optional SQLite persistence for session-wallet bindings (C4).
    binding_repo: Option<WalletBindingRepository>,
}

impl ExternalWalletStore {
    /// Creates an empty store (no persistence).
    pub fn new() -> Self {
        Self {
            pending:              Arc::new(DashMap::new()),
            bindings:             Arc::new(DashMap::new()),
            config_bound_pubkeys: Arc::new(DashSet::new()),
            binding_repo:         None,
        }
    }

    /// Sets the SQLite repository for durable binding persistence (C4).
    pub fn with_binding_repo(mut self, repo: WalletBindingRepository) -> Self {
        self.binding_repo = Some(repo);
        self
    }

    /// Loads previously persisted bindings into the in-memory store.
    /// Called once at daemon startup after the repo is set.
    pub async fn load_persisted_bindings(&self) -> Result<usize, String> {
        let repo = match &self.binding_repo {
            Some(r) => r,
            None => return Ok(0),
        };
        let rows = repo.load_all().await.map_err(|e| e.to_string())?;
        let count = rows.len();
        for row in rows {
            let sid = SessionId::from(
                row.session_id.parse::<uuid::Uuid>().map_err(|e| e.to_string())?,
            );
            self.bindings
                .entry(sid)
                .or_insert_with(DashSet::new)
                .insert(row.wallet_pubkey);
        }
        Ok(count)
    }

    /// Registers a pubkey as a config-declared external wallet (A3).
    /// Config-bound wallets are recognized across all sessions.
    pub fn register_config_external(&self, pubkey: &str) {
        self.config_bound_pubkeys.insert(pubkey.to_string());
    }

    /// Returns true if the pubkey is a config-declared external wallet.
    pub fn is_config_external(&self, pubkey: &str) -> bool {
        self.config_bound_pubkeys.contains(pubkey)
    }

    // ── Session-wallet binding ────────────────────────────────────────────────

    /// Binds an external wallet pubkey to a session.
    ///
    /// Updates the in-memory store immediately and persists to SQLite
    /// (best-effort — log on failure but don't block the caller).
    pub fn bind_wallet(&self, session_id: &SessionId, pubkey: &str) {
        self.bindings
            .entry(session_id.clone())
            .or_insert_with(DashSet::new)
            .insert(pubkey.to_string());

        // Write-through to SQLite (C4).
        if let Some(repo) = &self.binding_repo {
            let repo = repo.clone();
            let sid = session_id.to_string();
            let pk = pubkey.to_string();
            tokio::spawn(async move {
                if let Err(e) = repo.bind(&sid, &pk).await {
                    warn!(
                        session = %sid,
                        pubkey = %pk,
                        error = %e,
                        "C4: failed to persist wallet binding to SQLite"
                    );
                }
            });
        }
    }

    /// Returns `true` if the given pubkey is bound as an external wallet
    /// for the given session (runtime-bound) or declared as external in config (A3).
    pub fn is_external_wallet(&self, session_id: &SessionId, pubkey: &str) -> bool {
        // Config-declared external wallets are recognized across all sessions.
        if self.config_bound_pubkeys.contains(pubkey) {
            return true;
        }
        // Runtime-bound: session-specific binding via bind_wallet.
        self.bindings
            .get(session_id)
            .map(|set| set.contains(pubkey))
            .unwrap_or(false)
    }

    /// Returns all external wallet pubkeys bound to a session.
    pub fn wallets_for_session(&self, session_id: &SessionId) -> Vec<String> {
        self.bindings
            .get(session_id)
            .map(|set| set.iter().map(|s| s.clone()).collect())
            .unwrap_or_default()
    }

    /// Unbinds all external wallets for a session (on session close).
    pub fn unbind_session(&self, session_id: &SessionId) {
        self.bindings.remove(session_id);

        // Write-through to SQLite (C4).
        if let Some(repo) = &self.binding_repo {
            let repo = repo.clone();
            let sid = session_id.to_string();
            tokio::spawn(async move {
                if let Err(e) = repo.unbind_session(&sid).await {
                    warn!(
                        session = %sid,
                        error = %e,
                        "C4: failed to remove wallet bindings from SQLite"
                    );
                }
            });
        }
    }

    // ── Pending signature management ──────────────────────────────────────────

    /// Parks a transaction awaiting an external wallet signature.
    ///
    /// Returns a oneshot receiver the caller should await. The receiver
    /// yields `Some(signed_tx)` when the signed transaction is received,
    /// or `None` if the request is rejected/timed out.
    pub fn park(
        &self,
        request_id: Uuid,
        proposal: TransactionProposal,
        tx: &Transaction,
        simulation: SimulationResult,
        policy_verdict: PolicyVerdict,
        expected_signer: String,
    ) -> Result<oneshot::Receiver<Option<Transaction>>, String> {
        let tx_bytes = bincode::serialize(tx)
            .map_err(|e| format!("failed to serialize transaction: {e}"))?;

        let (response_tx, response_rx) = oneshot::channel();

        self.pending.insert(
            request_id,
            PendingWalletSignature {
                proposal,
                tx_bytes,
                simulation,
                policy_verdict,
                expected_signer,
                response_tx: Some(response_tx),
            },
        );

        Ok(response_rx)
    }

    /// Retrieves the pending entry (immutable) for verification purposes.
    pub fn get(&self, request_id: &Uuid) -> Option<Vec<u8>> {
        self.pending.get(request_id).map(|entry| entry.tx_bytes.clone())
    }

    /// Retrieves the expected signer for a pending request.
    pub fn expected_signer(&self, request_id: &Uuid) -> Option<String> {
        self.pending.get(request_id).map(|entry| entry.expected_signer.clone())
    }

    /// Returns metadata needed for rejection events without consuming the entry.
    /// Returns `(transaction_id, wallet_pubkey)`.
    pub fn rejection_metadata(&self, request_id: &Uuid) -> Option<(Uuid, String)> {
        self.pending.get(request_id).map(|entry| {
            (entry.proposal.id, entry.expected_signer.clone())
        })
    }

    /// Sends a verified signed transaction to the waiting task.
    ///
    /// Returns `true` if the signal was delivered.
    pub fn complete(&self, request_id: &Uuid, signed_tx: Transaction) -> bool {
        if let Some(mut entry) = self.pending.get_mut(request_id) {
            if let Some(tx) = entry.response_tx.take() {
                let _ = tx.send(Some(signed_tx));
                return true;
            }
        }
        false
    }

    /// Rejects a pending wallet signature request (verification failed).
    pub fn reject(&self, request_id: &Uuid) -> bool {
        if let Some(mut entry) = self.pending.get_mut(request_id) {
            if let Some(tx) = entry.response_tx.take() {
                let _ = tx.send(None);
                return true;
            }
        }
        false
    }

    /// Takes and removes a pending entry (for cleanup after completion).
    pub fn take(&self, request_id: &Uuid) -> Option<PendingWalletSignature> {
        self.pending.remove(request_id).map(|(_, v)| v)
    }

    /// Removes a pending entry without sending a response.
    pub fn remove(&self, request_id: &Uuid) {
        self.pending.remove(request_id);
    }

    /// Returns pending wallet signature requests for a given session.
    ///
    /// O(n) iteration over the pending set — acceptable because the set is small.
    /// Returns `(request_id, transaction_id, description, expected_signer, tx_bytes)`.
    pub fn pending_for_session(&self, session_id: &SessionId) -> Vec<(Uuid, Uuid, String, String, Vec<u8>)> {
        let mut results = Vec::new();
        for entry in self.pending.iter() {
            if &entry.value().proposal.session_id == session_id {
                results.push((
                    *entry.key(),
                    entry.value().proposal.id,
                    entry.value().proposal.description.clone(),
                    entry.value().expected_signer.clone(),
                    entry.value().tx_bytes.clone(),
                ));
            }
        }
        results
    }

    /// Returns the number of pending wallet signature requests.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Default for ExternalWalletStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── ExternalWalletBindings impl ─────────────────────────────────────────────
//
// Bridges the existing ExternalWalletStore into the Execution Plane's
// read-only binding trait. The adapter reads bindings via this trait;
// mutation (bind/unbind) remains gateway-side on ExternalWalletStore directly.

impl crate::orchestrator::adapter::ExternalWalletBindings for ExternalWalletStore {
    fn is_bound(&self, session_id: &SessionId, pubkey: &str) -> bool {
        self.is_external_wallet(session_id, pubkey)
    }
}

//! Wallet adapter trait for the Execution Plane.
//!
//! A `WalletAdapter` is a signing backend that handles signature requests
//! for specific wallet types. Adapters are stateless routing targets:
//! they receive finalized transaction bytes and produce signatures.
//!
//! # Execution Plane only
//!
//! Adapters must NOT:
//! - Perform policy evaluation or approval decisions
//! - Emit audit events or record spend
//! - Publish to EventBus
//! - Modify the transaction's message bytes
//! - Interpret the business meaning of the transaction

use async_trait::async_trait;

use claw_types::session::SessionId;

use super::types::{
    SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId,
};

// ── WalletAdapter trait ─────────────────────────────────────────────────────

/// Execution Plane only.
/// A signing backend that can handle signature requests for specific wallet types.
///
/// Adapters are stateless routing targets. They receive finalized transaction bytes
/// and produce signatures (synchronously or asynchronously).
///
/// # Allowed
///
/// - Sign the exact `finalized_tx_bytes` provided (for local signing)
/// - Verify that returned signatures match the exact original message bytes (for external signing)
/// - Return deterministic results for the same input
///
/// # Must NOT
///
/// - Perform policy evaluation or approval decisions
/// - Emit audit events or record spend
/// - Publish to EventBus
/// - Modify the transaction's message bytes (instructions, accounts, blockhash)
/// - Interpret the business meaning of the transaction
/// - Own pending-state tracking (the orchestrator is the sole pending-state owner)
#[async_trait]
pub trait WalletAdapter: Send + Sync {
    /// Returns the identifier for this adapter type.
    ///
    /// Must be a static, unique string (e.g., `"local"`, `"external"`, `"ledger"`).
    /// Used by the orchestrator to route `complete()` calls to the correct adapter.
    fn adapter_type(&self) -> &str;

    /// Returns `true` if this adapter can handle the given request.
    ///
    /// Receives the full `SignatureRequest` so the adapter can make routing decisions
    /// based on any combination of fields (pubkey, session_id, tx content, etc.).
    ///
    /// Must NOT have side effects. Must NOT block. Must be fast (no I/O).
    ///
    /// Invariant: if `supports()` returns `true`, `sign()` must not return `NoAdapter`.
    fn supports(&self, request: &SignatureRequest) -> bool;

    /// Initiate signing on a finalized transaction.
    ///
    /// For synchronous adapters (local keystore):
    ///   - Deserializes `finalized_tx_bytes`
    ///   - Calls the underlying signer
    ///   - Returns `SignatureOutcome::Signed`
    ///   - Must never return `SignatureOutcome::Pending`
    ///
    /// For asynchronous adapters (external wallet, ledger):
    ///   - Returns `SignatureOutcome::Pending` with the raw finalized tx bytes
    ///   - The orchestrator will later call `complete()` when the signed payload arrives
    ///
    /// Must NOT modify the transaction message bytes.
    /// Must NOT perform policy checks or approval decisions.
    /// Must NOT own pending-state — the orchestrator tracks pending requests.
    async fn sign(
        &self,
        request: &SignatureRequest,
    ) -> Result<SignatureOutcome, SignatureError>;

    /// Complete a pending asynchronous signing request.
    ///
    /// Called when the external party (user, hardware device) returns signed tx bytes.
    /// The orchestrator passes the consumed `SignatureRequest` (from its pending map)
    /// so the adapter has access to the original `finalized_tx_bytes` and `expected_signer`
    /// for verification. The adapter does NOT need its own pending storage.
    ///
    /// The adapter must:
    ///   1. Deserialize `signed_tx_bytes` into a Transaction
    ///   2. Deserialize `request.finalized_tx_bytes` into the original Transaction
    ///   3. Verify the signed payload matches the original (exact message bytes)
    ///   4. Verify the expected signer actually signed (ed25519 verification)
    ///   5. Return `SignatureOutcome::Signed` on success
    ///
    /// On verification failure: return `Err(SignatureError::VerificationFailed)`.
    ///
    /// Synchronous adapters (local) must return `Err(SignatureError::SigningFailed)`
    /// with a message indicating that `complete()` is not applicable.
    ///
    /// Must NOT emit audit events. Must NOT record spend.
    /// Must reuse existing pure verification logic (`verify_signed_tx`).
    async fn complete(
        &self,
        request: &SignatureRequest,
        signed_tx_bytes: &[u8],
    ) -> Result<SignatureOutcome, SignatureError>;

    /// Clean up internal adapter state for an expired or cancelled request.
    ///
    /// Called by the orchestrator during `expire()`.
    /// Must remove any internally-held data for this request.
    /// No-op if the adapter has no state for this request.
    ///
    /// Since the orchestrator is the sole pending-state owner and passes
    /// the `SignatureRequest` to `complete()`, most adapters will have no
    /// internal state to clean up. This method exists for adapters that
    /// maintain auxiliary caches.
    ///
    /// Must NOT have external side effects (no audit, no events).
    fn cleanup(&self, _request_id: &SignatureRequestId) {
        // Default: no-op. Most adapters have no internal state to clean.
    }
}

// ── ExternalWalletBindings ──────────────────────────────────────────────────

/// Execution Plane only.
/// Read-only view of session-to-external-wallet bindings.
///
/// The `ExternalWalletAdapter` uses this to answer `supports()` queries.
/// Mutation (bind/unbind) is owned by the gateway layer and is not
/// part of the wallet adapter contract.
///
/// # Must NOT
///
/// - Expose mutation methods (bind, unbind)
/// - Perform any side effects
pub trait ExternalWalletBindings: Send + Sync {
    /// Returns `true` if the pubkey is bound as an external wallet for this session.
    fn is_bound(&self, session_id: &SessionId, pubkey: &str) -> bool;
}

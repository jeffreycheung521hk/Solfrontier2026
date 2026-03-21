//! External wallet adapter — handles out-of-band signing (Phantom, etc.).
//!
//! # Execution Plane only
//!
//! This adapter handles signing for wallets that sign outside the daemon process.
//! `sign()` always returns `Pending`. Signing completes when `complete()` is called
//! with the externally-signed transaction bytes.
//!
//! # No duplicate pending state
//!
//! The `SignatureOrchestrator` is the sole pending-state owner.
//! This adapter does NOT maintain its own pending map. When `complete()` is called,
//! the orchestrator passes the consumed `SignatureRequest` (which contains the
//! original `finalized_tx_bytes` and `expected_signer`) so the adapter can verify
//! the signature without any internal lookup.
//!
//! # Must NOT
//!
//! - Perform policy checks or approval decisions
//! - Emit audit events or record spend
//! - Modify the finalized transaction bytes
//! - Own or mutate session-wallet bindings (reads only via `ExternalWalletBindings`)

use std::sync::Arc;

use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use tracing::{info, instrument, warn};

use super::adapter::{ExternalWalletBindings, WalletAdapter};
use super::types::{
    SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId,
};

// Re-use the existing pure verification function from external_wallet module.
use crate::external_wallet::verify_signed_tx;

/// Execution Plane only.
/// Handles signing for external wallets (Phantom, etc.) that sign out-of-band.
///
/// Asynchronous: `sign()` always returns `Pending`. Signing completes when
/// `complete()` is called with the externally-signed transaction bytes.
///
/// # No internal pending storage
///
/// Unlike the original `ExternalWalletStore`, this adapter does NOT park transactions
/// internally. The `SignatureOrchestrator` owns all pending state and passes the
/// consumed `SignatureRequest` to `complete()`, which contains the original
/// `finalized_tx_bytes` and `expected_signer` needed for verification.
///
/// # Verification on `complete()`
///
/// 1. Deserializes both the original and signed transactions
/// 2. Calls `verify_signed_tx()` — the existing pure 4-step verification function
/// 3. Returns `Signed` on success, `VerificationFailed` on failure
///
/// # Session-wallet bindings
///
/// `supports()` checks whether the `expected_signer` is bound as an external wallet
/// for the request's `session_id`. Bindings are managed externally (gateway bind_wallet API).
/// This adapter only reads them via the `ExternalWalletBindings` trait.
///
/// # Must NOT
///
/// - Perform policy checks or approval decisions
/// - Emit audit events or record spend
/// - Modify the finalized transaction bytes
/// - Own or mutate session-wallet bindings
pub struct ExternalWalletAdapter {
    /// Read-only access to session-wallet bindings.
    bindings: Arc<dyn ExternalWalletBindings>,
}

impl ExternalWalletAdapter {
    /// Creates an adapter with a read-only binding registry.
    ///
    /// The bindings reference is shared with the gateway's bind_wallet logic.
    /// The adapter reads from it (via `supports()`) but never writes to it.
    pub fn new(bindings: Arc<dyn ExternalWalletBindings>) -> Self {
        Self { bindings }
    }
}

#[async_trait]
impl WalletAdapter for ExternalWalletAdapter {
    fn adapter_type(&self) -> &str {
        "external"
    }

    /// Returns `true` if the expected signer is bound as an external wallet for this session.
    ///
    /// Reads from the shared `ExternalWalletBindings` registry.
    /// Must NOT have side effects. Must NOT block.
    fn supports(&self, request: &SignatureRequest) -> bool {
        self.bindings.is_bound(
            &request.session_id,
            &request.expected_signer.to_string(),
        )
    }

    /// Returns `Pending` with the raw finalized tx bytes.
    ///
    /// The orchestrator stores the `SignatureRequest` in its pending map.
    /// The caller encodes the bytes for transport and returns them to the user.
    /// When the user signs and submits, the caller calls `orchestrator.complete()`.
    ///
    /// This adapter has NO internal state to maintain.
    #[instrument(skip(self, request), fields(request_id = %request.id))]
    async fn sign(
        &self,
        request: &SignatureRequest,
    ) -> Result<SignatureOutcome, SignatureError> {
        info!(
            request_id = %request.id,
            expected_signer = %request.expected_signer,
            "external adapter: request pending, awaiting external signature"
        );

        Ok(SignatureOutcome::Pending {
            request_id: request.id,
            adapter_type: self.adapter_type().to_string(),
            finalized_tx_bytes: request.finalized_tx_bytes.clone(),
        })
    }

    /// Verify an externally-signed transaction and return the outcome.
    ///
    /// The orchestrator passes the consumed `SignatureRequest` which contains
    /// the original `finalized_tx_bytes` and `expected_signer`. No internal
    /// lookup required.
    ///
    /// # Verification steps (reuses existing `verify_signed_tx`)
    ///
    /// 1. Message bytes are identical (no instruction/blockhash tampering)
    /// 2. Expected signer is present in `account_keys`
    /// 3. The signature slot at the signer's index is non-default
    /// 4. The signature is cryptographically valid (ed25519)
    ///
    /// On failure: returns `Err(VerificationFailed)`. The orchestrator has already
    /// consumed the pending entry — there is no re-insertion.
    #[instrument(skip(self, request, signed_tx_bytes), fields(request_id = %request.id))]
    async fn complete(
        &self,
        request: &SignatureRequest,
        signed_tx_bytes: &[u8],
    ) -> Result<SignatureOutcome, SignatureError> {
        // 1. Deserialize the signed transaction.
        let signed_tx: Transaction = bincode::deserialize(signed_tx_bytes)
            .map_err(|e| {
                warn!(
                    request_id = %request.id,
                    error = %e,
                    "external adapter: failed to deserialize signed tx"
                );
                SignatureError::VerificationFailed(
                    format!("invalid signed transaction bytes: {e}"),
                )
            })?;

        // 2. Deserialize the original finalized transaction.
        let original_tx: Transaction = bincode::deserialize(&request.finalized_tx_bytes)
            .map_err(|e| {
                warn!(
                    request_id = %request.id,
                    error = %e,
                    "external adapter: failed to deserialize original tx (internal error)"
                );
                SignatureError::InvalidTransaction(
                    format!("failed to deserialize parked tx: {e}"),
                )
            })?;

        // 3. Verify using the existing pure verification function.
        verify_signed_tx(&original_tx, &signed_tx, &request.expected_signer)
            .map_err(|e| {
                warn!(
                    request_id = %request.id,
                    expected_signer = %request.expected_signer,
                    error = %e,
                    "external adapter: signature verification failed"
                );
                SignatureError::VerificationFailed(e.to_string())
            })?;

        // 4. Extract the signature at the signer's index.
        let signer_index = signed_tx.message.account_keys
            .iter()
            .position(|k| k == &request.expected_signer)
            .expect("verified by verify_signed_tx: signer is in account_keys");

        let signature = signed_tx.signatures[signer_index].to_string();

        // 5. Serialize the signed transaction.
        let signed_bytes = bincode::serialize(&signed_tx)
            .map_err(|e| SignatureError::SigningFailed(
                format!("failed to serialize verified signed tx: {e}"),
            ))?;

        info!(
            request_id = %request.id,
            signature = %signature,
            "external adapter: signature verified and accepted"
        );

        Ok(SignatureOutcome::Signed {
            request_id: request.id,
            signature,
            signed_tx_bytes: signed_bytes,
            adapter_type: self.adapter_type().to_string(),
            transaction_id: request.transaction_id,
            expected_signer: request.expected_signer.to_string(),
        })
    }

    // cleanup: default no-op is correct — no internal state (orchestrator owns pending).
}

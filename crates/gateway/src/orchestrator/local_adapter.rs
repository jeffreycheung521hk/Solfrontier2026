//! Local wallet adapter — signs using the in-process keystore.
//!
//! # Execution Plane only
//!
//! This adapter signs finalized transactions using a `Signer` from `claw-wallet-engine`.
//! It deserializes the `finalized_tx_bytes`, calls `signer.sign_transaction()`,
//! and returns the signature.
//!
//! # Must NOT
//!
//! - Call `pipeline.sign()` or inspect `ApprovalMode`
//! - Return `AwaitingApproval` or any approval-related state
//! - Return `SignatureOutcome::Pending` (local signing is always synchronous)
//! - Perform policy evaluation
//! - Emit audit or events

use async_trait::async_trait;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use tracing::{info, instrument};

use claw_wallet_engine::keystore::SecretKeystore;
use claw_wallet_engine::signer::SignerRef;

use super::adapter::WalletAdapter;
use super::types::{
    SignatureError, SignatureOutcome, SignatureRequest,
};

/// Execution Plane only.
/// Signs finalized transactions using a local keystore.
///
/// Synchronous: `sign()` always returns `Signed`, never `Pending`.
/// `complete()` is not applicable and always returns an error.
///
/// # Must NOT
///
/// - Call `pipeline.sign()` or inspect `ApprovalMode`
/// - Return `AwaitingApproval` or any approval-related state
/// - Perform policy evaluation
/// - Emit audit or events
///
/// # Signing flow
///
/// 1. Deserialize `finalized_tx_bytes` into a `Transaction`
/// 2. Call `signer.sign_transaction(&mut tx)` (the `Signer` trait from `claw-wallet-engine`)
/// 3. Serialize the signed `Transaction` back to bytes
/// 4. Return the signature string and signed bytes
///
/// # Routing
///
/// `supports()` returns `true` if the keystore contains the `expected_signer` pubkey.
pub struct LocalWalletAdapter {
    signer: SignerRef,
    keystore: SecretKeystore,
}

impl LocalWalletAdapter {
    /// Creates a local adapter backed by the given signer and keystore.
    ///
    /// The signer is the active signing key. The keystore is used for
    /// `supports()` routing — checking whether the expected pubkey is loaded.
    pub fn new(signer: SignerRef, keystore: SecretKeystore) -> Self {
        Self { signer, keystore }
    }
}

#[async_trait]
impl WalletAdapter for LocalWalletAdapter {
    fn adapter_type(&self) -> &str {
        "local"
    }

    /// Returns `true` if the keystore contains the expected signer pubkey.
    ///
    /// This is a fast, synchronous check with no I/O.
    fn supports(&self, request: &SignatureRequest) -> bool {
        self.keystore.contains(&request.expected_signer)
    }

    /// Signs the finalized transaction synchronously.
    ///
    /// Supports both legacy `Transaction` and V0 `VersionedTransaction` formats.
    /// The format is detected by trying V0 deserialize first — V0 has a version
    /// prefix byte (high bit set on the first byte of the serialized message)
    /// that makes it unambiguously distinguishable from legacy.
    ///
    /// 1. Try bincode-deserialize as V0 → sign via `sign_versioned`
    /// 2. Fallback: bincode-deserialize as legacy `Transaction` → sign via `sign_transaction`
    /// 3. Serialize the signed transaction back to bytes
    /// 4. Return `SignatureOutcome::Signed`
    ///
    /// Never returns `Pending`. Never inspects `ApprovalMode`.
    #[instrument(skip(self, request), fields(request_id = %request.id))]
    async fn sign(
        &self,
        request: &SignatureRequest,
    ) -> Result<SignatureOutcome, SignatureError> {
        // Try V0 first. If the bytes are a VersionedTransaction, this succeeds.
        // If they are legacy, this fails (bincode enum discriminant mismatch)
        // and we fall through to the legacy path.
        if let Ok(mut v0_tx) = bincode::deserialize::<VersionedTransaction>(
            &request.finalized_tx_bytes,
        ) {
            // Only accept if this is actually V0 (not legacy-wrapped).
            // Legacy messages MAY deserialize into a VersionedTransaction with
            // a Legacy message — in that case we want to delegate to the legacy
            // path so we keep using the existing `sign_transaction` API.
            use solana_sdk::message::VersionedMessage;
            if matches!(v0_tx.message, VersionedMessage::V0(_)) {
                let signature = self
                    .signer
                    .sign_versioned(&mut v0_tx)
                    .await
                    .map_err(|e| SignatureError::SigningFailed(e.to_string()))?;

                let signed_tx_bytes = bincode::serialize(&v0_tx).map_err(|e| {
                    SignatureError::SigningFailed(format!(
                        "failed to serialize signed v0 tx: {e}"
                    ))
                })?;
                let sig_str = signature.to_string();
                info!(
                    request_id = %request.id,
                    signature = %sig_str,
                    format = "v0",
                    "local adapter: v0 transaction signed"
                );
                return Ok(SignatureOutcome::Signed {
                    request_id: request.id,
                    signature: sig_str,
                    signed_tx_bytes,
                    adapter_type: self.adapter_type().to_string(),
                    transaction_id: request.transaction_id,
                    expected_signer: request.expected_signer.to_string(),
                });
            }
        }

        // Legacy path (unchanged behavior).
        let mut tx: Transaction = bincode::deserialize(&request.finalized_tx_bytes)
            .map_err(|e| SignatureError::InvalidTransaction(e.to_string()))?;

        let signature = self.signer.sign_transaction(&mut tx).await
            .map_err(|e| SignatureError::SigningFailed(e.to_string()))?;

        let signed_tx_bytes = bincode::serialize(&tx)
            .map_err(|e| SignatureError::SigningFailed(
                format!("failed to serialize signed tx: {e}"),
            ))?;

        let sig_str = signature.to_string();
        info!(
            request_id = %request.id,
            signature = %sig_str,
            format = "legacy",
            "local adapter: legacy transaction signed"
        );

        Ok(SignatureOutcome::Signed {
            request_id: request.id,
            signature: sig_str,
            signed_tx_bytes,
            adapter_type: self.adapter_type().to_string(),
            transaction_id: request.transaction_id,
            expected_signer: request.expected_signer.to_string(),
        })
    }

    /// Not applicable for local signing. Always returns an error.
    ///
    /// Local signing is synchronous — `sign()` returns the signature immediately.
    /// There is no asynchronous completion flow.
    async fn complete(
        &self,
        _request: &SignatureRequest,
        _signed_tx_bytes: &[u8],
    ) -> Result<SignatureOutcome, SignatureError> {
        Err(SignatureError::SigningFailed(
            "local adapter does not support async completion".to_string(),
        ))
    }

    // cleanup: default no-op is correct — no internal state.
}

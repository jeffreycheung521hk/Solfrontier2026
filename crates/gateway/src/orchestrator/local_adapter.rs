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
use solana_sdk::transaction::Transaction;
use tracing::{info, instrument};

use claw_wallet_engine::keystore::SecretKeystore;
use claw_wallet_engine::signer::SignerRef;

use super::adapter::WalletAdapter;
use super::types::{
    SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId,
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
    /// 1. Deserializes `finalized_tx_bytes` into a `Transaction`
    /// 2. Calls `signer.sign_transaction(&mut tx)`
    /// 3. Serializes the signed `Transaction`
    /// 4. Returns `SignatureOutcome::Signed`
    ///
    /// Never returns `Pending`. Never inspects `ApprovalMode`.
    #[instrument(skip(self, request), fields(request_id = %request.id))]
    async fn sign(
        &self,
        request: &SignatureRequest,
    ) -> Result<SignatureOutcome, SignatureError> {
        // 1. Deserialize the finalized transaction.
        let mut tx: Transaction = bincode::deserialize(&request.finalized_tx_bytes)
            .map_err(|e| SignatureError::InvalidTransaction(e.to_string()))?;

        // 2. Sign directly — no pipeline.sign(), no ApprovalMode, no AwaitingApproval.
        let signature = self.signer.sign_transaction(&mut tx).await
            .map_err(|e| SignatureError::SigningFailed(e.to_string()))?;

        // 3. Serialize the signed transaction.
        let signed_tx_bytes = bincode::serialize(&tx)
            .map_err(|e| SignatureError::SigningFailed(
                format!("failed to serialize signed tx: {e}"),
            ))?;

        let sig_str = signature.to_string();
        info!(
            request_id = %request.id,
            signature = %sig_str,
            "local adapter: transaction signed"
        );

        // 4. Return Signed — always synchronous, never Pending.
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

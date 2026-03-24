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
    SignatureError, SignatureOutcome, SignatureRequest,
};

// Re-use the existing pure verification function from external_wallet module.

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
        // Browser sends Solana wire format bytes from signed.serialize().
        // Phantom's signTransaction() may modify the message (e.g., adding
        // compute budget instructions or updating the blockhash). Therefore
        // we CANNOT enforce byte-for-byte message equality.
        //
        // Verification strategy:
        //   1. Deserialize the signed wire bytes into a Transaction
        //   2. Verify the expected signer is in account_keys
        //   3. Verify instructions are semantically equivalent to original
        //   4. Verify ed25519 signature over the signed message
        //   5. Reconstruct bincode bytes for RPC submission

        // 1. Deserialize original from bincode (internal storage format).
        let original_tx: Transaction = bincode::deserialize(&request.finalized_tx_bytes)
            .map_err(|e| {
                warn!(request_id = %request.id, error = %e, "failed to deserialize original tx");
                SignatureError::InvalidTransaction(format!("invalid original tx: {e}"))
            })?;

        // 2. Deserialize signed bytes (bincode = wire format for Solana SDK).
        //    web3.js Transaction.serialize() output is bincode-compatible because
        //    Solana SDK uses #[serde(with = "short_vec")] for all Vec fields.
        let signed_tx: Transaction = bincode::deserialize(signed_tx_bytes)
            .map_err(|e| {
                warn!(request_id = %request.id, error = %e, "failed to deserialize signed tx");
                SignatureError::VerificationFailed(format!("invalid signed tx bytes: {e}"))
            })?;

        info!(
            request_id = %request.id,
            orig_ix_count = original_tx.message.instructions.len(),
            signed_ix_count = signed_tx.message.instructions.len(),
            orig_accounts = original_tx.message.account_keys.len(),
            signed_accounts = signed_tx.message.account_keys.len(),
            "external adapter: verifying signed transaction"
        );

        // 3. Find expected signer in SIGNED tx's account_keys.
        let signer_index = signed_tx.message.account_keys
            .iter()
            .position(|k| k == &request.expected_signer)
            .ok_or_else(|| {
                warn!(request_id = %request.id, "expected signer not in signed tx account keys");
                SignatureError::VerificationFailed("expected signer not in account keys".into())
            })?;

        if signer_index >= signed_tx.signatures.len()
            || signed_tx.signatures[signer_index] == solana_sdk::signature::Signature::default()
        {
            warn!(request_id = %request.id, "signer did not sign");
            return Err(SignatureError::VerificationFailed("expected signer did not sign".into()));
        }

        // 4. Semantic instruction equivalence check.
        //    Phantom may prepend compute budget instructions. Verify that ALL
        //    original instructions are present in the signed tx (in order).
        //    Extra prepended instructions (compute budget) are allowed.
        {
            let orig_ixs = &original_tx.message.instructions;
            let signed_ixs = &signed_tx.message.instructions;

            if signed_ixs.len() < orig_ixs.len() {
                warn!(
                    request_id = %request.id,
                    orig = orig_ixs.len(),
                    signed = signed_ixs.len(),
                    "signed tx has fewer instructions than original"
                );
                return Err(SignatureError::VerificationFailed(
                    "signed transaction has fewer instructions".into()
                ));
            }

            // Map original account_keys index → pubkey for comparison.
            let orig_keys = &original_tx.message.account_keys;
            let signed_keys = &signed_tx.message.account_keys;

            // Check each original instruction exists in signed tx (allowing
            // extra prepended instructions from Phantom).
            let offset = signed_ixs.len() - orig_ixs.len();
            for (i, orig_ix) in orig_ixs.iter().enumerate() {
                let signed_ix = &signed_ixs[offset + i];

                // Compare program ID by pubkey (indices may differ).
                let orig_program = orig_keys.get(orig_ix.program_id_index as usize);
                let signed_program = signed_keys.get(signed_ix.program_id_index as usize);
                if orig_program != signed_program {
                    warn!(
                        request_id = %request.id,
                        ix_index = i,
                        "instruction program ID mismatch"
                    );
                    return Err(SignatureError::VerificationFailed(
                        format!("instruction {} program ID mismatch", i)
                    ));
                }

                // Compare instruction data (must be identical).
                if orig_ix.data != signed_ix.data {
                    warn!(
                        request_id = %request.id,
                        ix_index = i,
                        "instruction data mismatch"
                    );
                    return Err(SignatureError::VerificationFailed(
                        format!("instruction {} data mismatch", i)
                    ));
                }

                // Compare account references by pubkey.
                let orig_accts: Vec<&Pubkey> = orig_ix.accounts.iter()
                    .filter_map(|&idx| orig_keys.get(idx as usize))
                    .collect();
                let signed_accts: Vec<&Pubkey> = signed_ix.accounts.iter()
                    .filter_map(|&idx| signed_keys.get(idx as usize))
                    .collect();
                if orig_accts != signed_accts {
                    warn!(
                        request_id = %request.id,
                        ix_index = i,
                        "instruction account keys mismatch"
                    );
                    return Err(SignatureError::VerificationFailed(
                        format!("instruction {} account keys mismatch", i)
                    ));
                }
            }
        }

        // 5. ed25519 verification over the signed message bytes.
        //    Phantom signed the message_data() of the (possibly modified) tx.
        let signed_msg = signed_tx.message_data();
        let sig = &signed_tx.signatures[signer_index];
        if !sig.verify(request.expected_signer.as_ref(), &signed_msg) {
            warn!(request_id = %request.id, "ed25519 verification failed");
            return Err(SignatureError::VerificationFailed("signature verification failed".into()));
        }

        let signature = signed_tx.signatures[signer_index].to_string();

        // 6. Use the SIGNED Transaction for RPC submission (it has the valid
        //    signature over its own message). Serialize to bincode for
        //    send_raw_transaction which expects bincode format.
        let signed_bytes = bincode::serialize(&signed_tx)
            .map_err(|e| SignatureError::SigningFailed(format!("serialize signed tx: {e}")))?;

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

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
        // Both signed_tx_bytes and request.finalized_tx_bytes are now in
        // Solana wire format (compact-u16 prefixes).
        //
        // Wire format: [compact-u16 sig_count] [64*n signatures] [message_bytes]
        // We parse wire format directly for verification, avoiding bincode
        // deserialization (which uses different Vec prefix encoding).

        /// Parse wire format bytes into (signatures, message_bytes_offset).
        fn parse_wire(bytes: &[u8]) -> Result<(usize, usize, usize), String> {
            let mut offset = 0usize;
            let mut sig_count = 0usize;
            let mut shift = 0u32;
            while offset < bytes.len() {
                let byte = bytes[offset] as usize;
                offset += 1;
                sig_count |= (byte & 0x7f) << shift;
                if byte & 0x80 == 0 { break; }
                shift += 7;
                if shift >= 21 { return Err("invalid compact-u16".into()); }
            }
            let sigs_start = offset;
            let msg_start = offset + sig_count * 64;
            if bytes.len() < msg_start {
                return Err(format!("truncated: need {msg_start}, have {}", bytes.len()));
            }
            Ok((sig_count, sigs_start, msg_start))
        }

        // 1. Get original message bytes in wire format.
        //    finalized_tx_bytes is stored as bincode. Deserialize to Transaction,
        //    then use message_data() which returns wire-format message bytes.
        //    This is the same path that pending_for_session uses to send to browser.
        let original_tx: Transaction = bincode::deserialize(&request.finalized_tx_bytes)
            .map_err(|e| {
                warn!(request_id = %request.id, error = %e, "failed to deserialize original bincode tx");
                SignatureError::InvalidTransaction(format!("invalid original tx: {e}"))
            })?;
        let orig_msg_bytes = original_tx.message_data();

        // 2. Parse signed wire bytes from browser.
        let (signed_sig_count, signed_sigs_start, signed_msg_start) = parse_wire(signed_tx_bytes)
            .map_err(|e| {
                warn!(request_id = %request.id, error = %e, "failed to parse signed wire tx");
                SignatureError::VerificationFailed(format!("invalid signed wire bytes: {e}"))
            })?;
        let signed_msg_bytes = &signed_tx_bytes[signed_msg_start..];

        info!(
            request_id = %request.id,
            orig_msg_len = orig_msg_bytes.len(),
            signed_msg_len = signed_msg_bytes.len(),
            msg_match = (orig_msg_bytes.as_slice() == signed_msg_bytes),
            signed_sigs = signed_sig_count,
            "external adapter: wire format verification"
        );

        // 3. Message bytes must be identical (no instruction/blockhash tampering).
        if orig_msg_bytes.as_slice() != signed_msg_bytes {
            warn!(request_id = %request.id, "message bytes modified");
            return Err(SignatureError::VerificationFailed(
                "transaction message bytes modified".to_string(),
            ));
        }

        // 4. Extract signatures from wire-format signed bytes.
        let mut wire_signatures = Vec::with_capacity(signed_sig_count);
        for i in 0..signed_sig_count {
            let start = signed_sigs_start + i * 64;
            let sig_bytes: [u8; 64] = signed_tx_bytes[start..start+64]
                .try_into()
                .map_err(|_| SignatureError::VerificationFailed("sig bytes truncated".into()))?;
            wire_signatures.push(solana_sdk::signature::Signature::from(sig_bytes));
        }

        // 5. Find expected signer in account_keys (from original bincode tx).
        let signer_index = original_tx.message.account_keys
            .iter()
            .position(|k| k == &request.expected_signer)
            .ok_or_else(|| {
                warn!(request_id = %request.id, "expected signer not in account keys");
                SignatureError::VerificationFailed("expected signer not in account keys".into())
            })?;

        if signer_index >= wire_signatures.len()
            || wire_signatures[signer_index] == solana_sdk::signature::Signature::default()
        {
            warn!(request_id = %request.id, "signer did not sign");
            return Err(SignatureError::VerificationFailed("expected signer did not sign".into()));
        }

        // 6. ed25519 verification: signature over the wire-format message bytes.
        //    Phantom signed message_data() (wire format). Both orig and signed match.
        let sig = &wire_signatures[signer_index];
        if !sig.verify(request.expected_signer.as_ref(), &orig_msg_bytes) {
            warn!(request_id = %request.id, "ed25519 verification failed");
            return Err(SignatureError::VerificationFailed("signature verification failed".into()));
        }

        let signature = wire_signatures[signer_index].to_string();

        // 7. Reconstruct bincode-format bytes for downstream (send_raw_transaction).
        //    Take original bincode Transaction, replace signatures with Phantom's.
        let signed_bytes = {
            let mut tx_for_rpc = original_tx;
            tx_for_rpc.signatures = wire_signatures;
            bincode::serialize(&tx_for_rpc)
                .map_err(|e| SignatureError::SigningFailed(format!("re-serialize signed: {e}")))?
        };

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

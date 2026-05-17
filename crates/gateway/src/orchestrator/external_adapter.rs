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
use solana_sdk::message::v0::MessageAddressTableLookup;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use tracing::{info, instrument, warn};

use super::adapter::{ExternalWalletBindings, WalletAdapter};
use super::types::{
    SignatureError, SignatureOutcome, SignatureRequest,
};

/// Identity of an account slot in a V0 message, as seen from a single
/// instruction's index.
///
/// Used to compare instruction-account references across semantically
/// equivalent but byte-different V0 messages (e.g., Jupiter-produced
/// original vs. Phantom-canonicalized signed).
#[derive(Debug, Clone, PartialEq, Eq)]
enum V0AcctIdentity {
    /// Slot is in the static `account_keys` section — resolves to a known pubkey.
    Static(Pubkey),
    /// Slot is in an ALT expansion section. Resolution to a pubkey requires
    /// fetching the ALT; we compare the `(table, inner_index, writable)` tuple
    /// instead, which is sufficient for semantic-equivalence comparison.
    Alt { table: Pubkey, inner: u8, writable: bool },
    /// Index points past the end of the logical resolved list. Only appears
    /// on malformed input; always compares not-equal to itself so such an
    /// instruction never "matches" anything.
    OutOfRange,
}

/// Resolve an instruction-account index into a semantic identity.
///
/// V0 canonical account order is:
///   [static_account_keys] ++
///   [writable ALT expansions across all lookups, in lookup order, in inner-index order] ++
///   [readonly ALT expansions across all lookups, in lookup order, in inner-index order]
///
/// This function does NOT fetch any ALT contents; it only returns the
/// `(table, inner)` tuple for ALT-referenced slots, which is enough to
/// compare two V0 messages' semantic equivalence.
fn resolve_v0_account_identity(
    idx: u8,
    static_keys: &[Pubkey],
    alt_lookups: &[MessageAddressTableLookup],
) -> V0AcctIdentity {
    let i = idx as usize;
    if i < static_keys.len() {
        return V0AcctIdentity::Static(static_keys[i]);
    }
    let mut cursor = static_keys.len();
    for alt in alt_lookups {
        for &inner in &alt.writable_indexes {
            if cursor == i {
                return V0AcctIdentity::Alt {
                    table: alt.account_key,
                    inner,
                    writable: true,
                };
            }
            cursor += 1;
        }
    }
    for alt in alt_lookups {
        for &inner in &alt.readonly_indexes {
            if cursor == i {
                return V0AcctIdentity::Alt {
                    table: alt.account_key,
                    inner,
                    writable: false,
                };
            }
            cursor += 1;
        }
    }
    V0AcctIdentity::OutOfRange
}

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
        // Detect V0 vs legacy by sniffing the original tx format stored at approval time.
        // Jupiter JIT path stores a bincode-serialized VersionedTransaction (V0); all
        // other paths store a legacy Transaction. Try V0 first.
        if let Ok(original_v0) = bincode::deserialize::<VersionedTransaction>(&request.finalized_tx_bytes) {
            if matches!(original_v0.message, VersionedMessage::V0(_)) {
                return self.complete_v0(request, signed_tx_bytes, original_v0).await;
            }
        }

        self.complete_legacy(request, signed_tx_bytes).await
    }

    // cleanup: default no-op is correct — no internal state (orchestrator owns pending).
}

impl ExternalWalletAdapter {
    /// Verification path for legacy (non-V0) transactions.
    async fn complete_legacy(
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
            "external adapter: verifying signed legacy transaction"
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

            let orig_keys = &original_tx.message.account_keys;
            let signed_keys = &signed_tx.message.account_keys;

            let offset = signed_ixs.len() - orig_ixs.len();
            for (i, orig_ix) in orig_ixs.iter().enumerate() {
                let signed_ix = &signed_ixs[offset + i];

                let orig_program = orig_keys.get(orig_ix.program_id_index as usize);
                let signed_program = signed_keys.get(signed_ix.program_id_index as usize);
                if orig_program != signed_program {
                    warn!(request_id = %request.id, ix_index = i, "instruction program ID mismatch");
                    return Err(SignatureError::VerificationFailed(
                        format!("instruction {} program ID mismatch", i)
                    ));
                }

                if orig_ix.data != signed_ix.data {
                    warn!(request_id = %request.id, ix_index = i, "instruction data mismatch");
                    return Err(SignatureError::VerificationFailed(
                        format!("instruction {} data mismatch", i)
                    ));
                }

                let orig_accts: Vec<&Pubkey> = orig_ix.accounts.iter()
                    .filter_map(|&idx| orig_keys.get(idx as usize))
                    .collect();
                let signed_accts: Vec<&Pubkey> = signed_ix.accounts.iter()
                    .filter_map(|&idx| signed_keys.get(idx as usize))
                    .collect();
                if orig_accts != signed_accts {
                    warn!(request_id = %request.id, ix_index = i, "instruction account keys mismatch");
                    return Err(SignatureError::VerificationFailed(
                        format!("instruction {} account keys mismatch", i)
                    ));
                }
            }
        }

        // 5. ed25519 verification over the signed message bytes.
        let signed_msg = signed_tx.message_data();
        let sig = &signed_tx.signatures[signer_index];
        if !sig.verify(request.expected_signer.as_ref(), &signed_msg) {
            warn!(request_id = %request.id, "ed25519 verification failed");
            return Err(SignatureError::VerificationFailed("signature verification failed".into()));
        }

        let signature = signed_tx.signatures[signer_index].to_string();

        let signed_bytes = bincode::serialize(&signed_tx)
            .map_err(|e| SignatureError::SigningFailed(format!("serialize signed tx: {e}")))?;

        info!(
            request_id = %request.id,
            signature = %signature,
            "external adapter: legacy signature verified and accepted"
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

    /// Verification path for V0 (versioned) transactions — used by the Jupiter JIT path.
    ///
    /// V0 differences from legacy:
    /// - Account keys split into static keys + ALT-resolved keys
    /// - Message serialized via `message.serialize()` (not `message_data()`)
    /// - Instruction account indices may reference ALT slots — we verify program ID
    ///   and data bytes only; the ed25519 check covers the rest
    async fn complete_v0(
        &self,
        request: &SignatureRequest,
        signed_tx_bytes: &[u8],
        original: VersionedTransaction,
    ) -> Result<SignatureOutcome, SignatureError> {
        // 1. Deserialize signed bytes as VersionedTransaction.
        let signed: VersionedTransaction = bincode::deserialize(signed_tx_bytes)
            .map_err(|e| {
                warn!(request_id = %request.id, error = %e, "failed to deserialize signed V0 tx");
                SignatureError::VerificationFailed(format!("invalid signed V0 tx bytes: {e}"))
            })?;

        if !matches!(signed.message, VersionedMessage::V0(_)) {
            warn!(request_id = %request.id, "expected V0 signed transaction, got legacy");
            return Err(SignatureError::VerificationFailed(
                "expected V0 signed transaction but received legacy format".into()
            ));
        }

        // 2. Find expected signer in signed tx's static account keys.
        //    Signers are always in the static key section for V0.
        let signed_static = signed.message.static_account_keys();
        let signer_index = signed_static.iter()
            .position(|k| k == &request.expected_signer)
            .ok_or_else(|| {
                warn!(request_id = %request.id, "expected signer not in V0 static account keys");
                SignatureError::VerificationFailed("expected signer not in static account keys".into())
            })?;

        if signer_index >= signed.signatures.len()
            || signed.signatures[signer_index] == solana_sdk::signature::Signature::default()
        {
            warn!(request_id = %request.id, "signer did not sign V0 tx");
            return Err(SignatureError::VerificationFailed("expected signer did not sign".into()));
        }

        info!(
            request_id = %request.id,
            orig_ix_count = original.message.instructions().len(),
            signed_ix_count = signed.message.instructions().len(),
            orig_static_keys = original.message.static_account_keys().len(),
            signed_static_keys = signed_static.len(),
            "external adapter: verifying signed V0 transaction"
        );

        // 3a. Blockhash lock: Jupiter JIT builds with a specific blockhash for ALT validity.
        //     Unlike the legacy path (where Phantom may update blockhash), V0 must not allow
        //     blockhash substitution — ALT resolution depends on the slot that owned the blockhash.
        //     Return BlockhashModified (not VerificationFailed) so callers can trigger a
        //     fresh JIT rebuild from the approved intent rather than treating this as a hard error.
        if original.message.recent_blockhash() != signed.message.recent_blockhash() {
            warn!(request_id = %request.id, "V0 blockhash was modified by external wallet");
            return Err(SignatureError::BlockhashModified);
        }

        // 3b. Semantic instruction equivalence check.
        //
        //     Phantom-produced V0 txs often have a different byte layout than
        //     our JIT output: web3.js / Phantom canonicalises `account_keys`
        //     ordering and correspondingly updates every instruction's
        //     account-index array. Comparing raw `Vec<u8>` of indices would
        //     wrongly reject semantically-equivalent re-encodings.
        //
        //     We compare the *resolved account identities* instead — mapping
        //     each index through `(static_account_keys, address_table_lookups)`
        //     on both sides and requiring per-instruction pairwise equality
        //     of:
        //         (program_id pubkey, data bytes, [resolved identities])
        //
        //     A tampering attack (swap destination X → Y, re-sign) cannot
        //     pass this check: X and Y would resolve to different pubkeys or
        //     different (ALT, inner) tuples, and the instruction's identity
        //     list would differ.
        //
        //     Phantom may also prepend compute-budget instructions; we
        //     handle that with the existing offset.
        {
            let orig_ixs = original.message.instructions();
            let signed_ixs = signed.message.instructions();
            let orig_static = original.message.static_account_keys();

            // Extract ALT lookups from both sides (V0 only; we already
            // established both are V0 earlier).
            let orig_alts: &[MessageAddressTableLookup] = match &original.message {
                VersionedMessage::V0(m) => m.address_table_lookups.as_slice(),
                _ => &[],
            };
            let signed_alts: &[MessageAddressTableLookup] = match &signed.message {
                VersionedMessage::V0(m) => m.address_table_lookups.as_slice(),
                _ => &[],
            };

            if signed_ixs.len() < orig_ixs.len() {
                warn!(
                    request_id = %request.id,
                    orig = orig_ixs.len(),
                    signed = signed_ixs.len(),
                    "V0 signed tx has fewer instructions than original"
                );
                return Err(SignatureError::VerificationFailed(
                    "signed V0 transaction has fewer instructions".into()
                ));
            }

            let offset = signed_ixs.len() - orig_ixs.len();
            for (i, orig_ix) in orig_ixs.iter().enumerate() {
                let signed_ix = &signed_ixs[offset + i];

                // Program ID must resolve to the same static pubkey.
                // (V0 requires program_id_index to point into static_account_keys.)
                let orig_prog = orig_static.get(orig_ix.program_id_index as usize);
                let signed_prog = signed_static.get(signed_ix.program_id_index as usize);
                if orig_prog.is_none() || signed_prog.is_none() || orig_prog != signed_prog {
                    warn!(request_id = %request.id, ix_index = i, "V0 instruction program ID mismatch");
                    return Err(SignatureError::VerificationFailed(
                        format!("V0 instruction {} program ID mismatch", i)
                    ));
                }

                // Instruction data bytes must be identical.
                if orig_ix.data != signed_ix.data {
                    warn!(request_id = %request.id, ix_index = i, "V0 instruction data mismatch");
                    return Err(SignatureError::VerificationFailed(
                        format!("V0 instruction {} data mismatch", i)
                    ));
                }

                // Resolved account-identity list must match position-by-position.
                if orig_ix.accounts.len() != signed_ix.accounts.len() {
                    warn!(
                        request_id = %request.id,
                        ix_index = i,
                        orig_len = orig_ix.accounts.len(),
                        signed_len = signed_ix.accounts.len(),
                        "V0 instruction account count mismatch"
                    );
                    return Err(SignatureError::VerificationFailed(
                        format!("V0 instruction {} account count mismatch", i)
                    ));
                }
                for (pos, (&orig_idx, &signed_idx)) in orig_ix
                    .accounts
                    .iter()
                    .zip(signed_ix.accounts.iter())
                    .enumerate()
                {
                    let orig_id = resolve_v0_account_identity(orig_idx, orig_static, orig_alts);
                    let signed_id =
                        resolve_v0_account_identity(signed_idx, signed_static, signed_alts);
                    if orig_id != signed_id
                        || matches!(orig_id, V0AcctIdentity::OutOfRange)
                    {
                        warn!(
                            request_id = %request.id,
                            ix_index = i,
                            account_pos = pos,
                            ?orig_id,
                            ?signed_id,
                            "V0 instruction resolved-account-identity mismatch"
                        );
                        return Err(SignatureError::VerificationFailed(
                            format!(
                                "V0 instruction {} account {} resolved-identity mismatch",
                                i, pos
                            )
                        ));
                    }
                }
            }
        }

        // 4. ed25519 verification over the serialized V0 message bytes.
        //    `VersionedMessage::serialize()` produces the canonical signing bytes.
        let msg_bytes = signed.message.serialize();
        let sig = &signed.signatures[signer_index];
        if !sig.verify(request.expected_signer.as_ref(), &msg_bytes) {
            warn!(request_id = %request.id, "V0 ed25519 verification failed");
            return Err(SignatureError::VerificationFailed("V0 signature verification failed".into()));
        }

        let signature = signed.signatures[signer_index].to_string();

        // 5. Serialize the signed V0 tx for submission via send_raw_v0_transaction.
        let signed_bytes = bincode::serialize(&signed)
            .map_err(|e| SignatureError::SigningFailed(format!("serialize signed V0 tx: {e}")))?;

        info!(
            request_id = %request.id,
            signature = %signature,
            "external adapter: V0 signature verified and accepted"
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
}

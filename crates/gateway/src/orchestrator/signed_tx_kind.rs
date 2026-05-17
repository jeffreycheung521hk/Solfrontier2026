//! Classify externally-signed transaction bytes as V0 or legacy.
//!
//! Used by the daemon's wallet-signatures completion path to route signed
//! bytes to the correct `ClawRpcClient` method:
//!
//! - `SignedTxKind::V0`     → `send_raw_v0_transaction` (V0 + ALT bytes)
//! - `SignedTxKind::Legacy` → `send_raw_transaction`    (anything else)
//!
//! The classifier is a pure function. It attempts a bincode deserialize of
//! the bytes as `VersionedTransaction` — if that succeeds AND the message
//! is `VersionedMessage::V0(...)`, we return `V0`. Every other byte pattern
//! (legacy-wire tx, corrupted bytes, random bytes) routes to `Legacy`,
//! which is the safe fallback: `send_raw_transaction` is the legacy code
//! path that also existed before the V0 milestone and is what a client
//! without V0 awareness would get.
//!
//! # Why this lives in its own module
//!
//! The dispatch used to be inline inside `daemon.rs`'s
//! `submit_signed_tx_inner`. Extracting to a named, testable function lets
//! us lock the V0-routing contract with a deterministic unit test — which
//! is what Phase A1-Execute's test 6 demands. Per `DEBT.md` D-2, moving
//! integration-specific glue out of `daemon.rs` via attrition is the
//! target shape.

use solana_sdk::message::VersionedMessage;
use solana_sdk::transaction::VersionedTransaction;

/// Classification of a client-submitted signed transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedTxKind {
    /// Bytes parse as a V0 `VersionedTransaction`. Route to
    /// `ClawRpcClient::send_raw_v0_transaction`.
    V0,
    /// Not parseable as V0 (legacy wire format, corrupted bytes, random
    /// bytes). Route to the legacy `ClawRpcClient::send_raw_transaction`.
    Legacy,
}

/// Classify signed transaction bytes into `V0` or `Legacy` for RPC routing.
///
/// Deterministic. No side effects. Accepts any `&[u8]`.
pub fn classify_signed_tx_kind(bytes: &[u8]) -> SignedTxKind {
    match bincode::deserialize::<VersionedTransaction>(bytes) {
        Ok(vtx) if matches!(vtx.message, VersionedMessage::V0(_)) => SignedTxKind::V0,
        _ => SignedTxKind::Legacy,
    }
}

// ── Tests — deterministic V0-dispatch contract (A1-Execute test 6) ──────────

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        hash::Hash,
        instruction::{AccountMeta, Instruction},
        message::{v0, Message as LegacyMessage, VersionedMessage},
        pubkey::Pubkey,
        signature::{Keypair, Signature, Signer},
        transaction::{Transaction, VersionedTransaction},
    };

    fn make_payer() -> Keypair {
        Keypair::new()
    }

    fn make_v0_bytes() -> Vec<u8> {
        let payer = make_payer();
        let program = Pubkey::new_unique();
        let ix = Instruction {
            program_id: program,
            accounts: vec![AccountMeta::new(payer.pubkey(), true)],
            data: vec![1, 2, 3],
        };
        let msg = v0::Message::try_compile(&payer.pubkey(), &[ix], &[], Hash::new_unique())
            .expect("compile v0");
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(msg),
        };
        bincode::serialize(&tx).expect("serialize v0")
    }

    fn make_legacy_bytes_as_versioned_legacy() -> Vec<u8> {
        // Legacy wire format wrapped as VersionedMessage::Legacy; still
        // serializes through VersionedTransaction.
        let payer = make_payer();
        let program = Pubkey::new_unique();
        let ix = Instruction {
            program_id: program,
            accounts: vec![AccountMeta::new(payer.pubkey(), true)],
            data: vec![9, 9, 9],
        };
        let msg = LegacyMessage::new(&[ix], Some(&payer.pubkey()));
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(msg),
        };
        bincode::serialize(&tx).expect("serialize legacy-versioned")
    }

    fn make_raw_legacy_bytes() -> Vec<u8> {
        // Pure legacy Transaction bytes (no VersionedTransaction wrapper).
        // A client that doesn't know about V0 produces these.
        let payer = make_payer();
        let program = Pubkey::new_unique();
        let ix = Instruction {
            program_id: program,
            accounts: vec![AccountMeta::new(payer.pubkey(), true)],
            data: vec![5, 5],
        };
        let tx = Transaction::new_with_payer(&[ix], Some(&payer.pubkey()));
        bincode::serialize(&tx).expect("serialize raw legacy")
    }

    #[test]
    fn v0_bytes_classify_as_v0() {
        let bytes = make_v0_bytes();
        assert_eq!(classify_signed_tx_kind(&bytes), SignedTxKind::V0);
    }

    #[test]
    fn legacy_versioned_bytes_classify_as_legacy() {
        // VersionedTransaction whose inner message is Legacy → must NOT
        // route to V0 send path (V0 RPC config is different).
        let bytes = make_legacy_bytes_as_versioned_legacy();
        assert_eq!(classify_signed_tx_kind(&bytes), SignedTxKind::Legacy);
    }

    #[test]
    fn raw_legacy_bytes_classify_as_legacy() {
        let bytes = make_raw_legacy_bytes();
        assert_eq!(classify_signed_tx_kind(&bytes), SignedTxKind::Legacy);
    }

    #[test]
    fn empty_bytes_classify_as_legacy() {
        assert_eq!(classify_signed_tx_kind(&[]), SignedTxKind::Legacy);
    }

    #[test]
    fn random_garbage_classifies_as_legacy() {
        let bytes = vec![0xFFu8; 512];
        assert_eq!(classify_signed_tx_kind(&bytes), SignedTxKind::Legacy);
    }

    #[test]
    fn truncated_v0_bytes_classify_as_legacy() {
        let mut bytes = make_v0_bytes();
        bytes.truncate(bytes.len() / 2);
        assert_eq!(classify_signed_tx_kind(&bytes), SignedTxKind::Legacy);
    }

    #[test]
    fn classifier_never_panics_on_adversarial_input() {
        // Run a few adversarial inputs; the function must never panic —
        // it's on the daemon's HTTP handler path and reachable by any
        // authenticated client.
        for input in [
            &[][..],
            &[0u8; 1],
            &[0u8; 2],
            &[0xFFu8; 10],
            &[0x01, 0x02, 0x03, 0x04],
            // Fake short-vec length prefix claiming 255 items then no payload:
            &[0xFF, 0xFF, 0xFF],
        ] {
            let _ = classify_signed_tx_kind(input);
        }
    }
}

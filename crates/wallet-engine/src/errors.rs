//! Wallet engine error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WalletError {
    #[error("key not found for pubkey: {pubkey}")]
    KeyNotFound { pubkey: String },

    #[error("signing failed: {0}")]
    SigningFailed(String),

    #[error("wallet is read-only: {pubkey}")]
    ReadOnly { pubkey: String },

    #[error("approval denied by operator")]
    ApprovalDenied,

    #[error("approval timed out")]
    ApprovalTimedOut,

    #[error("simulation required before signing, but simulation has not been run")]
    SimulationRequired,

    /// Simulation ran but reported failure. Transaction cannot proceed.
    #[error("simulation failed: {error}")]
    SimulationFailed { error: String },

    /// A policy rule blocked this transaction. Verdict is included for audit.
    #[error("policy blocked transaction: {}", verdict.label())]
    PolicyBlocked { verdict: claw_types::policy::PolicyVerdict },

    /// The canonical Solana `Message` bytes changed between approval and sign:
    /// the approval-time hash commitment no longer matches a fresh re-derivation.
    /// Fail-closed — the transaction is NOT signed. (Q10 hash-binding guard.)
    #[error("approval drift detected: expected {expected}, actual {actual}")]
    ApprovalTxDrift {
        /// Hex of the approval-time commitment over `Message::serialize()`.
        expected: String,
        /// Hex of the sign-time re-derivation over the message about to be signed.
        actual: String,
    },

    #[error("invalid keypair data: {0}")]
    InvalidKeypair(String),

    #[error("Solana error: {0}")]
    Solana(#[from] claw_solana_core::errors::SolanaError),

    #[error("serialization error: {0}")]
    Serialization(String),
}

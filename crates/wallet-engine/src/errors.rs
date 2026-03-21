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

    #[error("invalid keypair data: {0}")]
    InvalidKeypair(String),

    #[error("Solana error: {0}")]
    Solana(#[from] claw_solana_core::errors::SolanaError),

    #[error("serialization error: {0}")]
    Serialization(String),
}

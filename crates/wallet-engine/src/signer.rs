//! The `Signer` trait — the single abstraction over all signing backends.
//!
//! All code that needs a signature works through this trait. The concrete
//! type (local keypair, Ledger, external) is hidden behind a `SignerRef`.
//! This makes it trivial to swap signing backends or test with a mock signer.

use async_trait::async_trait;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Signature,
    transaction::Transaction,
};
use std::sync::Arc;

use crate::errors::WalletError;

/// The core signing abstraction.
#[async_trait]
pub trait Signer: Send + Sync {
    /// Returns the public key of this signer.
    fn pubkey(&self) -> Pubkey;

    /// Signs the given transaction in-place.
    /// The transaction's `recent_blockhash` must be set before calling this.
    async fn sign_transaction(&self, tx: &mut Transaction) -> Result<Signature, WalletError>;

    /// Returns a human-readable description of this signer (for audit logs).
    fn description(&self) -> String;

    /// Returns `true` if this signer can produce signatures automatically
    /// (without waiting for external input). Ledger and external signers
    /// return `false` here.
    fn is_automatic(&self) -> bool;
}

/// A reference-counted, heap-allocated signer.
pub type SignerRef = Arc<dyn Signer>;

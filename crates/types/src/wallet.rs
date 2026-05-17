//! Wallet and signer identity types.
//!
//! Note: Private key material is never stored here. This crate contains
//! only the identifier and metadata types. The actual key material lives
//! in `claw-wallet-engine` behind the `SecretKeystore`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A reference to a registered wallet — identifier and metadata only.
/// Never contains key material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletRef {
    /// The base58-encoded public key.
    pub pubkey: String,

    /// Operator-assigned human-readable label.
    pub label: Option<String>,

    /// The type of signer backing this wallet.
    pub signer_type: SignerType,

    /// Which network this wallet is registered for.
    pub network: crate::solana::SolanaNetwork,

    pub created_at: DateTime<Utc>,
    pub is_active: bool,
}

/// The type of signer, determining how signing requests are routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerType {
    /// A local keypair loaded from config/keystore at startup.
    /// Key material is held in a locked memory region.
    LocalKeypair,

    /// A Ledger hardware wallet connected via USB/HID.
    /// Key material never leaves the device.
    Ledger,

    /// An external signing service (e.g., a multisig program, Squads).
    /// Requires a callback mechanism.
    External,

    /// Read-only wallet — can inspect state but cannot sign.
    ReadOnly,
}

impl std::fmt::Display for SignerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerType::LocalKeypair => write!(f, "local_keypair"),
            SignerType::Ledger       => write!(f, "ledger"),
            SignerType::External     => write!(f, "external"),
            SignerType::ReadOnly     => write!(f, "read_only"),
        }
    }
}

/// Scoping constraint: which wallets a session is allowed to touch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalletScope {
    /// The session can interact with any registered wallet.
    Any,
    /// The session is restricted to specific pubkeys.
    Only(Vec<String>),
    /// The session cannot interact with any wallets (read-only capability only).
    None,
}

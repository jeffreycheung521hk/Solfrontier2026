//! Solana-domain types used across the system.
//!
//! Note: We deliberately keep Solana SDK types (Pubkey, Transaction, etc.)
//! out of `claw-types` to avoid making every crate depend on the Solana SDK.
//! Instead we use `String` for pubkeys at the boundary layer and decode them
//! in `claw-solana-core`. Only the network enum and normalized event types
//! live here.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The Solana network (cluster) this system is operating against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SolanaNetwork {
    MainnetBeta,
    Devnet,
    Testnet,
    Localnet,
}

impl SolanaNetwork {
    /// Returns the default public RPC URL for this cluster.
    /// These are fallbacks only; production configs should override them.
    pub fn default_rpc_url(&self) -> &'static str {
        match self {
            SolanaNetwork::MainnetBeta => "https://api.mainnet-beta.solana.com",
            SolanaNetwork::Devnet     => "https://api.devnet.solana.com",
            SolanaNetwork::Testnet    => "https://api.testnet.solana.com",
            SolanaNetwork::Localnet   => "http://127.0.0.1:8899",
        }
    }

    /// Returns `true` if this network carries real value.
    /// Used by the risk engine to enforce stricter policy on mainnet.
    pub fn is_mainnet(&self) -> bool {
        matches!(self, SolanaNetwork::MainnetBeta)
    }
}

impl std::fmt::Display for SolanaNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SolanaNetwork::MainnetBeta => write!(f, "mainnet-beta"),
            SolanaNetwork::Devnet     => write!(f, "devnet"),
            SolanaNetwork::Testnet    => write!(f, "testnet"),
            SolanaNetwork::Localnet   => write!(f, "localnet"),
        }
    }
}

/// Solana commitment levels, mirroring the RPC semantics.
/// Ordering: Processed < Confirmed < Finalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommitmentLevel {
    /// Fastest; node has processed the transaction but it may be rolled back.
    Processed,
    /// Supermajority of validators have confirmed.
    Confirmed,
    /// Maximum lockout; irreversible.
    Finalized,
}

impl Default for CommitmentLevel {
    /// Default to `Confirmed` for most reads — a balance between speed and safety.
    fn default() -> Self {
        CommitmentLevel::Confirmed
    }
}

/// Normalized Solana events emitted by the subscription manager.
/// These are produced from raw websocket frames and placed on the event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SolanaEvent {
    AccountChanged(AccountChangedEvent),
    LogEmitted(LogEmittedEvent),
    SlotAdvanced(SlotAdvancedEvent),
    SignatureConfirmed(SignatureConfirmedEvent),
    ProgramAccountChanged(ProgramAccountChangedEvent),
}

/// Fired when an account's data or lamport balance changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountChangedEvent {
    pub id:         Uuid,
    pub occurred_at: DateTime<Utc>,
    pub pubkey:     String,
    pub lamports:   u64,
    pub owner:      String,
    pub slot:       u64,
    pub data_b64:   Option<String>,  // base64-encoded account data, may be omitted for large accounts
}

/// Fired when a transaction log matching a subscription filter is seen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEmittedEvent {
    pub id:         Uuid,
    pub occurred_at: DateTime<Utc>,
    pub signature:  String,
    pub slot:       u64,
    pub logs:       Vec<String>,
    pub err:        Option<String>,
}

/// Fired on every slot advance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotAdvancedEvent {
    pub id:         Uuid,
    pub occurred_at: DateTime<Utc>,
    pub slot:       u64,
    pub parent:     u64,
    pub root:       u64,
}

/// Fired when a submitted transaction reaches a given commitment level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureConfirmedEvent {
    pub id:          Uuid,
    pub occurred_at: DateTime<Utc>,
    pub signature:   String,
    pub slot:        u64,
    pub err:         Option<String>,
    pub commitment:  CommitmentLevel,
}

/// Fired when a program account changes (from `programSubscribe`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramAccountChangedEvent {
    pub id:          Uuid,
    pub occurred_at: DateTime<Utc>,
    pub program_id:  String,
    pub pubkey:      String,
    pub lamports:    u64,
    pub slot:        u64,
    pub data_b64:    Option<String>,
}

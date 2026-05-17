//! Transaction lifecycle types.
//!
//! The transaction pipeline is the most security-sensitive code path.
//! Every stage is a distinct type to prevent accidental stage skipping.
//!
//! Pipeline:
//!   TransactionProposal
//!     → SimulatedTransaction  (simulation result attached)
//!       → PolicyCheckedTransaction  (policy verdict attached)
//!         → ApprovedTransaction  (human or auto approval)
//!           → SignedTransaction  (signature attached)
//!             → SentTransaction  (signature + slot)
//!               → ConfirmedTransaction  (commitment + slot)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::policy::PolicyVerdict;
use crate::session::SessionId;

/// A transaction proposal at the beginning of the pipeline.
/// At this stage the transaction has been built but not simulated or signed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionProposal {
    pub id:             Uuid,
    pub session_id:     SessionId,
    pub wallet_pubkey:  String,
    pub network:        crate::solana::SolanaNetwork,

    /// Human-readable description of intent (shown in approval request).
    pub description:    String,

    /// Base64-encoded serialized transaction (unsigned).
    pub transaction_b64: String,

    /// The instructions decoded to human-readable form, if available.
    #[serde(default)]
    pub instructions_summary: Vec<InstructionSummary>,

    pub created_at:     DateTime<Utc>,
}

/// Human-readable summary of a single instruction in the transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionSummary {
    pub program_id:  String,
    pub program_name: Option<String>,
    pub description: String,
    /// SOL transfer amount in lamports (System Program transfer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_lamports: Option<u64>,
    /// SPL Token transfer details (mint, amount, accounts) if this is a
    /// `TransferChecked` instruction. `None` for legacy `Transfer` because
    /// V1 does not resolve the mint via RPC lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_transfer: Option<TokenTransfer>,
    /// Set to true if this instruction is a legacy SPL Token `Transfer`
    /// (tag 3), which does NOT include the mint in its accounts. This flag
    /// exists so token-aware policies can block legacy transfers explicitly
    /// rather than silently letting them bypass mint/amount checks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_legacy_token_transfer: bool,
    pub accounts:    Vec<AccountRole>,
}

/// Decoded SPL Token transfer details.
///
/// Populated when an instruction is an SPL Token `Transfer` or `TransferChecked`.
/// The mint is only directly available for `TransferChecked`; for legacy
/// `Transfer` it must be looked up via the source account (V1 limitation:
/// we only decode `TransferChecked` and Transfer with mint pre-resolved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTransfer {
    /// The token mint address (e.g., USDC mint pubkey).
    pub mint: String,
    /// The raw token amount (in smallest units; for USDC, multiply by 10^-decimals).
    pub amount: u64,
    /// Token decimals if known (TransferChecked carries this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    /// Source token account.
    pub source: String,
    /// Destination token account.
    pub destination: String,
}

/// An account referenced by an instruction, with its role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRole {
    pub pubkey:     String,
    pub label:      Option<String>,
    pub is_signer:  bool,
    pub is_writable: bool,
}

/// The result of simulating a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResult {
    pub success:            bool,
    pub error:              Option<String>,
    pub compute_units_used: Option<u64>,
    pub logs:               Vec<String>,
    pub return_data:        Option<String>,
    /// Account state changes observed during simulation.
    pub account_diffs:      Vec<AccountDiff>,
    pub fee_lamports:       Option<u64>,
}

/// An observed account state change from simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountDiff {
    pub pubkey:          String,
    pub lamports_before: Option<u64>,
    pub lamports_after:  Option<u64>,
    pub data_changed:    bool,
}

/// The overall status of a transaction in the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    Proposed,
    Simulated,
    PolicyChecked,
    AwaitingApproval,
    /// Waiting for an external wallet (e.g., Phantom) to sign the transaction.
    AwaitingWalletSignature,
    Approved,
    Rejected,
    Signed,
    Sent,
    Confirmed,
    Finalized,
    Failed,
    Expired,
}

impl std::fmt::Display for TransactionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{:?}", self));
        write!(f, "{}", s)
    }
}

/// A persisted record tracking a transaction through its full lifecycle.
/// This is the authoritative audit record for any transaction the system
/// has ever touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub id:                Uuid,
    pub session_id:        SessionId,
    pub wallet_pubkey:     String,
    pub network:           crate::solana::SolanaNetwork,
    pub status:            TransactionStatus,
    pub description:       String,
    pub proposal:          TransactionProposal,
    pub simulation_result: Option<SimulationResult>,
    pub policy_verdict:    Option<PolicyVerdict>,
    /// The on-chain signature, present once the transaction is sent.
    pub signature:         Option<String>,
    pub created_at:        DateTime<Utc>,
    pub updated_at:        DateTime<Utc>,
}

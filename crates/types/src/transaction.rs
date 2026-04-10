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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_lamports: Option<u64>,
    pub accounts:    Vec<AccountRole>,
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

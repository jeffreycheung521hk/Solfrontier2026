use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

/// On-chain Intent record.
///
/// Stage 1 Tail: stores ONLY the canonical hash plus fixed-size envelope
/// fields. Structured action fields (amount, destination, slippage, route,
/// etc.) are NOT stored on-chain — they are committed to via
/// `canonical_intent_hash`.
///
/// Field order is fixed because Borsh serializes struct fields in declared
/// order (https://borsh.io). Any change here is a wire-shape break.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct IntentRecord {
    pub schema_version: u8,
    pub intent_id: [u8; 16],
    pub user: Pubkey,
    pub canonical_intent_hash: [u8; 32],
    pub action_type: u8,
    pub expires_at_slot: u64,
    pub created_at_slot: u64,
    pub bump: u8,
}

impl IntentRecord {
    /// Borsh-serialized length in bytes. The struct has no Vec/Option fields,
    /// so the length is fixed and equal to the sum of its primitives.
    pub const LEN: usize = 1   // schema_version
        + 16  // intent_id
        + 32  // user
        + 32  // canonical_intent_hash
        + 1   // action_type
        + 8   // expires_at_slot
        + 8   // created_at_slot
        + 1; // bump
}

/// Discriminators for the supported action types. Stored on-chain as a `u8`.
///
/// These match the values documented in the program spec:
/// `1 = SolendDeposit`, `2 = SolendWithdrawAll`, `3 = JupiterSwap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActionType {
    SolendDeposit = 1,
    SolendWithdrawAll = 2,
    JupiterSwap = 3,
}

impl ActionType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::SolendDeposit),
            2 => Some(Self::SolendWithdrawAll),
            3 => Some(Self::JupiterSwap),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

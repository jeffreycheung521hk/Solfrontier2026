use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};

use crate::derive_intent_pda;

/// Top-level program instruction set.
///
/// Borsh serializes enums with a `u8` ordinal followed by variant data, so the
/// numeric position of each variant here is part of the wire shape. Adding new
/// variants must always *append* — never reorder.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub enum IntentInstruction {
    /// Record a canonical-intent commitment for `user`.
    ///
    /// Account layout:
    ///   0. `[signer, writable]` user           — rent payer for the PDA
    ///   1. `[writable]`         intent_pda     — PDA created by this ix
    ///   2. `[]`                 system_program — for create_account CPI
    ///
    /// Tamper-evidence only — sibling instructions in the transaction are
    /// NOT inspected. Strong action-binding is Stage 2.
    RecordIntent {
        schema_version: u8,
        intent_id: [u8; 16],
        canonical_intent_hash: [u8; 32],
        action_type: u8,
        expires_at_slot: u64,
        canonical_intent_bytes: Vec<u8>,
    },
}

/// Helper for building a `RecordIntent` instruction off-chain (tx builders,
/// tests). Produces the same account ordering the on-chain processor expects.
#[allow(clippy::too_many_arguments)]
pub fn record_intent_instruction(
    program_id: &Pubkey,
    user: &Pubkey,
    schema_version: u8,
    intent_id: [u8; 16],
    canonical_intent_hash: [u8; 32],
    action_type: u8,
    expires_at_slot: u64,
    canonical_intent_bytes: Vec<u8>,
) -> Instruction {
    let (intent_pda, _bump) = derive_intent_pda(program_id, schema_version, user, &intent_id);
    let data = borsh::to_vec(&IntentInstruction::RecordIntent {
        schema_version,
        intent_id,
        canonical_intent_hash,
        action_type,
        expires_at_slot,
        canonical_intent_bytes,
    })
    .expect("borsh serialization of IntentInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(intent_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

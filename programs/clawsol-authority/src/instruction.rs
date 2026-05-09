//! Top-level instruction set for the Stage 2 Authorization PDA program.
//!
//! Borsh-serialized enums use a `u8` ordinal tag followed by the variant
//! body (https://borsh.io). The numeric position of each variant in this
//! enum is therefore part of the on-chain wire shape and MUST be
//! append-only — never reorder, never delete.
//!
//! P1 scope (this slice) covers only authorization-lifecycle ixs:
//! `CreateAuthorization`, `Revoke`, `CloseAuthorization`. The
//! `ExecuteAction` ix (Stage 2 spec § 5/6) is intentionally absent; a
//! placeholder enum slot is NOT reserved either, because Borsh's
//! variant ordinal is the next free integer when the variant is added,
//! so a future append cannot accidentally reuse a P1 slot.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};

use crate::derive_authorization_pda;

/// Instruction payload accepted by `process_instruction`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum AuthorityInstruction {
    /// Create a new authorization PDA.
    ///
    /// Account layout:
    ///   0. `[signer, writable]` user           — rent payer + signer
    ///   1. `[writable]`         authz_pda      — PDA created by this ix
    ///   2. `[]`                 system_program — for create_account CPI
    CreateAuthorization {
        schema_version: u8,
        rule_id: [u8; 16],
        executor: Pubkey,
        delegated_wallet: Pubkey,
        canonical_rule_hash: [u8; 32],
        allowed_action_type: u8,
        max_input_amount_raw: u64,
        destination: Pubkey,
        expires_at_slot: u64,
    },

    /// Revoke an existing authorization PDA. Idempotent — a second
    /// call on the same PDA succeeds without changing state.
    ///
    /// Account layout:
    ///   0. `[signer]`           user      — must equal authz.user
    ///   1. `[writable]`         authz_pda — existing PDA
    Revoke,

    /// Close a revoked / completed / expired authorization PDA, return
    /// the rent lamports to the user.
    ///
    /// Account layout:
    ///   0. `[signer, writable]` user      — must equal authz.user
    ///                                       (writable = rent destination)
    ///   1. `[writable]`         authz_pda — existing PDA
    CloseAuthorization,
}

/// Build a `CreateAuthorization` instruction for off-chain tx builders
/// and tests.
#[allow(clippy::too_many_arguments)]
pub fn create_authorization_instruction(
    program_id: &Pubkey,
    user: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
    executor: Pubkey,
    delegated_wallet: Pubkey,
    canonical_rule_hash: [u8; 32],
    allowed_action_type: u8,
    max_input_amount_raw: u64,
    destination: Pubkey,
    expires_at_slot: u64,
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::CreateAuthorization {
        schema_version,
        rule_id,
        executor,
        delegated_wallet,
        canonical_rule_hash,
        allowed_action_type,
        max_input_amount_raw,
        destination,
        expires_at_slot,
    })
    .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(authz_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

/// Build a `Revoke` instruction for off-chain tx builders and tests.
pub fn revoke_authorization_instruction(
    program_id: &Pubkey,
    user: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::Revoke)
        .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*user, true),
            AccountMeta::new(authz_pda, false),
        ],
        data,
    }
}

/// Build a `CloseAuthorization` instruction for off-chain tx builders
/// and tests.
pub fn close_authorization_instruction(
    program_id: &Pubkey,
    user: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::CloseAuthorization)
        .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(authz_pda, false),
        ],
        data,
    }
}

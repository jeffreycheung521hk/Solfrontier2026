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

    /// Execute the action authorised by an existing authorization PDA.
    ///
    /// P2 lands the boundary skeleton only — the processor performs every
    /// `AuthorizationRecord` check the spec requires (signer, owner, PDA
    /// derivation, schema_version, rule_id, user, executor, rule hash,
    /// action_type, expiry, revoked, completed, input cap, monotone
    /// nonce, same-slot replay) and applies the one-shot state mutation
    /// (`used_amount_raw += input_amount_raw`, `completed = true`,
    /// `execution_nonce = arg.execution_nonce`,
    /// `last_execution_slot = current_slot`). It does **not** yet:
    ///
    /// - inspect or evaluate the rule's conditions[..] (Stage 2 O3),
    /// - verify a sibling Solend `RefreshReserve` ix (Stage 2 P3),
    /// - verify Jupiter `swap` sibling ix via the instructions sysvar
    ///   (Stage 2 P4),
    /// - take a token-balance pre/post bracket,
    /// - sweep funds (Stage 2 spec § 8.1 steps 2–3 — handled by Revoke
    ///   in a future slice).
    ///
    /// Variant order MUST stay append-only: this is the 4th variant
    /// (Borsh `u8` ordinal `3`) and a future-added variant goes after,
    /// never inserted before.
    ///
    /// Account layout:
    ///   0. `[signer]`           executor          — must equal authz.executor
    ///   1. `[writable]`         authz_pda         — existing PDA
    ///   2. `[]`                 user              — must equal authz.user (readonly)
    ///   3. `[]`                 delegated_wallet  — readonly placeholder (P3+ wires CPI)
    ExecuteAction {
        schema_version: u8,
        rule_id: [u8; 16],
        canonical_rule_hash: [u8; 32],
        action_type: u8,
        input_amount_raw: u64,
        execution_nonce: u64,
    },
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

/// Build an `ExecuteAction` instruction for off-chain tx builders and
/// tests. The account list is pinned to the same order the on-chain
/// processor reads (executor, authz_pda, user, delegated_wallet); P3+
/// will append Solend / Jupiter accounts after `delegated_wallet`.
#[allow(clippy::too_many_arguments)]
pub fn execute_action_instruction(
    program_id: &Pubkey,
    executor: &Pubkey,
    user: &Pubkey,
    delegated_wallet: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
    canonical_rule_hash: [u8; 32],
    action_type: u8,
    input_amount_raw: u64,
    execution_nonce: u64,
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::ExecuteAction {
        schema_version,
        rule_id,
        canonical_rule_hash,
        action_type,
        input_amount_raw,
        execution_nonce,
    })
    .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*executor, true),
            AccountMeta::new(authz_pda, false),
            AccountMeta::new_readonly(*user, false),
            AccountMeta::new_readonly(*delegated_wallet, false),
        ],
        data,
    }
}

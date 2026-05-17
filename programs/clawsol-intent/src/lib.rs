//! ClawSolana canonical intent recording program (Stage 1 Tail).
//!
//! # Security framing
//!
//! This program is **tamper-evident / forensic only**. It records that a user
//! signed a transaction whose canonical intent serialization hashes to a
//! specific `canonical_intent_hash`. It does **not**:
//!
//! - inspect or constrain sibling instructions in the transaction,
//! - prove the action instruction bytes correspond to the recorded hash,
//! - enforce conditions, escrow, or autonomous execution.
//!
//! Strong action-binding (Stage 2) is a follow-on slice that will introduce
//! an Authorization PDA and an action-bytes commitment.
//!
//! # Expected weak-binding usage
//!
//! Off-chain tx builders are expected to construct transactions of the form:
//!
//! - `ix[0]`: `record_intent`
//! - `ix[1..]`: action instruction(s) (Solend / Jupiter / etc.)
//!
//! This program records `ix[0]` only.
//!
//! # Sources consulted
//!
//! - solana-program 2.1.21 entrypoint signature
//!   (https://docs.rs/solana-program-entrypoint/2.1.21/solana_program_entrypoint/macro.entrypoint.html)
//! - PDA constraints `MAX_SEEDS = 16`, `MAX_SEED_LEN = 32`
//!   (https://raw.githubusercontent.com/anza-xyz/agave/v2.1.21/sdk/pubkey/src/lib.rs)
//! - `system_instruction::create_account` AccountMetas (both signer+writable;
//!   to_pubkey signature provided via invoke_signed PDA seeds)
//!   (https://docs.rs/solana-program/2.1.21/solana_program/system_instruction/fn.create_account.html)
//! - `Clock::get()` via the `Sysvar` trait (no sysvar account required)
//!   (https://docs.rs/solana-program/2.1.21/solana_program/sysvar/clock/struct.Clock.html)
//! - `solana_program::hash::hashv` is SHA-256, returns 32-byte `Hash`
//!   (https://docs.rs/solana-program/2.1.21/solana_program/hash/index.html)
//! - Borsh 1.x: structs serialize in declared field order; `Vec<T>` uses
//!   `u32` little-endian length prefix; `[u8; N]` has no length prefix;
//!   integers are little-endian; enums use a `u8` ordinal tag
//!   (https://borsh.io)

#![allow(unexpected_cfgs)]

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    hash::hashv,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::Sysvar,
};

pub mod error;
pub mod instruction;
pub mod state;

use crate::error::IntentError;
use crate::instruction::IntentInstruction;
use crate::state::{ActionType, IntentRecord};

/// PDA seed prefix. Combined with `[schema_version]`, `user.as_ref()`, and
/// `intent_id` to derive the canonical Intent PDA.
pub const INTENT_SEED_PREFIX: &[u8] = b"intent";

/// Sanity bound for `canonical_intent_bytes`. The Solana legacy transaction
/// size limit is 1232 bytes, so this is well above what could ever fit
/// alongside the rest of the instruction payload — but we still cap to keep
/// the hash recomputation cost predictable on-chain.
pub const MAX_CANONICAL_INTENT_BYTES: usize = 1024;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// On-chain entrypoint shim. Dispatches to the per-variant processor.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let instruction = IntentInstruction::try_from_slice(instruction_data)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    match instruction {
        IntentInstruction::RecordIntent {
            schema_version,
            intent_id,
            canonical_intent_hash,
            action_type,
            expires_at_slot,
            canonical_intent_bytes,
        } => process_record_intent(
            program_id,
            accounts,
            schema_version,
            intent_id,
            canonical_intent_hash,
            action_type,
            expires_at_slot,
            canonical_intent_bytes,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn process_record_intent(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    schema_version: u8,
    intent_id: [u8; 16],
    canonical_intent_hash: [u8; 32],
    action_type: u8,
    expires_at_slot: u64,
    canonical_intent_bytes: Vec<u8>,
) -> ProgramResult {
    if canonical_intent_bytes.len() > MAX_CANONICAL_INTENT_BYTES {
        return Err(IntentError::CanonicalIntentBytesTooLarge.into());
    }

    // Reject unknown discriminators before doing any other work — we never
    // want a PDA created for an action_type we cannot interpret later.
    if ActionType::from_u8(action_type).is_none() {
        return Err(IntentError::InvalidActionType.into());
    }

    let account_iter = &mut accounts.iter();
    let user_account = next_account_info(account_iter)?;
    let intent_pda_account = next_account_info(account_iter)?;
    let system_program_account = next_account_info(account_iter)?;

    if !user_account.is_signer {
        return Err(IntentError::UserMustSign.into());
    }

    if system_program_account.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Recompute SHA-256 over canonical_intent_bytes and require equality.
    // solana_program::hash::hashv is SHA-256 and returns a 32-byte Hash.
    let computed = hashv(&[&canonical_intent_bytes]);
    if computed.to_bytes() != canonical_intent_hash {
        return Err(IntentError::HashMismatch.into());
    }

    // Read Clock and require expiry strictly in the future. `Clock::get()`
    // works without passing the sysvar account because Clock impls Sysvar.
    let clock = Clock::get()?;
    if clock.slot >= expires_at_slot {
        return Err(IntentError::IntentExpired.into());
    }

    // Derive PDA and verify the caller passed the canonical address.
    let schema_version_bytes = [schema_version];
    let seeds: &[&[u8]] = &[
        INTENT_SEED_PREFIX,
        &schema_version_bytes,
        user_account.key.as_ref(),
        intent_id.as_ref(),
    ];
    let (expected_pda, bump) = Pubkey::find_program_address(seeds, program_id);
    if expected_pda != *intent_pda_account.key {
        return Err(IntentError::InvalidPda.into());
    }

    // Allocate + assign the PDA via system_program. If the PDA already exists
    // (duplicate intent_id/user/schema), create_account fails with
    // SystemError::AccountAlreadyInUse — i.e., duplicates are rejected by
    // the runtime, not by us.
    let space = IntentRecord::LEN as u64;
    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(IntentRecord::LEN);

    let create_ix = system_instruction::create_account(
        user_account.key,
        intent_pda_account.key,
        lamports,
        space,
        program_id,
    );

    let bump_arr = [bump];
    let signer_seeds: &[&[u8]] = &[
        INTENT_SEED_PREFIX,
        &schema_version_bytes,
        user_account.key.as_ref(),
        intent_id.as_ref(),
        &bump_arr,
    ];

    invoke_signed(
        &create_ix,
        &[
            user_account.clone(),
            intent_pda_account.clone(),
            system_program_account.clone(),
        ],
        &[signer_seeds],
    )?;

    let record = IntentRecord {
        schema_version,
        intent_id,
        user: *user_account.key,
        canonical_intent_hash,
        action_type,
        expires_at_slot,
        created_at_slot: clock.slot,
        bump,
    };

    record
        .serialize(&mut &mut intent_pda_account.data.borrow_mut()[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    // Concise event. sol_log_data emits a base64-encoded structured log line
    // that off-chain indexers can parse without scraping the human-readable
    // msg!() output.
    solana_program::log::sol_log_data(&[
        b"intent_recorded_v1",
        &intent_id,
        user_account.key.as_ref(),
        core::slice::from_ref(&action_type),
        &canonical_intent_hash,
        &expires_at_slot.to_le_bytes(),
        &clock.slot.to_le_bytes(),
    ]);
    msg!(
        "record_intent ok action_type={} expires_at_slot={} created_at_slot={}",
        action_type,
        expires_at_slot,
        clock.slot
    );

    Ok(())
}

/// Pure, deterministic PDA derivation for off-chain tx builders and tests.
///
/// Seeds: `["intent", [schema_version], user.as_ref(), intent_id]`.
pub fn derive_intent_pda(
    program_id: &Pubkey,
    schema_version: u8,
    user: &Pubkey,
    intent_id: &[u8; 16],
) -> (Pubkey, u8) {
    let schema_version_bytes = [schema_version];
    let seeds: &[&[u8]] = &[
        INTENT_SEED_PREFIX,
        &schema_version_bytes,
        user.as_ref(),
        intent_id.as_ref(),
    ];
    Pubkey::find_program_address(seeds, program_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pda_seeds_are_deterministic() {
        let program_id = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let intent_id = [7u8; 16];
        let a = derive_intent_pda(&program_id, 1, &user, &intent_id);
        let b = derive_intent_pda(&program_id, 1, &user, &intent_id);
        assert_eq!(a, b, "derive_intent_pda must be deterministic");
    }

    #[test]
    fn pda_changes_with_schema_version() {
        let program_id = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let intent_id = [7u8; 16];
        let (pda_v1, _) = derive_intent_pda(&program_id, 1, &user, &intent_id);
        let (pda_v2, _) = derive_intent_pda(&program_id, 2, &user, &intent_id);
        assert_ne!(pda_v1, pda_v2, "schema_version must domain-separate PDAs");
    }

    #[test]
    fn pda_changes_with_intent_id() {
        let program_id = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let (pda_a, _) = derive_intent_pda(&program_id, 1, &user, &[1u8; 16]);
        let (pda_b, _) = derive_intent_pda(&program_id, 1, &user, &[2u8; 16]);
        assert_ne!(pda_a, pda_b, "intent_id must domain-separate PDAs");
    }

    #[test]
    fn pda_changes_with_user() {
        let program_id = Pubkey::new_unique();
        let user_a = Pubkey::new_unique();
        let user_b = Pubkey::new_unique();
        let intent_id = [7u8; 16];
        let (pda_a, _) = derive_intent_pda(&program_id, 1, &user_a, &intent_id);
        let (pda_b, _) = derive_intent_pda(&program_id, 1, &user_b, &intent_id);
        assert_ne!(pda_a, pda_b, "user must domain-separate PDAs");
    }

    #[test]
    fn intent_record_len_matches_borsh_serialization() {
        let record = IntentRecord {
            schema_version: 1,
            intent_id: [0xAA; 16],
            user: Pubkey::new_unique(),
            canonical_intent_hash: [0xBB; 32],
            action_type: 3,
            expires_at_slot: 123_456_789,
            created_at_slot: 42,
            bump: 255,
        };
        let bytes = borsh::to_vec(&record).unwrap();
        assert_eq!(
            bytes.len(),
            IntentRecord::LEN,
            "IntentRecord::LEN ({}) must equal serialized size ({})",
            IntentRecord::LEN,
            bytes.len()
        );
        assert_eq!(IntentRecord::LEN, 99, "fixed-size record is 99 bytes");
    }

    #[test]
    fn intent_record_borsh_roundtrip_preserves_fields() {
        let original = IntentRecord {
            schema_version: 7,
            intent_id: [0x01; 16],
            user: Pubkey::new_unique(),
            canonical_intent_hash: [0x02; 32],
            action_type: 2,
            expires_at_slot: u64::MAX - 1,
            created_at_slot: 1,
            bump: 254,
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = IntentRecord::try_from_slice(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn intent_record_layout_matches_borsh_field_order() {
        // Borsh serializes structs in declared field order, integers
        // little-endian, fixed arrays without prefix. We anchor that to
        // explicit byte offsets so any future field reorder is loud.
        let user = Pubkey::new_from_array([0x33; 32]);
        let record = IntentRecord {
            schema_version: 0x12,
            intent_id: [0x44; 16],
            user,
            canonical_intent_hash: [0x55; 32],
            action_type: 0x66,
            expires_at_slot: 0x0102_0304_0506_0708,
            created_at_slot: 0x1112_1314_1516_1718,
            bump: 0xFE,
        };
        let bytes = borsh::to_vec(&record).unwrap();
        assert_eq!(bytes[0], 0x12, "byte 0 = schema_version");
        assert_eq!(&bytes[1..17], &[0x44u8; 16][..], "bytes 1..17 = intent_id");
        assert_eq!(&bytes[17..49], &[0x33u8; 32][..], "bytes 17..49 = user");
        assert_eq!(
            &bytes[49..81],
            &[0x55u8; 32][..],
            "bytes 49..81 = canonical_intent_hash"
        );
        assert_eq!(bytes[81], 0x66, "byte 81 = action_type");
        // little-endian u64
        assert_eq!(
            &bytes[82..90],
            &0x0102_0304_0506_0708u64.to_le_bytes()[..],
            "bytes 82..90 = expires_at_slot (LE u64)"
        );
        assert_eq!(
            &bytes[90..98],
            &0x1112_1314_1516_1718u64.to_le_bytes()[..],
            "bytes 90..98 = created_at_slot (LE u64)"
        );
        assert_eq!(bytes[98], 0xFE, "byte 98 = bump");
        assert_eq!(bytes.len(), 99);
    }

    #[test]
    fn action_type_discriminators_match_spec() {
        assert_eq!(ActionType::SolendDeposit.to_u8(), 1);
        assert_eq!(ActionType::SolendWithdrawAll.to_u8(), 2);
        assert_eq!(ActionType::JupiterSwap.to_u8(), 3);
        assert_eq!(ActionType::from_u8(1), Some(ActionType::SolendDeposit));
        assert_eq!(ActionType::from_u8(2), Some(ActionType::SolendWithdrawAll));
        assert_eq!(ActionType::from_u8(3), Some(ActionType::JupiterSwap));
        assert_eq!(ActionType::from_u8(0), None);
        assert_eq!(ActionType::from_u8(4), None);
        assert_eq!(ActionType::from_u8(255), None);
    }

    #[test]
    fn intent_record_does_not_carry_action_payload_fields() {
        // Compile-time-style assertion via field iteration: we can't reflect
        // in stable Rust, so we assert the structural property indirectly by
        // pinning the byte length. Anything that adds amount/destination/
        // slippage would inflate LEN past 99 and break this test.
        assert_eq!(
            IntentRecord::LEN,
            99,
            "IntentRecord must not store structured action fields"
        );
    }

    #[test]
    fn intent_instruction_borsh_roundtrip() {
        let original = IntentInstruction::RecordIntent {
            schema_version: 1,
            intent_id: [9u8; 16],
            canonical_intent_hash: [8u8; 32],
            action_type: 1,
            expires_at_slot: 1_000_000,
            canonical_intent_bytes: b"placeholder".to_vec(),
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = IntentInstruction::try_from_slice(&bytes).unwrap();
        assert_eq!(original, decoded);
    }
}

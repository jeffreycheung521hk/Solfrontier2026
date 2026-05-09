//! ClawSolana Stage 2 Authorization PDA program — P1 skeleton.
//!
//! # What this slice is
//!
//! Substrate only. P1 lands the on-chain `AuthorizationRecord`
//! plus the lifecycle ixs that bring it into and out of existence
//! (`CreateAuthorization`, `Revoke`, `CloseAuthorization`). It does
//! **not** include `ExecuteAction` — execution is the next slice and
//! requires:
//!
//! - the conditions evaluator (Pyth + Solend on-chain re-verifiers
//!   per Stage 2 spec § 7),
//! - the same-tx pre/post bracket and instructions-sysvar sibling-ix
//!   verification for Jupiter (spec § 6.1),
//! - the Solend variant-15 `Refresh + WithdrawObligationCollateralAnd
//!   RedeemReserveCollateral` orchestration (spec § 5.2),
//! - the funds custody / sweep model (spec § 8 / § 11).
//!
//! Adding any of those in P1 would force decisions we want to make
//! against fresh research, not buried inside a skeleton commit.
//!
//! # What's deferred to later slices (explicit TODO list)
//!
//! - `ExecuteAction` instruction + processor — gated on the
//!   instructions-sysvar work and Solend variant-15 account ordering.
//! - Token sweep on revoke (spec § 8.1 step 2/3) — gated on the funds
//!   custody decision (PDA-as-authority vs delegated-wallet-signs).
//!   `Revoke` here flips the `revoked` flag only; tokens stay where
//!   they are.
//! - Replay-protection state machine on `execute_action` —
//!   `execution_nonce`, `last_execution_slot`, and `completed` are
//!   present in the record and zero-initialised; no ix in P1 mutates
//!   them.
//! - Multi-use rules — `one_shot` is implicit in P1 (single execute,
//!   completed=true, AlreadyCompleted on second attempt). The PDA has
//!   the fields a future multi-use slice would need, but no logic
//!   uses them yet.
//!
//! # Stage isolation
//!
//! This program does **not** touch Stage 1's `clawsol-intent`
//! program. It does not share PDA seeds, account layouts, or
//! canonical-hash domains. A Stage 1 record_intent intent_id and a
//! Stage 2 rule_id collide-free as a matter of construction (different
//! seed prefixes: `b"intent"` vs `b"authz"`, plus separate program
//! IDs at deployment).
//!
//! # Sources consulted
//!
//! - solana-program 2.1.21 entrypoint
//!   (https://docs.rs/solana-program-entrypoint/2.1.21/solana_program_entrypoint/macro.entrypoint.html)
//! - PDA constraints `MAX_SEEDS = 16`, `MAX_SEED_LEN = 32`
//!   (https://raw.githubusercontent.com/anza-xyz/agave/v2.1.21/sdk/pubkey/src/lib.rs)
//! - `system_instruction::create_account`
//!   (https://docs.rs/solana-program/2.1.21/solana_program/system_instruction/fn.create_account.html)
//! - `Clock::get()` via the `Sysvar` trait (no sysvar account required)
//!   (https://docs.rs/solana-program/2.1.21/solana_program/sysvar/clock/struct.Clock.html)
//! - Account close pattern (assign back to system program, realloc to 0,
//!   transfer lamports — standard SPL idiom)
//!   (https://docs.rs/solana-program/2.1.21/solana_program/account_info/struct.AccountInfo.html)
//! - Borsh 1.x layout: declared field order, u32-LE length prefix on
//!   Vec, u8 enum ordinal, fixed `[u8; N]` raw, integers little-endian
//!   (https://borsh.io)
//! - Stage 2 Delegated Conditional Execution Spec
//!   (`docs/STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md`,
//!   §§ 3, 4, 5, 8)
//! - Stage 2 Official Docs Audit
//!   (`docs/STAGE2_OFFICIAL_DOCS_AUDIT.md`, audit-corrections record)

#![allow(unexpected_cfgs)]

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
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

use crate::error::AuthorityError;
use crate::instruction::AuthorityInstruction;
use crate::state::{
    AuthorizationRecord, Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION,
};

/// PDA seed prefix. Combined with `[schema_version]`, `user.as_ref()`,
/// and `rule_id` to derive the canonical Authorization PDA. The
/// prefix domain-separates Stage 2 PDAs from any other program in the
/// same address space (and from Stage 1's `b"intent"` prefix).
pub const AUTHZ_SEED_PREFIX: &[u8] = b"authz";

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// On-chain entrypoint. Dispatches to per-variant processors.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let instruction = AuthorityInstruction::try_from_slice(instruction_data)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    match instruction {
        AuthorityInstruction::CreateAuthorization {
            schema_version,
            rule_id,
            executor,
            delegated_wallet,
            canonical_rule_hash,
            allowed_action_type,
            max_input_amount_raw,
            destination,
            expires_at_slot,
        } => process_create_authorization(
            program_id,
            accounts,
            schema_version,
            rule_id,
            executor,
            delegated_wallet,
            canonical_rule_hash,
            allowed_action_type,
            max_input_amount_raw,
            destination,
            expires_at_slot,
        ),
        AuthorityInstruction::Revoke => process_revoke(program_id, accounts),
        AuthorityInstruction::CloseAuthorization => {
            process_close_authorization(program_id, accounts)
        }
        AuthorityInstruction::ExecuteAction {
            schema_version,
            rule_id,
            canonical_rule_hash,
            action_type,
            input_amount_raw,
            execution_nonce,
        } => process_execute_action(
            program_id,
            accounts,
            schema_version,
            rule_id,
            canonical_rule_hash,
            action_type,
            input_amount_raw,
            execution_nonce,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn process_create_authorization(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    schema_version: u8,
    rule_id: [u8; 16],
    executor: Pubkey,
    delegated_wallet: Pubkey,
    canonical_rule_hash: [u8; 32],
    allowed_action_type: u8,
    max_input_amount_raw: u64,
    destination: Pubkey,
    expires_at_slot: u64,
) -> ProgramResult {
    // Cheap structural checks first — a wrong schema_version or a
    // zero amount cap can never produce a usable authorization, so
    // reject before consuming any compute on the PDA derive / Clock /
    // account creation path.
    if schema_version != STAGE2_AUTHORITY_SCHEMA_VERSION {
        return Err(AuthorityError::UnsupportedSchemaVersion.into());
    }
    if Stage2ActionType::from_u8(allowed_action_type).is_none() {
        return Err(AuthorityError::InvalidActionType.into());
    }
    if max_input_amount_raw == 0 {
        return Err(AuthorityError::InvalidZeroMaxAmount.into());
    }

    let account_iter = &mut accounts.iter();
    let user_account = next_account_info(account_iter)?;
    let authz_pda_account = next_account_info(account_iter)?;
    let system_program_account = next_account_info(account_iter)?;

    if !user_account.is_signer {
        return Err(AuthorityError::UserMustSign.into());
    }
    if system_program_account.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Expiry must be strictly in the future. Clock::get() works without
    // passing the sysvar account because Clock impls Sysvar.
    let clock = Clock::get()?;
    if clock.slot >= expires_at_slot {
        return Err(AuthorityError::AuthorizationExpired.into());
    }

    // Derive PDA from canonical seeds and require the caller passed
    // the matching address.
    let schema_version_bytes = [schema_version];
    let seeds: &[&[u8]] = &[
        AUTHZ_SEED_PREFIX,
        &schema_version_bytes,
        user_account.key.as_ref(),
        rule_id.as_ref(),
    ];
    let (expected_pda, bump) = Pubkey::find_program_address(seeds, program_id);
    if expected_pda != *authz_pda_account.key {
        return Err(AuthorityError::InvalidPda.into());
    }

    // Allocate via system_program::create_account. If the PDA already
    // has lamports / data, create_account fails with
    // SystemError::AccountAlreadyInUse — the runtime rejects duplicate
    // (user, rule_id, schema_version) triples for us, no extra check
    // needed.
    let space = AuthorizationRecord::LEN as u64;
    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(AuthorizationRecord::LEN);

    let create_ix = system_instruction::create_account(
        user_account.key,
        authz_pda_account.key,
        lamports,
        space,
        program_id,
    );

    let bump_arr = [bump];
    let signer_seeds: &[&[u8]] = &[
        AUTHZ_SEED_PREFIX,
        &schema_version_bytes,
        user_account.key.as_ref(),
        rule_id.as_ref(),
        &bump_arr,
    ];

    invoke_signed(
        &create_ix,
        &[
            user_account.clone(),
            authz_pda_account.clone(),
            system_program_account.clone(),
        ],
        &[signer_seeds],
    )?;

    let record = AuthorizationRecord {
        schema_version,
        rule_id,
        user: *user_account.key,
        executor,
        delegated_wallet,
        canonical_rule_hash,
        allowed_action_type,
        max_input_amount_raw,
        used_amount_raw: 0,
        destination,
        expires_at_slot,
        revoked: false,
        completed: false,
        execution_nonce: 0,
        last_execution_slot: 0,
        bump,
    };

    record
        .serialize(&mut &mut authz_pda_account.data.borrow_mut()[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    // Structured event for off-chain indexers (matches the shape of
    // Stage 1's intent_recorded_v1 emission style).
    solana_program::log::sol_log_data(&[
        b"authz_created_v1",
        &rule_id,
        user_account.key.as_ref(),
        executor.as_ref(),
        delegated_wallet.as_ref(),
        core::slice::from_ref(&allowed_action_type),
        &canonical_rule_hash,
        &max_input_amount_raw.to_le_bytes(),
        &expires_at_slot.to_le_bytes(),
        &clock.slot.to_le_bytes(),
    ]);
    msg!(
        "create_authorization ok action_type={} max_input_amount_raw={} expires_at_slot={} created_at_slot={}",
        allowed_action_type,
        max_input_amount_raw,
        expires_at_slot,
        clock.slot
    );

    Ok(())
}

fn process_revoke(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let user_account = next_account_info(account_iter)?;
    let authz_pda_account = next_account_info(account_iter)?;

    if !user_account.is_signer {
        return Err(AuthorityError::UserMustSign.into());
    }
    if authz_pda_account.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let mut record = {
        let data = authz_pda_account.data.borrow();
        AuthorizationRecord::try_from_slice(&data)
            .map_err(|_| ProgramError::InvalidAccountData)?
    };

    // Authority gate: the user pubkey baked into the record (set at
    // creation) is the only key that can revoke. Cross-PDA replay
    // attempts (passing a different user's PDA) fail here.
    if record.user != *user_account.key {
        return Err(AuthorityError::RevokeWrongUser.into());
    }

    // Defence in depth: even though owner == program_id and
    // record.user matches the signer, also re-derive the PDA from the
    // record's own fields and confirm the address matches. Closes the
    // gap where a malicious previous-program-version PDA was repointed
    // by a hypothetical key-recovery exploit; cheap to keep, loud to
    // remove.
    let schema_version_bytes = [record.schema_version];
    let seeds: &[&[u8]] = &[
        AUTHZ_SEED_PREFIX,
        &schema_version_bytes,
        record.user.as_ref(),
        record.rule_id.as_ref(),
    ];
    let (expected_pda, _bump) = Pubkey::find_program_address(seeds, program_id);
    if expected_pda != *authz_pda_account.key {
        return Err(AuthorityError::InvalidPda.into());
    }

    if record.revoked {
        // Idempotent: a second revoke against the same PDA is a no-op
        // (no state change, no fail). This is the daemon-friendly
        // choice for retry — see the design note above on revoke
        // semantics.
        msg!("revoke_authorization noop already_revoked=true");
        return Ok(());
    }

    record.revoked = true;
    record
        .serialize(&mut &mut authz_pda_account.data.borrow_mut()[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    // TODO(Stage 2 spec § 8.1 steps 2–3): when the funds custody
    // decision lands (PDA-as-authority vs delegated-wallet-signs),
    // revoke must additionally sweep the delegated USDC ATA back to
    // the user main wallet and (per `revoke_policy.close_token_accounts`)
    // close empty token accounts. P1 only flips the flag.

    let clock = Clock::get()?;
    solana_program::log::sol_log_data(&[
        b"authz_revoked_v1",
        &record.rule_id,
        record.user.as_ref(),
        &clock.slot.to_le_bytes(),
    ]);
    msg!(
        "revoke_authorization ok rule_id_first_byte={} revoked_at_slot={}",
        record.rule_id[0],
        clock.slot
    );

    Ok(())
}

fn process_close_authorization(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let user_account = next_account_info(account_iter)?;
    let authz_pda_account = next_account_info(account_iter)?;

    if !user_account.is_signer {
        return Err(AuthorityError::UserMustSign.into());
    }
    if authz_pda_account.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let record = {
        let data = authz_pda_account.data.borrow();
        AuthorizationRecord::try_from_slice(&data)
            .map_err(|_| ProgramError::InvalidAccountData)?
    };
    if record.user != *user_account.key {
        return Err(AuthorityError::CloseWrongUser.into());
    }

    let clock = Clock::get()?;
    let is_expired = clock.slot >= record.expires_at_slot;
    if !record.revoked && !record.completed && !is_expired {
        return Err(AuthorityError::NotCloseable.into());
    }

    // Defence-in-depth PDA address re-derivation, same rationale as
    // process_revoke.
    let schema_version_bytes = [record.schema_version];
    let seeds: &[&[u8]] = &[
        AUTHZ_SEED_PREFIX,
        &schema_version_bytes,
        record.user.as_ref(),
        record.rule_id.as_ref(),
    ];
    let (expected_pda, _bump) = Pubkey::find_program_address(seeds, program_id);
    if expected_pda != *authz_pda_account.key {
        return Err(AuthorityError::InvalidPda.into());
    }

    // Standard close idiom: transfer all lamports to the user, then
    // reassign the data buffer to the system program and shrink to
    // zero so the account is fully reclaimable.
    let pda_lamports = authz_pda_account.lamports();
    let new_user_lamports = user_account
        .lamports()
        .checked_add(pda_lamports)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    **user_account.try_borrow_mut_lamports()? = new_user_lamports;
    **authz_pda_account.try_borrow_mut_lamports()? = 0;

    authz_pda_account.assign(&system_program::ID);
    authz_pda_account
        .realloc(0, false)
        .map_err(|_| ProgramError::InvalidAccountData)?;

    solana_program::log::sol_log_data(&[
        b"authz_closed_v1",
        &record.rule_id,
        record.user.as_ref(),
        &clock.slot.to_le_bytes(),
    ]);
    msg!(
        "close_authorization ok rule_id_first_byte={} closed_at_slot={}",
        record.rule_id[0],
        clock.slot
    );

    Ok(())
}

/// Execute the action authorised by an existing authorization PDA
/// (Stage 2 P2 boundary skeleton).
///
/// All `AuthorizationRecord` checks the spec demands run here. The
/// processor performs **no** CPI — Solend variant-15 and Jupiter
/// sibling-ix verification are deferred to P3 / P4 (see TODOs). The
/// happy-path mutation is the one-shot v1 set: `used_amount_raw +=
/// input_amount_raw`, `completed = true`, `execution_nonce` advances
/// by one, `last_execution_slot = clock.slot`.
///
/// Accounts (in order):
///   0. `[signer]`   executor          — must equal record.executor
///   1. `[writable]` authorization_pda — owned by this program
///   2. `[]`         user              — must equal record.user
///   3. `[]`         delegated_wallet  — readonly placeholder for P3+
#[allow(clippy::too_many_arguments)]
fn process_execute_action(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    schema_version: u8,
    rule_id: [u8; 16],
    canonical_rule_hash: [u8; 32],
    action_type: u8,
    input_amount_raw: u64,
    execution_nonce: u64,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let executor_account = next_account_info(account_iter)?;
    let authz_pda_account = next_account_info(account_iter)?;
    let user_account = next_account_info(account_iter)?;
    let _delegated_wallet_account = next_account_info(account_iter)?;
    // TODO(Stage 2 P3): Solend same-tx refresh + variant 15
    // TODO(Stage 2 P4): Jupiter sibling-ix verification via instructions sysvar
    // TODO(Stage 2): token balance pre/post bracket assertion

    // ── Boundary checks (fail-closed, in spec order) ────────────────────

    // 1. Executor must sign.
    if !executor_account.is_signer {
        return Err(AuthorityError::MissingExecutorSignature.into());
    }

    // 2. PDA must be owned by this program. We check this BEFORE
    //    deserializing record bytes so a foreign-owned account with
    //    look-alike data can never advance.
    if authz_pda_account.owner != program_id {
        return Err(AuthorityError::AuthorizationOwnerMismatch.into());
    }

    // 3. PDA derivation: address must match seeds.
    //    Seeds: [b"authz", [schema_version], user.as_ref(), rule_id]
    //    — identical to create / revoke / close, keyed off the *arg*
    //    schema_version + rule_id and the *passed* user account so the
    //    record body checks below catch any shuffling of which user
    //    account was supplied.
    let schema_version_bytes = [schema_version];
    let seeds: &[&[u8]] = &[
        AUTHZ_SEED_PREFIX,
        &schema_version_bytes,
        user_account.key.as_ref(),
        rule_id.as_ref(),
    ];
    let (expected_pda, _bump) = Pubkey::find_program_address(seeds, program_id);
    if expected_pda != *authz_pda_account.key {
        return Err(AuthorityError::InvalidAuthorizationPda.into());
    }

    // Decode the record. From here on, every check compares the on-chain
    // record (the user's signed contract) against the caller's args.
    let mut record = {
        let data = authz_pda_account.data.borrow();
        AuthorizationRecord::try_from_slice(&data)
            .map_err(|_| ProgramError::InvalidAccountData)?
    };

    // 4. Schema version: arg vs record. (The PDA derivation above also
    //    keys on schema_version, so a mismatch here would also fail (3),
    //    but the explicit check produces a precise error code.)
    if record.schema_version != schema_version {
        return Err(AuthorityError::UnsupportedSchemaVersion.into());
    }

    // 5. Rule id: arg vs record. Same belt-and-braces argument as (4).
    if record.rule_id != rule_id {
        return Err(AuthorityError::InvalidAuthorizationPda.into());
    }

    // 6. User account: must match record.user.
    if record.user != *user_account.key {
        return Err(AuthorityError::AuthorizationUserMismatch.into());
    }

    // 7. Executor signer: must match record.executor.
    if record.executor != *executor_account.key {
        return Err(AuthorityError::ExecutorMismatch.into());
    }

    // 8. Canonical rule hash: arg must equal what the user signed.
    //    This is the commitment to the off-chain WatchRule body.
    if record.canonical_rule_hash != canonical_rule_hash {
        return Err(AuthorityError::RuleHashMismatch.into());
    }

    // 9. Action type: arg must match the discriminator the user
    //    authorised. Stops a daemon from re-routing a Solend-authorised
    //    rule into a Jupiter execution.
    if record.allowed_action_type != action_type {
        return Err(AuthorityError::ActionTypeMismatch.into());
    }

    // 10. Expiry. Strict: `current_slot < expires_at_slot`. Mirrors the
    //     create-time check (process_create_authorization above) so the
    //     boundary `current_slot == expires_at_slot` is treated as
    //     expired everywhere. Reuses the existing P1 error variant.
    let clock = Clock::get()?;
    if clock.slot >= record.expires_at_slot {
        return Err(AuthorityError::AuthorizationExpired.into());
    }

    // 11. Revoked.
    if record.revoked {
        return Err(AuthorityError::AuthorizationRevoked.into());
    }

    // 12. Already completed (one-shot v1 — second execution rejected).
    if record.completed {
        return Err(AuthorityError::AuthorizationCompleted.into());
    }

    // 13. Input amount must be non-zero. Zero would advance the nonce
    //     without consuming any cap, which is uninteresting but worse —
    //     it would let an executor spin completed=true on a record
    //     without using any of the user's authorised budget.
    if input_amount_raw == 0 {
        return Err(AuthorityError::InputAmountZero.into());
    }

    // 14. Cap: used_amount_raw + input_amount_raw must not exceed
    //     max_input_amount_raw, with checked_add to refuse overflow.
    let new_used = record
        .used_amount_raw
        .checked_add(input_amount_raw)
        .ok_or(AuthorityError::InputAmountExceeded)?;
    if new_used > record.max_input_amount_raw {
        return Err(AuthorityError::InputAmountExceeded.into());
    }

    // 15. Monotone nonce: arg must be exactly record.execution_nonce + 1.
    //     P1 zero-initialises execution_nonce, so the first valid execute
    //     supplies arg.execution_nonce = 1. checked_add guards the
    //     u64::MAX edge.
    let expected_next_nonce = record
        .execution_nonce
        .checked_add(1)
        .ok_or(AuthorityError::ExecutionNonceMismatch)?;
    if execution_nonce != expected_next_nonce {
        return Err(AuthorityError::ExecutionNonceMismatch.into());
    }

    // 16. Same-slot replay guard. P1 zero-initialises last_execution_slot,
    //     so the sentinel `record.last_execution_slot == 0` permits the
    //     very first execute regardless of clock.slot. Subsequent
    //     executions (only relevant in a future multi-use slice) would
    //     compare against the prior slot — for one-shot v1 this is dead
    //     code on the happy path because (12) already blocks a second
    //     execute on the same record, but we keep it for forward-compat
    //     and to fail loudly if either invariant changes.
    if record.last_execution_slot != 0 && clock.slot == record.last_execution_slot {
        return Err(AuthorityError::SameSlotReplay.into());
    }

    // ── State mutations (one-shot v1) ───────────────────────────────────
    record.used_amount_raw = new_used;
    record.completed = true;
    record.execution_nonce = execution_nonce;
    record.last_execution_slot = clock.slot;

    record
        .serialize(&mut &mut authz_pda_account.data.borrow_mut()[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    // TODO(Stage 2 O3): verify watch-rule conditions before accepting execution
    // TODO(Stage 2): revoke-and-sweep handler

    solana_program::log::sol_log_data(&[
        b"authz_executed_v1",
        &rule_id,
        record.user.as_ref(),
        record.executor.as_ref(),
        core::slice::from_ref(&record.allowed_action_type),
        &input_amount_raw.to_le_bytes(),
        &record.used_amount_raw.to_le_bytes(),
        &execution_nonce.to_le_bytes(),
        &clock.slot.to_le_bytes(),
    ]);
    msg!(
        "execute_action ok action_type={} input_amount_raw={} used_amount_raw={} nonce={} executed_at_slot={}",
        record.allowed_action_type,
        input_amount_raw,
        record.used_amount_raw,
        execution_nonce,
        clock.slot
    );

    Ok(())
}

/// Pure, deterministic PDA derivation for off-chain tx builders and
/// tests. Seeds: `["authz", [schema_version], user.as_ref(), rule_id]`.
pub fn derive_authorization_pda(
    program_id: &Pubkey,
    schema_version: u8,
    user: &Pubkey,
    rule_id: &[u8; 16],
) -> (Pubkey, u8) {
    let schema_version_bytes = [schema_version];
    let seeds: &[&[u8]] = &[
        AUTHZ_SEED_PREFIX,
        &schema_version_bytes,
        user.as_ref(),
        rule_id.as_ref(),
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
        let rule_id = [7u8; 16];
        let a = derive_authorization_pda(&program_id, 1, &user, &rule_id);
        let b = derive_authorization_pda(&program_id, 1, &user, &rule_id);
        assert_eq!(a, b, "derive_authorization_pda must be deterministic");
    }

    #[test]
    fn pda_changes_with_schema_version() {
        let program_id = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let rule_id = [7u8; 16];
        let (pda_v1, _) = derive_authorization_pda(&program_id, 1, &user, &rule_id);
        let (pda_v2, _) = derive_authorization_pda(&program_id, 2, &user, &rule_id);
        assert_ne!(
            pda_v1, pda_v2,
            "schema_version must domain-separate authorization PDAs"
        );
    }

    #[test]
    fn pda_changes_with_user() {
        let program_id = Pubkey::new_unique();
        let user_a = Pubkey::new_unique();
        let user_b = Pubkey::new_unique();
        let rule_id = [7u8; 16];
        let (pda_a, _) = derive_authorization_pda(&program_id, 1, &user_a, &rule_id);
        let (pda_b, _) = derive_authorization_pda(&program_id, 1, &user_b, &rule_id);
        assert_ne!(pda_a, pda_b, "user must domain-separate authorization PDAs");
    }

    #[test]
    fn pda_changes_with_rule_id() {
        let program_id = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let (pda_a, _) = derive_authorization_pda(&program_id, 1, &user, &[1u8; 16]);
        let (pda_b, _) = derive_authorization_pda(&program_id, 1, &user, &[2u8; 16]);
        assert_ne!(pda_a, pda_b, "rule_id must domain-separate authorization PDAs");
    }

    #[test]
    fn stage1_and_stage2_seed_prefixes_are_disjoint() {
        // Stage 1's seed prefix is b"intent" (programs/clawsol-intent/src/lib.rs::INTENT_SEED_PREFIX).
        // We don't import that crate here (no dev-dep on Stage 1) but
        // the byte literal is part of the Stage 1 wire shape and is
        // safe to assert against.
        assert_ne!(AUTHZ_SEED_PREFIX, b"intent");
        // Defensive: the byte length must also differ to avoid any
        // accidental run-on collision.
        assert_ne!(AUTHZ_SEED_PREFIX.len(), b"intent".len());
    }

    #[test]
    fn authorization_record_len_matches_borsh_serialization() {
        let record = AuthorizationRecord {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0xAA; 16],
            user: Pubkey::new_unique(),
            executor: Pubkey::new_unique(),
            delegated_wallet: Pubkey::new_unique(),
            canonical_rule_hash: [0xBB; 32],
            allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: Pubkey::new_unique(),
            expires_at_slot: 415_700_000,
            revoked: false,
            completed: false,
            execution_nonce: 0,
            last_execution_slot: 0,
            bump: 255,
        };
        let bytes = borsh::to_vec(&record).unwrap();
        assert_eq!(
            bytes.len(),
            AuthorizationRecord::LEN,
            "AuthorizationRecord::LEN ({}) must equal serialized size ({})",
            AuthorizationRecord::LEN,
            bytes.len()
        );
        // Pin the absolute size — this fails loudly if any field is
        // added, removed, or its type changes.
        assert_eq!(AuthorizationRecord::LEN, 221, "fixed-size record is 221 bytes");
    }

    #[test]
    fn authorization_record_borsh_roundtrip_preserves_fields() {
        let original = AuthorizationRecord {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0x01; 16],
            user: Pubkey::new_unique(),
            executor: Pubkey::new_unique(),
            delegated_wallet: Pubkey::new_unique(),
            canonical_rule_hash: [0x02; 32],
            allowed_action_type: Stage2ActionType::JupiterBuySolWithUsdc.to_u8(),
            max_input_amount_raw: u64::MAX - 7,
            used_amount_raw: 42,
            destination: Pubkey::new_unique(),
            expires_at_slot: u64::MAX - 1,
            revoked: true,
            completed: true,
            execution_nonce: 99,
            last_execution_slot: 415_500_001,
            bump: 254,
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = AuthorizationRecord::try_from_slice(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn authorization_record_layout_matches_borsh_field_order() {
        // Borsh serializes structs in declared field order, integers
        // little-endian, fixed arrays without prefix, bool as 1 byte.
        // Anchor every field at an explicit byte offset so any future
        // reorder is loud.
        let user = Pubkey::new_from_array([0x33; 32]);
        let executor = Pubkey::new_from_array([0x44; 32]);
        let delegated_wallet = Pubkey::new_from_array([0x55; 32]);
        let destination = Pubkey::new_from_array([0x77; 32]);
        let record = AuthorizationRecord {
            schema_version: 0x01,
            rule_id: [0x22; 16],
            user,
            executor,
            delegated_wallet,
            canonical_rule_hash: [0x66; 32],
            allowed_action_type: 0x01,
            max_input_amount_raw: 0x0102_0304_0506_0708,
            used_amount_raw: 0,
            destination,
            expires_at_slot: 0x1112_1314_1516_1718,
            revoked: true,
            completed: false,
            execution_nonce: 0x2122_2324_2526_2728,
            last_execution_slot: 0x3132_3334_3536_3738,
            bump: 0xFE,
        };
        let bytes = borsh::to_vec(&record).unwrap();
        assert_eq!(bytes[0], 0x01, "byte 0 = schema_version");
        assert_eq!(&bytes[1..17], &[0x22u8; 16][..], "bytes 1..17 = rule_id");
        assert_eq!(&bytes[17..49], &[0x33u8; 32][..], "bytes 17..49 = user");
        assert_eq!(&bytes[49..81], &[0x44u8; 32][..], "bytes 49..81 = executor");
        assert_eq!(
            &bytes[81..113],
            &[0x55u8; 32][..],
            "bytes 81..113 = delegated_wallet"
        );
        assert_eq!(
            &bytes[113..145],
            &[0x66u8; 32][..],
            "bytes 113..145 = canonical_rule_hash"
        );
        assert_eq!(bytes[145], 0x01, "byte 145 = allowed_action_type");
        assert_eq!(
            &bytes[146..154],
            &0x0102_0304_0506_0708u64.to_le_bytes()[..],
            "bytes 146..154 = max_input_amount_raw (LE u64)"
        );
        assert_eq!(
            &bytes[154..162],
            &0u64.to_le_bytes()[..],
            "bytes 154..162 = used_amount_raw (LE u64)"
        );
        assert_eq!(
            &bytes[162..194],
            &[0x77u8; 32][..],
            "bytes 162..194 = destination"
        );
        assert_eq!(
            &bytes[194..202],
            &0x1112_1314_1516_1718u64.to_le_bytes()[..],
            "bytes 194..202 = expires_at_slot (LE u64)"
        );
        assert_eq!(bytes[202], 0x01, "byte 202 = revoked (true)");
        assert_eq!(bytes[203], 0x00, "byte 203 = completed (false)");
        assert_eq!(
            &bytes[204..212],
            &0x2122_2324_2526_2728u64.to_le_bytes()[..],
            "bytes 204..212 = execution_nonce (LE u64)"
        );
        assert_eq!(
            &bytes[212..220],
            &0x3132_3334_3536_3738u64.to_le_bytes()[..],
            "bytes 212..220 = last_execution_slot (LE u64)"
        );
        assert_eq!(bytes[220], 0xFE, "byte 220 = bump");
        assert_eq!(bytes.len(), 221);
    }

    #[test]
    fn authorization_record_does_not_carry_conditions_or_action_payload() {
        // The PDA stores the canonical rule hash (32 bytes) — a
        // commitment to the conditions vector and the action payload —
        // not the conditions or payload themselves. Any future PR that
        // grows the record into a Vec<Condition> or an ActionSpec will
        // inflate LEN past this fixed value and trip this test.
        assert_eq!(
            AuthorizationRecord::LEN,
            221,
            "AuthorizationRecord must store only fixed scalars + a hash, not the full rule body"
        );
    }

    #[test]
    fn stage2_action_type_round_trip() {
        for v in [
            Stage2ActionType::SolendWithdrawAllDelegated,
            Stage2ActionType::JupiterBuySolWithUsdc,
        ] {
            assert_eq!(Stage2ActionType::from_u8(v.to_u8()), Some(v));
        }
        assert_eq!(Stage2ActionType::from_u8(0), None);
        assert_eq!(Stage2ActionType::from_u8(3), None);
        assert_eq!(Stage2ActionType::from_u8(255), None);
        assert_eq!(Stage2ActionType::SolendWithdrawAllDelegated.to_u8(), 1);
        assert_eq!(Stage2ActionType::JupiterBuySolWithUsdc.to_u8(), 2);
    }

    #[test]
    fn authority_instruction_borsh_roundtrip() {
        let original = AuthorityInstruction::CreateAuthorization {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [9u8; 16],
            executor: Pubkey::new_unique(),
            delegated_wallet: Pubkey::new_unique(),
            canonical_rule_hash: [8u8; 32],
            allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
            max_input_amount_raw: 5_000_000,
            destination: Pubkey::new_unique(),
            expires_at_slot: 1_000_000,
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = AuthorityInstruction::try_from_slice(&bytes).unwrap();
        assert_eq!(original, decoded);

        let revoke = AuthorityInstruction::Revoke;
        let bytes = borsh::to_vec(&revoke).unwrap();
        let decoded = AuthorityInstruction::try_from_slice(&bytes).unwrap();
        assert_eq!(revoke, decoded);

        let close = AuthorityInstruction::CloseAuthorization;
        let bytes = borsh::to_vec(&close).unwrap();
        let decoded = AuthorityInstruction::try_from_slice(&bytes).unwrap();
        assert_eq!(close, decoded);
    }

    #[test]
    fn execute_action_borsh_roundtrip() {
        let original = AuthorityInstruction::ExecuteAction {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0xAB; 16],
            canonical_rule_hash: [0xCD; 32],
            action_type: Stage2ActionType::JupiterBuySolWithUsdc.to_u8(),
            input_amount_raw: 5_000_000,
            execution_nonce: 1,
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = AuthorityInstruction::try_from_slice(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn execute_action_borsh_field_order_is_pinned() {
        // Borsh: enum tag is `u8` ordinal in declaration order, then
        // variant body in declared field order, integers little-endian,
        // fixed `[u8; N]` arrays raw. ExecuteAction is the 4th variant
        // (CreateAuthorization=0, Revoke=1, CloseAuthorization=2,
        // ExecuteAction=3). Anchor every byte so a future reorder is
        // loud.
        let ix = AuthorityInstruction::ExecuteAction {
            schema_version: 0x01,
            rule_id: [0x22; 16],
            canonical_rule_hash: [0x66; 32],
            action_type: 0x02,
            input_amount_raw: 0x0102_0304_0506_0708,
            execution_nonce: 0x1112_1314_1516_1718,
        };
        let bytes = borsh::to_vec(&ix).unwrap();
        // Total: 1 (tag) + 1 + 16 + 32 + 1 + 8 + 8 = 67 bytes
        assert_eq!(bytes.len(), 67);
        assert_eq!(bytes[0], 0x03, "byte 0 = enum tag (ExecuteAction = 3)");
        assert_eq!(bytes[1], 0x01, "byte 1 = schema_version");
        assert_eq!(&bytes[2..18], &[0x22u8; 16][..], "bytes 2..18 = rule_id");
        assert_eq!(
            &bytes[18..50],
            &[0x66u8; 32][..],
            "bytes 18..50 = canonical_rule_hash"
        );
        assert_eq!(bytes[50], 0x02, "byte 50 = action_type");
        assert_eq!(
            &bytes[51..59],
            &0x0102_0304_0506_0708u64.to_le_bytes()[..],
            "bytes 51..59 = input_amount_raw (LE u64)"
        );
        assert_eq!(
            &bytes[59..67],
            &0x1112_1314_1516_1718u64.to_le_bytes()[..],
            "bytes 59..67 = execution_nonce (LE u64)"
        );
    }

    #[test]
    fn authority_instruction_variant_tags_are_stable() {
        // Pin the Borsh enum tag for every variant. Adding a future
        // variant must extend this list; reordering MUST fail this test.
        let create = borsh::to_vec(&AuthorityInstruction::CreateAuthorization {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0; 16],
            executor: Pubkey::new_from_array([0; 32]),
            delegated_wallet: Pubkey::new_from_array([0; 32]),
            canonical_rule_hash: [0; 32],
            allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
            max_input_amount_raw: 1,
            destination: Pubkey::new_from_array([0; 32]),
            expires_at_slot: 1,
        })
        .unwrap();
        assert_eq!(create[0], 0, "CreateAuthorization tag = 0");

        let revoke = borsh::to_vec(&AuthorityInstruction::Revoke).unwrap();
        assert_eq!(revoke, vec![1], "Revoke tag = 1, no body");

        let close = borsh::to_vec(&AuthorityInstruction::CloseAuthorization).unwrap();
        assert_eq!(close, vec![2], "CloseAuthorization tag = 2, no body");

        let execute = borsh::to_vec(&AuthorityInstruction::ExecuteAction {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0; 16],
            canonical_rule_hash: [0; 32],
            action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
            input_amount_raw: 1,
            execution_nonce: 1,
        })
        .unwrap();
        assert_eq!(execute[0], 3, "ExecuteAction tag = 3");
    }

    #[test]
    fn authority_error_discriminants_are_pinned() {
        // Numeric values are encoded into ProgramError::Custom(u32) and
        // are part of the on-chain wire shape. Renumbering, reordering,
        // or removing any value is a hard wire break.
        assert_eq!(AuthorityError::UserMustSign as u32, 0);
        assert_eq!(AuthorityError::InvalidPda as u32, 1);
        assert_eq!(AuthorityError::AuthorizationExpired as u32, 2);
        assert_eq!(AuthorityError::InvalidZeroMaxAmount as u32, 3);
        assert_eq!(AuthorityError::UnsupportedSchemaVersion as u32, 4);
        assert_eq!(AuthorityError::InvalidActionType as u32, 5);
        assert_eq!(AuthorityError::RevokeWrongUser as u32, 6);
        assert_eq!(AuthorityError::CloseWrongUser as u32, 7);
        assert_eq!(AuthorityError::NotCloseable as u32, 8);
        // P2 ExecuteAction additions (append-only).
        assert_eq!(AuthorityError::MissingExecutorSignature as u32, 9);
        assert_eq!(AuthorityError::InvalidAuthorizationPda as u32, 10);
        assert_eq!(AuthorityError::AuthorizationOwnerMismatch as u32, 11);
        assert_eq!(AuthorityError::AuthorizationUserMismatch as u32, 12);
        assert_eq!(AuthorityError::ExecutorMismatch as u32, 13);
        assert_eq!(AuthorityError::RuleHashMismatch as u32, 14);
        assert_eq!(AuthorityError::ActionTypeMismatch as u32, 15);
        assert_eq!(AuthorityError::AuthorizationRevoked as u32, 16);
        assert_eq!(AuthorityError::AuthorizationCompleted as u32, 17);
        assert_eq!(AuthorityError::InputAmountZero as u32, 18);
        assert_eq!(AuthorityError::InputAmountExceeded as u32, 19);
        assert_eq!(AuthorityError::ExecutionNonceMismatch as u32, 20);
        assert_eq!(AuthorityError::SameSlotReplay as u32, 21);
    }

    #[test]
    fn pda_seeds_unchanged_from_p1() {
        // Belt-and-braces guard that P2's processor still uses the same
        // four-component seed list as P1. If the seed scheme changes,
        // any deployed PDA becomes unreachable — fail loudly here long
        // before that happens on chain.
        let program_id = Pubkey::new_from_array([7u8; 32]);
        let user = Pubkey::new_from_array([0x11; 32]);
        let rule_id = [0x22u8; 16];
        let (pda, _bump) = derive_authorization_pda(&program_id, 1, &user, &rule_id);

        let schema_version_bytes = [1u8];
        let manual_seeds: &[&[u8]] = &[
            b"authz",
            &schema_version_bytes,
            user.as_ref(),
            rule_id.as_ref(),
        ];
        let (manual_pda, _) = Pubkey::find_program_address(manual_seeds, &program_id);
        assert_eq!(pda, manual_pda);
        assert_eq!(AUTHZ_SEED_PREFIX, b"authz");
    }
}

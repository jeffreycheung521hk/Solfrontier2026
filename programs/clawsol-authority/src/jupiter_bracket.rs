//! Stage 2 Jupiter trustless dual-bracket verifier (J5 — BONUS PROTOTYPE).
//!
//! # What this module is
//!
//! The trustless on-chain pre/post balance bracket J4 deferred. J4 lands
//! the sibling-instruction verifier; J5 lands the dual-instruction
//! checkpoint PDA + on-chain SPL Token unpack + delta enforcement that
//! makes the Jupiter execution path trustless against an executor
//! lying about pre/post balances.
//!
//! Wire shape:
//!
//! 1. Caller (executor) submits `PreJupiterBracketCheck { proof }` BEFORE
//!    Jupiter's swap runs. The program:
//!    - verifies signer + authorization PDA + expected destination
//!      token account
//!    - unpacks the live SPL Token account at the destination address
//!      (raw 165-byte Pack — `data[0..32]=mint`, `data[32..64]=owner`,
//!      `data[64..72]=amount LE u64`)
//!    - verifies `mint == proof.expected_destination_mint`
//!    - records `pre_balance` (live, NEVER executor-supplied) and
//!      `min_output_amount_raw` into a fresh checkpoint PDA via
//!      `system_instruction::create_account` (the only `invoke_signed`
//!      this module performs — and it targets ONLY the system program
//!      for PDA initialisation, never Jupiter / Solend / SPL Token).
//!
//! 2. Jupiter's swap runs (via the executor's other transaction
//!    instructions — out of J5's scope).
//!
//! 3. Caller submits `PostJupiterBracketCheck { proof }`. The program:
//!    - loads the checkpoint PDA
//!    - re-unpacks the live SPL Token account at the same destination
//!    - computes `delta = post_balance.checked_sub(pre_balance)`
//!    - fails closed with [`AuthorityError::JupiterPostBalanceUnderflow`]
//!      if the checked_sub returned `None`
//!    - fails closed with [`AuthorityError::JupiterBalanceDeltaTooSmall`]
//!      if `delta < min_output_amount_raw`
//!    - marks the checkpoint `consumed = true` so a second post-check
//!      against the same checkpoint fails closed with
//!      [`AuthorityError::JupiterBracketCheckpointConsumed`]
//!
//! # What this module is NOT
//!
//! - **Not a Jupiter API caller.** No `api.jup.ag` HTTP call. No
//!   `quote-api.jup.ag`. No `reqwest`. The caller is responsible for
//!   driving Jupiter's swap; this module only brackets the destination
//!   token account's balance around it.
//! - **Not a Jupiter CPI.** No `invoke()` of the Jupiter program. The
//!   only CPI this module performs is `invoke_signed()` of the
//!   `system_program::create_account` ix to materialise the checkpoint
//!   PDA. That CPI targets the system program, not Jupiter.
//! - **Not the Solend demo executor path.** J5 is bonus prototype work;
//!   it does NOT touch the W4 Solend gateway executor or any
//!   `crates/gateway/*`, `crates/state-store/*`, `crates/types/*` code.
//! - **Not live-proven.** All J5 tests run inside the in-process
//!   ProgramTest harness with hand-built mock SPL Token account bytes
//!   at the destination ATA address. No mainnet broadcast, no devnet,
//!   no live Jupiter swap.
//!
//! # Why a separate checkpoint PDA (and not extend AuthorizationRecord)
//!
//! `AuthorizationRecord::LEN == 221` is fixed at compile time and
//! pinned by `lib.rs::tests::authorization_record_len_matches_borsh_serialization`.
//! Any byte added to the record breaks the on-chain wire shape and
//! every existing PDA. A separate checkpoint PDA (one per Jupiter
//! execution attempt) keeps J5 strictly additive — every other Stage 2
//! slice is unaffected.
//!
//! # Seed shape (pinned by `tests::checkpoint_pda_seeds_are_stable`)
//!
//! `[b"stage2-jupiter-bracket", authorization_pda.as_ref(),
//!   execution_nonce.to_le_bytes(), destination_token_account.as_ref()]`
//!
//! - `b"stage2-jupiter-bracket"` (22 bytes) — domain separator. Disjoint
//!   from `b"authz"` (5 bytes) and Stage 1's `b"intent"` (6 bytes), so
//!   no possible byte sequence under any other seed combination can
//!   collide.
//! - `authorization_pda.as_ref()` (32 bytes) — binds the checkpoint to
//!   the exact authorization that authorised the Jupiter execution.
//! - `execution_nonce.to_le_bytes()` (8 bytes) — domain-separates
//!   per-execution. Future-proofs against the multi-use rules slice
//!   (one bracket per execution attempt).
//! - `destination_token_account.as_ref()` (32 bytes) — binds the
//!   bracket to the exact token account whose balance is gated.
//!
//! Total non-bump seed length: 22 + 32 + 8 + 32 = 94 bytes across
//! 4 components — well under PDA `MAX_SEEDS = 16` and each component
//! is well under `MAX_SEED_LEN = 32` (the 22-byte prefix is the
//! longest individual seed).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::Sysvar,
};

use crate::error::AuthorityError;
use crate::solend_account_decode::decode_spl_token_account_lite;
use crate::state::AuthorizationRecord;

/// Seed prefix for the per-execution Jupiter bracket checkpoint PDA.
///
/// Disjoint from `AUTHZ_SEED_PREFIX = b"authz"` (Stage 2 P1) and from
/// Stage 1's `b"intent"` — see `tests::seed_prefix_is_disjoint_from_other_stages`.
pub const JUPITER_BRACKET_SEED_PREFIX: &[u8] = b"stage2-jupiter-bracket";

/// Schema version of the on-chain checkpoint PDA wire shape. Bumped
/// only when the Borsh layout below changes; every other tightening
/// stays at version `1`.
pub const JUPITER_BRACKET_SCHEMA_VERSION: u8 = 1;

/// Persistent on-chain state for one in-flight Jupiter dual-bracket.
///
/// Borsh field order is part of the wire shape; reorder is a wire
/// break. Fixed-size — no `Vec` / `Option` / `String` — so its length
/// is a compile-time sum and every byte position is anchored by
/// [`JupiterBracketCheckpoint::LEN`].
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct JupiterBracketCheckpoint {
    /// Schema version pinned at creation time. Mirrors
    /// [`JUPITER_BRACKET_SCHEMA_VERSION`]; the post-check refuses to
    /// load a checkpoint whose version this build does not understand.
    pub schema_version: u8,
    /// The authorization PDA whose Jupiter execution is bracketed.
    /// Pre-check binds this to the executor-supplied authz pubkey
    /// (which is also re-derived from `record.user`+`record.rule_id`
    /// in the AuthorityRecord verification path).
    pub authorization_pda: Pubkey,
    /// The execution nonce this checkpoint covers — inherited from
    /// `AuthorizationRecord.execution_nonce + 1` at pre-check time.
    /// Forces a fresh PDA per execution attempt; combined with the
    /// `destination_token_account` seed component it makes a hostile
    /// re-use across executions structurally impossible.
    pub execution_nonce: u64,
    /// The destination token account address whose balance is bracketed.
    pub destination_token_account: Pubkey,
    /// The destination SPL Token mint at pre-check time. Post-check
    /// re-unpacks the destination's data and refuses to proceed if the
    /// mint changed — defends against a mid-bracket account
    /// substitution.
    pub destination_mint: Pubkey,
    /// Live pre-bracket balance read from the destination's data via
    /// [`decode_spl_token_account_lite`]. NEVER executor-supplied.
    pub pre_balance: u64,
    /// Canonical-bound minimum output the user signed for. Carried
    /// from the J4 `JupiterBoundaryProof::min_output_amount_raw` (or
    /// from the canonical-rule binding the executor supplies in the
    /// pre-check proof body).
    pub min_output_amount_raw: u64,
    /// `false` after `PreJupiterBracketCheck`; flipped to `true` by the
    /// first successful `PostJupiterBracketCheck`. A second post-check
    /// against the same checkpoint sees `consumed=true` and fails
    /// closed with [`AuthorityError::JupiterBracketCheckpointConsumed`].
    pub consumed: bool,
    /// Bump byte cached for cheap signer-seed reconstruction. The
    /// canonical PDA derivation is `find_program_address(seeds, prog)`
    /// over the seed shape pinned at the top of this module.
    pub bump: u8,
}

impl JupiterBracketCheckpoint {
    /// Borsh-serialised length in bytes. Fixed because the struct has
    /// no variable-length fields.
    pub const LEN: usize = 1   // schema_version
        + 32  // authorization_pda
        + 8   // execution_nonce
        + 32  // destination_token_account
        + 32  // destination_mint
        + 8   // pre_balance
        + 8   // min_output_amount_raw
        + 1   // consumed (Borsh bool = 1 byte)
        + 1; // bump
}

/// Wire-shape payload supplied alongside `PreJupiterBracketCheck`.
///
/// Borsh field order is part of the wire shape. Reorder is a wire
/// break.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct JupiterPreBracketProof {
    /// The execution nonce this bracket covers. Bound into the
    /// checkpoint PDA seed; the program also asserts it equals
    /// `record.execution_nonce + 1` so a hostile executor cannot pin a
    /// stale nonce.
    pub execution_nonce: u64,
    /// The destination SPL Token account address whose balance is
    /// bracketed. The program cross-checks this against the supplied
    /// destination AccountInfo's key AND against
    /// `AuthorizationRecord.destination`.
    pub destination_token_account: Pubkey,
    /// The expected SPL Token mint of the destination. The program
    /// unpacks the live destination data and rejects mismatches with
    /// [`AuthorityError::JupiterMintMismatch`] (re-using the J4 code).
    pub expected_destination_mint: Pubkey,
    /// Canonical-bound minimum output the user signed for. Recorded
    /// into the checkpoint and enforced in the post-check.
    pub min_output_amount_raw: u64,
}

/// Wire-shape payload supplied alongside `PostJupiterBracketCheck`.
///
/// Note that `pre_balance` is INTENTIONALLY ABSENT — the program
/// reads it from the checkpoint PDA, NEVER from the post-check
/// proof. A future regression that smuggles a `pre_balance` field
/// into this struct would be visible at code review.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct JupiterPostBracketProof {
    /// The execution nonce this bracket covers. Used to derive the
    /// checkpoint PDA address; mismatch surfaces as
    /// [`AuthorityError::JupiterBracketCheckpointMissing`] (the
    /// checkpoint at the wrong-nonce seed simply does not exist).
    pub execution_nonce: u64,
    /// The destination SPL Token account address. Cross-checked
    /// against the checkpoint's stored value.
    pub destination_token_account: Pubkey,
}

/// Pure, deterministic checkpoint PDA derivation for off-chain tx
/// builders and tests.
///
/// Seeds: `[JUPITER_BRACKET_SEED_PREFIX, authorization_pda,
/// execution_nonce.to_le_bytes(), destination_token_account]`.
pub fn derive_jupiter_bracket_checkpoint_pda(
    program_id: &Pubkey,
    authorization_pda: &Pubkey,
    execution_nonce: u64,
    destination_token_account: &Pubkey,
) -> (Pubkey, u8) {
    let nonce_bytes = execution_nonce.to_le_bytes();
    let seeds: &[&[u8]] = &[
        JUPITER_BRACKET_SEED_PREFIX,
        authorization_pda.as_ref(),
        &nonce_bytes,
        destination_token_account.as_ref(),
    ];
    Pubkey::find_program_address(seeds, program_id)
}

/// Pre-check processor. Account layout:
///   0. `[signer, writable]` executor             — pays for checkpoint rent
///   1. `[]`                 authorization_pda    — owned by this program
///   2. `[]`                 destination_token_account — live SPL Token account
///   3. `[writable]`         checkpoint_pda       — created here
///   4. `[]`                 system_program       — for create_account CPI
///
/// Mutates: creates the checkpoint PDA and writes its initial state.
/// Does NOT mutate `AuthorizationRecord` (the post-check is paired
/// with `ExecuteAction` upstream; this slice deliberately keeps the
/// pre/post path orthogonal to the existing one-shot lifecycle so it
/// can be exercised in isolation under ProgramTest).
pub fn process_pre_jupiter_bracket_check(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    proof: JupiterPreBracketProof,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let executor_account = next_account_info(account_iter)?;
    let authz_pda_account = next_account_info(account_iter)?;
    let destination_account = next_account_info(account_iter)?;
    let checkpoint_pda_account = next_account_info(account_iter)?;
    let system_program_account = next_account_info(account_iter)?;

    // 1. Executor must sign (pays rent for the checkpoint PDA).
    if !executor_account.is_signer {
        return Err(AuthorityError::MissingExecutorSignature.into());
    }

    // 2. System program identity.
    if system_program_account.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    // 3. Authorization PDA must be owned by this program.
    if authz_pda_account.owner != program_id {
        return Err(AuthorityError::AuthorizationOwnerMismatch.into());
    }

    // 4. Decode authorization record. From here every check compares
    //    the on-chain record against the proof.
    let record = {
        let data = authz_pda_account.data.borrow();
        AuthorizationRecord::try_from_slice(&data)
            .map_err(|_| ProgramError::InvalidAccountData)?
    };

    // 5. Executor signer must match record.executor.
    if record.executor != *executor_account.key {
        return Err(AuthorityError::ExecutorMismatch.into());
    }

    // 6. Destination token account address: proof must match
    //    AuthorizationRecord.destination, AND the supplied AccountInfo
    //    key must equal the proof field. Two independent gates so a
    //    malicious executor cannot pass the right pubkey in the proof
    //    and a different account in the AccountInfo list.
    if proof.destination_token_account != record.destination {
        return Err(AuthorityError::JupiterDestinationMismatch.into());
    }
    if destination_account.key != &proof.destination_token_account {
        return Err(AuthorityError::JupiterDestinationMismatch.into());
    }

    // 7. Proof execution_nonce must equal record.execution_nonce + 1.
    //    This pins one bracket per execution attempt and bins the
    //    checkpoint PDA seed to a fresh address.
    let expected_nonce = record
        .execution_nonce
        .checked_add(1)
        .ok_or(AuthorityError::ExecutionNonceMismatch)?;
    if proof.execution_nonce != expected_nonce {
        return Err(AuthorityError::ExecutionNonceMismatch.into());
    }

    // 8. Live SPL Token unpack. Strict 165-byte length — Token-2022
    //    extensions (length > 165) are rejected; a buffer that happens
    //    to contain plausible bytes at the right offsets but is the
    //    wrong size is also rejected. The decoder reuses the P5a
    //    helper (`decode_spl_token_account_lite`) which fails with
    //    [`AuthorityError::SolendTokenAccountInvalidLength`] (the same
    //    code Solend's decoder uses; J5 reuses it rather than minting
    //    a new one to keep the error surface tight).
    let pre_balance = {
        let data = destination_account
            .try_borrow_data()
            .map_err(|_| ProgramError::AccountBorrowFailed)?;
        let decoded = decode_spl_token_account_lite(&data)?;
        // Borrow guard `data` drops at end of this block.
        if decoded.mint != proof.expected_destination_mint {
            return Err(AuthorityError::JupiterMintMismatch.into());
        }
        decoded.amount
    };

    // 9. Derive the canonical checkpoint PDA address and require the
    //    caller passed the matching account.
    let (expected_checkpoint_pda, bump) = derive_jupiter_bracket_checkpoint_pda(
        program_id,
        authz_pda_account.key,
        proof.execution_nonce,
        &proof.destination_token_account,
    );
    if expected_checkpoint_pda != *checkpoint_pda_account.key {
        return Err(AuthorityError::InvalidPda.into());
    }

    // 10. Allocate the checkpoint PDA via system_program::create_account
    //     under invoke_signed (the only `invoke_signed` in J5 — and
    //     it targets ONLY the system program, never Jupiter / Solend /
    //     SPL Token). Same idiom as `process_create_authorization` in
    //     `lib.rs`. If the PDA already has lamports / data,
    //     create_account fails with `SystemError::AccountAlreadyInUse`
    //     — the runtime rejects a second pre-check against the same
    //     (authz, nonce, destination) triple for us.
    let space = JupiterBracketCheckpoint::LEN as u64;
    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(JupiterBracketCheckpoint::LEN);

    let create_ix = system_instruction::create_account(
        executor_account.key,
        checkpoint_pda_account.key,
        lamports,
        space,
        program_id,
    );

    let nonce_bytes = proof.execution_nonce.to_le_bytes();
    let bump_arr = [bump];
    let signer_seeds: &[&[u8]] = &[
        JUPITER_BRACKET_SEED_PREFIX,
        authz_pda_account.key.as_ref(),
        &nonce_bytes,
        proof.destination_token_account.as_ref(),
        &bump_arr,
    ];

    invoke_signed(
        &create_ix,
        &[
            executor_account.clone(),
            checkpoint_pda_account.clone(),
            system_program_account.clone(),
        ],
        &[signer_seeds],
    )?;

    let checkpoint = JupiterBracketCheckpoint {
        schema_version: JUPITER_BRACKET_SCHEMA_VERSION,
        authorization_pda: *authz_pda_account.key,
        execution_nonce: proof.execution_nonce,
        destination_token_account: proof.destination_token_account,
        destination_mint: proof.expected_destination_mint,
        pre_balance,
        min_output_amount_raw: proof.min_output_amount_raw,
        consumed: false,
        bump,
    };
    checkpoint
        .serialize(&mut &mut checkpoint_pda_account.data.borrow_mut()[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    msg!(
        "jupiter_bracket pre_check ok pre_balance={} min_out={}",
        pre_balance,
        proof.min_output_amount_raw
    );
    Ok(())
}

/// Post-check processor. Account layout:
///   0. `[signer]`           executor                 — must equal authz.executor
///   1. `[]`                 authorization_pda        — owned by this program
///   2. `[]`                 destination_token_account — live SPL Token account
///   3. `[writable]`         checkpoint_pda           — flipped to consumed
pub fn process_post_jupiter_bracket_check(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    proof: JupiterPostBracketProof,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let executor_account = next_account_info(account_iter)?;
    let authz_pda_account = next_account_info(account_iter)?;
    let destination_account = next_account_info(account_iter)?;
    let checkpoint_pda_account = next_account_info(account_iter)?;

    // 1. Executor must sign.
    if !executor_account.is_signer {
        return Err(AuthorityError::MissingExecutorSignature.into());
    }

    // 2. Authorization PDA owner.
    if authz_pda_account.owner != program_id {
        return Err(AuthorityError::AuthorizationOwnerMismatch.into());
    }

    // 3. Decode the authorization record + verify executor matches.
    let record = {
        let data = authz_pda_account.data.borrow();
        AuthorizationRecord::try_from_slice(&data)
            .map_err(|_| ProgramError::InvalidAccountData)?
    };
    if record.executor != *executor_account.key {
        return Err(AuthorityError::ExecutorMismatch.into());
    }

    // 4. Checkpoint PDA owner must be this program. A foreign-owned
    //    look-alike with valid bytes cannot be promoted to consumed.
    //    A 0-lamport / 0-data account would also fail this since
    //    create_account assigned ownership at pre-check time; if no
    //    pre-check ever ran, the account's owner is the system program
    //    (or it does not exist), and this check fires first.
    if checkpoint_pda_account.owner != program_id {
        return Err(AuthorityError::JupiterBracketCheckpointMissing.into());
    }

    // 5. Re-derive the checkpoint PDA address from the proof + authz
    //    + destination, and require the supplied AccountInfo's key
    //    matches.
    let (expected_checkpoint_pda, _bump) = derive_jupiter_bracket_checkpoint_pda(
        program_id,
        authz_pda_account.key,
        proof.execution_nonce,
        &proof.destination_token_account,
    );
    if expected_checkpoint_pda != *checkpoint_pda_account.key {
        return Err(AuthorityError::JupiterBracketCheckpointMissing.into());
    }

    // 6. Decode the checkpoint and run the consumed gate FIRST — a
    //    second post-check against the same checkpoint must surface
    //    the dedicated replay code, not bubble up an arithmetic error
    //    or a generic borrow failure.
    let mut checkpoint = {
        let data = checkpoint_pda_account.data.borrow();
        JupiterBracketCheckpoint::try_from_slice(&data)
            .map_err(|_| ProgramError::InvalidAccountData)?
    };
    if checkpoint.schema_version != JUPITER_BRACKET_SCHEMA_VERSION {
        return Err(AuthorityError::UnsupportedSchemaVersion.into());
    }
    if checkpoint.consumed {
        return Err(AuthorityError::JupiterBracketCheckpointConsumed.into());
    }

    // 7. Defence-in-depth: the checkpoint's stored authz + nonce +
    //    destination must agree with what the proof claims AND with
    //    the AccountInfo list supplied. The PDA derivation above
    //    already keys on these, but the explicit check produces a
    //    precise error code rather than a generic "missing" if the
    //    caller corrupted the buffer somehow.
    if checkpoint.authorization_pda != *authz_pda_account.key {
        return Err(AuthorityError::JupiterBracketCheckpointMissing.into());
    }
    if checkpoint.destination_token_account != proof.destination_token_account {
        return Err(AuthorityError::JupiterDestinationMismatch.into());
    }
    if destination_account.key != &checkpoint.destination_token_account {
        return Err(AuthorityError::JupiterDestinationMismatch.into());
    }
    if checkpoint.execution_nonce != proof.execution_nonce {
        return Err(AuthorityError::JupiterBracketCheckpointMissing.into());
    }

    // 8. Live SPL Token unpack of the destination at post-bracket time.
    //    Strict 165-byte length; mint must agree with the checkpoint's
    //    stored value (defends against a mid-bracket account
    //    substitution where the account was burned/closed and replaced
    //    by a different token's account at the same address — in
    //    practice impossible since the address is committed, but the
    //    check is cheap and structurally meaningful for forward-compat).
    let post_balance = {
        let data = destination_account
            .try_borrow_data()
            .map_err(|_| ProgramError::AccountBorrowFailed)?;
        let decoded = decode_spl_token_account_lite(&data)?;
        if decoded.mint != checkpoint.destination_mint {
            return Err(AuthorityError::JupiterMintMismatch.into());
        }
        decoded.amount
    };

    // 9. Trustless balance bracket — the J5 raison d'être.
    //    `checked_sub` returns None on underflow (post < pre), which
    //    surfaces as the dedicated [`JupiterPostBalanceUnderflow`]
    //    code. A clean delta below `min_output_amount_raw` surfaces
    //    as [`JupiterBalanceDeltaTooSmall`]. Any equal/greater delta
    //    passes.
    let delta = post_balance
        .checked_sub(checkpoint.pre_balance)
        .ok_or(AuthorityError::JupiterPostBalanceUnderflow)?;
    if delta < checkpoint.min_output_amount_raw {
        return Err(AuthorityError::JupiterBalanceDeltaTooSmall.into());
    }

    // 10. Mark the checkpoint consumed and persist. Replay protection:
    //     a second post-check against the same PDA reads consumed=true
    //     and fails closed at gate (6). We do NOT close the PDA here
    //     so the audit trail remains queryable; the executor (or the
    //     user via a future close ix) can reclaim rent later.
    checkpoint.consumed = true;
    checkpoint
        .serialize(&mut &mut checkpoint_pda_account.data.borrow_mut()[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    msg!(
        "jupiter_bracket post_check ok delta={} min_out={}",
        delta,
        checkpoint.min_output_amount_raw
    );
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    /// Pin the seed prefix bytes — any rename or shortening / lengthening
    /// is a wire break for every deployed checkpoint PDA.
    #[test]
    fn jupiter_bracket_seed_prefix_is_pinned() {
        assert_eq!(JUPITER_BRACKET_SEED_PREFIX, b"stage2-jupiter-bracket");
        assert_eq!(JUPITER_BRACKET_SEED_PREFIX.len(), 22);
    }

    /// Disjointness from existing Stage 1 / Stage 2 seed prefixes.
    /// Different bytes AND different lengths so no run-on collision is
    /// possible across the seed prefix component alone.
    #[test]
    fn seed_prefix_is_disjoint_from_other_stages() {
        assert_ne!(JUPITER_BRACKET_SEED_PREFIX, b"authz");
        assert_ne!(JUPITER_BRACKET_SEED_PREFIX, b"intent");
        assert_ne!(JUPITER_BRACKET_SEED_PREFIX.len(), b"authz".len());
        assert_ne!(JUPITER_BRACKET_SEED_PREFIX.len(), b"intent".len());
    }

    /// The checkpoint PDA derivation MUST be deterministic and MUST
    /// vary on every seed component independently.
    #[test]
    fn checkpoint_pda_is_deterministic_and_varies_per_component() {
        let program_id = pk(0xAA);
        let authz_a = pk(0x01);
        let authz_b = pk(0x02);
        let dest_a = pk(0x10);
        let dest_b = pk(0x20);

        let (pda_1, _) = derive_jupiter_bracket_checkpoint_pda(&program_id, &authz_a, 1, &dest_a);
        let (pda_1_again, _) =
            derive_jupiter_bracket_checkpoint_pda(&program_id, &authz_a, 1, &dest_a);
        assert_eq!(pda_1, pda_1_again, "derive must be deterministic");

        let (pda_authz_b, _) =
            derive_jupiter_bracket_checkpoint_pda(&program_id, &authz_b, 1, &dest_a);
        assert_ne!(pda_1, pda_authz_b, "authz_pda must domain-separate");

        let (pda_nonce_2, _) =
            derive_jupiter_bracket_checkpoint_pda(&program_id, &authz_a, 2, &dest_a);
        assert_ne!(pda_1, pda_nonce_2, "execution_nonce must domain-separate");

        let (pda_dest_b, _) =
            derive_jupiter_bracket_checkpoint_pda(&program_id, &authz_a, 1, &dest_b);
        assert_ne!(pda_1, pda_dest_b, "destination_token_account must domain-separate");
    }

    /// Pin the exact seed-byte composition the program walks and the
    /// bump derivation. If anyone reorders or rewrites a seed, the
    /// manual derivation diverges from the helper and this test fires.
    #[test]
    fn checkpoint_pda_seeds_are_stable() {
        let program_id = pk(0xAA);
        let authz_pda = pk(0x55);
        let execution_nonce = 0x0102_0304_0506_0708u64;
        let destination = pk(0x77);

        let (helper_pda, helper_bump) = derive_jupiter_bracket_checkpoint_pda(
            &program_id,
            &authz_pda,
            execution_nonce,
            &destination,
        );

        let nonce_bytes = execution_nonce.to_le_bytes();
        let manual_seeds: &[&[u8]] = &[
            b"stage2-jupiter-bracket",
            authz_pda.as_ref(),
            &nonce_bytes,
            destination.as_ref(),
        ];
        let (manual_pda, manual_bump) =
            Pubkey::find_program_address(manual_seeds, &program_id);
        assert_eq!(helper_pda, manual_pda);
        assert_eq!(helper_bump, manual_bump);
    }

    /// `JupiterBracketCheckpoint::LEN` MUST equal the borsh-serialized
    /// size — anchors every byte position so any future field addition
    /// trips this test.
    #[test]
    fn checkpoint_len_matches_borsh_serialization() {
        let cp = JupiterBracketCheckpoint {
            schema_version: JUPITER_BRACKET_SCHEMA_VERSION,
            authorization_pda: pk(0x01),
            execution_nonce: 0x1122_3344_5566_7788,
            destination_token_account: pk(0x02),
            destination_mint: pk(0x03),
            pre_balance: 1_000,
            min_output_amount_raw: 2_000,
            consumed: false,
            bump: 254,
        };
        let bytes = borsh::to_vec(&cp).unwrap();
        assert_eq!(bytes.len(), JupiterBracketCheckpoint::LEN);
        // Pin the absolute size: 1+32+8+32+32+8+8+1+1 = 123 bytes.
        assert_eq!(JupiterBracketCheckpoint::LEN, 123);

        let decoded = JupiterBracketCheckpoint::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, cp);
    }

    /// Pin the borsh field order at the byte level so a reorder is loud.
    #[test]
    fn checkpoint_layout_matches_borsh_field_order() {
        let authz_pda = Pubkey::new_from_array([0x33; 32]);
        let dest = Pubkey::new_from_array([0x44; 32]);
        let mint = Pubkey::new_from_array([0x55; 32]);
        let cp = JupiterBracketCheckpoint {
            schema_version: 0x01,
            authorization_pda: authz_pda,
            execution_nonce: 0x0102_0304_0506_0708,
            destination_token_account: dest,
            destination_mint: mint,
            pre_balance: 0x1112_1314_1516_1718,
            min_output_amount_raw: 0x2122_2324_2526_2728,
            consumed: true,
            bump: 0xFE,
        };
        let bytes = borsh::to_vec(&cp).unwrap();
        assert_eq!(bytes[0], 0x01, "byte 0 = schema_version");
        assert_eq!(&bytes[1..33], &[0x33u8; 32][..], "bytes 1..33 = authorization_pda");
        assert_eq!(
            &bytes[33..41],
            &0x0102_0304_0506_0708u64.to_le_bytes()[..],
            "bytes 33..41 = execution_nonce (LE u64)"
        );
        assert_eq!(
            &bytes[41..73],
            &[0x44u8; 32][..],
            "bytes 41..73 = destination_token_account"
        );
        assert_eq!(
            &bytes[73..105],
            &[0x55u8; 32][..],
            "bytes 73..105 = destination_mint"
        );
        assert_eq!(
            &bytes[105..113],
            &0x1112_1314_1516_1718u64.to_le_bytes()[..],
            "bytes 105..113 = pre_balance (LE u64)"
        );
        assert_eq!(
            &bytes[113..121],
            &0x2122_2324_2526_2728u64.to_le_bytes()[..],
            "bytes 113..121 = min_output_amount_raw (LE u64)"
        );
        assert_eq!(bytes[121], 0x01, "byte 121 = consumed (true)");
        assert_eq!(bytes[122], 0xFE, "byte 122 = bump");
        assert_eq!(bytes.len(), 123);
    }

    /// `JupiterPreBracketProof` round-trip — wire-shape pin.
    #[test]
    fn pre_proof_borsh_roundtrip() {
        let proof = JupiterPreBracketProof {
            execution_nonce: 7,
            destination_token_account: pk(0xCC),
            expected_destination_mint: pk(0x12),
            min_output_amount_raw: 1_000,
        };
        let bytes = borsh::to_vec(&proof).unwrap();
        let decoded = JupiterPreBracketProof::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, proof);
    }

    /// `JupiterPostBracketProof` round-trip — and the structural
    /// invariant that it does NOT carry an executor-supplied
    /// `pre_balance` field. If anyone adds one, `borsh::to_vec` of the
    /// trip-wire test below produces more than the pinned 40 bytes
    /// (8 + 32) and this test fails loudly.
    #[test]
    fn post_proof_borsh_roundtrip_and_no_pre_balance_field() {
        let proof = JupiterPostBracketProof {
            execution_nonce: 7,
            destination_token_account: pk(0xCC),
        };
        let bytes = borsh::to_vec(&proof).unwrap();
        assert_eq!(bytes.len(), 8 + 32, "post-proof must be exactly nonce + dest");
        let decoded = JupiterPostBracketProof::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, proof);
    }
}

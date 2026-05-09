//! Stage 2 Solend delegated withdraw boundary skeleton (P4).
//!
//! # What this module is
//!
//! A *boundary verifier*. The on-chain `process_execute_action` path
//! calls `verify_solend_delegated_boundary` before mutating any
//! `AuthorizationRecord` field, when the action is
//! `SolendWithdrawAllDelegated`. The verifier enforces, in order:
//!
//! 1. Solend program-id pinning (against `SOLEND_PROGRAM_ID_MAINNET`).
//! 2. Obligation account-level owner is Solend (`account.owner ==
//!    SOLEND_PROGRAM_ID_MAINNET`).
//! 3. **Delegated obligation invariant** — the Solend
//!    `Obligation.owner` field MUST equal
//!    `AuthorizationRecord.delegated_wallet`. The user's main wallet
//!    case is rejected with a more specific error
//!    (`SolendMainWalletObligationRejected`) so attempted main-wallet
//!    attacks are pinpointable in the audit trail.
//! 4. Lending-market consistency between obligation fixture and proof.
//! 5. Destination pubkey identity against `AuthorizationRecord.destination`.
//! 6. Same-transaction `RefreshReserve + Withdraw` pair: exactly one
//!    relevant pair, in order, no extras and no conflicting Solend
//!    instructions for the same obligation/reserve.
//!
//! # What this module is NOT
//!
//! - **Not a Solend CPI.** No `invoke_signed`, no live Solend program
//!   call. The verifier only does program-side input validation.
//! - **Not a live obligation/reserve account parser.** Inputs are
//!   deterministic Borsh fixture structs supplied by the executor.
//!   Replacing the fixture inputs with on-chain account-bytes parsing
//!   is a future slice — see TODOs below.
//! - **Not an instructions-sysvar consumer.** Sibling-instruction
//!   verification runs over a deterministic
//!   `Vec<SiblingIxDescriptor>` the executor supplies. Replacing this
//!   with `solana_program::sysvar::instructions::*` is a future slice
//!   — see TODO at `verify_same_tx_refresh_and_withdraw`.
//! - **Not a Jupiter pre/post bracket.** Jupiter sibling-ix
//!   verification is a separate slice (Stage 2 spec § 6.1.3).
//!
//! # Why deterministic fixtures here, not live byte parsing
//!
//! The Stage 2 Solend research (`docs/STAGE2_SOLEND_APY_RESEARCH.md`
//! § 8 B-O1) calls out that the Solend `Reserve` Pack layout has
//! offsets that have NOT been re-verified end-to-end against the
//! mainnet `Pack::pack_into_slice` for several rate-config fields.
//! The on-chain crate has no parity-fixture coverage for live
//! obligation/reserve bytes either. Per the P4 prompt "Do not guess
//! Solend byte offsets... use deterministic fixture structs", we use
//! Borsh structs that the daemon will populate from RPC and the
//! eventual on-chain re-verifier will populate from decoded account
//! bytes. The boundary algorithm is the same; only the input source
//! changes.
//!
//! TODO(P5+): Replace `ObligationFixture` with on-chain decoding of
//! the obligation account passed in `accounts[..]`, using the verified
//! mainnet `Obligation` `Pack` layout
//! (`token-lending/sdk/src/state/obligation.rs`). Add a
//! short-buffer / unaligned-read failure path test the moment the
//! byte reader is introduced.
//!
//! TODO(P5+): Replace `Vec<SiblingIxDescriptor>` with consumption of
//! the instructions sysvar
//! (`Sysvar1nstructions1111111111111111111111111`,
//! `solana_program::sysvar::instructions::load_instruction_at_checked`).
//! The transaction-level account index for the sysvar must be added
//! to `accounts[..]` at the same time.
//!
//! # Sources consulted
//!
//! - Solend mainnet `token-lending/sdk/src/instruction.rs` —
//!   `LendingInstruction` ordinals: `RefreshReserve = 3`,
//!   `WithdrawObligationCollateralAndRedeemReserveCollateral = 15`.
//!   ([raw source](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/instruction.rs))
//! - Solend mainnet `token-lending/sdk/src/state/obligation.rs` —
//!   `Obligation.owner: Pubkey` is the wallet that controls the
//!   obligation; `lending_market: Pubkey` is the market the
//!   obligation is opened against.
//!   ([raw source](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/obligation.rs))
//! - Solend mainnet `token-lending/sdk/src/state/last_update.rs` —
//!   `STALE_AFTER_SLOTS_ELAPSED = 1`. Drives the same-tx
//!   `RefreshReserve + Withdraw` constraint.
//!   ([raw source](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/last_update.rs))
//! - Stage 2 spec `STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md`
//!   § 5 (Ownership Boundary, Execution Lifecycle, Withdraw cap),
//!   § 7.2 (variant 15 account ordering verbatim), § 14 (audit B-2).
//! - Repo pin parity: `crates/gateway/src/integrations/solend/raw.rs`
//!   line 17 — `SOLEND_PROGRAM_ID_BS58 =
//!   "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo"`. The on-chain
//!   crate inlines the same value to avoid pulling the gateway
//!   dependency into the BPF build.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{msg, program_error::ProgramError, pubkey::Pubkey};

use crate::error::AuthorityError;
use crate::state::AuthorizationRecord;

/// Solend mainnet program id, base58
/// `So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo`.
///
/// Pinned at compile time — the on-chain BPF build does not depend on
/// the gateway crate, so the constant has to travel with the
/// boundary. Parity with the gateway value is asserted in tests.
pub const SOLEND_PROGRAM_ID_MAINNET: Pubkey =
    solana_program::pubkey!("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo");

/// `LendingInstruction::RefreshReserve` ordinal in the mainnet
/// token-lending program. Source: `token-lending/sdk/src/instruction.rs`.
pub const SOLEND_VARIANT_REFRESH_RESERVE: u8 = 3;

/// `LendingInstruction::WithdrawObligationCollateralAndRedeemReserveCollateral`
/// ordinal in the mainnet token-lending program. Source:
/// `token-lending/sdk/src/instruction.rs`.
pub const SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM: u8 = 15;

/// Sanity bound on the deterministic sibling-instruction list that
/// the executor supplies. Picked at 16 — comfortably above the 2
/// Solend ixs the boundary actually needs (RefreshReserve + Withdraw)
/// while keeping wire size + per-ix walk cost predictable.
pub const MAX_SIBLING_INSTRUCTIONS: usize = 16;

/// Deterministic projection of a Solend Obligation account.
///
/// The two fields below correspond to (account-level owner) and
/// (Obligation data-struct `owner` field) respectively. They are
/// *both* called "owner" in different layers of Solana / Solend
/// vocabulary; the names below disambiguate the two:
///
/// - `account_owner_program` = `AccountInfo.owner` (the Solana
///   account-level owner, i.e. which program owns the account data;
///   for a Solend obligation this MUST be Solend).
/// - `obligation_authority` = `Obligation.owner` (the wallet pubkey
///   stored in the data struct; for Stage 2 this MUST be
///   `AuthorizationRecord.delegated_wallet` and MUST NOT be the
///   user's main wallet).
///
/// TODO(P5+): replace this fixture with on-chain decoding of the
/// passed obligation account.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct ObligationFixture {
    pub account_owner_program: Pubkey,
    pub obligation_authority: Pubkey,
    pub lending_market: Pubkey,
}

/// Deterministic descriptor of a single instruction sibling to the
/// `execute_action` ix in the same transaction.
///
/// `target_*` fields are populated for the two Solend variants that
/// the boundary cares about (RefreshReserve, Withdraw); for other
/// variants the fields are advisory and may be `None`.
///
/// TODO(P5+): replace this with consumption of the instructions
/// sysvar (`Sysvar1nstructions1111111111111111111111111`).
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct SiblingIxDescriptor {
    pub program_id: Pubkey,
    pub variant_byte: u8,
    pub target_reserve: Option<Pubkey>,
    pub target_obligation: Option<Pubkey>,
    pub target_lending_market: Option<Pubkey>,
    pub target_destination: Option<Pubkey>,
}

/// The Borsh payload supplied alongside `ExecuteAction` for the
/// `SolendWithdrawAllDelegated` action type.
///
/// Wire shape is part of the on-chain instruction wire shape; field
/// reorder is a wire break.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct SolendBoundaryProof {
    pub solend_program_id: Pubkey,
    pub obligation_pubkey: Pubkey,
    pub obligation: ObligationFixture,
    pub reserve_pubkey: Pubkey,
    pub lending_market_pubkey: Pubkey,
    pub destination_pubkey: Pubkey,
    pub sibling_instructions: Vec<SiblingIxDescriptor>,
}

/// Verify the Solend delegated withdraw boundary against an existing
/// `AuthorizationRecord`. Called from `process_execute_action`
/// **before** any state mutation. Returns `Err(_)` on any boundary
/// failure; the caller MUST propagate and refuse to mutate.
///
/// See module-level docs for the exact gate ordering.
pub fn verify_solend_delegated_boundary(
    record: &AuthorizationRecord,
    proof: &SolendBoundaryProof,
) -> Result<(), ProgramError> {
    // Gate 1 — pinned Solend program id.
    if proof.solend_program_id != SOLEND_PROGRAM_ID_MAINNET {
        return Err(AuthorityError::SolendProgramMismatch.into());
    }

    // Gate 2 — obligation account-level owner (account.owner) is Solend.
    if proof.obligation.account_owner_program != SOLEND_PROGRAM_ID_MAINNET {
        return Err(AuthorityError::SolendObligationOwnerMismatch.into());
    }

    // Gate 3 — delegated obligation invariant. The user main-wallet
    // case is checked first so the audit trail surfaces a specific
    // error code on attempted main-wallet attacks.
    if proof.obligation.obligation_authority == record.user {
        return Err(AuthorityError::SolendMainWalletObligationRejected.into());
    }
    if proof.obligation.obligation_authority != record.delegated_wallet {
        return Err(AuthorityError::SolendObligationAuthorityMismatch.into());
    }

    // Gate 4 — lending-market consistency between the obligation's
    // own `lending_market` field and the proof's pinned market.
    if proof.obligation.lending_market != proof.lending_market_pubkey {
        return Err(AuthorityError::SolendLendingMarketMismatch.into());
    }

    // Gate 5 — destination pubkey identity against the
    // AuthorizationRecord. TODO(P5+): also assert
    // `destination_pubkey` is an SPL token account owned by the
    // Token Program, with mint == expected liquidity mint and
    // authority == expected destination owner. For the skeleton, we
    // pin pubkey identity only.
    if proof.destination_pubkey != record.destination {
        return Err(AuthorityError::SolendDestinationMismatch.into());
    }

    // Gate 6 — same-tx RefreshReserve + Withdraw pair, no conflicts.
    if proof.sibling_instructions.len() > MAX_SIBLING_INSTRUCTIONS {
        return Err(AuthorityError::SolendBoundaryVerificationFailed.into());
    }
    verify_same_tx_refresh_and_withdraw(
        &proof.sibling_instructions,
        &proof.reserve_pubkey,
        &proof.obligation_pubkey,
        &proof.lending_market_pubkey,
        &proof.destination_pubkey,
    )?;

    msg!("solend_boundary ok");
    Ok(())
}

/// Walk the deterministic sibling-instruction list and require:
///
/// * exactly one Solend `RefreshReserve` (variant 3) targeting
///   `expected_reserve`,
/// * exactly one Solend `WithdrawObligationCollateralAndRedeemReserveCollateral`
///   (variant 15) targeting `expected_obligation` whose every other
///   account also matches the expected boundary,
/// * the refresh's index in the list is strictly less than the
///   withdraw's index,
/// * no other Solend instruction touches the expected reserve or
///   obligation (defends against e.g. a malicious second withdraw or
///   an unexpected `RepayObligationLiquidity` siphoning collateral).
///
/// Non-Solend instructions are ignored — the parent `execute_action`
/// itself, ATA-create ixs, sweep ixs, etc. are allowed to coexist in
/// the tx. (Verification of *those* sibling ixs is the Jupiter slice's
/// concern, not P4.)
///
/// TODO(P5+): drive this from the instructions sysvar instead of a
/// caller-supplied descriptor list.
fn verify_same_tx_refresh_and_withdraw(
    siblings: &[SiblingIxDescriptor],
    expected_reserve: &Pubkey,
    expected_obligation: &Pubkey,
    expected_lending_market: &Pubkey,
    expected_destination: &Pubkey,
) -> Result<(), ProgramError> {
    let mut refresh_index: Option<usize> = None;
    let mut withdraw_index: Option<usize> = None;

    for (i, ix) in siblings.iter().enumerate() {
        // Non-Solend ixs are ignored — they're outside this gate's
        // remit. Ordering invariants apply only among Solend ixs.
        if ix.program_id != SOLEND_PROGRAM_ID_MAINNET {
            continue;
        }
        match ix.variant_byte {
            SOLEND_VARIANT_REFRESH_RESERVE => {
                let target_reserve = ix
                    .target_reserve
                    .ok_or(AuthorityError::SolendBoundaryVerificationFailed)?;
                if &target_reserve == expected_reserve {
                    if refresh_index.is_some() {
                        return Err(
                            AuthorityError::SolendDuplicateOrConflictingInstruction.into(),
                        );
                    }
                    refresh_index = Some(i);
                }
                // RefreshReserve targeting an unrelated reserve is
                // allowed — the daemon may need to refresh multiple
                // reserves for unrelated reasons. Not our concern.
            }
            SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM => {
                let target_reserve = ix
                    .target_reserve
                    .ok_or(AuthorityError::SolendBoundaryVerificationFailed)?;
                let target_obligation = ix
                    .target_obligation
                    .ok_or(AuthorityError::SolendBoundaryVerificationFailed)?;
                let target_lending_market = ix
                    .target_lending_market
                    .ok_or(AuthorityError::SolendBoundaryVerificationFailed)?;
                let target_destination = ix
                    .target_destination
                    .ok_or(AuthorityError::SolendBoundaryVerificationFailed)?;

                if &target_obligation == expected_obligation {
                    // Targets OUR obligation — every other field must
                    // match or it's an attempted spoof.
                    if &target_reserve != expected_reserve {
                        return Err(AuthorityError::SolendReserveMismatch.into());
                    }
                    if &target_lending_market != expected_lending_market {
                        return Err(AuthorityError::SolendLendingMarketMismatch.into());
                    }
                    if &target_destination != expected_destination {
                        return Err(AuthorityError::SolendDestinationMismatch.into());
                    }
                    if withdraw_index.is_some() {
                        return Err(
                            AuthorityError::SolendDuplicateOrConflictingInstruction.into(),
                        );
                    }
                    withdraw_index = Some(i);
                } else if &target_reserve == expected_reserve {
                    // Targets our reserve but a different obligation
                    // — Stage 2 only allows one in-tx withdraw against
                    // our reserve, namely the one for our obligation.
                    return Err(
                        AuthorityError::SolendDuplicateOrConflictingInstruction.into(),
                    );
                }
                // else: unrelated obligation AND unrelated reserve —
                // an unrelated withdraw in the same tx is allowed.
            }
            _ => {
                // Some other Solend instruction. Fail closed if it
                // touches our target obligation OR reserve — that's
                // the "extra conflicting Solend instruction for the
                // same target" case. (Borrow / Repay / Liquidate / …
                // would all match this branch.)
                let touches_target = ix
                    .target_reserve
                    .map(|r| &r == expected_reserve)
                    .unwrap_or(false)
                    || ix
                        .target_obligation
                        .map(|o| &o == expected_obligation)
                        .unwrap_or(false);
                if touches_target {
                    return Err(
                        AuthorityError::SolendDuplicateOrConflictingInstruction.into(),
                    );
                }
            }
        }
    }

    let r = refresh_index.ok_or(AuthorityError::SolendRefreshMissing)?;
    let w = withdraw_index.ok_or(AuthorityError::SolendWithdrawMissing)?;
    if r >= w {
        return Err(AuthorityError::SolendInstructionOrderInvalid.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AuthorizationRecord, Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION};

    fn pk(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    /// Authz record fixture. user=0xAA, delegated_wallet=0xBB,
    /// destination=0xCC.
    fn record_fixture() -> AuthorizationRecord {
        AuthorizationRecord {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0xD0; 16],
            user: pk(0xAA),
            executor: pk(0xEE),
            delegated_wallet: pk(0xBB),
            canonical_rule_hash: [0; 32],
            allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk(0xCC),
            expires_at_slot: 1_000_000,
            revoked: false,
            completed: false,
            execution_nonce: 0,
            last_execution_slot: 0,
            bump: 255,
        }
    }

    /// The minimal "passing" sibling-ix list — one RefreshReserve, one
    /// Withdraw, in order, both targeting the expected accounts.
    fn good_sibling_list(
        reserve: Pubkey,
        obligation: Pubkey,
        lending_market: Pubkey,
        destination: Pubkey,
    ) -> Vec<SiblingIxDescriptor> {
        vec![
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_REFRESH_RESERVE,
                target_reserve: Some(reserve),
                target_obligation: None,
                target_lending_market: None,
                target_destination: None,
            },
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM,
                target_reserve: Some(reserve),
                target_obligation: Some(obligation),
                target_lending_market: Some(lending_market),
                target_destination: Some(destination),
            },
        ]
    }

    fn good_proof(record: &AuthorizationRecord) -> SolendBoundaryProof {
        let reserve = pk(0x10);
        let obligation = pk(0x11);
        let lending_market = pk(0x01);
        let destination = record.destination;
        SolendBoundaryProof {
            solend_program_id: SOLEND_PROGRAM_ID_MAINNET,
            obligation_pubkey: obligation,
            obligation: ObligationFixture {
                account_owner_program: SOLEND_PROGRAM_ID_MAINNET,
                obligation_authority: record.delegated_wallet,
                lending_market,
            },
            reserve_pubkey: reserve,
            lending_market_pubkey: lending_market,
            destination_pubkey: destination,
            sibling_instructions: good_sibling_list(reserve, obligation, lending_market, destination),
        }
    }

    fn assert_err(actual: ProgramError, expected: AuthorityError) {
        assert_eq!(
            actual,
            ProgramError::Custom(expected as u32),
            "expected {expected:?} (code {}); got {actual:?}",
            expected as u32,
        );
    }

    // ── Pinning ────────────────────────────────────────────────────────

    #[test]
    fn solend_program_id_matches_repo_pin() {
        // Parity with `crates/gateway/src/integrations/solend/raw.rs`
        // line 17. Asserted here as a literal so the on-chain crate
        // doesn't depend on the gateway crate.
        assert_eq!(
            SOLEND_PROGRAM_ID_MAINNET.to_string(),
            "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo",
        );
    }

    #[test]
    fn solend_variant_bytes_match_official_source() {
        // Source: solendprotocol/solana-program-library mainnet branch,
        // token-lending/sdk/src/instruction.rs LendingInstruction enum.
        assert_eq!(SOLEND_VARIANT_REFRESH_RESERVE, 3);
        assert_eq!(
            SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM, 15,
        );
    }

    // ── Happy path ────────────────────────────────────────────────────

    #[test]
    fn delegated_obligation_passes() {
        let record = record_fixture();
        let proof = good_proof(&record);
        verify_solend_delegated_boundary(&record, &proof).unwrap();
    }

    // ── Delegated obligation invariant — REQUIRED POISON TEST ─────────

    #[test]
    fn main_wallet_obligation_rejected_with_specific_error() {
        // Required by P4 prompt: an obligation fixture whose
        // authority == user main wallet must fail with a SPECIFIC
        // SolendMainWalletObligationRejected error (NOT just a
        // generic wrong-wallet error).
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.obligation.obligation_authority = record.user; // poison
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendMainWalletObligationRejected);
    }

    #[test]
    fn random_wrong_wallet_rejected_with_authority_mismatch() {
        // Sibling test for completeness: a non-user, non-delegated
        // wallet authority is rejected via the *generic* mismatch
        // error (so the main-wallet test above proves the dedicated
        // error path is genuinely separate).
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.obligation.obligation_authority = pk(0x99); // not user, not delegated
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendObligationAuthorityMismatch);
    }

    #[test]
    fn obligation_account_owner_must_be_solend_program() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.obligation.account_owner_program = pk(0xDE); // not Solend
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendObligationOwnerMismatch);
    }

    // ── Pubkey-identity gates ─────────────────────────────────────────

    #[test]
    fn wrong_solend_program_id_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.solend_program_id = pk(0x77); // not Solend
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendProgramMismatch);
    }

    #[test]
    fn wrong_lending_market_within_obligation_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.obligation.lending_market = pk(0x99); // diverges from proof.lending_market_pubkey
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendLendingMarketMismatch);
    }

    #[test]
    fn wrong_destination_against_record_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.destination_pubkey = pk(0x77); // not record.destination
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendDestinationMismatch);
    }

    // ── Sibling-ix gates ──────────────────────────────────────────────

    #[test]
    fn missing_refresh_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions.remove(0); // drop the refresh
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendRefreshMissing);
    }

    #[test]
    fn missing_withdraw_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions.remove(1); // drop the withdraw
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendWithdrawMissing);
    }

    #[test]
    fn wrong_order_refresh_after_withdraw_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions.swap(0, 1);
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendInstructionOrderInvalid);
    }

    #[test]
    fn malicious_extra_withdraw_for_same_obligation_rejected() {
        // valid refresh + malicious withdraw + valid withdraw
        // The duplicate withdraw fires the conflict check before the
        // legitimate one is recorded.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        let valid_withdraw = proof.sibling_instructions[1].clone();
        // Insert an additional withdraw at index 1 (between refresh and
        // valid withdraw) — same obligation, valid fields.
        proof
            .sibling_instructions
            .insert(1, valid_withdraw.clone());
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendDuplicateOrConflictingInstruction);
    }

    #[test]
    fn extra_solend_instruction_touching_same_obligation_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // Attach a Solend "BorrowObligationLiquidity"-shaped ix
        // (variant 10, irrelevant for our boundary semantics) that
        // happens to target our obligation.
        proof.sibling_instructions.push(SiblingIxDescriptor {
            program_id: SOLEND_PROGRAM_ID_MAINNET,
            variant_byte: 10, // BorrowObligationLiquidity
            target_reserve: None,
            target_obligation: Some(proof.obligation_pubkey),
            target_lending_market: None,
            target_destination: None,
        });
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendDuplicateOrConflictingInstruction);
    }

    #[test]
    fn extra_solend_instruction_touching_same_reserve_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions.push(SiblingIxDescriptor {
            program_id: SOLEND_PROGRAM_ID_MAINNET,
            variant_byte: 11, // RepayObligationLiquidity
            target_reserve: Some(proof.reserve_pubkey),
            target_obligation: None,
            target_lending_market: None,
            target_destination: None,
        });
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendDuplicateOrConflictingInstruction);
    }

    #[test]
    fn unrelated_solend_refresh_for_different_reserve_allowed() {
        // Only the refresh+withdraw targeting *our* reserve/obligation
        // matters. An unrelated refresh that happens to share a tx is
        // not a conflict — daemons may refresh multiple reserves in
        // one tx for unrelated reasons.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions.insert(
            0,
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_REFRESH_RESERVE,
                target_reserve: Some(pk(0x99)), // unrelated reserve
                target_obligation: None,
                target_lending_market: None,
                target_destination: None,
            },
        );
        verify_solend_delegated_boundary(&record, &proof).unwrap();
    }

    #[test]
    fn non_solend_sibling_instructions_are_ignored() {
        // The parent `execute_action` ix and unrelated ixs (e.g. an
        // ATA-create from the system program) must not be flagged.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions.insert(
            0,
            SiblingIxDescriptor {
                program_id: pk(0x12), // not Solend, not anything special
                variant_byte: 0,
                target_reserve: None,
                target_obligation: None,
                target_lending_market: None,
                target_destination: None,
            },
        );
        verify_solend_delegated_boundary(&record, &proof).unwrap();
    }

    #[test]
    fn withdraw_targeting_our_obligation_with_wrong_reserve_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions[1].target_reserve = Some(pk(0x42)); // not proof.reserve_pubkey
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendReserveMismatch);
    }

    #[test]
    fn withdraw_targeting_our_obligation_with_wrong_lending_market_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions[1].target_lending_market = Some(pk(0x42));
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendLendingMarketMismatch);
    }

    #[test]
    fn withdraw_targeting_our_obligation_with_wrong_destination_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions[1].target_destination = Some(pk(0x42));
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendDestinationMismatch);
    }

    #[test]
    fn withdraw_for_different_obligation_on_same_reserve_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // Insert a withdraw of a different obligation but same reserve
        // BEFORE the legitimate one. This must fail (we never want
        // anyone else's withdraw against our reserve in the same tx).
        proof.sibling_instructions.insert(
            1,
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM,
                target_reserve: Some(proof.reserve_pubkey),
                target_obligation: Some(pk(0xEF)), // different obligation
                target_lending_market: Some(proof.lending_market_pubkey),
                target_destination: Some(proof.destination_pubkey),
            },
        );
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendDuplicateOrConflictingInstruction);
    }

    #[test]
    fn refresh_descriptor_missing_target_reserve_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions[0].target_reserve = None;
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendBoundaryVerificationFailed);
    }

    #[test]
    fn withdraw_descriptor_missing_target_obligation_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions[1].target_obligation = None;
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendBoundaryVerificationFailed);
    }

    #[test]
    fn sibling_list_too_long_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // Pad with allowed unrelated ixs until we exceed the cap.
        for _ in 0..MAX_SIBLING_INSTRUCTIONS {
            proof.sibling_instructions.push(SiblingIxDescriptor {
                program_id: pk(0x12),
                variant_byte: 0,
                target_reserve: None,
                target_obligation: None,
                target_lending_market: None,
                target_destination: None,
            });
        }
        assert!(proof.sibling_instructions.len() > MAX_SIBLING_INSTRUCTIONS);
        let err = verify_solend_delegated_boundary(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::SolendBoundaryVerificationFailed);
    }

    #[test]
    fn boundary_proof_borsh_roundtrip() {
        let record = record_fixture();
        let proof = good_proof(&record);
        let bytes = borsh::to_vec(&proof).unwrap();
        let decoded = SolendBoundaryProof::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, proof);
    }

    #[test]
    fn obligation_fixture_borsh_roundtrip() {
        let original = ObligationFixture {
            account_owner_program: SOLEND_PROGRAM_ID_MAINNET,
            obligation_authority: pk(0x42),
            lending_market: pk(0x01),
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = ObligationFixture::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn sibling_descriptor_borsh_roundtrip() {
        let original = SiblingIxDescriptor {
            program_id: SOLEND_PROGRAM_ID_MAINNET,
            variant_byte: SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM,
            target_reserve: Some(pk(0x10)),
            target_obligation: Some(pk(0x11)),
            target_lending_market: Some(pk(0x01)),
            target_destination: Some(pk(0xCC)),
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = SiblingIxDescriptor::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, original);
    }
}

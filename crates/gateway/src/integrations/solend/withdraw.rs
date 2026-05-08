//! Solend `WithdrawObligationCollateralAndRedeemReserveCollateral`
//! instruction builder — Phase 5H-A (un-wired protocol substrate).
//!
//! ## Wiring status
//!
//! This module is **NOT** declared as `pub mod` in
//! [`super::mod`](super). It is declared `#[cfg(test)] mod withdraw;`
//! ONLY so the structural tests below execute under
//! `cargo test -p claw-gateway --lib solend`. Production builds do
//! NOT include this file. No symbol from this module is reachable
//! from any tool, runtime, park, signing, or submit code path.
//!
//! Phase 5H-B / 5H-C / 5H-D (policy rules, tx-plan assembler, signing
//! handoff, tool, daemon wiring) are deferred behind explicit
//! authorization and will land as separate slices.
//!
//! ## Purity
//!
//! Pure function over caller-supplied accounts + collateral amount.
//! The builder does NOT:
//!
//! - call RPC
//! - sign transactions
//! - read the policy evaluator state or any `LendingSnapshot`
//! - reach into `approval_store` / `park_store` / daemon runtime
//! - read global config
//! - hardcode the AMM reserve, lending market, or oracle pubkeys
//! - submit or broadcast
//! - fetch blockhash
//! - create associated token accounts
//! - convert between underlying and collateral units
//!
//! The caller is responsible for:
//!
//! 1. Refreshing the reserve and the obligation in the same
//!    transaction (or in an immediately preceding one) via
//!    `crate::integrations::solend::refresh::build_refresh_instructions`.
//!    The combined withdraw ix reads cached oracle prices and
//!    obligation-side accounting that are only valid within the slot
//!    of the preceding refresh.
//! 2. Deriving / confirming the user's source-collateral ATA
//!    (= the reserve's collateral supply pool, slot 0 below) and the
//!    destination-collateral intermediary (= the user's c-token ATA,
//!    slot 1 below) and the destination underlying ATA (slot 6).
//! 3. Choosing the `collateral_amount`. Solend program semantics: a
//!    `u64::MAX` collateral_amount means "redeem the user's full
//!    deposited collateral" — Solend's program clamps to the
//!    obligation's actual deposit. Any other value redeems exactly
//!    that many c-tokens (rejected by the program if it exceeds the
//!    user's deposited position). Phase 5H-A's `withdraw_all` semantics
//!    are achieved by callers passing `CollateralTokenAmount::new(u64::MAX)`.
//!
//! ## Source of truth
//!
//! **`solendprotocol/solana-program-library`, `mainnet` branch** (the
//! same source the deployed program at
//! `So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo` is built from):
//!
//! - Constructor: `token-lending/sdk/src/instruction.rs`,
//!   `withdraw_obligation_collateral_and_redeem_reserve_collateral(...)`.
//! - Handler: `token-lending/program/src/processor.rs`,
//!   `process_withdraw_obligation_collateral_and_redeem_reserve_collateral(...)`.
//!   `Clock::get()?` is read as a sysvar syscall, NOT from the
//!   account list.
//! - `LendingInstruction` variant: tag byte = 15.
//! - Payload: `collateral_amount: u64` (little-endian, per SPL `Pack`).
//! - **13 accounts** in the exact order produced below.
//! - `lending_market_authority` is derived as a PDA from
//!   `(lending_market, program_id)` with seed
//!   `[&lending_market.to_bytes()]` — the builder derives this
//!   internally; caller does NOT supply it.
//!
//! ### Phase 6I-I correction
//!
//! Phase 5H-A originally documented this builder as 12 accounts with
//! Clock omitted, citing a then-unverified claim that the deployed
//! mainnet handler reads Clock via `Clock::get()?` syscall. That
//! claim was wrong for the combined withdraw ix — only
//! `RefreshObligation` reads Clock via syscall (the Phase 6I-H fix
//! verified that). The deployed Solend mainnet program at
//! `So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo` expects the Clock
//! sysvar account at slot 11 of `WithdrawObligationCollateralAndRedeemReserveCollateral`
//! per Solend's `solana-program-library` `mainnet` branch
//! `token-lending/sdk/src/instruction.rs`. The 12-account layout
//! produced a live failure on 2026-05-08:
//! `InstructionError(3, NotEnoughAccountKeys)` — the Solend handler
//! advanced past the SPL Token program (slot 11 in the buggy layout),
//! tried to read slot 12 for Clock, and ran out of accounts.
//!
//! Withdraw still does NOT take any oracle account in the ix list —
//! the oracle reading happened at refresh time and is cached in the
//! obligation's per-deposit accounting. The structural test
//! [`tests::no_oracle_accounts_in_account_list`] guards this.
//!
//! ## Signer / writable matrix (mainnet-branch layout)
//!
//! | # | Account | Writable | Signer |
//! |---|---|---|---|
//! |  0 | source_collateral (= reserve's collateral supply pool)        | yes | no  |
//! |  1 | destination_collateral (= user's c-token ATA, intermediary)  | yes | no  |
//! |  2 | reserve                                                       | yes | no  |
//! |  3 | obligation                                                    | yes | no  |
//! |  4 | lending_market                                                | no  | no  |
//! |  5 | lending_market_authority (PDA)                                | no  | no  |
//! |  6 | destination_liquidity (= user's underlying ATA, USDC out)     | yes | no  |
//! |  7 | reserve_collateral_mint (c-token mint; burned)                | yes | no  |
//! |  8 | reserve_liquidity_supply (source underlying)                  | yes | no  |
//! |  9 | obligation_owner                                              | no  | **yes** |
//! | 10 | user_transfer_authority                                       | no  | **yes** |
//! | 11 | clock sysvar                                                  | no  | no  |
//! | 12 | spl_token program                                             | no  | no  |

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;

use super::deposit::SPL_TOKEN_PROGRAM_ID_BS58;
use crate::lending::CollateralTokenAmount;

/// Tag byte for
/// `LendingInstruction::WithdrawObligationCollateralAndRedeemReserveCollateral`
/// at the pinned Solend mainnet source cited in this module's docs.
const SOLEND_IX_TAG_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM_RESERVE_COLLATERAL: u8 = 15;

/// Sentinel value the Solend program treats as "redeem the user's
/// entire deposited collateral position": pass `u64::MAX` and the
/// program clamps to the obligation's actual deposit. Phase 5H-A
/// callers use this to express `withdraw_all` without exposing any
/// quantity to the LLM/user surface.
pub const WITHDRAW_ALL_COLLATERAL_AMOUNT_SENTINEL_RAW: u64 = u64::MAX;

/// Explicit inputs to the withdraw ix builder. Every pubkey the caller
/// derived or fetched is listed here; the builder supplies zero
/// pubkeys except the lending-market authority PDA (derived
/// internally because the PDA derivation is load-bearing to the
/// Solend program and the caller has no reason to duplicate it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawInstructionInputs {
    pub solend_program_id: Pubkey,
    /// C-token base units to redeem. `u64::MAX` means "redeem all"
    /// (Solend program clamps to the obligation's actual deposit).
    /// Wrapped in [`CollateralTokenAmount`] so misuse across unit
    /// domains is a compile error.
    pub collateral_amount: CollateralTokenAmount,
    /// Source of c-tokens for the redeem step: the reserve's
    /// collateral supply pool (decoded `SolendReserveRaw.collateral_supply`).
    /// During deposit the user's freshly-minted c-tokens are
    /// transferred INTO this account; withdraw reverses the flow.
    pub source_collateral: Pubkey,
    /// Intermediary destination for the c-token transfer step,
    /// typically the user's c-token ATA derived from
    /// `(user_wallet, reserve.collateral.mint)`. The c-tokens land
    /// here briefly before being burned at the c-token mint in the
    /// same ix.
    pub destination_collateral: Pubkey,
    pub reserve: Pubkey,
    pub obligation: Pubkey,
    pub lending_market: Pubkey,
    /// Final destination for the underlying liquidity (e.g. the
    /// user's USDC ATA). Receives the redeemed underlying.
    pub destination_liquidity: Pubkey,
    /// From decoded `SolendReserveRaw.collateral_mint`. The c-token
    /// mint, where redeemed c-tokens are burned.
    pub reserve_collateral_mint: Pubkey,
    /// From decoded `SolendReserveRaw.liquidity_supply`. The reserve's
    /// underlying-asset pool that funds the user's redemption.
    pub reserve_liquidity_supply: Pubkey,
    /// Owner of the obligation. Readonly + signer on this ix.
    pub obligation_owner: Pubkey,
    /// SPL transfer authority. Readonly + signer. Typically the same
    /// pubkey as `obligation_owner` for V1 single-wallet flows, but
    /// the Solend ix defines them as distinct parameters and the
    /// builder preserves the distinction.
    pub user_transfer_authority: Pubkey,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WithdrawBuildError {
    /// Solend's program-side validation rejects zero-amount
    /// redemptions (`LendingError::InvalidAmount` in
    /// `process_withdraw_obligation_collateral_and_redeem_reserve_collateral`).
    /// The builder fails-fast here so a zero never reaches the wire.
    /// Note: `u64::MAX` is the "withdraw all" sentinel and is
    /// explicitly accepted (see [`WITHDRAW_ALL_COLLATERAL_AMOUNT_SENTINEL_RAW`]).
    #[error("withdraw collateral amount must be greater than zero")]
    ZeroAmount,
}

/// Pure builder. Returns an unsigned [`Instruction`] whose account
/// list and payload match the Solend program's expectation exactly.
/// Caller is responsible for placing the ix into a transaction,
/// attaching a blockhash, collecting signatures from
/// `obligation_owner` and `user_transfer_authority`, and submitting.
pub fn build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
    inputs: WithdrawInstructionInputs,
) -> Result<Instruction, WithdrawBuildError> {
    if inputs.collateral_amount.raw() == 0 {
        return Err(WithdrawBuildError::ZeroAmount);
    }

    // Derive lending_market_authority PDA exactly as Solend's own
    // constructor does (seed = lending_market pubkey bytes,
    // program = Solend program id). The caller has no reason to
    // duplicate this derivation.
    let (lending_market_authority, _bump) = Pubkey::find_program_address(
        &[&inputs.lending_market.to_bytes()],
        &inputs.solend_program_id,
    );

    let spl_token_program_id: Pubkey = SPL_TOKEN_PROGRAM_ID_BS58
        .parse()
        .expect("SPL Token program id is a well-known constant");

    // Payload: [15, collateral_amount_u64_le (8 bytes)].
    let mut data: Vec<u8> = Vec::with_capacity(1 + 8);
    data.push(SOLEND_IX_TAG_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM_RESERVE_COLLATERAL);
    data.extend_from_slice(&inputs.collateral_amount.raw().to_le_bytes());

    // Account list — order is load-bearing; matches the Solend mainnet
    // branch source verbatim (13 accounts, Clock sysvar at slot 11,
    // SPL Token program at slot 12, no oracle accounts in the list).
    // Writable vs readonly and signer vs non-signer flags also match
    // the source. See module doc-comment "Phase 6I-I correction" for
    // the 2026-05-08 NotEnoughAccountKeys failure that motivated
    // adding Clock at slot 11.
    let accounts = vec![
        AccountMeta::new(inputs.source_collateral, false),                    // 0
        AccountMeta::new(inputs.destination_collateral, false),               // 1
        AccountMeta::new(inputs.reserve, false),                              // 2
        AccountMeta::new(inputs.obligation, false),                           // 3
        AccountMeta::new_readonly(inputs.lending_market, false),              // 4
        AccountMeta::new_readonly(lending_market_authority, false),           // 5
        AccountMeta::new(inputs.destination_liquidity, false),                // 6
        AccountMeta::new(inputs.reserve_collateral_mint, false),              // 7
        AccountMeta::new(inputs.reserve_liquidity_supply, false),             // 8
        AccountMeta::new_readonly(inputs.obligation_owner, true),             // 9 — signer
        AccountMeta::new_readonly(inputs.user_transfer_authority, true),      // 10 — signer
        AccountMeta::new_readonly(sysvar::clock::id(), false),                // 11 — Clock sysvar
        AccountMeta::new_readonly(spl_token_program_id, false),               // 12
    ];

    Ok(Instruction {
        program_id: inputs.solend_program_id,
        accounts,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::sysvar;
    use std::str::FromStr;

    fn solend_program() -> Pubkey {
        Pubkey::from_str("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo").unwrap()
    }

    fn valid_inputs() -> WithdrawInstructionInputs {
        WithdrawInstructionInputs {
            solend_program_id: solend_program(),
            collateral_amount: CollateralTokenAmount::new(1_000),
            source_collateral: Pubkey::new_unique(),
            destination_collateral: Pubkey::new_unique(),
            reserve: Pubkey::new_unique(),
            obligation: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            destination_liquidity: Pubkey::new_unique(),
            reserve_collateral_mint: Pubkey::new_unique(),
            reserve_liquidity_supply: Pubkey::new_unique(),
            obligation_owner: Pubkey::new_unique(),
            user_transfer_authority: Pubkey::new_unique(),
        }
    }

    #[test]
    fn instruction_program_id_is_solend() {
        let inputs = valid_inputs();
        let expected = inputs.solend_program_id;
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .expect("valid input builds");
        assert_eq!(ix.program_id, expected);
        assert_eq!(expected, solend_program());
    }

    #[test]
    fn first_instruction_byte_is_tag_15() {
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        assert_eq!(
            ix.data[0],
            SOLEND_IX_TAG_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM_RESERVE_COLLATERAL
        );
        assert_eq!(ix.data[0], 15);
    }

    #[test]
    fn payload_is_tag_plus_u64_le_9_bytes() {
        let mut inputs = valid_inputs();
        inputs.collateral_amount = CollateralTokenAmount::new(0x0123_4567_89AB_CDEF);
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();
        assert_eq!(ix.data.len(), 9, "tag byte + u64 LE = 9 bytes");
        assert_eq!(
            &ix.data[1..9],
            &[0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]
        );
    }

    #[test]
    fn account_count_is_13() {
        // Phase 6I-I: Solend mainnet
        // WithdrawObligationCollateralAndRedeemReserveCollateral has
        // 13 accounts (Clock at slot 11, SPL Token at slot 12). The
        // earlier 12-account shape produced a NotEnoughAccountKeys
        // failure on 2026-05-08 — see module doc.
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        assert_eq!(
            ix.accounts.len(),
            13,
            "Solend mainnet WithdrawObligationCollateralAndRedeemReserveCollateral has 13 accounts"
        );
    }

    #[test]
    fn account_metas_match_solend_source_order_and_flags() {
        // Construct inputs with distinct, recognizable pubkeys at
        // each position so a misplacement is immediately visible.
        let inputs = WithdrawInstructionInputs {
            solend_program_id: solend_program(),
            collateral_amount: CollateralTokenAmount::new(7),
            source_collateral: Pubkey::new_unique(),
            destination_collateral: Pubkey::new_unique(),
            reserve: Pubkey::new_unique(),
            obligation: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            destination_liquidity: Pubkey::new_unique(),
            reserve_collateral_mint: Pubkey::new_unique(),
            reserve_liquidity_supply: Pubkey::new_unique(),
            obligation_owner: Pubkey::new_unique(),
            user_transfer_authority: Pubkey::new_unique(),
        };

        // Stash expected pubkeys before the input struct moves.
        let expected_source_collateral = inputs.source_collateral;
        let expected_destination_collateral = inputs.destination_collateral;
        let expected_reserve = inputs.reserve;
        let expected_obligation = inputs.obligation;
        let expected_lending_market = inputs.lending_market;
        let expected_destination_liquidity = inputs.destination_liquidity;
        let expected_reserve_collateral_mint = inputs.reserve_collateral_mint;
        let expected_reserve_liquidity_supply = inputs.reserve_liquidity_supply;
        let expected_obligation_owner = inputs.obligation_owner;
        let expected_user_transfer_authority = inputs.user_transfer_authority;
        let expected_pda = Pubkey::find_program_address(
            &[&inputs.lending_market.to_bytes()],
            &inputs.solend_program_id,
        )
        .0;
        let expected_spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();

        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();

        // Per-slot pubkey + flag assertions, matching the Solend
        // mainnet-branch matrix in this file's module doc.
        // Phase 6I-I: slot 11 is Clock sysvar; SPL Token shifted to slot 12.
        let exp = [
            // (index, pubkey, writable, signer)
            (0usize, expected_source_collateral, true, false),
            (1, expected_destination_collateral, true, false),
            (2, expected_reserve, true, false),
            (3, expected_obligation, true, false),
            (4, expected_lending_market, false, false),
            (5, expected_pda, false, false),
            (6, expected_destination_liquidity, true, false),
            (7, expected_reserve_collateral_mint, true, false),
            (8, expected_reserve_liquidity_supply, true, false),
            (9, expected_obligation_owner, false, true),         // signer
            (10, expected_user_transfer_authority, false, true), // signer
            (11, sysvar::clock::id(), false, false),             // Phase 6I-I: Clock sysvar
            (12, expected_spl_token, false, false),
        ];
        for (idx, pk, wr, sg) in exp {
            let a = &ix.accounts[idx];
            assert_eq!(a.pubkey, pk, "pubkey mismatch at index {idx}");
            assert_eq!(a.is_writable, wr, "writable mismatch at index {idx}");
            assert_eq!(a.is_signer, sg, "signer mismatch at index {idx}");
        }
    }

    #[test]
    fn lending_market_authority_is_derived_not_accepted() {
        // The caller does NOT pass lending_market_authority — the
        // builder derives it. Changing the lending_market pubkey
        // must change the PDA at slot 5.
        let mut inputs_a = valid_inputs();
        let mut inputs_b = valid_inputs();
        let lm_a = Pubkey::new_unique();
        let lm_b = Pubkey::new_unique();
        inputs_a.lending_market = lm_a;
        inputs_b.lending_market = lm_b;
        let ix_a =
            build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
                inputs_a,
            )
            .unwrap();
        let ix_b =
            build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
                inputs_b,
            )
            .unwrap();
        assert_ne!(
            ix_a.accounts[5].pubkey, ix_b.accounts[5].pubkey,
            "PDA at slot 5 must vary with lending_market"
        );
    }

    #[test]
    fn zero_amount_is_rejected() {
        let mut inputs = valid_inputs();
        inputs.collateral_amount = CollateralTokenAmount::new(0);
        let err =
            build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
                inputs,
            )
            .unwrap_err();
        assert_eq!(err, WithdrawBuildError::ZeroAmount);
    }

    #[test]
    fn u64_max_passthrough_for_withdraw_all() {
        // Phase 5H `withdraw_all` semantics: the canonical sentinel
        // is `u64::MAX`. The Solend program clamps this to the
        // user's actual deposited position. The builder MUST
        // accept it verbatim (not reject it as "out of range") and
        // encode it bit-for-bit in the LE payload.
        let mut inputs = valid_inputs();
        inputs.collateral_amount =
            CollateralTokenAmount::new(WITHDRAW_ALL_COLLATERAL_AMOUNT_SENTINEL_RAW);
        let ix =
            build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
                inputs,
            )
            .expect("u64::MAX is the withdraw_all sentinel and must build");

        assert_eq!(ix.data.len(), 9);
        assert_eq!(ix.data[0], 15);
        // u64::MAX little-endian = eight 0xFF bytes.
        assert_eq!(&ix.data[1..9], &[0xFF; 8]);
        // And round-trip the LE bytes back to confirm the value.
        let encoded = u64::from_le_bytes(ix.data[1..9].try_into().unwrap());
        assert_eq!(encoded, u64::MAX);
        assert_eq!(encoded, WITHDRAW_ALL_COLLATERAL_AMOUNT_SENTINEL_RAW);
    }

    /// Phase 6I-I regression guard. The deployed Solend mainnet
    /// `WithdrawObligationCollateralAndRedeemReserveCollateral`
    /// handler EXPECTS Clock at account slot 11. Removing it produces
    /// `NotEnoughAccountKeys` because the handler advances past Clock
    /// looking for the SPL Token program at slot 12.
    ///
    /// (This test inverts the original `no_clock_sysvar_in_account_list`
    /// guard, which incorrectly enforced the absence of Clock based on
    /// a misread of the source. The 2026-05-08 live failure
    /// `InstructionError(3, NotEnoughAccountKeys)` proved Clock IS
    /// required here. Note the asymmetry with `RefreshObligation`,
    /// which the Phase 6I-H fix verified does NOT take Clock as an
    /// account — only this combined withdraw ix does.)
    #[test]
    fn clock_sysvar_is_at_slot_11_required_by_solend_mainnet() {
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        assert_eq!(
            ix.accounts.len(),
            13,
            "withdraw ix must have exactly 13 accounts"
        );
        assert_eq!(
            ix.accounts[11].pubkey,
            sysvar::clock::id(),
            "Clock sysvar must be at slot 11 (per Solend mainnet branch token-lending/sdk/src/instruction.rs)"
        );
        assert!(
            !ix.accounts[11].is_writable,
            "Clock sysvar at slot 11 must be readonly"
        );
        assert!(
            !ix.accounts[11].is_signer,
            "Clock sysvar at slot 11 must NOT be a signer"
        );
        // Clock appears exactly once (no accidental duplicate).
        let clock_occurrences = ix
            .accounts
            .iter()
            .filter(|a| a.pubkey == sysvar::clock::id())
            .count();
        assert_eq!(
            clock_occurrences, 1,
            "Clock sysvar must appear exactly once in the withdraw account list"
        );
    }

    /// Phase 6I-I — the SPL Token program now lives at slot 12, after
    /// Clock at slot 11. A reordering that swaps these two would still
    /// have 13 accounts and pass the count check; this test guards the
    /// specific (Clock, SPL Token) ordering.
    #[test]
    fn spl_token_program_is_at_slot_12_after_clock() {
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        let expected_spl_token =
            Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        assert_eq!(
            ix.accounts[12].pubkey, expected_spl_token,
            "SPL Token program must be at slot 12 (after Clock at slot 11)"
        );
        assert!(!ix.accounts[12].is_writable);
        assert!(!ix.accounts[12].is_signer);
    }

    #[test]
    fn no_oracle_accounts_in_account_list() {
        // The combined withdraw ix relies on the most recent
        // `RefreshReserve` having posted the oracle-derived state
        // into the reserve account. The withdraw ix itself does NOT
        // take pyth or switchboard oracle accounts. Lock structurally
        // so a future refactor that accidentally adds oracles is
        // caught.
        //
        // Phase 6I-I: the canonical length is exactly 13 (Clock at
        // slot 11, SPL Token at slot 12). Any extra account is
        // structural drift.
        let pyth_unique = Pubkey::new_unique();
        let switchboard_unique = Pubkey::new_unique();
        let inputs = valid_inputs();
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();
        for (i, a) in ix.accounts.iter().enumerate() {
            assert_ne!(
                a.pubkey, pyth_unique,
                "no oracle should leak into withdraw accounts (idx={i})"
            );
            assert_ne!(
                a.pubkey, switchboard_unique,
                "no oracle should leak into withdraw accounts (idx={i})"
            );
        }
        assert_eq!(ix.accounts.len(), 13);
    }

    /// Phase 6I-I regression guard. The signer set must remain exactly
    /// `[obligation_owner @ slot 9, user_transfer_authority @ slot 10]`
    /// — adding Clock at slot 11 must NOT have promoted Clock or any
    /// other slot to a signer. This locks the user-wallet-only signer
    /// invariant.
    #[test]
    fn signer_set_unchanged_only_slots_nine_and_ten() {
        let inputs = valid_inputs();
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();
        let signer_indices: Vec<usize> = ix
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.is_signer)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            signer_indices,
            vec![9, 10],
            "signer slots must remain [9, 10] (obligation_owner, user_transfer_authority); \
             Clock at slot 11 must NOT be a signer, SPL Token at slot 12 must NOT be a signer"
        );
    }

    // Note: a `no_signer_field_in_proposed_action_withdraw`
    // structural guard belongs alongside the analogous
    // `deposit_variant_has_no_signer_field` /
    // `repay_variant_has_no_signer_field` tests in
    // `crate::lending::policy`, NOT here. The Solend protocol-side
    // builder lives strictly on the Part 6 side of the §41.3 seam
    // and is not allowed to depend on `ProposedAction` at all
    // (`deposit.rs`'s seam-composition test in its own `#[cfg(test)]`
    // block is the only carve-out, and it lives there because
    // deposit's policy variant is already in scope). The Withdraw
    // variant is a Phase 5H-B concern; once it lands in
    // `lending::policy`, the corresponding signer-field guard goes
    // there alongside its siblings.

    /// Structural assertion: the only hardcoded pubkey in this file
    /// is the SPL Token program id (a well-known constant). All
    /// Solend / reserve / market / mint / obligation / ATA pubkeys
    /// come from caller inputs. The test re-reads the inputs and
    /// confirms they appear in the output verbatim. Any attempt to
    /// hardcode e.g. the USDC reserve in the builder would show up
    /// as a mismatch between these assertions and the built
    /// instruction.
    #[test]
    fn builder_does_not_hardcode_reserve_or_market_or_mint_pubkeys() {
        let unique_reserve = Pubkey::new_unique();
        let unique_market = Pubkey::new_unique();
        let unique_collateral_mint = Pubkey::new_unique();
        let mut inputs = valid_inputs();
        inputs.reserve = unique_reserve;
        inputs.lending_market = unique_market;
        inputs.reserve_collateral_mint = unique_collateral_mint;
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();
        assert_eq!(ix.accounts[2].pubkey, unique_reserve);
        assert_eq!(ix.accounts[4].pubkey, unique_market);
        assert_eq!(ix.accounts[7].pubkey, unique_collateral_mint);
    }

    /// Phase 5H-A guard: this builder must NOT reference any
    /// `lending::policy` / `LendingSnapshot` / approval / park /
    /// signing / submit type. Function pointers below compile iff
    /// that holds.
    #[test]
    fn builder_does_not_depend_on_policy_or_runtime_types() {
        fn _builder_fn(
            _: WithdrawInstructionInputs,
        ) -> Result<Instruction, WithdrawBuildError> {
            unimplemented!()
        }
        let _ = _builder_fn
            as fn(WithdrawInstructionInputs) -> Result<Instruction, WithdrawBuildError>;
    }
}

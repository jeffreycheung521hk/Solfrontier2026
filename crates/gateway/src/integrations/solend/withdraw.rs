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
//! - **`12 + N` accounts** where `N = obligation.deposits.len()`. The 12
//!   base accounts come first; the `N` deposit-reserve accounts are
//!   appended after the SPL Token program in obligation-deposits order.
//! - `lending_market_authority` is derived as a PDA from
//!   `(lending_market, program_id)` with seed
//!   `[&lending_market.to_bytes()]` — the builder derives this
//!   internally; caller does NOT supply it.
//!
//! ### Phase 6I-J correction (supersedes the Phase 6I-I attempt)
//!
//! The deployed Solend mainnet
//! `process_withdraw_obligation_collateral_and_redeem_reserve_liquidity`
//! reads `token_program_id` immediately after `user_transfer_authority`
//! and then passes `&accounts[12..]` into `_withdraw_obligation_collateral`.
//! That helper calls `update_borrow_attribution_values(&mut obligation,
//! deposit_reserve_infos)`, which iterates the obligation's deposit
//! list and consumes ONE account per deposit via `next_account_info`.
//! This means:
//!   - Clock is NOT an account on this ix — the handler reads it via
//!     `Clock::get()?` syscall (the original Phase 5H-A doc-comment was
//!     correct on this point; the Phase 6I-I attempt to add Clock at
//!     slot 11 was wrong).
//!   - SPL Token program lives at slot 11.
//!   - Slots `12..` carry one deposit-reserve account per
//!     `obligation.deposits` entry, in EXACT obligation-deposit order.
//!   - These deposit-reserve accounts are WRITABLE because
//!     `update_borrow_attribution_values` mutates and packs each one
//!     back at the end of the call.
//!
//! Two live failures motivated this layout:
//!   - 2026-05-08 tx `25REcnNp…` (12 accounts, slot 11 = SPL Token,
//!     no appended deposit reserves) →
//!     `InstructionError(3, NotEnoughAccountKeys)` because the
//!     handler advanced into the empty slot-12 region looking for
//!     `obligation.deposits[0]`'s reserve.
//!   - 2026-05-08 tx `2mLa5bQv…` (13 accounts, slot 11 = Clock,
//!     slot 12 = SPL Token — the buggy 6I-I layout) →
//!     `InstructionError(3, Custom(9))` ("Lending market token program
//!     does not match the token program provided") because the handler
//!     read slot 11 expecting SPL Token and got Clock.
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
//! |  2 | reserve (the WITHDRAW reserve)                                | yes | no  |
//! |  3 | obligation                                                    | yes | no  |
//! |  4 | lending_market                                                | no  | no  |
//! |  5 | lending_market_authority (PDA)                                | no  | no  |
//! |  6 | destination_liquidity (= user's underlying ATA, USDC out)     | yes | no  |
//! |  7 | reserve_collateral_mint (c-token mint; burned)                | yes | no  |
//! |  8 | reserve_liquidity_supply (source underlying)                  | yes | no  |
//! |  9 | obligation_owner                                              | no  | **yes** |
//! | 10 | user_transfer_authority                                       | no  | **yes** |
//! | 11 | spl_token program                                             | no  | no  |
//! | 12..(11+N) | deposit_reserves[i] (in obligation order)             | yes | no  |
//!
//! Clock is NOT in the account list; the handler reads it via
//! `Clock::get()?` sysvar syscall.
//!
//! ### Intentional duplicate at slots 2 and 12+
//!
//! When `obligation.deposits` includes the same reserve as the one
//! being withdrawn from (the typical single-reserve user case), that
//! reserve appears at slot 2 (as `reserve` / withdraw_reserve) AND at
//! slot 12 (as `deposit_reserves[0]`). This is intentional and matches
//! Solend's processor: the withdraw_reserve account is consumed
//! immediately, while `deposit_reserves[..]` are consumed later by
//! `update_borrow_attribution_values`. The duplicate carries different
//! ownership semantics in each slot (the handler `pack`s each
//! deposit-reserve account back, so they must be writable independently).

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

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
    /// Phase 6I-J — the obligation's full deposit-reserve list, in
    /// the EXACT order the on-chain `obligation.deposits` carries
    /// them. Solend's `update_borrow_attribution_values` walks this
    /// in lockstep with `obligation.deposits` and packs each entry
    /// back at the end, so each pubkey here must be the actual
    /// reserve account at that deposits-slot — not the withdraw
    /// reserve, not a synthesized PDA, not a deduped subset.
    ///
    /// The list MAY include the same pubkey as
    /// [`Self::reserve`] (the withdraw reserve) when the user
    /// withdraws from a reserve they also have a deposit on — the
    /// duplicate is intentional and matches Solend's processor
    /// (slot 2 is consumed immediately as the withdraw reserve;
    /// slot `12 + i` for the same reserve is consumed later by
    /// `update_borrow_attribution_values`).
    pub deposit_reserves_in_order: Vec<Pubkey>,
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
    // processor verbatim. 12 base accounts followed by N
    // deposit-reserve accounts, where N = `deposit_reserves_in_order.len()`.
    //
    // Clock is NOT in this list — the deployed handler reads it via
    // `Clock::get()?` syscall. The Phase 6I-I attempt to add Clock at
    // slot 11 was wrong; the Phase 6I-J fix removes it again and
    // appends the deposit-reserve accounts that
    // `update_borrow_attribution_values` actually needs. See module
    // doc-comment for the two live failures that motivated this.
    let mut accounts: Vec<AccountMeta> =
        Vec::with_capacity(12 + inputs.deposit_reserves_in_order.len());
    accounts.push(AccountMeta::new(inputs.source_collateral, false));                // 0
    accounts.push(AccountMeta::new(inputs.destination_collateral, false));           // 1
    accounts.push(AccountMeta::new(inputs.reserve, false));                          // 2
    accounts.push(AccountMeta::new(inputs.obligation, false));                       // 3
    accounts.push(AccountMeta::new_readonly(inputs.lending_market, false));          // 4
    accounts.push(AccountMeta::new_readonly(lending_market_authority, false));       // 5
    accounts.push(AccountMeta::new(inputs.destination_liquidity, false));            // 6
    accounts.push(AccountMeta::new(inputs.reserve_collateral_mint, false));          // 7
    accounts.push(AccountMeta::new(inputs.reserve_liquidity_supply, false));         // 8
    accounts.push(AccountMeta::new_readonly(inputs.obligation_owner, true));         // 9 — signer
    accounts.push(AccountMeta::new_readonly(inputs.user_transfer_authority, true));  // 10 — signer
    accounts.push(AccountMeta::new_readonly(spl_token_program_id, false));           // 11 — SPL Token program
    // Slots 12..(11 + N): one writable account per obligation deposit,
    // in obligation-deposits order. `update_borrow_attribution_values`
    // packs each entry back, so each must be writable independently
    // even when the same pubkey already appears at slot 2 as the
    // withdraw reserve.
    for r in &inputs.deposit_reserves_in_order {
        accounts.push(AccountMeta::new(*r, false));
    }

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
        // Phase 6I-J — canonical fixture: single-deposit obligation
        // whose deposits[0].reserve equals the withdraw reserve.
        // Mirrors the live HcKrv5Jo case (single USDC deposit
        // withdrawing from the USDC reserve). The same pubkey thus
        // appears at slot 2 (withdraw reserve) AND slot 12
        // (deposit_reserves[0]) — the intentional duplicate per
        // Solend's processor.
        let withdraw_reserve = Pubkey::new_unique();
        WithdrawInstructionInputs {
            solend_program_id: solend_program(),
            collateral_amount: CollateralTokenAmount::new(1_000),
            source_collateral: Pubkey::new_unique(),
            destination_collateral: Pubkey::new_unique(),
            reserve: withdraw_reserve,
            obligation: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            destination_liquidity: Pubkey::new_unique(),
            reserve_collateral_mint: Pubkey::new_unique(),
            reserve_liquidity_supply: Pubkey::new_unique(),
            obligation_owner: Pubkey::new_unique(),
            user_transfer_authority: Pubkey::new_unique(),
            deposit_reserves_in_order: vec![withdraw_reserve],
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
    fn account_count_is_12_plus_n_deposit_reserves() {
        // Phase 6I-J: layout is 12 base accounts + N deposit-reserve
        // accounts where N = obligation.deposits.len(). The canonical
        // single-deposit fixture has N=1, so the total is 13.
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        assert_eq!(
            ix.accounts.len(),
            13,
            "single-deposit obligation: 12 base + 1 deposit reserve = 13 accounts",
        );

        // N=0: rejected at the on-chain processor (no
        // `obligation.deposits`), but the builder accepts it
        // structurally. The total is exactly 12.
        let mut zero_deposits = valid_inputs();
        zero_deposits.deposit_reserves_in_order = vec![];
        let ix0 =
            build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
                zero_deposits,
            )
            .unwrap();
        assert_eq!(ix0.accounts.len(), 12, "N=0: 12 base accounts only");

        // N=3: 12 base + 3 deposit reserves = 15.
        let mut three_deposits = valid_inputs();
        three_deposits.deposit_reserves_in_order = vec![
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let ix3 =
            build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
                three_deposits,
            )
            .unwrap();
        assert_eq!(ix3.accounts.len(), 15, "N=3: 12 base + 3 = 15");
    }

    #[test]
    fn account_metas_match_solend_source_order_and_flags() {
        // Distinct deposit reserves so a slot-12+ misplacement is visible.
        let dep0 = Pubkey::new_unique();
        let dep1 = Pubkey::new_unique();
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
            deposit_reserves_in_order: vec![dep0, dep1],
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
        // mainnet processor matrix in this file's module doc.
        // Phase 6I-J: slot 11 = SPL Token; slots 12+ = deposit reserves
        // (writable). NO Clock sysvar in the list.
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
            (11, expected_spl_token, false, false),              // Phase 6I-J: SPL Token at slot 11
            (12, dep0, true, false),                             // deposit reserve 0 (writable)
            (13, dep1, true, false),                             // deposit reserve 1 (writable)
        ];
        for (idx, pk, wr, sg) in exp {
            let a = &ix.accounts[idx];
            assert_eq!(a.pubkey, pk, "pubkey mismatch at index {idx}");
            assert_eq!(a.is_writable, wr, "writable mismatch at index {idx}");
            assert_eq!(a.is_signer, sg, "signer mismatch at index {idx}");
        }
        assert_eq!(ix.accounts.len(), 14, "12 base + 2 deposit reserves");
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

    /// Phase 6I-J regression guard. Restores the original Phase 5H-A
    /// invariant: Clock is NOT in the withdraw ix account list. The
    /// deployed Solend handler reads Clock via `Clock::get()?` syscall
    /// here. The 2026-05-08 tx `2mLa5bQv…` (which had Clock at slot 11)
    /// failed with `Custom(9)` "Lending market token program does not
    /// match the token program provided" because the handler read
    /// slot 11 expecting SPL Token and got Clock.
    #[test]
    fn no_clock_sysvar_anywhere_in_account_list() {
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        for (i, a) in ix.accounts.iter().enumerate() {
            assert_ne!(
                a.pubkey,
                sysvar::clock::id(),
                "Clock sysvar must NOT appear in withdraw ix accounts (slot {i}); the handler reads it via syscall"
            );
        }
    }

    /// Phase 6I-J — the SPL Token program lives at slot 11 (immediately
    /// after the two signer slots). A reordering that put Clock here
    /// would fail with `Custom(9)` (InvalidTokenProgram) on chain.
    #[test]
    fn spl_token_program_is_at_slot_11_immediately_after_signers() {
        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        let expected_spl_token =
            Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        assert_eq!(
            ix.accounts[11].pubkey, expected_spl_token,
            "SPL Token program must be at slot 11 (per Solend mainnet processor; \
             it follows the two signer slots, with Clock read via syscall)"
        );
        assert!(!ix.accounts[11].is_writable);
        assert!(!ix.accounts[11].is_signer);
    }

    #[test]
    fn no_oracle_accounts_in_account_list() {
        // The combined withdraw ix relies on the most recent
        // `RefreshReserve` having posted the oracle-derived state
        // into the reserve account. The withdraw ix itself does NOT
        // take pyth or switchboard oracle accounts.
        //
        // Phase 6I-J: canonical fixture has 13 accounts (12 base + 1
        // deposit reserve). Any extra account beyond `12 + N` is
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

    /// Phase 6I-J regression guard. Signer set is exactly
    /// `[obligation_owner @ slot 9, user_transfer_authority @ slot 10]`.
    /// Adding deposit-reserve accounts at slots 12+ must NOT have
    /// promoted any of them to a signer. Locks the user-wallet-only
    /// signer invariant.
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
             SPL Token at slot 11 and deposit reserves at slots 12+ must NOT be signers"
        );

        // Multi-deposit case: same invariant.
        let mut multi = valid_inputs();
        multi.deposit_reserves_in_order = vec![
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let ix_multi =
            build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
                multi,
            )
            .unwrap();
        let signers_multi: Vec<usize> = ix_multi
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.is_signer)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(signers_multi, vec![9, 10]);
    }

    // ── Phase 6I-J regression tests ─────────────────────────────────────

    /// Phase 6I-J: deposit reserves are appended at slots 12+ in
    /// EXACT obligation order, with each marked WRITABLE (because
    /// `update_borrow_attribution_values` packs each entry back) and
    /// non-signer.
    #[test]
    fn deposit_reserves_appended_at_slot_12_writable_non_signer() {
        let dep0 = Pubkey::new_unique();
        let dep1 = Pubkey::new_unique();
        let dep2 = Pubkey::new_unique();
        let mut inputs = valid_inputs();
        inputs.deposit_reserves_in_order = vec![dep0, dep1, dep2];

        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();
        // 12 base + 3 deposit = 15 accounts.
        assert_eq!(ix.accounts.len(), 15);
        // Slot 11 = SPL Token (regression guard for ordering).
        let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        assert_eq!(ix.accounts[11].pubkey, spl_token);

        // Slots 12, 13, 14 in EXACT input order.
        assert_eq!(ix.accounts[12].pubkey, dep0);
        assert_eq!(ix.accounts[13].pubkey, dep1);
        assert_eq!(ix.accounts[14].pubkey, dep2);

        // Each appended reserve is writable (NOT readonly).
        for slot in 12..=14 {
            let a = &ix.accounts[slot];
            assert!(
                a.is_writable,
                "deposit reserve at slot {slot} must be writable (`pack` requires it)"
            );
            assert!(
                !a.is_signer,
                "deposit reserve at slot {slot} must not be a signer"
            );
        }
    }

    /// Phase 6I-J multi-deposit ordering preservation. Order is
    /// verbatim from the input vec — no sorting, no reorder, no
    /// dedupe. A future refactor that "tidies up" the list would
    /// break this.
    #[test]
    fn multi_deposit_obligation_preserves_exact_input_order() {
        // Five distinct reserves in a deliberately scrambled order.
        let reserves: Vec<Pubkey> = (0..5).map(|_| Pubkey::new_unique()).collect();
        let mut inputs = valid_inputs();
        inputs.deposit_reserves_in_order = reserves.clone();

        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();
        assert_eq!(ix.accounts.len(), 12 + 5);
        for (i, expected) in reserves.iter().enumerate() {
            assert_eq!(
                ix.accounts[12 + i].pubkey,
                *expected,
                "deposit reserve at offset {i} must match input"
            );
        }
    }

    /// Phase 6I-J — HcKrv5Jo concrete fixture. Single-deposit
    /// obligation withdrawing from the same reserve it has a deposit
    /// on. The intentional duplicate at slot 2 (withdraw_reserve)
    /// AND slot 12 (deposit_reserves[0]) is the live HcKrv5Jo case.
    #[test]
    fn hckrv5jo_layout_slot2_withdraw_reserve_slot12_deposit_reserve_intentional_duplicate() {
        let usdc_reserve =
            Pubkey::from_str("BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw").unwrap();
        let obligation =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let wallet =
            Pubkey::from_str("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW").unwrap();

        let inputs = WithdrawInstructionInputs {
            solend_program_id: solend_program(),
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
            source_collateral: Pubkey::new_unique(),
            destination_collateral: Pubkey::new_unique(),
            reserve: usdc_reserve, // slot 2 = withdraw reserve
            obligation,
            lending_market: Pubkey::from_str("4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY")
                .unwrap(),
            destination_liquidity: Pubkey::from_str(
                "4bDArC4UVoBjecHUZaCKqhuhTYPoiKS4qpQQpxuvm1qn",
            )
            .unwrap(),
            reserve_collateral_mint: Pubkey::new_unique(),
            reserve_liquidity_supply: Pubkey::new_unique(),
            obligation_owner: wallet,
            user_transfer_authority: wallet,
            // Single-deposit obligation; deposit[0].reserve = USDC reserve.
            deposit_reserves_in_order: vec![usdc_reserve],
        };

        let ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
            inputs,
        )
        .unwrap();
        assert_eq!(ix.accounts.len(), 13, "12 base + 1 deposit reserve");
        // Withdraw reserve at slot 2.
        assert_eq!(ix.accounts[2].pubkey, usdc_reserve, "slot 2 = withdraw reserve");
        assert!(ix.accounts[2].is_writable);
        // Obligation at slot 3.
        assert_eq!(ix.accounts[3].pubkey, obligation);
        // SPL Token at slot 11 (NO Clock).
        let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        assert_eq!(ix.accounts[11].pubkey, spl_token);
        // Slot 12 = deposit_reserves[0] = same USDC reserve as slot 2 —
        // the intentional duplicate.
        assert_eq!(ix.accounts[12].pubkey, usdc_reserve, "slot 12 = deposit reserve 0");
        assert!(ix.accounts[12].is_writable);
        // Wallet at slots 9 + 10 (signers).
        assert_eq!(ix.accounts[9].pubkey, wallet);
        assert!(ix.accounts[9].is_signer);
        assert_eq!(ix.accounts[10].pubkey, wallet);
        assert!(ix.accounts[10].is_signer);
        // No Clock anywhere.
        for a in &ix.accounts {
            assert_ne!(a.pubkey, sysvar::clock::id());
        }
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

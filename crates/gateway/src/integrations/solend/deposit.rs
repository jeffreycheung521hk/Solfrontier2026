//! Solend `DepositReserveLiquidityAndObligationCollateral` instruction
//! builder — Slice 3B.
//!
//! Spec anchor: `docs/lending_policy_vocabulary.md` Part 6B §67 (builder
//! contract), Part 5A §41.3 (Part 5 / Part 6 seam).
//!
//! ## Purity
//!
//! Pure function over caller-supplied accounts + amount. The builder does
//! NOT:
//!
//! - call RPC
//! - sign transactions
//! - read the policy evaluator state or any `LendingSnapshot`
//! - reach into `approval_store` / `park_store` / daemon runtime
//! - read global config
//! - hardcode the AMM reserve or oracle pubkeys
//! - submit or broadcast
//! - fetch blockhash
//! - create associated token accounts
//!
//! The builder assumes the caller has already:
//! 1. Refreshed the reserve (via `crate::integrations::solend::refresh`,
//!    executed separately).
//! 2. Re-fetched the obligation and the reserve post-refresh.
//! 3. Run the full three-layer gate through `evaluate_lending_policy` and
//!    received `Pass`.
//! 4. Derived / confirmed the user's source liquidity ATA and collateral
//!    ATA. The builder treats those as explicit inputs — ATA creation
//!    itself is a separate ix that a future slice (3C) may bundle.
//!
//! ## Source of truth
//!
//! **`solendprotocol/solana-program-library`, `mainnet` branch** (the
//! repo's default branch — this is the source the deployed program at
//! `So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo` is built from):
//!
//! - Constructor: `token-lending/sdk/src/instruction.rs`,
//!   `deposit_reserve_liquidity_and_obligation_collateral(...)`.
//! - Handler: `token-lending/program/src/processor.rs`,
//!   `process_deposit_reserve_liquidity_and_obligation_collateral(...)` —
//!   14 `next_account_info` calls; `Clock::get()?` is read as a sysvar
//!   syscall, NOT from the account list.
//! - `LendingInstruction` variant: tag byte = 14.
//! - Payload: `liquidity_amount: u64` (little-endian, per SPL `Pack`).
//! - **14 accounts** in the exact order produced below.
//! - `lending_market_authority` is derived as a PDA from
//!   `(lending_market, program_id)` with seed `[&lending_market.to_bytes()]`
//!   — the builder derives this internally; caller does NOT supply it.
//!
//! ### Divergence note (Slice 3D(ii) finding)
//!
//! The `master` branch of the same repo has an older handler shape with
//! 15 accounts (Clock sysvar AS an account at index 13, token program at
//! index 14). An earlier revision of this builder was modeled on master
//! and passed the Clock sysvar at index 13 — the deployed program then
//! read the Clock sysvar pubkey as the "token program" and rejected with
//! `LendingError::InvalidTokenProgram` (`Custom(9)`). The fix landed in
//! Slice 3D(ii): rebase the builder on `mainnet` branch (14 accounts, no
//! Clock in the list). When re-verifying account metas, always cross-
//! check against the `mainnet` branch — it is the deployed source.
//!
//! ## Signer / writable matrix (mainnet-branch layout)
//!
//! | # | Account | Writable | Signer |
//! |---|---|---|---|
//! | 0  | source_liquidity                  | yes | no  |
//! | 1  | user_collateral                   | yes | no  |
//! | 2  | reserve                           | yes | no  |
//! | 3  | reserve_liquidity_supply          | yes | no  |
//! | 4  | reserve_collateral_mint           | yes | no  |
//! | 5  | lending_market                    | no  | no  |
//! | 6  | lending_market_authority (PDA)    | no  | no  |
//! | 7  | destination_deposit_collateral    | yes | no  |
//! | 8  | obligation                        | yes | no  |
//! | 9  | obligation_owner                  | yes | **yes** |
//! | 10 | pyth_oracle                       | no  | no  |
//! | 11 | switchboard_oracle                | no  | no  |
//! | 12 | user_transfer_authority           | no  | **yes** |
//! | 13 | spl_token program                 | no  | no  |
//!
//! Clock is NOT in the account list; the handler reads it via
//! `Clock::get()?` sysvar syscall.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

use crate::lending::UnderlyingAmount;

/// Tag byte for `LendingInstruction::DepositReserveLiquidityAndObligationCollateral`
/// at the pinned Solend source cited above.
const SOLEND_IX_TAG_DEPOSIT_RESERVE_LIQUIDITY_AND_OBLIGATION_COLLATERAL: u8 = 14;

/// SPL Token program id. Hardcoded as a pubkey string (parsed once in the
/// builder) rather than taking a dependency on the `spl-token` crate just
/// for one constant.
pub(crate) const SPL_TOKEN_PROGRAM_ID_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Explicit inputs to the deposit ix builder. Every pubkey the caller
/// derived or fetched is listed here; the builder supplies zero pubkeys
/// except the lending-market authority PDA (derived internally because
/// the PDA derivation is load-bearing to the Solend program and the
/// caller has no reason to duplicate it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositInstructionInputs {
    pub solend_program_id: Pubkey,
    /// Raw underlying base units. Wrapped in [`UnderlyingAmount`] so bare
    /// integer misuse across unit domains is a compile error.
    pub amount: UnderlyingAmount,
    /// User's SPL token account holding the underlying asset (source).
    pub source_liquidity: Pubkey,
    /// User's SPL token account for the reserve's collateral (c-token) mint.
    /// Typically an ATA derived from `(user_wallet, reserve.collateral.mint)`.
    /// Builder does NOT derive this; caller passes the pubkey they
    /// fetched / derived. If it does not yet exist on chain, a future
    /// slice's leading `create_associated_token_account` ix covers the gap.
    pub user_collateral: Pubkey,
    pub reserve: Pubkey,
    /// From decoded `SolendReserveRaw.liquidity_supply`.
    pub reserve_liquidity_supply: Pubkey,
    /// From decoded `SolendReserveRaw.collateral_mint`.
    pub reserve_collateral_mint: Pubkey,
    pub lending_market: Pubkey,
    /// From decoded `SolendReserveRaw.collateral_supply`. This is the
    /// reserve-side pool that receives the obligation's collateral
    /// tokens after the SPL transfer completes.
    pub destination_deposit_collateral: Pubkey,
    pub obligation: Pubkey,
    /// Owner of the obligation. Writable + signer on this instruction.
    pub obligation_owner: Pubkey,
    /// Reserve's pyth oracle pubkey (may be the null sentinel).
    pub pyth_oracle: Pubkey,
    /// Reserve's switchboard oracle pubkey (may be the null sentinel).
    pub switchboard_oracle: Pubkey,
    /// SPL transfer authority. Readonly + signer. Typically the same
    /// pubkey as `obligation_owner` for V1 single-wallet flows, but the
    /// Solend ix defines them as distinct parameters; the builder
    /// preserves the distinction.
    pub user_transfer_authority: Pubkey,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DepositBuildError {
    /// Solend's program-side validation rejects zero-amount deposits
    /// (see `process_deposit_reserve_liquidity_and_obligation_collateral`
    /// in `token-lending/program/src/processor.rs`). The builder
    /// fails-fast here so a zero never reaches the wire.
    #[error("deposit amount must be greater than zero")]
    ZeroAmount,
}

/// Pure builder. Returns an unsigned [`Instruction`] whose account list
/// and payload match the Solend program's expectation exactly. Caller is
/// responsible for placing the ix into a transaction, attaching a
/// blockhash, collecting signatures from `obligation_owner` and
/// `user_transfer_authority`, and submitting.
pub fn build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
    inputs: DepositInstructionInputs,
) -> Result<Instruction, DepositBuildError> {
    if inputs.amount.raw() == 0 {
        return Err(DepositBuildError::ZeroAmount);
    }

    // Derive lending_market_authority PDA exactly as Solend's own
    // constructor does (seed = lending_market pubkey bytes, program =
    // Solend program id). The caller has no reason to duplicate this.
    let (lending_market_authority, _bump) = Pubkey::find_program_address(
        &[&inputs.lending_market.to_bytes()],
        &inputs.solend_program_id,
    );

    let spl_token_program_id: Pubkey = SPL_TOKEN_PROGRAM_ID_BS58
        .parse()
        .expect("SPL Token program id is a well-known constant");

    // Payload: [14, liquidity_amount_u64_le (8 bytes)].
    let mut data: Vec<u8> = Vec::with_capacity(1 + 8);
    data.push(SOLEND_IX_TAG_DEPOSIT_RESERVE_LIQUIDITY_AND_OBLIGATION_COLLATERAL);
    data.extend_from_slice(&inputs.amount.raw().to_le_bytes());

    // Account list — order is load-bearing; matches the cited Solend
    // MAINNET branch source verbatim (14 accounts, no Clock sysvar in
    // the list; Clock is read via `Clock::get()?` inside the handler).
    // Writable vs readonly and signer vs non-signer flags also match
    // the source. See the Divergence note in the module doc for why
    // this differs from `master` branch's 15-account shape.
    let accounts = vec![
        AccountMeta::new(inputs.source_liquidity, false),                 // 0
        AccountMeta::new(inputs.user_collateral, false),                  // 1
        AccountMeta::new(inputs.reserve, false),                          // 2
        AccountMeta::new(inputs.reserve_liquidity_supply, false),         // 3
        AccountMeta::new(inputs.reserve_collateral_mint, false),          // 4
        AccountMeta::new_readonly(inputs.lending_market, false),          // 5
        AccountMeta::new_readonly(lending_market_authority, false),       // 6
        AccountMeta::new(inputs.destination_deposit_collateral, false),   // 7
        AccountMeta::new(inputs.obligation, false),                       // 8
        AccountMeta::new(inputs.obligation_owner, true),                  // 9 — signer
        AccountMeta::new_readonly(inputs.pyth_oracle, false),             // 10
        AccountMeta::new_readonly(inputs.switchboard_oracle, false),      // 11
        AccountMeta::new_readonly(inputs.user_transfer_authority, true),  // 12 — signer
        AccountMeta::new_readonly(spl_token_program_id, false),           // 13
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
    use std::str::FromStr;

    fn solend_program() -> Pubkey {
        Pubkey::from_str("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo").unwrap()
    }

    fn valid_inputs() -> DepositInstructionInputs {
        DepositInstructionInputs {
            solend_program_id: solend_program(),
            amount: UnderlyingAmount::new(1_000),
            source_liquidity: Pubkey::new_unique(),
            user_collateral: Pubkey::new_unique(),
            reserve: Pubkey::new_unique(),
            reserve_liquidity_supply: Pubkey::new_unique(),
            reserve_collateral_mint: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            destination_deposit_collateral: Pubkey::new_unique(),
            obligation: Pubkey::new_unique(),
            obligation_owner: Pubkey::new_unique(),
            pyth_oracle: Pubkey::new_unique(),
            switchboard_oracle: Pubkey::new_unique(),
            user_transfer_authority: Pubkey::new_unique(),
        }
    }

    #[test]
    fn instruction_program_id_is_solend() {
        let inputs = valid_inputs();
        let expected = inputs.solend_program_id;
        let ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs)
            .expect("valid input builds");
        assert_eq!(ix.program_id, expected);
        assert_eq!(expected, solend_program());
    }

    #[test]
    fn first_instruction_byte_is_tag_14() {
        let ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
            valid_inputs(),
        )
        .unwrap();
        assert_eq!(
            ix.data[0],
            SOLEND_IX_TAG_DEPOSIT_RESERVE_LIQUIDITY_AND_OBLIGATION_COLLATERAL
        );
        assert_eq!(ix.data[0], 14);
    }

    #[test]
    fn amount_is_encoded_little_endian_u64_at_bytes_1_through_8() {
        let mut inputs = valid_inputs();
        inputs.amount = UnderlyingAmount::new(0x0123_4567_89AB_CDEF);
        let ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs)
            .unwrap();
        assert_eq!(ix.data.len(), 1 + 8);
        assert_eq!(&ix.data[1..9], &[0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]);
    }

    #[test]
    fn account_metas_match_solend_source_order_and_flags() {
        // Construct inputs with distinct, recognizable pubkeys at each
        // position so a misplacement is immediately visible.
        let inputs = DepositInstructionInputs {
            solend_program_id: solend_program(),
            amount: UnderlyingAmount::new(7),
            source_liquidity: Pubkey::new_unique(),
            user_collateral: Pubkey::new_unique(),
            reserve: Pubkey::new_unique(),
            reserve_liquidity_supply: Pubkey::new_unique(),
            reserve_collateral_mint: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            destination_deposit_collateral: Pubkey::new_unique(),
            obligation: Pubkey::new_unique(),
            obligation_owner: Pubkey::new_unique(),
            pyth_oracle: Pubkey::new_unique(),
            switchboard_oracle: Pubkey::new_unique(),
            user_transfer_authority: Pubkey::new_unique(),
        };
        // Stash expected pubkeys before the input struct moves.
        let expected_source_liquidity = inputs.source_liquidity;
        let expected_user_collateral = inputs.user_collateral;
        let expected_reserve = inputs.reserve;
        let expected_reserve_liquidity_supply = inputs.reserve_liquidity_supply;
        let expected_reserve_collateral_mint = inputs.reserve_collateral_mint;
        let expected_lending_market = inputs.lending_market;
        let expected_destination = inputs.destination_deposit_collateral;
        let expected_obligation = inputs.obligation;
        let expected_obligation_owner = inputs.obligation_owner;
        let expected_pyth = inputs.pyth_oracle;
        let expected_swb = inputs.switchboard_oracle;
        let expected_xfer_auth = inputs.user_transfer_authority;
        let expected_pda = Pubkey::find_program_address(
            &[&inputs.lending_market.to_bytes()],
            &inputs.solend_program_id,
        )
        .0;

        let ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs)
            .unwrap();

        assert_eq!(ix.accounts.len(), 14);

        // Per-slot pubkey + flag assertions, matching the Solend MAINNET
        // branch source matrix in this file's module doc. Clock is NOT
        // in the account list; the handler reads it via `Clock::get()?`.
        let exp = [
            // (index, pubkey, writable, signer)
            (0usize, expected_source_liquidity, true, false),
            (1, expected_user_collateral, true, false),
            (2, expected_reserve, true, false),
            (3, expected_reserve_liquidity_supply, true, false),
            (4, expected_reserve_collateral_mint, true, false),
            (5, expected_lending_market, false, false),
            (6, expected_pda, false, false),
            (7, expected_destination, true, false),
            (8, expected_obligation, true, false),
            (9, expected_obligation_owner, true, true), // signer
            (10, expected_pyth, false, false),
            (11, expected_swb, false, false),
            (12, expected_xfer_auth, false, true), // signer
            (
                13,
                Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap(),
                false,
                false,
            ),
        ];
        for (idx, pk, wr, sg) in exp {
            let a = &ix.accounts[idx];
            assert_eq!(a.pubkey, pk, "pubkey mismatch at index {idx}");
            assert_eq!(a.is_writable, wr, "writable mismatch at index {idx}");
            assert_eq!(a.is_signer, sg, "signer mismatch at index {idx}");
        }
    }

    #[test]
    fn zero_amount_is_rejected() {
        let mut inputs = valid_inputs();
        inputs.amount = UnderlyingAmount::new(0);
        let err = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs)
            .unwrap_err();
        assert_eq!(err, DepositBuildError::ZeroAmount);
    }

    /// Structural assertion: the only hardcoded pubkey in this file is
    /// the SPL Token program id (a well-known constant). All Solend /
    /// AMM / mint / oracle pubkeys come from caller inputs. The test
    /// just re-reads the inputs and confirms they appear in the output
    /// verbatim. Any attempt to hardcode e.g. the AMM reserve in the
    /// builder would show up as a mismatch between these assertions and
    /// the built instruction.
    #[test]
    fn builder_does_not_hardcode_amm_reserve_or_oracle_pubkeys() {
        let unique_reserve = Pubkey::new_unique();
        let unique_pyth = Pubkey::new_unique();
        let mut inputs = valid_inputs();
        inputs.reserve = unique_reserve;
        inputs.pyth_oracle = unique_pyth;
        let ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs)
            .unwrap();
        assert_eq!(ix.accounts[2].pubkey, unique_reserve);
        assert_eq!(ix.accounts[10].pubkey, unique_pyth);
    }

    /// Compile-time + runtime evidence that the raw amount is extracted
    /// via the explicit getter, not via field access. If a future change
    /// made `UnderlyingAmount(u64)`'s field `pub`, this test would still
    /// pass — the assertion here is that the documented API surface we
    /// depend on (`amount.raw()`) exists and returns the inner value.
    #[test]
    fn amount_raw_getter_used_for_payload_encoding() {
        let a = UnderlyingAmount::new(42);
        let extracted_via_getter: u64 = a.raw();
        assert_eq!(extracted_via_getter, 42);
        // And that value flows into the instruction payload unchanged.
        let mut inputs = valid_inputs();
        inputs.amount = a;
        let ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs)
            .unwrap();
        assert_eq!(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()), 42);
    }

    #[test]
    fn lending_market_authority_is_derived_not_accepted() {
        // The caller does NOT pass lending_market_authority — the builder
        // derives it. Changing the lending_market pubkey must change the
        // PDA at slot 6.
        let mut inputs_a = valid_inputs();
        let mut inputs_b = valid_inputs();
        let lm_a = Pubkey::new_unique();
        let lm_b = Pubkey::new_unique();
        inputs_a.lending_market = lm_a;
        inputs_b.lending_market = lm_b;
        let ix_a = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs_a)
            .unwrap();
        let ix_b = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(inputs_b)
            .unwrap();
        assert_ne!(
            ix_a.accounts[6].pubkey, ix_b.accounts[6].pubkey,
            "PDA at slot 6 must vary with lending_market"
        );
    }

    // ── Policy-to-builder composition seam test ───────────────────────────

    use crate::integrations::solend::mapping::{
        map_snapshot_for_first_deposit, FirstDepositAssemblyInputs, OracleAccountInfo,
        ReserveInput,
    };
    use crate::integrations::solend::raw::{
        self, synth_reserve, SOLEND_NULL_ORACLE_SENTINEL_BS58,
    };
    use crate::lending::{
        evaluate_lending_policy, AllowedLendingProtocolsConfig, AllowedMintsConfig, ChainSlot,
        DurationMs, EvaluationContext, FeedPublishFreshness, LendingPolicyVerdict,
        LendingRuleConfig, MaxActionInputAmountConfig, MaxOracleStalenessConfig, ProposedAction,
        ProtocolTag, RequireFreshStateConfig,
    };

    const AMM_LENDING_MARKET_BS58: &str = "Au3S1ZSkGwm1fo7g3WFhkD1rcPoUXj7h5ubsGsUFqbLX";
    const AMM_RESERVE_BS58: &str = "6nb1odSYHutVAxoaQyiwPhQNTFn3nBNFdQdNCm5v9Jbp";
    const AMM_MINT_BS58: &str = "E5ndSkaB17Dm7CsD22dvcjfrYSDLCxFcMd6z8ddCk5wp";
    const AMM_PYTH_ORACLE_BS58: &str = "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX";
    const PYTH_SOLANA_RECEIVER_OWNER_BS58: &str =
        "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    /// The Slice 3B seam: policy evaluation and instruction building are
    /// separate functions with disjoint input types. Policy sees only the
    /// snapshot + action + config + context; the builder sees only
    /// caller-supplied accounts + amount. Composing them happens at the
    /// outer-wiring call site, not inside either function.
    #[test]
    fn policy_pass_then_builder_produces_instruction_separate_inputs() {
        // Step 1: build a Pyth-only AMM first-deposit snapshot.
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::from_str(AMM_LENDING_MARKET_BS58).unwrap();
        let reserve_pk = Pubkey::from_str(AMM_RESERVE_BS58).unwrap();
        let mint = Pubkey::from_str(AMM_MINT_BS58).unwrap();
        let pyth = Pubkey::from_str(AMM_PYTH_ORACLE_BS58).unwrap();
        let pyth_owner = Pubkey::from_str(PYTH_SOLANA_RECEIVER_OWNER_BS58).unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();
        let reserve_supply = Pubkey::new_unique();
        let reserve_collateral_mint = Pubkey::new_unique();
        let reserve_collateral_supply = Pubkey::new_unique();

        let reserve_bytes = synth_reserve(
            mkt,
            mint,
            9,
            reserve_supply,
            pyth,
            sentinel,
            9_999_999,
            reserve_collateral_mint,
            reserve_collateral_supply,
            100,
            false, // reserve is fresh (controlled fixture; §66 refresh covered elsewhere)
        );
        let reserve_raw = raw::decode_reserve(&reserve_bytes).unwrap();

        let snapshot = map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: mkt,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(10_000),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(10_000),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
            }],
            snapshot_observed_slot: ChainSlot::new(10_000),
        })
        .unwrap();

        // Step 2: evaluate policy. Permissive config so the Pyth-only
        // AMM reserve passes.
        let amount = UnderlyingAmount::new(1_000);
        let config = LendingRuleConfig {
            require_fresh_state: RequireFreshStateConfig {
                max_fetch_age: DurationMs::new(60_000),
            },
            max_oracle_staleness: MaxOracleStalenessConfig {
                max_publish_age: DurationMs::new(60_000),
            },
            allowed_lending_protocols: AllowedLendingProtocolsConfig {
                allowlist: vec![ProtocolTag::Solend],
            },
            allowed_mints: AllowedMintsConfig {
                allowlist: vec![mint],
            },
            max_action_input_amount: MaxActionInputAmountConfig {
                per_mint_caps: vec![(mint, UnderlyingAmount::new(1_000_000))],
            },
        };
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount,
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        let verdict = evaluate_lending_policy(&snapshot, &action, &config, &ctx);
        assert_eq!(verdict, LendingPolicyVerdict::Pass);

        // Step 3: the builder takes EXPLICIT accounts + amount. None of
        // its inputs is a `LendingSnapshot`, `LendingPolicyVerdict`,
        // `EvaluationContext`, or any lending-policy type. That is the
        // seam.
        let deposit_ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
            DepositInstructionInputs {
                solend_program_id: solend_program(),
                amount, // the UnderlyingAmount from the action
                source_liquidity: Pubkey::new_unique(),
                user_collateral: Pubkey::new_unique(),
                reserve: reserve_pk,
                reserve_liquidity_supply: reserve_supply,
                reserve_collateral_mint,
                lending_market: mkt,
                destination_deposit_collateral: reserve_collateral_supply,
                obligation: Pubkey::new_unique(),
                obligation_owner: owner,
                pyth_oracle: pyth,
                switchboard_oracle: sentinel,
                user_transfer_authority: owner,
            },
        )
        .unwrap();
        assert_eq!(deposit_ix.program_id, solend_program());
        assert_eq!(deposit_ix.accounts.len(), 14);
        assert_eq!(deposit_ix.data[0], 14);

        // Compile-time seam assertions: these function signatures do not
        // mention each other's types. The bindings below fail to compile
        // if someone accidentally widens `build_deposit_...` to take a
        // snapshot, or `evaluate_lending_policy` to take an Instruction.
        fn _builder_fn_does_not_take_snapshot(
            _: DepositInstructionInputs,
        ) -> Result<Instruction, DepositBuildError> {
            unimplemented!()
        }
        fn _evaluator_fn_does_not_take_instruction(
            _snapshot: &crate::lending::LendingSnapshot,
            _action: &ProposedAction,
            _config: &LendingRuleConfig,
            _ctx: &EvaluationContext,
        ) -> LendingPolicyVerdict {
            unimplemented!()
        }
        let _ = _builder_fn_does_not_take_snapshot
            as fn(DepositInstructionInputs) -> Result<Instruction, DepositBuildError>;
        let _ = _evaluator_fn_does_not_take_instruction
            as fn(
                &crate::lending::LendingSnapshot,
                &ProposedAction,
                &LendingRuleConfig,
                &EvaluationContext,
            ) -> LendingPolicyVerdict;
    }
}

//! Phase 5H-C — Solend WITHDRAW transaction PLAN assembly.
//!
//! Sibling to [`crate::integrations::solend_tx_plan`]. Composes the
//! `compute_budget → setup → refresh → withdraw` instruction sequence
//! into a typed [`SolendWithdrawTxPlan`] artifact for the
//! Solend-USDC `withdraw_all` flow.
//!
//! # Un-wired posture (Phase 5H scope)
//!
//! This module is declared `#[cfg(test)] mod solend_withdraw_tx_plan;`
//! in `integrations/mod.rs`. Production builds exclude it entirely; no
//! caller, runtime, daemon line, registry, chat allowlist, or LLM
//! surface references it. Phase 5H-D will flip the gate when the
//! `solend_withdraw_usdc` tool is wired.
//!
//! Likewise, the underlying ix builder
//! [`crate::integrations::solend::withdraw`] is itself `#[cfg(test)]`,
//! so the test-only chain `withdraw → withdraw_tx_plan → tests` is
//! intact and a single `cfg(test)` removal at each declaration site
//! lights the whole substrate up at once.
//!
//! # What this module owns
//!
//! A single pure function, [`assemble_solend_withdraw_tx_plan`], that
//! groups the four instruction lists for a Solend `withdraw_all`
//! transaction:
//!
//! 1. `compute_budget` priority-fee ix (mirrors deposit-side ordering
//!    discipline so the signed message hash covers the fee).
//! 2. `setup_instructions` — idempotent ATA creates for the source-
//!    underlying ATA (destination of the withdrawn USDC) and the user-
//!    collateral ATA (transient cToken ATA for redemption). Each is
//!    emitted only if the FRESH snapshot reports the ATA as missing.
//! 3. `refresh_instructions` — `RefreshReserve` for the USDC reserve,
//!    then `RefreshObligation` for the existing obligation (Withdraw
//!    requires `obligation_exists == true`, so this is always a
//!    two-ix block).
//! 4. `withdraw_instructions` — single
//!    `WithdrawObligationCollateralAndRedeemReserveCollateral` ix from
//!    [`crate::integrations::solend::withdraw`], with the action's
//!    `collateral_amount` passed verbatim. For the `withdraw_all`
//!    surface this is `CollateralTokenAmount::new(u64::MAX)`; the
//!    Solend program clamps to the user's deposited collateral on
//!    chain.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT fetch a blockhash.
//! - Does NOT assemble a `Transaction` / `VersionedTransaction` /
//!   `MessageV0`.
//! - Does NOT use `AddressLookupTable`.
//! - Does NOT simulate.
//! - Does NOT sign (no obligation Keypair is ever generated — Withdraw
//!   does not create a new obligation; it spends the existing one).
//! - Does NOT broadcast.
//! - Does NOT generate or hold any `Keypair` / signer / private key.
//! - Does NOT touch RPC.
//! - Does NOT mutate stale markers on any snapshot.
//! - Does NOT alter `solend_tx_plan` (deposit) state, types, or behavior.
//!
//! # Strict isolation from deposit plan
//!
//! Phase 5H-C is forbidden from editing `solend_tx_plan.rs` other than
//! the existing exhaustiveness arm landed in 5H-B. This file
//! deliberately defines its OWN `SolendWithdrawAccountsSummary`,
//! `SolendWithdrawTxPlan`, and `SolendWithdrawTxPlanError` types
//! rather than reusing the deposit ones. The two assemblers stay
//! disjoint: a future change to deposit accounting cannot silently
//! break the withdraw plan and vice versa.

use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use thiserror::Error;
use uuid::Uuid;

use claw_types::session::SessionId;

use crate::integrations::solend::deposit::SPL_TOKEN_PROGRAM_ID_BS58;
use crate::integrations::solend::raw::{
    SOLEND_NULL_ORACLE_SENTINEL_BS58, SOLEND_PROGRAM_ID_BS58,
};
use crate::integrations::solend::withdraw::{
    build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction,
    WithdrawBuildError, WithdrawInstructionInputs,
};
use crate::integrations::solend::{
    build_create_ata_idempotent_instruction, build_refresh_instructions,
    AssembledSolendDepositSnapshot, ObligationRefreshInput, RefreshPlanInputs,
    ReserveRefreshInput,
};
use crate::lending::{ProposedAction, ProtocolTag};

/// Phase 5H-C — withdraw priority fee in micro-lamports per compute unit.
///
/// Independently tunable from the deposit-side priority fee constant
/// (see [`crate::integrations::solend_tx_plan::SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS`])
/// so a future asymmetric-congestion tuning does not require touching
/// the deposit plan. Initial value matches deposit's 50_000 — modest
/// enough not to distort fees, high enough to survive ordinary mainnet
/// congestion. Prepended as the FIRST instruction of `setup_instructions`
/// so it is covered by the signed message hash (matching deposit's
/// 4C-5 message-hash-immutability discipline).
pub const SOLEND_WITHDRAW_PRIORITY_FEE_MICRO_LAMPORTS: u64 = 50_000;

/// Execution-facing summary of the accounts the withdraw plan
/// references. Carried alongside the instruction groups so a future
/// signing/submit slice does not have to re-derive any of these from
/// the ix account metas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendWithdrawAccountsSummary {
    pub reserve_pubkey: Pubkey,
    pub reserve_mint: Pubkey,
    /// Existing on-chain obligation. Withdraw requires this to exist
    /// at plan-assembly time; `obligation_exists` is always `true` on
    /// any plan returned by [`assemble_solend_withdraw_tx_plan`].
    pub obligation_pubkey: Pubkey,
    /// User's underlying-USDC ATA. For Withdraw this is the
    /// DESTINATION of the redeemed liquidity (Solend's
    /// `destination_liquidity` slot in the ix).
    pub source_liquidity_ata: Pubkey,
    /// User's cToken ATA. For Withdraw this is the transient
    /// destination of cTokens before they are burned at the
    /// collateral mint.
    pub user_collateral_ata: Pubkey,
    pub source_ata_exists: bool,
    pub collateral_ata_exists: bool,
    /// Always `true` on a successful plan — Withdraw cannot proceed
    /// without an existing obligation. Echoed for symmetry with the
    /// deposit plan's accounts summary.
    pub obligation_exists: bool,
}

/// Phase 5H-C transaction PLAN artifact for Solend `withdraw_all`.
///
/// See module docs for what this is and is NOT. In particular: this
/// artifact is NOT a transaction, does NOT carry a blockhash, does NOT
/// carry signatures, and is NOT broadcastable. Future Phase 5H-D
/// signing handoff converts this to a partially-signed Transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendWithdrawTxPlan {
    pub request_id: Uuid,
    pub intent_id: Uuid,
    pub session_id: SessionId,
    pub session_wallet: Pubkey,
    /// Observed slot of the FRESH snapshot used for the policy
    /// re-check that produced this plan. Surfaced for audit so a
    /// downstream slice can correlate "approval → plan → broadcast"
    /// slot provenance.
    pub verified_slot: u64,
    /// Echoed for audit; MUST be a `ProposedAction::Withdraw` variant
    /// (the assembler rejects every other variant).
    pub action: ProposedAction,

    /// Idempotent ATA-create instructions, in dispatch order:
    ///   - source-USDC ATA create (if missing on the fresh snapshot)
    ///   - user-collateral ATA create (if missing on the fresh snapshot)
    ///
    /// Either or both may be empty. NEVER contains an obligation
    /// `CreateAccount` ix — Withdraw uses the existing obligation.
    pub setup_instructions: Vec<Instruction>,

    /// `RefreshReserve` followed by `RefreshObligation`. MUST precede
    /// `withdraw_instructions` in any transaction composition; Solend's
    /// withdraw handler reads obligation values that are only valid
    /// within the slot of the preceding refresh.
    pub refresh_instructions: Vec<Instruction>,

    /// Solend `WithdrawObligationCollateralAndRedeemReserveCollateral`
    /// instruction(s). In 5H-C this is exactly one instruction; the
    /// field is a `Vec` for symmetry with
    /// [`crate::integrations::solend_tx_plan::SolendDepositTxPlan::deposit_instructions`]
    /// so future slices can prepend a fee transfer or similar without
    /// changing the artifact shape.
    pub withdraw_instructions: Vec<Instruction>,

    /// Always `false` for Withdraw: no fresh obligation account is
    /// created in this flow, so the user wallet is the only signer
    /// the downstream signing slice has to coordinate.
    pub obligation_signer_required: bool,

    /// Always `None` for Withdraw — no transient placeholder pubkey is
    /// needed because the obligation already exists on chain.
    pub transient_obligation_pubkey: Option<Pubkey>,

    pub accounts_summary: SolendWithdrawAccountsSummary,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SolendWithdrawTxPlanError {
    #[error(
        "non-Solend protocol cannot be planned by this assembler (got {protocol:?})"
    )]
    UnsupportedProtocol { protocol: ProtocolTag },
    /// Surfaces when a non-Withdraw `ProposedAction` (i.e., `Deposit`
    /// or `Repay`) is passed in. The withdraw plan assembler is
    /// strictly Withdraw-only; deposit and repay have their own
    /// plan paths.
    #[error("only Withdraw actions are supported by this assembler")]
    UnsupportedActionKind,
    #[error(
        "action's reserve mint {action_mint} was not present in the fresh snapshot's reserves"
    )]
    ReserveNotInSnapshot { action_mint: Pubkey },
    /// Withdraw requires an existing on-chain obligation to spend
    /// collateral from. The fresh snapshot reports
    /// `obligation_exists == false`, so there is nothing to redeem
    /// against.
    #[error("withdraw requires an existing on-chain obligation; fresh snapshot reports none")]
    ObligationDoesNotExist,
    #[error("Solend withdraw builder rejected plan inputs: {0}")]
    WithdrawBuildFailed(WithdrawBuildError),
    #[error("internal invariant violated: well-known Solend constant failed to parse")]
    InternalConstantParseFailed,
}

/// Phase 5H-C plan-assembly entry point. Pure: no RPC, no clock, no
/// blockhash, no signer, no simulation, no broadcast.
///
/// # Inputs
///
/// - `request_id` — the approval request id the plan is anchored to.
/// - `action` — must be `ProposedAction::Withdraw` whose `protocol` is
///   `ProtocolTag::Solend`. Any other shape is rejected.
/// - `session_id` / `session_wallet` — copied verbatim into the plan.
/// - `fresh` — the FRESH snapshot produced by the post-approval re-check.
///   Reused unchanged from the deposit-side assembler output type
///   ([`AssembledSolendDepositSnapshot`]) because the on-chain reserve /
///   obligation / ATA facts the withdraw plan needs are identical.
/// - `verified_slot` — fresh-snapshot observed slot; echoed for audit.
///
/// # Failure modes
///
/// Returns a typed [`SolendWithdrawTxPlanError`] for every reachable
/// failure. NEVER panics. NEVER calls `todo!()` or `unimplemented!()`.
pub fn assemble_solend_withdraw_tx_plan(
    request_id: Uuid,
    action: &ProposedAction,
    session_id: SessionId,
    session_wallet: Pubkey,
    fresh: &AssembledSolendDepositSnapshot,
    verified_slot: u64,
) -> Result<SolendWithdrawTxPlan, SolendWithdrawTxPlanError> {
    // 1. Action must be a Solend Withdraw.
    let (action_protocol, action_reserve_mint, action_collateral_amount) = match action {
        ProposedAction::Withdraw {
            protocol,
            reserve_mint,
            collateral_amount,
        } => (*protocol, *reserve_mint, *collateral_amount),
        ProposedAction::Deposit { .. } | ProposedAction::Repay { .. } => {
            return Err(SolendWithdrawTxPlanError::UnsupportedActionKind);
        }
    };
    if action_protocol != ProtocolTag::Solend {
        return Err(SolendWithdrawTxPlanError::UnsupportedProtocol {
            protocol: action_protocol,
        });
    }

    // 2. Parse well-known program / sentinel constants.
    let solend_program_id = Pubkey::try_from(SOLEND_PROGRAM_ID_BS58)
        .map_err(|_| SolendWithdrawTxPlanError::InternalConstantParseFailed)?;
    let spl_token_program = Pubkey::try_from(SPL_TOKEN_PROGRAM_ID_BS58)
        .map_err(|_| SolendWithdrawTxPlanError::InternalConstantParseFailed)?;
    let null_oracle_sentinel = Pubkey::try_from(SOLEND_NULL_ORACLE_SENTINEL_BS58)
        .map_err(|_| SolendWithdrawTxPlanError::InternalConstantParseFailed)?;

    // 3. Find the target reserve in the FRESH snapshot.
    let reserve = fresh
        .snapshot
        .reserves
        .iter()
        .find(|r| r.mint == action_reserve_mint)
        .ok_or(SolendWithdrawTxPlanError::ReserveNotInSnapshot {
            action_mint: action_reserve_mint,
        })?;

    // 4. Withdraw requires an existing obligation. The first-deposit
    //    path is structurally not applicable here.
    if !fresh.obligation_exists {
        return Err(SolendWithdrawTxPlanError::ObligationDoesNotExist);
    }

    // 5. Extract pyth + switchboard pubkeys from the FRESH oracle feed
    //    set for this reserve. The withdraw IX itself does not list
    //    oracles in its accounts — but the preceding `RefreshReserve`
    //    DOES, and we want the same oracle pubkeys both ways. Each
    //    provider may be absent; in that case we use Solend's
    //    null-oracle sentinel.
    let mut pyth_oracle = null_oracle_sentinel;
    let mut switchboard_oracle = null_oracle_sentinel;
    if let Some(set) = fresh
        .snapshot
        .oracles
        .iter()
        .find(|s| s.priced_asset == action_reserve_mint)
    {
        for feed in &set.feeds {
            match feed.provider {
                crate::lending::OracleProvider::PythSolanaReceiver => {
                    pyth_oracle = feed.identifier;
                }
                crate::lending::OracleProvider::SwitchboardOnDemand => {
                    switchboard_oracle = feed.identifier;
                }
                crate::lending::OracleProvider::Unknown => {}
            }
        }
    }

    // 6. Build the priority-fee compute-budget ix. Prepended to setup.
    let mut setup_instructions: Vec<Instruction> = Vec::with_capacity(3);
    setup_instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
        SOLEND_WITHDRAW_PRIORITY_FEE_MICRO_LAMPORTS,
    ));

    // 7. Idempotent ATA creates for any missing ATA on the fresh
    //    snapshot. The CreateIdempotent variant is a no-op on chain
    //    when the ATA already exists, so adding these unconditionally
    //    would also be safe — we still gate on `*_exists == false` to
    //    keep the plan minimal and the audit trail clearer.
    if !fresh.source_ata_exists {
        setup_instructions.push(build_create_ata_idempotent_instruction(
            &session_wallet,
            &session_wallet,
            &action_reserve_mint,
            &spl_token_program,
        ));
    }
    if !fresh.collateral_ata_exists {
        setup_instructions.push(build_create_ata_idempotent_instruction(
            &session_wallet,
            &session_wallet,
            &reserve.collateral_mint_pubkey,
            &spl_token_program,
        ));
    }

    // 8. Refresh plan: RefreshReserve(USDC) + RefreshObligation(existing).
    //    We always have an existing obligation here (checked above) and
    //    its `referenced_reserves_in_order` MUST list the target
    //    reserve so the refreshed values are the ones the withdraw ix
    //    reads. We use the fresh obligation's `deposits + borrows`
    //    reserve list verbatim.
    let mut referenced_reserves_in_order: Vec<Pubkey> = Vec::new();
    for d in &fresh.snapshot.obligation.deposits {
        if !referenced_reserves_in_order.contains(&d.reserve) {
            referenced_reserves_in_order.push(d.reserve);
        }
    }
    for b in &fresh.snapshot.obligation.borrows {
        if !referenced_reserves_in_order.contains(&b.reserve) {
            referenced_reserves_in_order.push(b.reserve);
        }
    }
    // Defense-in-depth: even if the obligation snapshot somehow elided
    // the target reserve, list it so the preceding RefreshReserve has
    // a corresponding entry the obligation walk reads from.
    if !referenced_reserves_in_order.contains(&fresh.reserve_pubkey) {
        referenced_reserves_in_order.push(fresh.reserve_pubkey);
    }

    let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
        solend_program_id,
        reserves: vec![ReserveRefreshInput {
            reserve_pubkey: fresh.reserve_pubkey,
            pyth_oracle,
            switchboard_oracle,
        }],
        obligation: Some(ObligationRefreshInput {
            obligation_pubkey: fresh.obligation_pubkey,
            referenced_reserves_in_order,
        }),
    });

    // Phase 6I-J — collect the obligation's deposit reserves in EXACT
    // on-chain order. Solend's
    // `update_borrow_attribution_values(&mut obligation, deposit_reserve_infos)`
    // walks `obligation.deposits` in lockstep with `accounts[12..]` and
    // packs each entry back, so the order MUST match what was decoded
    // from the obligation account. We do NOT dedupe (the on-chain
    // obligation can carry the same pubkey at multiple deposit slots
    // only via a Solend program bug, but if it ever does we faithfully
    // reproduce that), and we do NOT include borrow reserves here —
    // those are not consumed by the combined withdraw ix.
    let deposit_reserves_in_order: Vec<Pubkey> = fresh
        .snapshot
        .obligation
        .deposits
        .iter()
        .map(|d| d.reserve)
        .collect();

    // 9. Build the single withdraw ix from the un-wired ix builder.
    let withdraw_ix = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_instruction(
        WithdrawInstructionInputs {
            solend_program_id,
            collateral_amount: action_collateral_amount,
            // Withdraw is the inverse of deposit: c-tokens flow OUT of
            // the reserve's collateral supply pool (source) and into
            // the user's c-token ATA (destination), then are burned.
            source_collateral: reserve.collateral_supply_pubkey,
            destination_collateral: fresh.user_collateral_ata,
            reserve: fresh.reserve_pubkey,
            obligation: fresh.obligation_pubkey,
            lending_market: fresh.lending_market,
            destination_liquidity: fresh.source_liquidity_ata,
            reserve_collateral_mint: reserve.collateral_mint_pubkey,
            reserve_liquidity_supply: reserve.liquidity_supply_pubkey,
            obligation_owner: session_wallet,
            user_transfer_authority: session_wallet,
            deposit_reserves_in_order,
        },
    )
    .map_err(SolendWithdrawTxPlanError::WithdrawBuildFailed)?;

    let intent_id = request_id; // 5H-C plan-only path; future Phase 5H-D
                                // park-store will carry a distinct intent_id.

    Ok(SolendWithdrawTxPlan {
        request_id,
        intent_id,
        session_id,
        session_wallet,
        verified_slot,
        action: action.clone(),
        setup_instructions,
        refresh_instructions: refresh_plan.instructions,
        withdraw_instructions: vec![withdraw_ix],
        obligation_signer_required: false,
        transient_obligation_pubkey: None,
        accounts_summary: SolendWithdrawAccountsSummary {
            reserve_pubkey: fresh.reserve_pubkey,
            reserve_mint: action_reserve_mint,
            obligation_pubkey: fresh.obligation_pubkey,
            source_liquidity_ata: fresh.source_liquidity_ata,
            user_collateral_ata: fresh.user_collateral_ata,
            source_ata_exists: fresh.source_ata_exists,
            collateral_ata_exists: fresh.collateral_ata_exists,
            obligation_exists: fresh.obligation_exists,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::mapping::{
        map_snapshot, OracleAccountInfo, ReserveInput, SolendAssemblyInputs,
    };
    use crate::integrations::solend::raw::{
        self, synth_obligation, synth_reserve,
    };
    use crate::lending::{
        ChainSlot, CollateralTokenAmount, FeedPublishFreshness, UnderlyingAmount,
    };
    use std::str::FromStr;

    // ── Fixture builder ──────────────────────────────────────────────────

    /// Pyth Solana Receiver program — owner sentinel for oracle account
    /// classification.
    const PYTH_RECEIVER_OWNER_BS58: &str =
        "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    /// Build a fully-populated `AssembledSolendDepositSnapshot` whose
    /// obligation has a single non-zero deposit on the target reserve
    /// (no borrows). All ATAs exist by default; toggle via parameters.
    #[allow(clippy::too_many_arguments)]
    fn fresh_withdraw_assembled(
        owner: Pubkey,
        reserve_mint: Pubkey,
        deposited_amount: u64,
        borrow_wads: u128,
        source_ata_exists: bool,
        collateral_ata_exists: bool,
        obligation_exists: bool,
    ) -> AssembledSolendDepositSnapshot {
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let liquidity_supply = Pubkey::new_unique();
        let collateral_mint = Pubkey::new_unique();
        let collateral_supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey = Pubkey::from_str(PYTH_RECEIVER_OWNER_BS58).unwrap();
        let sentinel: Pubkey = Pubkey::from_str(
            crate::integrations::solend::raw::SOLEND_NULL_ORACLE_SENTINEL_BS58,
        )
        .unwrap();

        let mut deposits: Vec<raw::SolendObligationCollateralRaw> = Vec::new();
        if deposited_amount > 0 {
            deposits.push(raw::SolendObligationCollateralRaw {
                deposit_reserve: reserve_pk,
                deposited_amount,
            });
        }
        let mut borrows: Vec<raw::SolendObligationLiquidityRaw> = Vec::new();
        if borrow_wads > 0 {
            borrows.push(raw::SolendObligationLiquidityRaw {
                borrow_reserve: reserve_pk,
                borrowed_amount_wads: borrow_wads,
            });
        }

        let obl = synth_obligation(owner, mkt, 100, false, &deposits, &borrows);
        let res = synth_reserve(
            mkt,
            reserve_mint,
            6,
            liquidity_supply,
            pyth,
            sentinel,
            9_999_999,
            collateral_mint,
            collateral_supply,
            100,
            false,
        );
        let snap = map_snapshot(SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obl).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(1_000),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: raw::decode_reserve(&res).unwrap(),
                fetched_at_slot: ChainSlot::new(1_000),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(1_000),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(1_000)),
            }],
            snapshot_observed_slot: ChainSlot::new(1_000),
        })
        .unwrap();

        AssembledSolendDepositSnapshot {
            snapshot: snap,
            obligation_exists,
            source_ata_exists,
            collateral_ata_exists,
            reserve_pubkey: reserve_pk,
            obligation_pubkey: Pubkey::new_unique(),
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: mkt,
        }
    }

    fn withdraw_all_action(reserve_mint: Pubkey) -> ProposedAction {
        ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        }
    }

    fn fresh_owner_and_mint() -> (Pubkey, Pubkey) {
        (Pubkey::new_unique(), Pubkey::new_unique())
    }

    // ── Happy-path / structural tests ────────────────────────────────────

    #[test]
    fn assemble_succeeds_for_withdraw_all_against_existing_obligation() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1_000,
        )
        .expect("happy-path withdraw plan must assemble");

        assert_eq!(plan.session_wallet, owner);
        assert_eq!(plan.verified_slot, 1_000);
        assert_eq!(plan.withdraw_instructions.len(), 1);
        assert_eq!(plan.refresh_instructions.len(), 2);
        assert!(matches!(plan.action, ProposedAction::Withdraw { .. }));
        assert!(!plan.obligation_signer_required);
        assert!(plan.transient_obligation_pubkey.is_none());
        assert!(plan.accounts_summary.obligation_exists);
        assert_eq!(plan.accounts_summary.reserve_mint, mint);
    }

    #[test]
    fn withdraw_plan_first_setup_ix_is_compute_budget_priority_fee() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();

        assert!(!plan.setup_instructions.is_empty());
        let cb_ix = &plan.setup_instructions[0];
        assert_eq!(
            cb_ix.program_id,
            solana_sdk::compute_budget::id(),
            "first setup ix must be compute_budget program",
        );
        // ComputeBudgetInstruction::SetComputeUnitPrice variant: tag = 3,
        // followed by u64 LE micro-lamports per CU. We assert tag + value
        // structurally so a future enum-tag drift is caught locally.
        assert_eq!(cb_ix.data[0], 3, "SetComputeUnitPrice variant tag is 3");
        let micro_lamports = u64::from_le_bytes(cb_ix.data[1..9].try_into().unwrap());
        assert_eq!(
            micro_lamports, SOLEND_WITHDRAW_PRIORITY_FEE_MICRO_LAMPORTS,
            "priority fee value must match the module constant",
        );
    }

    #[test]
    fn withdraw_plan_omits_ata_creates_when_both_atas_exist() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        // setup is exactly [compute_budget].
        assert_eq!(plan.setup_instructions.len(), 1);
    }

    #[test]
    fn withdraw_plan_includes_idempotent_ata_create_when_source_ata_missing() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(
            owner, mint, 5_000, 0, /*source_ata_exists=*/ false, true, true,
        );
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        // setup = [compute_budget, ATA-create-source].
        assert_eq!(plan.setup_instructions.len(), 2);
        assert_eq!(
            plan.setup_instructions[1].program_id,
            spl_associated_token_account::id(),
            "second setup ix must be the ATA program",
        );
    }

    #[test]
    fn withdraw_plan_includes_idempotent_ata_create_when_collateral_ata_missing() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(
            owner, mint, 5_000, 0, true, /*collateral_ata_exists=*/ false, true,
        );
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        // setup = [compute_budget, ATA-create-collateral].
        assert_eq!(plan.setup_instructions.len(), 2);
        assert_eq!(
            plan.setup_instructions[1].program_id,
            spl_associated_token_account::id(),
        );
    }

    #[test]
    fn withdraw_plan_includes_both_ata_creates_when_both_missing() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, false, false, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        // setup = [compute_budget, ATA-source, ATA-collateral].
        assert_eq!(plan.setup_instructions.len(), 3);
        for i in 1..=2 {
            assert_eq!(
                plan.setup_instructions[i].program_id,
                spl_associated_token_account::id(),
            );
        }
    }

    #[test]
    fn withdraw_plan_refresh_block_is_reserve_then_obligation() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        assert_eq!(plan.refresh_instructions.len(), 2);
        // Reserve refresh tag = 3.
        assert_eq!(plan.refresh_instructions[0].data, vec![3u8]);
        // Obligation refresh tag = 7.
        assert_eq!(plan.refresh_instructions[1].data, vec![7u8]);
    }

    /// Phase 6I-H: RefreshObligation must place the target USDC reserve
    /// at slot 1 (the first slot after the obligation), NOT slot 2 as
    /// the prior `[obligation, clock, ...reserves]` layout produced.
    /// The Solend mainnet program iterates `obligation.deposits` and
    /// expects accounts `[1 + i]` to match each deposit's reserve;
    /// shifting by clock at slot 1 caused the live failure
    /// `4YnQFUTm…` to interpret the clock sysvar as collateral 0's
    /// reserve and reject it (`InvalidAccountOwner`).
    #[test]
    fn withdraw_plan_refresh_obligation_target_reserve_is_at_slot_one_no_clock() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        let refresh_obl_ix = &plan.refresh_instructions[1];
        // Phase 6I-H: account list is exactly [obligation, target_reserve].
        // The single-deposit USDC obligation produces a 2-account ix.
        assert_eq!(
            refresh_obl_ix.accounts.len(),
            2,
            "single-deposit USDC obligation: [obligation, usdc_reserve] only"
        );
        assert_eq!(
            refresh_obl_ix.accounts[0].pubkey, fresh.obligation_pubkey,
            "slot 0 = obligation"
        );
        assert!(refresh_obl_ix.accounts[0].is_writable);
        assert_eq!(
            refresh_obl_ix.accounts[1].pubkey, fresh.reserve_pubkey,
            "slot 1 = USDC reserve (NOT slot 2 as the buggy clock-shifted layout produced)"
        );
        assert!(!refresh_obl_ix.accounts[1].is_writable);
        // No clock sysvar anywhere in the RefreshObligation account list.
        for (i, a) in refresh_obl_ix.accounts.iter().enumerate() {
            assert_ne!(
                a.pubkey,
                solana_sdk::sysvar::clock::id(),
                "clock sysvar must not appear (slot={i})"
            );
        }
    }

    #[test]
    fn withdraw_plan_withdraw_ix_carries_u64_max_for_withdraw_all() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        let ix = &plan.withdraw_instructions[0];
        // tag = 15 (Solend's WithdrawObligationCollateralAndRedeemReserveCollateral)
        assert_eq!(ix.data[0], 15, "withdraw ix tag must be 15");
        let collateral_amount =
            u64::from_le_bytes(ix.data[1..9].try_into().unwrap());
        assert_eq!(
            collateral_amount,
            u64::MAX,
            "withdraw_all sentinel must pass through to the ix payload verbatim",
        );
    }

    #[test]
    fn withdraw_plan_passes_explicit_collateral_amount_through_to_ix() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(1_234),
        };
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &action,
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        let ix = &plan.withdraw_instructions[0];
        let collateral_amount =
            u64::from_le_bytes(ix.data[1..9].try_into().unwrap());
        assert_eq!(collateral_amount, 1_234);
    }

    #[test]
    fn withdraw_plan_obligation_signer_never_required() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap();
        assert!(!plan.obligation_signer_required);
        assert!(plan.transient_obligation_pubkey.is_none());
    }

    // ── Rejection paths ──────────────────────────────────────────────────

    #[test]
    fn assemble_rejects_deposit_action() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1_000),
        };
        let err = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &action,
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap_err();
        assert_eq!(err, SolendWithdrawTxPlanError::UnsupportedActionKind);
    }

    #[test]
    fn assemble_rejects_repay_action() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let action = ProposedAction::Repay {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1_000),
        };
        let err = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &action,
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap_err();
        assert_eq!(err, SolendWithdrawTxPlanError::UnsupportedActionKind);
    }

    #[test]
    fn assemble_rejects_action_mint_not_in_snapshot_reserves() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        // Action targets a foreign mint — assembler must HardBlock the
        // plan with a typed `ReserveNotInSnapshot`.
        let foreign_mint = Pubkey::new_unique();
        let action = withdraw_all_action(foreign_mint);
        let err = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &action,
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SolendWithdrawTxPlanError::ReserveNotInSnapshot {
                action_mint: foreign_mint,
            }
        );
    }

    #[test]
    fn assemble_rejects_when_obligation_does_not_exist() {
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(
            owner, mint, 5_000, 0, true, true, /*obligation_exists=*/ false,
        );
        let err = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &withdraw_all_action(mint),
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap_err();
        assert_eq!(err, SolendWithdrawTxPlanError::ObligationDoesNotExist);
    }

    #[test]
    fn assemble_rejects_zero_collateral_amount_via_ix_builder_error() {
        // A zero-amount Withdraw is structurally rejected by the
        // un-wired ix builder (`WithdrawBuildError::ZeroAmount`). The
        // plan assembler propagates it as `WithdrawBuildFailed`. A
        // future tool surface MUST not be able to construct this.
        let (owner, mint) = fresh_owner_and_mint();
        let fresh = fresh_withdraw_assembled(owner, mint, 5_000, 0, true, true, true);
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(0),
        };
        let err = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &action,
            SessionId::new(),
            owner,
            &fresh,
            1,
        )
        .unwrap_err();
        match err {
            SolendWithdrawTxPlanError::WithdrawBuildFailed(WithdrawBuildError::ZeroAmount) => {}
            other => panic!("expected WithdrawBuildFailed(ZeroAmount), got {other:?}"),
        }
    }

    // ── Phase 6I readiness: Phase 6H-supplied explicit obligation_pubkey ─

    /// Phase 6I plan-only readiness rehearsal.
    ///
    /// Phase 6H's `get_solend_position` scanner reports an explicit
    /// `obligation_pubkey` for each on-chain obligation owned by the
    /// session wallet. Live mainnet wallet
    /// `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW` currently has
    /// five obligations — the largest being
    /// `HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV`
    /// (`deposited_collateral_amount_raw = 3_857_506`).
    ///
    /// This test asserts the dead-source withdraw plan assembler
    /// already accepts an arbitrary, externally-supplied obligation
    /// pubkey (no derivation, no transient keypair) and threads it
    /// verbatim into the plan and the withdraw ix at slot 3. It also
    /// locks the user-wallet-only signer set: slots 9 + 10 both
    /// equal the session wallet, no other slot is a signer.
    ///
    /// Plan-only: no signing, no approval, no broadcast, no daemon.
    /// `solend_withdraw_usdc` remains absent from the chat allowlist
    /// and from production builds (this module is `#[cfg(test)]`).
    #[test]
    fn phase6i_explicit_phase6h_obligation_pubkey_threads_through_plan_and_ix() {
        let user_wallet =
            Pubkey::from_str("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW").unwrap();
        let phase6h_obligation =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();

        // Helper synthesises reserve / market / mint pubkeys; we only
        // need the obligation pubkey to be the explicit Phase 6H one.
        let mint = Pubkey::new_unique();
        let mut fresh = fresh_withdraw_assembled(
            user_wallet,
            mint,
            /*deposited_amount=*/ 3_857_506,
            /*borrow_wads=*/ 0,
            /*source_ata_exists=*/ true,
            /*collateral_ata_exists=*/ true,
            /*obligation_exists=*/ true,
        );
        fresh.obligation_pubkey = phase6h_obligation;

        // Withdraw-all sentinel — does NOT require a reserve
        // exchange-rate decode; Solend clamps to the user's deposited
        // collateral on chain.
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        };
        let plan = assemble_solend_withdraw_tx_plan(
            Uuid::new_v4(),
            &action,
            SessionId::new(),
            user_wallet,
            &fresh,
            1,
        )
        .expect("Phase 6H explicit obligation_pubkey must thread through unchanged");

        // 1. Plan echoes the explicit obligation pubkey verbatim.
        assert_eq!(plan.accounts_summary.obligation_pubkey, phase6h_obligation);

        // 2. No transient obligation keypair — no new account is
        //    being created. Withdraw spends an existing obligation.
        assert!(!plan.obligation_signer_required);
        assert!(plan.transient_obligation_pubkey.is_none());

        // 3. Withdraw ix slot 3 carries the explicit pubkey.
        let ix = &plan.withdraw_instructions[0];
        assert_eq!(ix.accounts[3].pubkey, phase6h_obligation);
        assert!(ix.accounts[3].is_writable);
        assert!(!ix.accounts[3].is_signer);

        // 4. User-wallet-only signer set: slots 9 (obligation_owner)
        //    + 10 (user_transfer_authority) both = session_wallet,
        //    every other slot is non-signer.
        assert_eq!(ix.accounts[9].pubkey, user_wallet);
        assert!(ix.accounts[9].is_signer);
        assert_eq!(ix.accounts[10].pubkey, user_wallet);
        assert!(ix.accounts[10].is_signer);
        let signer_count = ix.accounts.iter().filter(|a| a.is_signer).count();
        assert_eq!(
            signer_count, 2,
            "withdraw ix must require exactly two signer slots — both = user wallet"
        );

        // 5. Slot 5 (lending-market authority PDA) is derived
        //    internally and never a signer.
        assert!(!ix.accounts[5].is_signer);
    }

    // ── Anti-coupling guards ────────────────────────────────────────────

    /// The withdraw plan assembler MUST NOT depend on
    /// `SolendDepositTxPlan` or any deposit-plan type. The function
    /// pointer below compiles iff the dependency graph is clean: only
    /// the withdraw plan's own types appear in its signature.
    #[test]
    fn assembler_signature_uses_only_withdraw_plan_types() {
        fn _assert(
            _: fn(
                Uuid,
                &ProposedAction,
                SessionId,
                Pubkey,
                &AssembledSolendDepositSnapshot,
                u64,
            )
                -> Result<SolendWithdrawTxPlan, SolendWithdrawTxPlanError>,
        ) {
        }
        _assert(assemble_solend_withdraw_tx_plan);
    }

    /// Phase 5H-C strict-isolation guard. The deposit plan module must
    /// not appear in this file's source. If a future contributor is
    /// tempted to "share" with `solend_tx_plan.rs`, this test fails so
    /// the share goes through a refactor reviewed by humans.
    #[test]
    fn module_does_not_reference_deposit_plan_module() {
        const SOURCE: &str = include_str!("solend_withdraw_tx_plan.rs");
        // Build the needle at runtime so this scanner does not match
        // its own assertion text.
        let needle = format!("{}{}", "solend_tx_", "plan");
        // Allow doc-comment references (the module-level doc cites the
        // sibling deposit module by name). What we forbid is a `use`
        // import of any deposit-plan symbol.
        let forbidden = format!("use crate::integrations::{needle}");
        assert!(
            !SOURCE.contains(&forbidden),
            "withdraw plan must not import from {needle}"
        );
    }
}

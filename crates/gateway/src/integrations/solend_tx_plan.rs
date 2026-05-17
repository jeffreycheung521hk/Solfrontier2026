//! Phase 4C-2 — Solend deposit transaction PLAN assembly.
//!
//! # What this module owns
//!
//! A single pure function,
//! [`assemble_solend_deposit_tx_plan`], that composes the Slice 3G
//! `setup → refresh → deposit` instruction sequence into a typed
//! [`SolendDepositTxPlan`] artifact.
//!
//! The plan is intentionally a **grouped** set of instructions, not a
//! flat vector: Phase 4C-3+ will chunk groups into individual
//! transactions, so the grouping is the load-bearing contract.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT fetch a blockhash.
//! - Does NOT assemble a `Transaction` / `VersionedTransaction` / `MessageV0`.
//! - Does NOT use `AddressLookupTable`.
//! - Does NOT simulate.
//! - Does NOT sign.
//! - Does NOT broadcast.
//! - Does NOT generate or hold a `Keypair` / signer / private key.
//! - Does NOT touch RPC. The `rent_exempt_lamports` value is passed in,
//!   NOT fetched. See [`PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS`].
//!
//! These absences are load-bearing for 4C-2. Every downstream phase
//! (preflight/simulate in 4C-3, signing in 4C-4, broadcast in 4C-5)
//! will build on this artifact; this slice must remain execution-free.
//!
//! # Artifact semantics
//!
//! The returned [`SolendDepositTxPlan`] is a PLAN, not a
//! broadcastable transaction:
//!
//! - NO blockhash.
//! - NO signatures.
//! - NO `VersionedTransaction`.
//! - `transient_obligation_pubkey` may hold a fresh random `Pubkey`
//!   used as an identifier placeholder across setup/init/deposit ixs.
//!   It has NO private key and is NOT signable. Phase 4C-3/4C-5 MUST
//!   replace this placeholder with the pubkey of a generated
//!   obligation keypair before signing, and MUST refuse to sign while
//!   [`SolendDepositTxPlan::obligation_signer_required`] is `true`
//!   unless that replacement has been performed.
//! - `planning_rent_exempt_lamports` is a 4C-2 planning-time constant
//!   (see below). Phase 4C-3+ MUST re-fetch / verify rent exemption
//!   via RPC before signing.

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
use crate::integrations::solend::{
    build_create_ata_idempotent_instruction, build_create_obligation_account_instruction,
    build_deposit_reserve_liquidity_and_obligation_collateral_instruction,
    build_init_obligation_instruction, build_refresh_instructions,
    AssembledSolendDepositSnapshot, CreateObligationAccountInputs, DepositBuildError,
    DepositInstructionInputs, InitObligationInputs, ObligationRefreshInput, RefreshPlanInputs,
    ReserveRefreshInput,
};
use crate::integrations::solend_park::ParkedSolendDepositIntent;
use crate::lending::{OracleProvider, ProposedAction, ProtocolTag};

/// Planning-time rent-exempt lamports for the 1300-byte Solend obligation
/// account.
///
/// **This is NOT a protocol invariant.** The value matches the Slice 3G
/// live-mainnet observation for `getMinimumBalanceForRentExemption(1300)`
/// at the time of that proof. It is injected into the plan so Phase 4C-2
/// can remain RPC-free.
///
/// Phase 4C-3+ MUST verify rent exemption via RPC before signing /
/// broadcasting — rent parameters can change via governance
/// (`system_program` rent sysvar updates), so treating this constant as
/// authoritative at broadcast time is unsafe.
pub const PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS: u64 = 9_938_880;

/// Phase 4C-8G-Fix — priority fee in micro-lamports per compute unit.
///
/// The first 4C-8G live mainnet attempt produced a valid signature
/// (`3PUtp…21QT`) that was accepted by the RPC but never landed on
/// chain — `getSignatureStatuses(searchTransactionHistory: true)`
/// returned `null` for the full block-validity window. The classical
/// cause for that pattern is leader-queue contention dropping a
/// zero-priority-fee transaction during mainnet congestion.
///
/// 50_000 micro-lamports per CU is the same value Slice 3G's
/// `live_solend_deposit_execute` proof used (modest enough that it
/// does not distort fees; high enough to survive ordinary mainnet
/// congestion). It is added to the planned transaction BEFORE 4C-3
/// preflight, so the simulator and the signer see the same fee'd
/// instruction stream the network does.
///
/// **Where this is used:** prepended as the FIRST instruction of
/// `setup_instructions` in [`assemble_solend_deposit_tx_plan`]. The
/// resulting concatenated single-tx order is:
///
///   ComputeBudget::set_compute_unit_price → setup → refresh → deposit
///
/// Adding it inside the plan (rather than at submit time) preserves
/// 4C-5's message-hash immutability: the signed message hash already
/// covers the priority-fee instruction, so submit verification still
/// catches any post-handoff tampering.
pub const SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS: u64 = 50_000;

/// Execution-facing summary of the accounts the plan references.
///
/// Carried alongside the instruction groups so Phase 4C-3+ does not have
/// to re-derive any of these from the ix account metas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendDepositAccountsSummary {
    pub reserve_pubkey: Pubkey,
    pub reserve_mint: Pubkey,
    pub obligation_pubkey: Pubkey,
    pub source_liquidity_ata: Pubkey,
    pub user_collateral_ata: Pubkey,
    pub source_ata_exists: bool,
    pub collateral_ata_exists: bool,
    pub obligation_exists: bool,
}

/// Phase 4C-2 transaction PLAN artifact.
///
/// See module docs for what this is and is NOT. In particular: this
/// artifact is NOT a transaction, does NOT carry a blockhash, does NOT
/// carry signatures, and is NOT broadcastable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendDepositTxPlan {
    pub request_id: Uuid,
    pub intent_id: Uuid,
    pub session_id: SessionId,
    pub session_wallet: Pubkey,
    /// The observed slot of the FRESH snapshot used for the re-check
    /// that produced this plan. Surfaced so Phase 4C-3 can audit
    /// "approval → plan → broadcast" slot provenance.
    pub verified_slot: u64,
    pub action: ProposedAction,

    /// Instructions that create the accounts the deposit ix depends on.
    /// Ordering inside the group is: idempotent source ATA create (if
    /// missing) → SystemProgram::CreateAccount for the obligation +
    /// InitObligation (if missing) → idempotent collateral ATA create
    /// (if missing). Any subset may be empty if the corresponding
    /// `*_exists` flag on the fresh snapshot was `true`.
    pub setup_instructions: Vec<Instruction>,

    /// Solend `RefreshReserve` (+ `RefreshObligation` on existing
    /// obligations) instructions. MUST precede `deposit_instructions`
    /// in any transaction composition performed by a downstream phase —
    /// Solend's deposit handler reads reserve-derived values that are
    /// only valid within the slot of the preceding refresh.
    pub refresh_instructions: Vec<Instruction>,

    /// Solend `DepositReserveLiquidityAndObligationCollateral`
    /// instruction(s). In 4C-2 this is exactly one instruction; the
    /// field is a `Vec` so future slices can chain (e.g.,
    /// pre-deposit transfer-fee ixs) without a shape change.
    pub deposit_instructions: Vec<Instruction>,

    /// `Some(pubkey)` when the plan references a freshly-generated
    /// placeholder pubkey as the obligation account identifier. The
    /// placeholder is NOT a keypair; Phase 4C-3/4C-5 MUST replace it
    /// with the pubkey of a newly generated obligation keypair before
    /// signing. `None` when the fresh snapshot already observed an
    /// on-chain obligation account (no generation needed).
    pub transient_obligation_pubkey: Option<Pubkey>,

    /// `true` iff the plan's setup ixs include a `CreateAccount` for a
    /// fresh obligation — i.e., the obligation keypair must sign. This
    /// is precisely `!fresh.obligation_exists` at plan-assembly time.
    /// The plan is NOT ready to sign while this is `true` unless the
    /// placeholder in `transient_obligation_pubkey` has been replaced
    /// by a real-keypair pubkey.
    pub obligation_signer_required: bool,

    pub accounts_summary: SolendDepositAccountsSummary,

    /// The rent-exempt lamports value actually used by this plan's
    /// `CreateAccount` ix (if any). Echoed here for audit — Phase 4C-3+
    /// must re-verify against RPC before signing.
    pub planning_rent_exempt_lamports: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SolendTxPlanError {
    #[error(
        "non-Solend protocol cannot be planned by this assembler (got {protocol:?})"
    )]
    UnsupportedProtocol { protocol: ProtocolTag },
    #[error("only Deposit actions are supported by this assembler in Phase 4C-2")]
    UnsupportedActionKind,
    #[error(
        "action's reserve mint {action_mint} was not present in the fresh snapshot's reserves"
    )]
    ReserveNotInSnapshot { action_mint: Pubkey },
    #[error("Solend deposit builder rejected plan inputs: {0}")]
    DepositBuildFailed(DepositBuildError),
    #[error("internal invariant violated: well-known Solend constant failed to parse")]
    InternalConstantParseFailed,
}

/// Phase 4C-2 plan-assembly entry point. Pure: no RPC, no clock, no
/// blockhash, no signer, no simulation, no broadcast.
///
/// # Inputs
///
/// - `request_id` — the approval request id the plan is anchored to.
/// - `parked` — the parked intent from [`crate::integrations::solend_park`].
///   Provides the action + session identity only; its SNAPSHOT is NOT
///   used as authoritative state for the plan.
/// - `fresh` — the FRESH snapshot produced by the 4C-1 post-approval
///   re-check. All `*_exists` flags, reserve pubkeys, oracle pubkeys,
///   and ATA pubkeys come from this value.
/// - `verified_slot` — the fresh snapshot's observed slot; echoed into
///   the plan for audit.
/// - `rent_exempt_lamports` — caller-injected planning-time constant.
///   Production code passes [`PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS`].
///   Phase 4C-3+ MUST re-verify via RPC before signing.
pub fn assemble_solend_deposit_tx_plan(
    request_id: Uuid,
    parked: &ParkedSolendDepositIntent,
    fresh: &AssembledSolendDepositSnapshot,
    verified_slot: u64,
    rent_exempt_lamports: u64,
) -> Result<SolendDepositTxPlan, SolendTxPlanError> {
    // 1. Action must be a Solend Deposit.
    let (action_protocol, action_reserve_mint, action_amount) = match &parked.action {
        ProposedAction::Deposit { protocol, reserve_mint, amount } => {
            (*protocol, *reserve_mint, *amount)
        }
        ProposedAction::Repay { .. } => {
            return Err(SolendTxPlanError::UnsupportedActionKind);
        }
        // Phase 5H — Withdraw is a protocol-level action variant but the
        // deposit tx-plan assembler only knows how to compose deposit
        // ixs. Return the existing typed unsupported-action error so the
        // resume task surfaces this as `RecheckPassedPlanAssemblyFailed`
        // rather than a panic. The withdraw flow has its own (un-wired,
        // future) plan assembler — see Phase 5H roadmap. Note: this arm
        // is unreachable in production today because no code path
        // constructs `ProposedAction::Withdraw` (the future withdraw
        // tool that would do so is not wired in 5H-B).
        ProposedAction::Withdraw { .. } => {
            return Err(SolendTxPlanError::UnsupportedActionKind);
        }
    };
    if action_protocol != ProtocolTag::Solend {
        return Err(SolendTxPlanError::UnsupportedProtocol {
            protocol: action_protocol,
        });
    }

    // 2. Parse well-known program / sentinel constants. Compile-time
    //    literals; any parse failure is a programming error.
    let solend_program_id = Pubkey::try_from(SOLEND_PROGRAM_ID_BS58)
        .map_err(|_| SolendTxPlanError::InternalConstantParseFailed)?;
    let spl_token_program = Pubkey::try_from(SPL_TOKEN_PROGRAM_ID_BS58)
        .map_err(|_| SolendTxPlanError::InternalConstantParseFailed)?;
    let null_oracle_sentinel = Pubkey::try_from(SOLEND_NULL_ORACLE_SENTINEL_BS58)
        .map_err(|_| SolendTxPlanError::InternalConstantParseFailed)?;

    // 3. Find the target reserve in the FRESH snapshot — never the
    //    parked snapshot.
    let reserve = fresh
        .snapshot
        .reserves
        .iter()
        .find(|r| r.mint == action_reserve_mint)
        .ok_or(SolendTxPlanError::ReserveNotInSnapshot {
            action_mint: action_reserve_mint,
        })?;

    // 4. Extract pyth + switchboard pubkeys from the FRESH oracle feed
    //    set for this reserve. Each provider may be absent; in that case
    //    we use Solend's null-oracle sentinel. The policy layer has
    //    already decided the feed set is fresh enough to pass.
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
                OracleProvider::PythSolanaReceiver => pyth_oracle = feed.identifier,
                OracleProvider::SwitchboardOnDemand => switchboard_oracle = feed.identifier,
                OracleProvider::Unknown => {}
            }
        }
    }

    // 5. Resolve obligation identity from FRESH state.
    //
    //    - Existing obligation → use the fresh pubkey directly. No
    //      transient placeholder. Obligation signer NOT required because
    //      no CreateAccount ix is emitted.
    //    - Missing obligation → generate a transient placeholder pubkey
    //      for consistent use across setup/init/deposit ixs. This is
    //      ONLY an identifier; no keypair is generated, no secret key
    //      is stored. Phase 4C-3/4C-5 MUST swap it with the pubkey of
    //      a real obligation keypair before signing.
    let (obligation_pubkey, transient_obligation_pubkey, obligation_signer_required) = if fresh
        .obligation_exists
    {
        (fresh.obligation_pubkey, None, false)
    } else {
        let transient = Pubkey::new_unique();
        (transient, Some(transient), true)
    };

    let session_wallet = parked.session_wallet;

    // 6. Setup instructions — fresh flags only (spec §4).
    //
    // Phase 4C-8G-Fix: priority fee MUST be the first instruction in
    // the concatenated single-tx order, so we unconditionally seed
    // `setup_instructions` with `ComputeBudget::set_compute_unit_price`.
    // This places it before any conditional setup ix, before refresh,
    // and before deposit. Because it ships inside the PLAN, the 4C-3
    // preflight simulator, the 4C-4 signing handoff, and the 4C-5
    // submit verification all see the same fee'd instruction stream
    // the network does.
    let mut setup_instructions: Vec<Instruction> = vec![
        ComputeBudgetInstruction::set_compute_unit_price(
            SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS,
        ),
    ];
    if !fresh.source_ata_exists {
        setup_instructions.push(build_create_ata_idempotent_instruction(
            &session_wallet,
            &session_wallet,
            &reserve.mint,
            &spl_token_program,
        ));
    }
    if !fresh.obligation_exists {
        setup_instructions.push(build_create_obligation_account_instruction(
            CreateObligationAccountInputs {
                funder: session_wallet,
                new_obligation: obligation_pubkey,
                rent_exempt_lamports,
                solend_program_id,
            },
        ));
        setup_instructions.push(build_init_obligation_instruction(InitObligationInputs {
            solend_program_id,
            obligation: obligation_pubkey,
            lending_market: fresh.lending_market,
            obligation_owner: session_wallet,
        }));
    }
    if !fresh.collateral_ata_exists {
        setup_instructions.push(build_create_ata_idempotent_instruction(
            &session_wallet,
            &session_wallet,
            &reserve.collateral_mint_pubkey,
            &spl_token_program,
        ));
    }

    // 7. Refresh instructions. Always refresh the target reserve; if
    //    the obligation exists, also emit a RefreshObligation walking
    //    its deposits in the on-chain order.
    let obligation_refresh = if fresh.obligation_exists {
        let referenced_reserves_in_order: Vec<Pubkey> = fresh
            .snapshot
            .obligation
            .deposits
            .iter()
            .map(|d| d.reserve)
            .collect();
        Some(ObligationRefreshInput {
            obligation_pubkey,
            referenced_reserves_in_order,
        })
    } else {
        None
    };
    let refresh_plan = build_refresh_instructions(RefreshPlanInputs {
        solend_program_id,
        reserves: vec![ReserveRefreshInput {
            reserve_pubkey: reserve.identifier,
            pyth_oracle,
            switchboard_oracle,
        }],
        obligation: obligation_refresh,
    });
    let refresh_instructions = refresh_plan.instructions;

    // 8. Deposit instruction.
    let deposit_ix = build_deposit_reserve_liquidity_and_obligation_collateral_instruction(
        DepositInstructionInputs {
            solend_program_id,
            amount: action_amount,
            source_liquidity: fresh.source_liquidity_ata,
            user_collateral: fresh.user_collateral_ata,
            reserve: reserve.identifier,
            reserve_liquidity_supply: reserve.liquidity_supply_pubkey,
            reserve_collateral_mint: reserve.collateral_mint_pubkey,
            lending_market: fresh.lending_market,
            destination_deposit_collateral: reserve.collateral_supply_pubkey,
            obligation: obligation_pubkey,
            obligation_owner: session_wallet,
            pyth_oracle,
            switchboard_oracle,
            // V1 single-wallet: user_transfer_authority == obligation_owner.
            user_transfer_authority: session_wallet,
        },
    )
    .map_err(SolendTxPlanError::DepositBuildFailed)?;
    let deposit_instructions = vec![deposit_ix];

    // 9. Accounts summary — echo of fresh state only.
    let accounts_summary = SolendDepositAccountsSummary {
        reserve_pubkey: reserve.identifier,
        reserve_mint: reserve.mint,
        obligation_pubkey,
        source_liquidity_ata: fresh.source_liquidity_ata,
        user_collateral_ata: fresh.user_collateral_ata,
        source_ata_exists: fresh.source_ata_exists,
        collateral_ata_exists: fresh.collateral_ata_exists,
        obligation_exists: fresh.obligation_exists,
    };

    Ok(SolendDepositTxPlan {
        request_id,
        intent_id: parked.intent_id,
        session_id: parked.session_id.clone(),
        session_wallet,
        verified_slot,
        action: parked.action.clone(),
        setup_instructions,
        refresh_instructions,
        deposit_instructions,
        transient_obligation_pubkey,
        obligation_signer_required,
        accounts_summary,
        planning_rent_exempt_lamports: rent_exempt_lamports,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::mapping::{
        map_snapshot, map_snapshot_for_first_deposit, FirstDepositAssemblyInputs,
        OracleAccountInfo, ReserveInput, SolendAssemblyInputs,
    };
    use crate::integrations::solend::raw::{decode_obligation, decode_reserve, synth_obligation, synth_reserve};
    use crate::lending::{
        ChainSlot, FeedPublishFreshness, LendingPolicyVerdict, UnderlyingAmount,
    };
    use chrono::{Duration, Utc};
    use claw_types::session::SessionId;

    // ── Fixture helpers ───────────────────────────────────────────────────

    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const PYTH_RECEIVER_PROGRAM_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    fn usdc_mint() -> Pubkey {
        Pubkey::try_from(USDC_MINT_BS58).unwrap()
    }

    /// Build a fresh, Pass-verdict `AssembledSolendDepositSnapshot` for
    /// `session_wallet` at `snapshot_slot`, with the `*_exists` flags
    /// controllable by the caller.
    #[allow(clippy::too_many_arguments)]
    fn fresh_assembled(
        session_wallet: Pubkey,
        snapshot_slot: u64,
        obligation_exists: bool,
        source_ata_exists: bool,
        collateral_ata_exists: bool,
        pyth_identifier: Option<Pubkey>,
    ) -> AssembledSolendDepositSnapshot {
        let reserve_mint = usdc_mint();
        let market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = pyth_identifier.unwrap_or_else(Pubkey::new_unique);
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();
        let pyth_owner: Pubkey = PYTH_RECEIVER_PROGRAM_BS58.parse().unwrap();

        let res_bytes = synth_reserve(
            market,
            reserve_mint,
            6,
            supply,
            pyth,
            sentinel, // switchboard sentinel
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            /*stale=*/ false,
        );
        let reserve_raw = decode_reserve(&res_bytes).unwrap();

        let snapshot = if obligation_exists {
            // Existing obligation, not stale.
            let obl_bytes = synth_obligation(session_wallet, market, 100, false, &[], &[]);
            let obligation_raw = decode_obligation(&obl_bytes).unwrap();
            map_snapshot(SolendAssemblyInputs {
                session_wallet,
                obligation_pubkey: Pubkey::new_unique(),
                obligation_raw,
                obligation_fetched_at_slot: ChainSlot::new(snapshot_slot),
                reserves: vec![ReserveInput {
                    pubkey: reserve_pubkey,
                    raw: reserve_raw.clone(),
                    fetched_at_slot: ChainSlot::new(snapshot_slot),
                }],
                oracles: vec![OracleAccountInfo {
                    pubkey: pyth,
                    owner_program: pyth_owner,
                    fetched_at_slot: ChainSlot::new(snapshot_slot),
                    publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(snapshot_slot)),
                }],
                snapshot_observed_slot: ChainSlot::new(snapshot_slot),
            })
            .unwrap()
        } else {
            map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
                session_wallet,
                obligation_pubkey: Pubkey::new_unique(),
                lending_market: market,
                target_reserves: vec![ReserveInput {
                    pubkey: reserve_pubkey,
                    raw: reserve_raw.clone(),
                    fetched_at_slot: ChainSlot::new(snapshot_slot),
                }],
                oracles: vec![OracleAccountInfo {
                    pubkey: pyth,
                    owner_program: pyth_owner,
                    fetched_at_slot: ChainSlot::new(snapshot_slot),
                    publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(snapshot_slot)),
                }],
                snapshot_observed_slot: ChainSlot::new(snapshot_slot),
            })
            .unwrap()
        };

        AssembledSolendDepositSnapshot {
            snapshot,
            obligation_exists,
            source_ata_exists,
            collateral_ata_exists,
            reserve_pubkey,
            obligation_pubkey: Pubkey::new_unique(),
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: market,
        }
    }

    fn parked_intent(
        session_wallet: Pubkey,
        amount_raw: u64,
    ) -> ParkedSolendDepositIntent {
        // The parked snapshot is audit context only — the plan assembler
        // must never read it for authoritative flags. We fabricate a
        // minimal valid one here.
        let fresh_for_parking = fresh_assembled(session_wallet, 1_000, false, false, false, None);
        ParkedSolendDepositIntent {
            intent_id: Uuid::new_v4(),
            action: ProposedAction::Deposit {
                protocol: ProtocolTag::Solend,
                reserve_mint: usdc_mint(),
                amount: UnderlyingAmount::new(amount_raw),
            },
            snapshot: fresh_for_parking.snapshot,
            obligation_exists: fresh_for_parking.obligation_exists,
            source_ata_exists: fresh_for_parking.source_ata_exists,
            collateral_ata_exists: fresh_for_parking.collateral_ata_exists,
            verdict_at_propose: LendingPolicyVerdict::Pass,
            proposed_at: Utc::now(),
            expires_at: Utc::now() + Duration::seconds(60),
            session_id: SessionId::new(),
            session_wallet,
        }
    }

    // Count SystemProgram::CreateAccount ixs (by program id) in a group.
    fn count_system_create_account_like(group: &[Instruction]) -> usize {
        let system = solana_sdk::system_program::id();
        group
            .iter()
            .filter(|ix| ix.program_id == system)
            .count()
    }

    fn spl_token_program() -> Pubkey {
        Pubkey::try_from(SPL_TOKEN_PROGRAM_ID_BS58).unwrap()
    }

    fn associated_token_program() -> Pubkey {
        spl_associated_token_account::ID
    }

    fn solend_program() -> Pubkey {
        Pubkey::try_from(SOLEND_PROGRAM_ID_BS58).unwrap()
    }

    const RENT: u64 = PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS;

    // ── (A) Happy path: Pass → plan produced ──────────────────────────────

    #[test]
    fn plan_is_produced_for_passing_fresh_snapshot() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .expect("plan assembles");

        assert_eq!(plan.verified_slot, 2_000);
        assert_eq!(plan.session_wallet, wallet);
        assert_eq!(plan.action, parked.action);
        assert_eq!(plan.planning_rent_exempt_lamports, RENT);
        // First-deposit shape: obligation missing → signer required.
        assert!(plan.obligation_signer_required);
        assert!(plan.transient_obligation_pubkey.is_some());
        assert!(!plan.setup_instructions.is_empty());
        assert!(!plan.refresh_instructions.is_empty());
        assert_eq!(plan.deposit_instructions.len(), 1);
    }

    // ── (B) Fresh flags override parked flags ─────────────────────────────

    #[test]
    fn plan_uses_fresh_flags_not_parked_flags_source_ata() {
        let wallet = Pubkey::new_unique();
        // Parked snapshot: source_ata_exists = false (see helper default).
        let mut parked = parked_intent(wallet, 1_000);
        parked.source_ata_exists = false;
        parked.obligation_exists = false;
        parked.collateral_ata_exists = false;

        // Fresh snapshot: source ATA NOW exists → plan must NOT include a
        // source ATA create.
        let fresh = fresh_assembled(wallet, 3_000, false, /*source_ata_exists=*/ true, false, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            3_000,
            RENT,
        )
        .unwrap();

        // Source ATA create absent.
        // ATA create program id is the Associated Token Account program.
        let ata_program = associated_token_program();
        let source_mint = usdc_mint();
        let source_ata_create_count = plan
            .setup_instructions
            .iter()
            .filter(|ix| ix.program_id == ata_program)
            .filter(|ix| ix.accounts.iter().any(|a| a.pubkey == source_mint))
            .count();
        assert_eq!(
            source_ata_create_count, 0,
            "fresh source_ata_exists=true must skip source ATA create"
        );

        // Accounts summary reflects fresh, not parked.
        assert!(plan.accounts_summary.source_ata_exists);
    }

    // ── (C) Missing obligation → transient placeholder ────────────────────

    #[test]
    fn missing_obligation_generates_transient_placeholder_and_flags_signer() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let transient = plan
            .transient_obligation_pubkey
            .expect("transient placeholder present");
        assert_ne!(transient, Pubkey::default(), "must not be Pubkey::default()");
        assert!(plan.obligation_signer_required);
        assert_eq!(plan.accounts_summary.obligation_pubkey, transient);

        // The placeholder must appear consistently in:
        //   - SystemProgram::CreateAccount (setup)
        //   - InitObligation (setup)
        //   - Deposit (deposit group)
        let in_setup_create = plan
            .setup_instructions
            .iter()
            .filter(|ix| ix.program_id == solana_sdk::system_program::id())
            .any(|ix| ix.accounts.iter().any(|a| a.pubkey == transient));
        assert!(in_setup_create, "placeholder must appear in CreateAccount ix");

        let in_init = plan
            .setup_instructions
            .iter()
            .filter(|ix| ix.program_id == solend_program())
            .any(|ix| ix.accounts.iter().any(|a| a.pubkey == transient));
        assert!(in_init, "placeholder must appear in InitObligation ix");

        let in_deposit = plan
            .deposit_instructions
            .iter()
            .any(|ix| ix.accounts.iter().any(|a| a.pubkey == transient));
        assert!(in_deposit, "placeholder must appear in Deposit ix");
    }

    // ── (D) Existing obligation → no create/init ──────────────────────────

    #[test]
    fn existing_obligation_omits_create_and_init_and_has_no_transient() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(
            wallet, 2_000, /*obligation_exists=*/ true, true, true, None,
        );

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        assert!(plan.transient_obligation_pubkey.is_none());
        assert!(!plan.obligation_signer_required);

        // No SystemProgram::CreateAccount in setup.
        assert_eq!(count_system_create_account_like(&plan.setup_instructions), 0);
        // No InitObligation either (Solend ix tag 6) — just check: no
        // instruction in setup is Solend-owned when both ATAs also exist.
        assert!(plan
            .setup_instructions
            .iter()
            .all(|ix| ix.program_id != solend_program()));
    }

    // ── (E) Missing source ATA → idempotent source ATA create included ────

    #[test]
    fn missing_source_ata_includes_idempotent_create() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, true, /*source=*/ false, true, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let ata_program = associated_token_program();
        let source_mint = usdc_mint();
        // Idempotent variant discriminator: spl-associated-token-account
        // exposes `create_associated_token_account_idempotent`, whose
        // instruction data is a single byte `1`. The non-idempotent
        // variant uses `0`. We assert on the data byte AND the target mint.
        let found = plan.setup_instructions.iter().any(|ix| {
            ix.program_id == ata_program
                && ix.data == vec![1]
                && ix.accounts.iter().any(|a| a.pubkey == source_mint)
        });
        assert!(
            found,
            "plan must include idempotent source ATA create (discriminator byte 1) for source mint"
        );
    }

    // ── (F) Existing source ATA → no source ATA create ───────────────────

    #[test]
    fn existing_source_ata_omits_create() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, true, /*source=*/ true, true, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let ata_program = associated_token_program();
        let source_mint = usdc_mint();
        assert!(plan
            .setup_instructions
            .iter()
            .filter(|ix| ix.program_id == ata_program)
            .all(|ix| !ix.accounts.iter().any(|a| a.pubkey == source_mint)));
    }

    // ── (G) Missing collateral ATA → idempotent collateral ATA create ─────

    #[test]
    fn missing_collateral_ata_includes_idempotent_create() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, true, true, /*coll=*/ false, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let ata_program = associated_token_program();
        let collateral_mint = plan.accounts_summary.obligation_pubkey; // not what we want
        // Pull the reserve's collateral mint from the fresh snapshot.
        let collateral_mint = fresh.snapshot.reserves[0].collateral_mint_pubkey;
        let _ = collateral_mint; // keep both for clarity; second shadow wins
        let coll_mint = fresh.snapshot.reserves[0].collateral_mint_pubkey;
        let found = plan.setup_instructions.iter().any(|ix| {
            ix.program_id == ata_program
                && ix.data == vec![1]
                && ix.accounts.iter().any(|a| a.pubkey == coll_mint)
        });
        assert!(
            found,
            "plan must include idempotent collateral ATA create for cToken mint"
        );
    }

    // ── (H) Existing collateral ATA → no collateral ATA create ────────────

    #[test]
    fn existing_collateral_ata_omits_create() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, true, true, /*coll=*/ true, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let ata_program = associated_token_program();
        let coll_mint = fresh.snapshot.reserves[0].collateral_mint_pubkey;
        assert!(plan
            .setup_instructions
            .iter()
            .filter(|ix| ix.program_id == ata_program)
            .all(|ix| !ix.accounts.iter().any(|a| a.pubkey == coll_mint)));
    }

    // ── (I) Refresh precedes deposit and both are non-empty ───────────────

    #[test]
    fn refresh_precedes_deposit_as_separate_groups() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);

        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        assert!(!plan.refresh_instructions.is_empty());
        assert!(!plan.deposit_instructions.is_empty());
        // The groups are separate Vecs — structurally guaranteed.
        // Additionally: every refresh ix targets the Solend program.
        let solend = solend_program();
        assert!(plan
            .refresh_instructions
            .iter()
            .all(|ix| ix.program_id == solend));
        // And the deposit ix targets Solend.
        assert!(plan.deposit_instructions.iter().all(|ix| ix.program_id == solend));
    }

    // ── (J) No blockhash / signatures / VersionedTransaction ──────────────
    //
    // Structural guarantee: `SolendDepositTxPlan` has NO field that could
    // carry a blockhash or signatures. We pattern-match to destructure so
    // any future field addition forces review.

    #[test]
    fn plan_has_no_blockhash_no_signatures_no_versioned_tx_fields() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(), &parked, &fresh, 2_000, RENT,
        )
        .unwrap();

        // Exhaustive destructure — compilation fails if new fields land.
        let SolendDepositTxPlan {
            request_id: _,
            intent_id: _,
            session_id: _,
            session_wallet: _,
            verified_slot: _,
            action: _,
            setup_instructions: _,
            refresh_instructions: _,
            deposit_instructions: _,
            transient_obligation_pubkey: _,
            obligation_signer_required: _,
            accounts_summary: _,
            planning_rent_exempt_lamports: _,
        } = plan;
    }

    // ── (K) Production module source has no simulate / broadcast / sign ──
    //
    // Grep-backed structural guard on the PRODUCTION portion of this file
    // only (up to `#[cfg(test)]`). Tests legitimately reference these
    // tokens when asserting absence.

    #[test]
    fn production_source_has_no_simulate_broadcast_sign_or_alt_path() {
        let source = include_str!("solend_tx_plan.rs");
        let production: &str = match source.find("#[cfg(test)]") {
            Some(i) => &source[..i],
            None => source,
        };
        // Note: `ComputeBudgetInstruction` is intentionally NOT in this
        // list. Phase 4C-8G-Fix legitimately uses it to prepend a
        // `set_compute_unit_price` instruction (priority fee) at plan
        // build time. Compute-budget instructions are pure-data
        // builders — no signer, no RPC, no broadcast — so they belong
        // to the planning layer.
        let forbidden = [
            "VersionedTransaction",
            "Transaction::new",
            "MessageV0",
            "AddressLookupTable",
            "get_latest_blockhash(",
            "latest_blockhash(",
            "simulate_transaction(",
            "simulateTransaction",
            "send_transaction(",
            "send_raw_transaction(",
            "send_raw_v0_transaction(",
            "Keypair::",
            "Signer::",
            "panic!(",
            "todo!(",
            "unimplemented!(",
            "Pubkey::default(",
            // Non-idempotent ATA create (guarded against regression).
            "create_associated_token_account(",
        ];
        for line in production.lines() {
            let t = line.trim_start();
            if t.starts_with("//!") || t.starts_with("///") || t.starts_with("//") {
                continue;
            }
            for tok in forbidden {
                assert!(
                    !line.contains(tok),
                    "solend_tx_plan.rs production must not reference `{tok}`; line: {line:?}"
                );
            }
        }
    }

    // ── (L) verified_slot echoes the fresh snapshot's slot ────────────────

    #[test]
    fn verified_slot_in_plan_matches_fresh_snapshot_slot() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        // Parked snapshot was at 1_000; fresh at 7_777.
        let fresh = fresh_assembled(wallet, 7_777, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            7_777,
            RENT,
        )
        .unwrap();
        assert_eq!(plan.verified_slot, 7_777);
        // And the fresh snapshot's observed slot is what we used.
        assert_eq!(
            fresh.snapshot.fetched_at.observed_slot.raw(),
            plan.verified_slot,
            "verified_slot must equal fresh snapshot's observed slot"
        );
    }

    // ── (N) No ALT / no v0 tx in this slice ───────────────────────────────
    //
    // Same forbidden scan as (K), ALT-specific case already covered.
    // Additional assertion: the plan has Vec<Instruction> only — no
    // AddressLookupTableAccount / v0 message type. Compile-time: a missing
    // type would change the public shape.

    #[test]
    fn plan_uses_raw_instructions_not_v0_or_alt() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(), &parked, &fresh, 2_000, RENT,
        )
        .unwrap();

        // Type-level check via variable bindings.
        let _setup: &Vec<Instruction> = &plan.setup_instructions;
        let _refresh: &Vec<Instruction> = &plan.refresh_instructions;
        let _deposit: &Vec<Instruction> = &plan.deposit_instructions;
    }

    // ── (O) Rent constant is the documented 4C-2 placeholder ──────────────

    #[test]
    fn rent_constant_equals_9_938_880_and_is_used_for_create_account() {
        assert_eq!(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS, 9_938_880);
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(), &parked, &fresh, 2_000, RENT,
        )
        .unwrap();
        assert_eq!(plan.planning_rent_exempt_lamports, RENT);
        // And the create_account ix carries that lamports value.
        // `system_instruction::create_account` encodes lamports at
        // offset 4 (4-byte discriminator `0u32 LE`, then 8-byte lamports
        // LE). Assert that byte layout matches.
        let create = plan
            .setup_instructions
            .iter()
            .find(|ix| ix.program_id == solana_sdk::system_program::id())
            .expect("CreateAccount ix present in setup for first-deposit plan");
        assert!(create.data.len() >= 12);
        let lamports_bytes: [u8; 8] = create.data[4..12].try_into().unwrap();
        let lamports = u64::from_le_bytes(lamports_bytes);
        assert_eq!(lamports, RENT);
    }

    // ── (P) ATA creation uses the idempotent variant ──────────────────────

    #[test]
    fn ata_create_uses_idempotent_variant_discriminator() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        // Both ATAs missing → plan should emit two idempotent creates.
        let fresh = fresh_assembled(wallet, 2_000, true, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(), &parked, &fresh, 2_000, RENT,
        )
        .unwrap();

        let ata_program = associated_token_program();
        let ata_ixs: Vec<&Instruction> = plan
            .setup_instructions
            .iter()
            .filter(|ix| ix.program_id == ata_program)
            .collect();
        assert_eq!(ata_ixs.len(), 2, "expected two ATA create ixs for two missing ATAs");
        for ix in ata_ixs {
            assert_eq!(
                ix.data,
                vec![1],
                "ATA create must use idempotent discriminator byte 1; got {:?}",
                ix.data
            );
        }
    }

    // ── (M) Post-approval mutation trap property: the plan assembler is
    //       never called on a HardBlock — the resume task test covers that
    //       in integrations::solend_park. Here we lock the plan assembler's
    //       "do not touch parked snapshot" contract via B: fresh flags
    //       always override parked flags. No separate test duplicated.

    // ── Error surface ─────────────────────────────────────────────────────

    #[test]
    fn repay_action_rejected_as_unsupported_kind() {
        let wallet = Pubkey::new_unique();
        let mut parked = parked_intent(wallet, 1_000);
        parked.action = ProposedAction::Repay {
            protocol: ProtocolTag::Solend,
            reserve_mint: usdc_mint(),
            amount: UnderlyingAmount::new(1_000),
        };
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let res = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(), &parked, &fresh, 2_000, RENT,
        );
        assert_eq!(res, Err(SolendTxPlanError::UnsupportedActionKind));
    }

    #[test]
    fn mismatched_reserve_mint_rejected() {
        let wallet = Pubkey::new_unique();
        let other_mint = Pubkey::new_unique();
        let mut parked = parked_intent(wallet, 1_000);
        parked.action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: other_mint,
            amount: UnderlyingAmount::new(1_000),
        };
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let res = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(), &parked, &fresh, 2_000, RENT,
        );
        assert_eq!(
            res,
            Err(SolendTxPlanError::ReserveNotInSnapshot {
                action_mint: other_mint,
            })
        );
    }

    #[test]
    fn zero_amount_rejected_by_deposit_builder() {
        let wallet = Pubkey::new_unique();
        let mut parked = parked_intent(wallet, 0);
        parked.action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: usdc_mint(),
            amount: UnderlyingAmount::new(0),
        };
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let res = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(), &parked, &fresh, 2_000, RENT,
        );
        match res {
            Err(SolendTxPlanError::DepositBuildFailed(_)) => {}
            other => panic!("expected DepositBuildFailed, got {other:?}"),
        }
    }

    // ── (P4C-8G-Fix) Priority fee placement + value + uniqueness ────────────
    //
    // Locks the Phase 4C-8G-Fix invariants the live mainnet attempt
    // needs:
    //   1. The compute-budget price ix is the FIRST instruction in
    //      `setup_instructions`, regardless of which conditional setup
    //      ixs follow.
    //   2. It encodes `set_compute_unit_price(50_000)`.
    //   3. It appears EXACTLY ONCE across all three plan groups
    //      (setup ∪ refresh ∪ deposit).
    //   4. In the concatenated single-tx instruction stream that
    //      4C-3 preflight, 4C-4 signing, and 4C-5 submit-verification
    //      all consume, the priority-fee ix is at index 0.
    //   5. Existing instruction grouping (refresh precedes deposit;
    //      deposit count == 1) is preserved.

    fn compute_budget_program_id() -> Pubkey {
        // `solana_sdk::compute_budget::id()` returns the canonical
        // ComputeBudget program id; we derive it via the helper to
        // match what `ComputeBudgetInstruction::*` produces.
        solana_sdk::compute_budget::id()
    }

    /// Decodes the SetComputeUnitPrice variant payload from a
    /// ComputeBudget instruction's `data`. The discriminator for that
    /// variant is `3`, followed by an LE u64 micro-lamports value.
    fn decode_set_compute_unit_price_micro_lamports(ix: &Instruction) -> Option<u64> {
        if ix.program_id != compute_budget_program_id() {
            return None;
        }
        if ix.data.first().copied() != Some(3) {
            return None;
        }
        if ix.data.len() < 1 + 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&ix.data[1..9]);
        Some(u64::from_le_bytes(buf))
    }

    fn count_compute_budget_price_ixs(group: &[Instruction]) -> usize {
        group
            .iter()
            .filter(|ix| decode_set_compute_unit_price_micro_lamports(ix).is_some())
            .count()
    }

    #[test]
    fn priority_fee_constant_value_locked_at_50_000_micro_lamports() {
        assert_eq!(SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS, 50_000);
    }

    #[test]
    fn priority_fee_is_first_instruction_in_setup_for_first_deposit() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        // First-deposit: missing source ATA, missing obligation,
        // missing collateral ATA — all conditional setup ixs follow.
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let first = plan
            .setup_instructions
            .first()
            .expect("setup_instructions must include at least the priority-fee ix");
        assert_eq!(
            first.program_id,
            compute_budget_program_id(),
            "setup_instructions[0] must be the ComputeBudget program"
        );
        let value = decode_set_compute_unit_price_micro_lamports(first)
            .expect("setup_instructions[0] must decode as set_compute_unit_price");
        assert_eq!(value, SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS);
        assert_eq!(value, 50_000);
    }

    #[test]
    fn priority_fee_is_first_instruction_in_setup_for_existing_obligation() {
        // Existing obligation, both ATAs exist — only the priority fee
        // remains in setup. This proves the fee is unconditionally
        // first, even when no other setup ix would have run.
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, true, true, true, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        assert_eq!(
            plan.setup_instructions.len(),
            1,
            "setup_instructions must contain ONLY the priority fee when nothing else is required"
        );
        let only = &plan.setup_instructions[0];
        assert_eq!(only.program_id, compute_budget_program_id());
        assert_eq!(
            decode_set_compute_unit_price_micro_lamports(only),
            Some(50_000)
        );
    }

    #[test]
    fn priority_fee_appears_exactly_once_across_all_plan_groups() {
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let setup_count = count_compute_budget_price_ixs(&plan.setup_instructions);
        let refresh_count = count_compute_budget_price_ixs(&plan.refresh_instructions);
        let deposit_count = count_compute_budget_price_ixs(&plan.deposit_instructions);
        assert_eq!(setup_count, 1, "exactly one fee ix in setup");
        assert_eq!(refresh_count, 0, "no fee ix in refresh");
        assert_eq!(deposit_count, 0, "no fee ix in deposit");
        assert_eq!(setup_count + refresh_count + deposit_count, 1);
    }

    #[test]
    fn priority_fee_is_first_in_concatenated_single_tx_order() {
        // The downstream consumers (4C-3 preflight, 4C-4 signing,
        // 4C-5 submit verification) concatenate setup → refresh →
        // deposit into a single message. This test asserts the
        // resulting position-0 instruction is the priority fee — the
        // exact invariant `Message::new(&all_ixs, ..)` will produce
        // for transaction-level fee priority.
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        let mut all = Vec::new();
        all.extend(plan.setup_instructions.iter().cloned());
        all.extend(plan.refresh_instructions.iter().cloned());
        all.extend(plan.deposit_instructions.iter().cloned());

        assert!(
            !all.is_empty(),
            "plan must produce at least one concatenated ix"
        );
        let first = &all[0];
        assert_eq!(first.program_id, compute_budget_program_id());
        assert_eq!(
            decode_set_compute_unit_price_micro_lamports(first),
            Some(SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS)
        );
    }

    #[test]
    fn priority_fee_does_not_disturb_refresh_then_deposit_grouping() {
        // Regression guard: the priority fee insertion must not change
        // the existing 4C-2 invariant that refresh precedes deposit
        // and deposit is a single instruction.
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();

        // Refresh group should have at least one Solend RefreshReserve;
        // deposit group should be exactly one Solend deposit ix.
        assert!(
            !plan.refresh_instructions.is_empty(),
            "refresh group must be non-empty"
        );
        assert_eq!(
            plan.deposit_instructions.len(),
            1,
            "deposit group must remain a single instruction"
        );
        // Refresh ixs are Solend-program-id (not ComputeBudget).
        for ix in &plan.refresh_instructions {
            assert_eq!(
                ix.program_id,
                solend_program(),
                "refresh ixs must be Solend program"
            );
        }
        // Deposit ix is Solend-program-id (not ComputeBudget).
        assert_eq!(plan.deposit_instructions[0].program_id, solend_program());
    }

    #[test]
    fn priority_fee_is_set_unit_price_only_not_set_unit_limit() {
        // 4C-8G-Fix is intentionally price-only; locking that unit-LIMIT
        // is NOT inserted (would require its own slice + simulation
        // analysis). Discriminator `2` is `set_compute_unit_limit`;
        // discriminator `3` is `set_compute_unit_price`.
        let wallet = Pubkey::new_unique();
        let parked = parked_intent(wallet, 1_000);
        let fresh = fresh_assembled(wallet, 2_000, false, false, false, None);
        let plan = assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &fresh,
            2_000,
            RENT,
        )
        .unwrap();
        let mut all = Vec::new();
        all.extend(plan.setup_instructions.iter().cloned());
        all.extend(plan.refresh_instructions.iter().cloned());
        all.extend(plan.deposit_instructions.iter().cloned());
        let cb_ixs: Vec<&Instruction> = all
            .iter()
            .filter(|ix| ix.program_id == compute_budget_program_id())
            .collect();
        assert_eq!(cb_ixs.len(), 1, "exactly one ComputeBudget ix");
        let disc = cb_ixs[0].data.first().copied();
        assert_eq!(
            disc,
            Some(3),
            "ComputeBudget ix must be SetComputeUnitPrice (discriminator 3), not SetComputeUnitLimit (discriminator 2). got {disc:?}"
        );
    }
}

//! V1 lending policy surface and evaluator skeleton.
//!
//! Spec anchors: `docs/lending_policy_vocabulary.md`
//!   Part 3A §18 (V1 mandatory rules),
//!   Part 3B §22 – §29 (verdict contract, three-layer gate, fail-fast),
//!   Part 5B §46 – §53 (V1 Rust type shape + control flow).
//!
//! Slice 2A scope: policy surface types + three-layer gate sequencing +
//! fail-fast dispatcher. Four of the five V1 rules are implemented against
//! snapshot fields that Slice 1 already populates. One rule
//! (`MaxOracleStalenessMs`) is a conservative stub pending Slice 2B
//! provider-specific oracle binary decoders (see §65).
//!
//! # Invariants
//! - `ProposedAction` admits `Deposit`, `Repay`, and `Withdraw` (Phase 5H
//!   scope unlock; see the variant doc for the narrowed risk-reducing
//!   posture). `Borrow` remains structurally unrepresentable (§49.2).
//! - `EvaluationContext` carries only a `session_wallet`. No RPC client,
//!   clock, cache, DB, signer, approval_store, park_store, or daemon handle.
//!   Adding a field is a spec change, not an implementation refinement
//!   (§52.2).
//! - The evaluator is a pure function. No I/O. No async. No ambient state.
//!   No chain-state read after entry (§43.2).
//! - The three gates run in fixed order and short-circuit on the first
//!   failure (§27.1, §52.3).
//! - No `LendingRule` trait. Five rules are direct function calls
//!   (§50.2).
//! - `HardBlockReason` preserves Part 3B's three-category taxonomy
//!   (`SystemInvariantFailed` / `ScopeBoundary` / `RuleRejected`) even
//!   though `ScopeBoundary` is not constructible in V1 because all its
//!   possible triggers are eliminated at the type level.

use solana_sdk::pubkey::Pubkey;

use super::gate::{check_system_invariants, SystemInvariantError};
use super::snapshot::{FeedPublishFreshness, LendingSnapshot, ProtocolTag, StaleMarker};
use super::types::{CollateralTokenAmount, DurationMs, UnderlyingAmount};

// ── ProposedAction ─────────────────────────────────────────────────────────

/// V1 lending action.
///
/// `Borrow` is intentionally absent — that variant is Part 1 §3.2
/// scope-boundary blocked and enforced at the type level.
///
/// `Withdraw` is REPRESENTABLE as of Phase 5H (a deliberate scope
/// decision, not a schema correction). It exists as a protocol-level
/// action so the daemon can model "withdraw collateral" internally for
/// future tooling. It is a *risk-increasing* action in general; the
/// V1 narrowed-scope unlock is enforced in policy:
///
/// - `Withdraw` HardBlocks if the obligation has any non-zero borrow
///   (`RuleRejectionDetail::WithdrawWithDebt`).
/// - `Withdraw` HardBlocks if the obligation has no deposit on the
///   target reserve (`RuleRejectionDetail::WithdrawWithoutDeposit`).
///
/// Critically, the `Withdraw` variant carries `collateral_amount:
/// CollateralTokenAmount` (cToken base units), NOT
/// `UnderlyingAmount`. This unit choice matches the Solend
/// `WithdrawObligationCollateralAndRedeemReserveCollateral`
/// instruction's payload semantics and makes any
/// underlying-vs-collateral confusion a compile-time error.
///
/// The action does NOT carry a signer / wallet pubkey. Signer identity
/// comes from the `SessionBoundWallet` seam via [`EvaluationContext`].
/// Allowing the action to carry one would let an upstream caller override
/// the session binding — the exact hole the A2-Execute proof closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedAction {
    Deposit {
        protocol: ProtocolTag,
        reserve_mint: Pubkey,
        amount: UnderlyingAmount,
    },
    Repay {
        protocol: ProtocolTag,
        reserve_mint: Pubkey,
        amount: UnderlyingAmount,
    },
    /// Phase 5H — Solend USDC `withdraw_all` substrate.
    ///
    /// `collateral_amount` is in cToken base units (e.g. cUSDC for the
    /// Solend USDC reserve). The future LLM-facing UX is `withdraw_all:
    /// true`, which constructs this variant with
    /// `CollateralTokenAmount::new(u64::MAX)`; Solend's program clamps
    /// to the user's actual deposited collateral. The LLM never sees
    /// or sets a numeric amount.
    Withdraw {
        protocol: ProtocolTag,
        reserve_mint: Pubkey,
        collateral_amount: CollateralTokenAmount,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Deposit,
    Repay,
    Withdraw,
}

impl ProposedAction {
    pub fn protocol(&self) -> ProtocolTag {
        match self {
            ProposedAction::Deposit { protocol, .. }
            | ProposedAction::Repay { protocol, .. }
            | ProposedAction::Withdraw { protocol, .. } => *protocol,
        }
    }

    pub fn reserve_mint(&self) -> Pubkey {
        match self {
            ProposedAction::Deposit { reserve_mint, .. }
            | ProposedAction::Repay { reserve_mint, .. }
            | ProposedAction::Withdraw { reserve_mint, .. } => *reserve_mint,
        }
    }

    /// Underlying-asset amount for actions whose unit is the underlying
    /// (Deposit / Repay). Returns `None` for Withdraw, whose unit is
    /// cToken collateral; use [`Self::collateral_amount`] for that case.
    pub fn amount_underlying(&self) -> Option<UnderlyingAmount> {
        match self {
            ProposedAction::Deposit { amount, .. }
            | ProposedAction::Repay { amount, .. } => Some(*amount),
            ProposedAction::Withdraw { .. } => None,
        }
    }

    /// cToken-collateral amount for Withdraw. Returns `None` for
    /// Deposit / Repay, whose unit is the underlying asset; use
    /// [`Self::amount_underlying`] for those cases.
    pub fn collateral_amount(&self) -> Option<CollateralTokenAmount> {
        match self {
            ProposedAction::Withdraw {
                collateral_amount, ..
            } => Some(*collateral_amount),
            ProposedAction::Deposit { .. } | ProposedAction::Repay { .. } => None,
        }
    }

    /// Raw `u64` amount for audit / display payloads only. The unit
    /// depends on the action variant — Deposit/Repay raw is in the
    /// underlying base units; Withdraw raw is in cToken base units.
    /// Callers that care about unit safety MUST use
    /// [`Self::amount_underlying`] or [`Self::collateral_amount`]
    /// instead. Audit JSON should annotate the unit alongside the raw
    /// number.
    pub fn amount_raw(&self) -> u64 {
        match self {
            ProposedAction::Deposit { amount, .. }
            | ProposedAction::Repay { amount, .. } => amount.raw(),
            ProposedAction::Withdraw {
                collateral_amount, ..
            } => collateral_amount.raw(),
        }
    }

    pub fn kind(&self) -> ActionKind {
        match self {
            ProposedAction::Deposit { .. } => ActionKind::Deposit,
            ProposedAction::Repay { .. } => ActionKind::Repay,
            ProposedAction::Withdraw { .. } => ActionKind::Withdraw,
        }
    }
}

// ── EvaluationContext ──────────────────────────────────────────────────────

/// Non-ambient evaluator input per Part 5B §52.2.
///
/// This struct has exactly one field and intentionally no others. The
/// evaluator never has access to RPC, clocks, caches, databases, signer
/// state, approval_store, park_store, or daemon-runtime handles. Any need
/// for additional context must be satisfied by enlarging `LendingSnapshot`
/// through a separate spec prompt — never by adding a field here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationContext {
    pub session_wallet: Pubkey,
}

// ── LendingRuleConfig ──────────────────────────────────────────────────────

/// Closed config for the five V1 mandatory rules (Part 3A §18).
///
/// Deliberately not `Default`-deriving: each sub-config is explicit so an
/// accidentally-empty config cannot quietly pass a wide action surface.
/// Callers construct configs by field name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LendingRuleConfig {
    pub require_fresh_state: RequireFreshStateConfig,
    pub max_oracle_staleness: MaxOracleStalenessConfig,
    pub allowed_lending_protocols: AllowedLendingProtocolsConfig,
    pub allowed_mints: AllowedMintsConfig,
    pub max_action_input_amount: MaxActionInputAmountConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequireFreshStateConfig {
    /// Maximum allowed Claw-fetch age for any snapshot component relative
    /// to the snapshot-level observed slot (§51.4 slot-to-ms conversion).
    /// Unit-tagged: see [`DurationMs`].
    pub max_fetch_age: DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaxOracleStalenessConfig {
    /// Maximum allowed oracle publish-time age. In Slice 2A this field is
    /// not yet consumed by the rule (oracle binary decoders land in
    /// Slice 2B / Part 6B §65), but the unit tag is locked here so the
    /// future enforcement path cannot silently accept a bare integer.
    pub max_publish_age: DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedLendingProtocolsConfig {
    /// Closed allowlist of `ProtocolTag`s. Empty list = reject every
    /// action, which is the correct conservative default.
    pub allowlist: Vec<ProtocolTag>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedMintsConfig {
    /// Closed allowlist of mint pubkeys permitted as the action's target.
    /// Empty list = reject every action.
    pub allowlist: Vec<Pubkey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaxActionInputAmountConfig {
    /// Per-mint caps for actions whose unit is the underlying asset
    /// (Deposit / Repay). An action whose mint has no entry here is
    /// rejected by [`RuleRejectionDetail::NoCapConfigured`] — the
    /// conservative posture: a missing cap is not an implicit "no limit."
    pub per_mint_caps: Vec<(Pubkey, UnderlyingAmount)>,
    /// Phase 5H — per-mint caps for actions whose unit is collateral
    /// cTokens (Withdraw). Disjoint from `per_mint_caps`: callers MUST
    /// explicitly populate this list for a Withdraw on a given mint to
    /// pass [`max_action_input_amount`]. Empty list → every Withdraw is
    /// rejected by `NoCapConfigured`. Sentinel passthrough
    /// (`CollateralTokenAmount::new(u64::MAX)` for "withdraw all") is
    /// expected and the policy cap MUST be set to `u64::MAX` for the
    /// withdraw_all surface; otherwise a literal `u64::MAX` action will
    /// HardBlock here. The position-size cap on Withdraw is a separate
    /// concern enforced via the obligation's deposited amount during
    /// the `allowed_mints` cross-reference (a future tightening can move
    /// it here too).
    pub per_mint_collateral_caps: Vec<(Pubkey, CollateralTokenAmount)>,
}

// ── Verdict / HardBlock reason taxonomy ───────────────────────────────────

/// Part 3B §22 two-variant verdict contract. No soft-fail, no warn,
/// no retry signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LendingPolicyVerdict {
    Pass,
    HardBlock(HardBlockReason),
}

/// Part 3B §28.1 three reason categories.
///
/// Note: `ScopeBoundary` is structurally unreachable in V1 because every
/// ScopeBoundary trigger (unsupported action kind, out-of-scope protocol
/// at action level) is eliminated at the `ProposedAction` type level
/// (§49.2). The variant remains so the public taxonomy continues to match
/// Part 3B and future V2 work (raw / untyped action parsers) can produce
/// it without reshuffling the enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardBlockReason {
    SystemInvariantFailed(SystemInvariantSubreason),
    ScopeBoundary(ScopeBoundarySubreason),
    RuleRejected(RuleKind, RuleRejectionDetail),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemInvariantSubreason {
    OwnerMismatch,
    ProtocolTagMismatch,
}

impl From<SystemInvariantError> for SystemInvariantSubreason {
    fn from(e: SystemInvariantError) -> Self {
        match e {
            SystemInvariantError::OwnerMismatch { .. } => Self::OwnerMismatch,
            SystemInvariantError::ProtocolTagMismatch { .. } => Self::ProtocolTagMismatch,
        }
    }
}

/// Empty in V1 — the enum has no constructible variants. This is
/// deliberate: see the note on [`HardBlockReason::ScopeBoundary`]. Tests
/// lock the type-level non-representability of `Withdraw` / `Borrow` in
/// [`ProposedAction`] instead of attempting to construct a runtime
/// ScopeBoundary outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeBoundarySubreason {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    RequireFreshState,
    MaxOracleStalenessMs,
    AllowedLendingProtocols,
    AllowedMints,
    MaxActionInputAmount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleRejectionDetail {
    /// Protocol-native `StaleMarker::Stale` on the obligation or any reserve.
    ProtocolNativeStale,
    /// Claw-fetch freshness exceeded `max_fetch_age` (§51.4 slot-based).
    FetchAgeExceeded,
    /// Oracle feed-set empty or missing for the priced asset the action touches.
    /// Distinct from `OraclePublishFreshnessUnknown`: here the feed set itself
    /// is empty (no configured feed, or all feeds were sentinel / unreachable).
    OracleFeedSetEmpty,
    /// Oracle feed present but its publish freshness could not be safely
    /// decoded. Slice 2B treats this as fail-closed per Part 6B §65 —
    /// provider-specific decoders land in a dedicated verification slice.
    OraclePublishFreshnessUnknown,
    /// Oracle feed's publish timestamp is older than `max_publish_age`.
    OraclePublishAgeExceeded,
    /// Action's protocol tag not in the configured allowlist.
    ProtocolNotAllowed,
    /// Action's reserve mint not in the configured allowlist.
    MintNotAllowed,
    /// Action targets a reserve not present in the snapshot.
    ReserveNotInSnapshot,
    /// Repay targets a mint the obligation holds no debt in.
    RepayWithoutDebt,
    /// Phase 5H — Withdraw targets a reserve the obligation holds no
    /// (non-zero) deposited collateral in. There is nothing to redeem.
    /// Surfaced under `RuleKind::AllowedMints` cross-reference, parallel
    /// to `RepayWithoutDebt`.
    WithdrawWithoutDeposit,
    /// Phase 5H — Withdraw is risk-increasing in general; V1's narrowed
    /// scope unlock only allows `Withdraw` on obligations with zero
    /// outstanding debt. If any non-zero borrow exists, the rule
    /// HardBlocks. Surfaced under `RuleKind::AllowedMints`
    /// cross-reference.
    WithdrawWithDebt,
    /// No cap entry configured for the action's mint.
    NoCapConfigured,
    /// Action amount exceeds the configured per-mint cap.
    AmountOverCap,
}

// ── Evaluator entry ────────────────────────────────────────────────────────

/// §51.4 conservative slot-duration constant. Nominal Solana slot time is
/// ~400 ms; we use the nominal value for V1 and leave the open question of
/// empirical tightening to a future spec (§39.5).
const CONSERVATIVE_SLOT_MS: u64 = 400;

/// V1 lending policy evaluator. Pure function over its four inputs.
///
/// Ordering (fixed):
/// 1. System Invariant Gate — owner + protocol-tag consistency (§24).
/// 2. Scope Boundary Gate   — tautological on typed `ProposedAction`
///                            (§25); preserved as a named stage.
/// 3. Rule Evaluation       — five rules, fail-fast (§27.1):
///      a. RequireFreshState
///      b. MaxOracleStalenessMs
///      c. AllowedLendingProtocols
///      d. AllowedMints
///      e. MaxActionInputAmount
///
/// The first failing stage determines the verdict. No rule runs after a
/// failure in any upstream stage.
pub fn evaluate_lending_policy(
    snapshot: &LendingSnapshot,
    action: &ProposedAction,
    config: &LendingRuleConfig,
    context: &EvaluationContext,
) -> LendingPolicyVerdict {
    // Stage 1: System Invariant Gate.
    if let Err(e) = check_system_invariants(
        snapshot,
        &context.session_wallet,
        action.protocol(),
    ) {
        return LendingPolicyVerdict::HardBlock(
            HardBlockReason::SystemInvariantFailed(e.into()),
        );
    }

    // Stage 2: Scope Boundary Gate.
    //
    // `ProposedAction` is restricted to `Deposit | Repay` at the type level
    // (§49.2). Every runtime value that reaches this line is therefore in
    // scope; no fallible check exists in V1. The stage is kept as a named
    // sequencing step so a future V2 raw/untyped-action parser can hook in
    // here without reordering the gate structure.

    // Stage 3: Rule Evaluation, fail-fast.
    if let Err(detail) = require_fresh_state(snapshot, &config.require_fresh_state) {
        return LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
            RuleKind::RequireFreshState,
            detail,
        ));
    }
    if let Err(detail) =
        max_oracle_staleness(snapshot, action, &config.max_oracle_staleness)
    {
        return LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
            RuleKind::MaxOracleStalenessMs,
            detail,
        ));
    }
    if let Err(detail) =
        allowed_lending_protocols(action, &config.allowed_lending_protocols)
    {
        return LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
            RuleKind::AllowedLendingProtocols,
            detail,
        ));
    }
    if let Err(detail) = allowed_mints(snapshot, action, &config.allowed_mints) {
        return LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
            RuleKind::AllowedMints,
            detail,
        ));
    }
    if let Err(detail) =
        max_action_input_amount(action, &config.max_action_input_amount)
    {
        return LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
            RuleKind::MaxActionInputAmount,
            detail,
        ));
    }

    LendingPolicyVerdict::Pass
}

// ── Rules ──────────────────────────────────────────────────────────────────

fn require_fresh_state(
    snapshot: &LendingSnapshot,
    config: &RequireFreshStateConfig,
) -> Result<(), RuleRejectionDetail> {
    // (a) Protocol-native stale marker on obligation or any reserve — §18.1.
    if snapshot.obligation.protocol_native_stale == StaleMarker::Stale {
        return Err(RuleRejectionDetail::ProtocolNativeStale);
    }
    for r in &snapshot.reserves {
        if r.protocol_native_stale == StaleMarker::Stale {
            return Err(RuleRejectionDetail::ProtocolNativeStale);
        }
    }

    // (b) Claw-fetch age (§51.4, slot-based). Conservative slot duration.
    let snapshot_slot = snapshot.fetched_at.observed_slot;
    let max_slots = config
        .max_fetch_age
        .raw()
        .saturating_div(CONSERVATIVE_SLOT_MS);

    let obl_age = snapshot_slot.slots_since(snapshot.obligation.freshness.observed_slot);
    if obl_age > max_slots {
        return Err(RuleRejectionDetail::FetchAgeExceeded);
    }
    for r in &snapshot.reserves {
        let age = snapshot_slot.slots_since(r.freshness.observed_slot);
        if age > max_slots {
            return Err(RuleRejectionDetail::FetchAgeExceeded);
        }
    }
    Ok(())
}

fn max_oracle_staleness(
    snapshot: &LendingSnapshot,
    action: &ProposedAction,
    config: &MaxOracleStalenessConfig,
) -> Result<(), RuleRejectionDetail> {
    // Part 3A §18.2: bound the oracle's *publish-time* staleness. Scope
    // is the priced asset the action touches. Separation from
    // `RequireFreshState`: this rule never reads obligation / reserve
    // protocol-native stale markers or Claw-fetch ages; those belong to
    // §18.1 exclusively.
    //
    // Feed-set combination semantic (§11.1 / §34.2): V1 uses **conjunction**
    // — every configured feed for the priced asset must be fresh. Any
    // unknown / stale feed HardBlocks. This is the fail-closed choice;
    // §39.2 records the open question of whether this decision stays
    // here or moves to a per-protocol spec.
    let action_mint = action.reserve_mint();
    let set = snapshot
        .oracles
        .iter()
        .find(|s| s.priced_asset == action_mint)
        .ok_or(RuleRejectionDetail::OracleFeedSetEmpty)?;
    if set.feeds.is_empty() {
        return Err(RuleRejectionDetail::OracleFeedSetEmpty);
    }

    // §51.4: publish age is derived in slots from the snapshot-level clock.
    let snapshot_slot = snapshot.fetched_at.observed_slot;
    let max_slots = config
        .max_publish_age
        .raw()
        .saturating_div(CONSERVATIVE_SLOT_MS);

    // Worst-feed dominates: iterate every feed, fail-fast on first offender.
    for feed in &set.feeds {
        let publish_slot = match feed.publish {
            FeedPublishFreshness::KnownSlot(s) => s,
            FeedPublishFreshness::Unknown => {
                return Err(RuleRejectionDetail::OraclePublishFreshnessUnknown);
            }
        };
        let age = snapshot_slot.slots_since(publish_slot);
        if age > max_slots {
            return Err(RuleRejectionDetail::OraclePublishAgeExceeded);
        }
    }
    Ok(())
}

fn allowed_lending_protocols(
    action: &ProposedAction,
    config: &AllowedLendingProtocolsConfig,
) -> Result<(), RuleRejectionDetail> {
    if config.allowlist.contains(&action.protocol()) {
        Ok(())
    } else {
        Err(RuleRejectionDetail::ProtocolNotAllowed)
    }
}

fn allowed_mints(
    snapshot: &LendingSnapshot,
    action: &ProposedAction,
    config: &AllowedMintsConfig,
) -> Result<(), RuleRejectionDetail> {
    let mint = action.reserve_mint();
    if !config.allowlist.contains(&mint) {
        return Err(RuleRejectionDetail::MintNotAllowed);
    }
    // Cross-reference: the snapshot MUST contain a reserve for this mint —
    // otherwise the action has no target reserve to apply against.
    let reserve = snapshot
        .reserves
        .iter()
        .find(|r| r.mint == mint)
        .ok_or(RuleRejectionDetail::ReserveNotInSnapshot)?;
    match action.kind() {
        ActionKind::Deposit => {}
        // Repay-specific cross-reference: the obligation must carry a
        // borrow on this reserve. Part 3A §18.4's "a Repay against a
        // mint the obligation has no debt in is rejected by this rule's
        // cross-reference."
        ActionKind::Repay => {
            let has_debt = snapshot
                .obligation
                .borrows
                .iter()
                .any(|b| b.reserve == reserve.identifier);
            if !has_debt {
                return Err(RuleRejectionDetail::RepayWithoutDebt);
            }
        }
        // Phase 5H — Withdraw cross-references:
        //   1. The obligation MUST hold a non-zero deposit on this
        //      reserve (otherwise there is nothing to redeem).
        //   2. The obligation MUST have ZERO outstanding debt across
        //      ALL borrow positions. Withdraw is risk-increasing in the
        //      general case; V1's narrowed scope unlock only permits it
        //      against obligations with no debt at all, so this rule is
        //      a hard precondition rather than a per-mint check.
        ActionKind::Withdraw => {
            let has_deposit = snapshot
                .obligation
                .deposits
                .iter()
                .any(|d| d.reserve == reserve.identifier && d.deposited.raw() > 0);
            if !has_deposit {
                return Err(RuleRejectionDetail::WithdrawWithoutDeposit);
            }
            let has_any_borrow = snapshot
                .obligation
                .borrows
                .iter()
                .any(|b| b.borrowed_wads.raw() > 0);
            if has_any_borrow {
                return Err(RuleRejectionDetail::WithdrawWithDebt);
            }
        }
    }
    Ok(())
}

fn max_action_input_amount(
    action: &ProposedAction,
    config: &MaxActionInputAmountConfig,
) -> Result<(), RuleRejectionDetail> {
    let mint = action.reserve_mint();
    // Dispatch on the action's amount-unit kind. Deposit / Repay carry
    // an `UnderlyingAmount` and check against `per_mint_caps`. Withdraw
    // carries a `CollateralTokenAmount` and checks against the disjoint
    // `per_mint_collateral_caps` list. Cross-list lookups are
    // structurally impossible — a missing cap in the appropriate list
    // surfaces `NoCapConfigured`, never silently "no limit."
    match action {
        ProposedAction::Deposit { amount, .. }
        | ProposedAction::Repay { amount, .. } => {
            match config.per_mint_caps.iter().find(|(m, _)| *m == mint) {
                None => Err(RuleRejectionDetail::NoCapConfigured),
                Some((_, cap)) => {
                    if *amount > *cap {
                        Err(RuleRejectionDetail::AmountOverCap)
                    } else {
                        Ok(())
                    }
                }
            }
        }
        ProposedAction::Withdraw {
            collateral_amount, ..
        } => {
            match config
                .per_mint_collateral_caps
                .iter()
                .find(|(m, _)| *m == mint)
            {
                None => Err(RuleRejectionDetail::NoCapConfigured),
                Some((_, cap)) => {
                    if *collateral_amount > *cap {
                        Err(RuleRejectionDetail::AmountOverCap)
                    } else {
                        Ok(())
                    }
                }
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::mapping::{
        map_snapshot, OracleAccountInfo, ReserveInput, SolendAssemblyInputs,
    };
    use crate::integrations::solend::raw::{
        self, synth_obligation, synth_reserve, SOLEND_NULL_ORACLE_SENTINEL_BS58,
    };
    use crate::lending::{ChainSlot, FeedPublishFreshness, OracleProvider};
    use std::str::FromStr;

    // ── Fixture helpers ───────────────────────────────────────────────────

    /// A fully-usable V1 snapshot with one reserve, one oracle feed whose
    /// publish freshness is `KnownSlot` at the same slot as the snapshot
    /// (so `MaxOracleStalenessMs` passes for any non-zero threshold),
    /// fresh markers, and observed slots that pass `max_fetch_age`.
    fn fresh_snapshot(
        owner: Pubkey,
        reserve_mint: Pubkey,
    ) -> (LendingSnapshot, Pubkey) {
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();

        let obl = synth_obligation(owner, mkt, 100, /*stale=*/ false, &[], &[]);
        let res = synth_reserve(
            mkt,
            reserve_mint,
            6,
            supply,
            pyth,
            sentinel, // switchboard slot sentinel
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            /*stale=*/ false,
        );

        let inputs = SolendAssemblyInputs {
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
        };
        (map_snapshot(inputs).unwrap(), reserve_mint)
    }

    fn permissive_config(
        allowed_mint: Pubkey,
        cap: UnderlyingAmount,
    ) -> LendingRuleConfig {
        LendingRuleConfig {
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
                allowlist: vec![allowed_mint],
            },
            max_action_input_amount: MaxActionInputAmountConfig {
                per_mint_caps: vec![(allowed_mint, cap)],
                per_mint_collateral_caps: vec![],
            },
        }
    }

    // ── API shape tests (lock the seam at compile + runtime) ──────────────

    /// Pattern-matching exhaustion: if a V1 variant is ever added (e.g.
    /// `Borrow` sneaking back), this test fails to compile. Phase 5H
    /// added `Withdraw` as a deliberate scope unlock; that variant is
    /// listed here.
    #[test]
    fn proposed_action_is_deposit_repay_or_withdraw_only() {
        fn _exhaustive(a: &ProposedAction) {
            match a {
                ProposedAction::Deposit { .. } => {}
                ProposedAction::Repay { .. } => {}
                ProposedAction::Withdraw { .. } => {}
            }
        }
        let d = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: Pubkey::new_unique(),
            amount: UnderlyingAmount::new(1),
        };
        let r = ProposedAction::Repay {
            protocol: ProtocolTag::Solend,
            reserve_mint: Pubkey::new_unique(),
            amount: UnderlyingAmount::new(1),
        };
        let w = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: Pubkey::new_unique(),
            collateral_amount: CollateralTokenAmount::new(1),
        };
        _exhaustive(&d);
        _exhaustive(&r);
        _exhaustive(&w);
    }

    /// Destructuring without `..` — if a field is ever added to the action's
    /// Deposit variant, this test fails to compile. Lock every named field.
    #[test]
    fn deposit_variant_has_no_signer_field() {
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: Pubkey::new_unique(),
            amount: UnderlyingAmount::new(10),
        };
        let ProposedAction::Deposit {
            protocol: _,
            reserve_mint: _,
            amount: _,
        } = action
        else {
            panic!("expected Deposit");
        };
    }

    /// Same lock for Repay. Signer/wallet must not appear.
    #[test]
    fn repay_variant_has_no_signer_field() {
        let action = ProposedAction::Repay {
            protocol: ProtocolTag::Solend,
            reserve_mint: Pubkey::new_unique(),
            amount: UnderlyingAmount::new(10),
        };
        let ProposedAction::Repay {
            protocol: _,
            reserve_mint: _,
            amount: _,
        } = action
        else {
            panic!("expected Repay");
        };
    }

    /// Phase 5H — same lock for Withdraw. Signer/wallet must not appear.
    /// If a future contributor adds an `owner: Pubkey` or
    /// `wallet_pubkey: Pubkey` field to `ProposedAction::Withdraw`, this
    /// test fails to compile because the destructure pattern lists every
    /// expected field by name. The only valid fields are `protocol`,
    /// `reserve_mint`, and `collateral_amount` (cToken units, NOT
    /// underlying).
    #[test]
    fn withdraw_variant_has_no_signer_field() {
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: Pubkey::new_unique(),
            collateral_amount: CollateralTokenAmount::new(10),
        };
        let ProposedAction::Withdraw {
            protocol: _,
            reserve_mint: _,
            collateral_amount: _,
        } = action
        else {
            panic!("expected Withdraw");
        };
    }

    /// Phase 5H — accessor unit-tagging contract. `amount_underlying()`
    /// returns `Some` only for Deposit / Repay; `collateral_amount()`
    /// returns `Some` only for Withdraw. The two are disjoint by
    /// construction. `amount_raw()` returns the inner u64 for any
    /// variant (unit-dependent — caller is responsible for labeling).
    #[test]
    fn action_amount_accessors_are_unit_disjoint() {
        let mint = Pubkey::new_unique();
        let d = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(11),
        };
        let r = ProposedAction::Repay {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(22),
        };
        let w = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(33),
        };

        assert_eq!(d.amount_underlying(), Some(UnderlyingAmount::new(11)));
        assert_eq!(d.collateral_amount(), None);
        assert_eq!(d.amount_raw(), 11);

        assert_eq!(r.amount_underlying(), Some(UnderlyingAmount::new(22)));
        assert_eq!(r.collateral_amount(), None);
        assert_eq!(r.amount_raw(), 22);

        assert_eq!(w.amount_underlying(), None);
        assert_eq!(w.collateral_amount(), Some(CollateralTokenAmount::new(33)));
        assert_eq!(w.amount_raw(), 33);
    }

    /// Lock EvaluationContext shape. Adding a field breaks compilation.
    #[test]
    fn evaluation_context_has_only_session_wallet() {
        let ctx = EvaluationContext {
            session_wallet: Pubkey::new_unique(),
        };
        let EvaluationContext { session_wallet: _ } = ctx;
    }

    /// Verdict is exactly two-variant (Pass / HardBlock).
    #[test]
    fn verdict_has_only_pass_and_hardblock() {
        fn _exhaustive(v: &LendingPolicyVerdict) {
            match v {
                LendingPolicyVerdict::Pass => {}
                LendingPolicyVerdict::HardBlock(_) => {}
            }
        }
        _exhaustive(&LendingPolicyVerdict::Pass);
    }

    /// Part 3B §28.1 three reason categories remain distinguishable.
    #[test]
    fn hardblock_reason_has_three_categories() {
        fn _exhaustive(r: &HardBlockReason) {
            match r {
                HardBlockReason::SystemInvariantFailed(_) => {}
                HardBlockReason::ScopeBoundary(_) => {}
                HardBlockReason::RuleRejected(_, _) => {}
            }
        }
        _exhaustive(&HardBlockReason::SystemInvariantFailed(
            SystemInvariantSubreason::OwnerMismatch,
        ));
        _exhaustive(&HardBlockReason::RuleRejected(
            RuleKind::AllowedMints,
            RuleRejectionDetail::MintNotAllowed,
        ));
    }

    /// ScopeBoundarySubreason is deliberately non-constructible in V1. This
    /// test documents that fact — the function signature never receives a
    /// value at runtime. Exhaustive matching on `!` / empty enum compiles.
    #[test]
    fn scope_boundary_subreason_is_empty_in_v1() {
        fn _consume(s: ScopeBoundarySubreason) -> ! {
            match s {}
        }
        // No call site exists. That is the point.
        let _ = _consume as fn(ScopeBoundarySubreason) -> !;
    }

    /// Config is a plain struct, constructed by field name, not via a
    /// trait registry. This test just compiles; its presence prevents a
    /// future contributor from reshaping `LendingRuleConfig` around
    /// `Vec<Box<dyn Rule>>`.
    #[test]
    fn lending_rule_config_is_closed_direct_struct() {
        let _ = LendingRuleConfig {
            require_fresh_state: RequireFreshStateConfig {
                max_fetch_age: DurationMs::new(1_000),
            },
            max_oracle_staleness: MaxOracleStalenessConfig {
                max_publish_age: DurationMs::new(1_000),
            },
            allowed_lending_protocols: AllowedLendingProtocolsConfig {
                allowlist: vec![ProtocolTag::Solend],
            },
            allowed_mints: AllowedMintsConfig {
                allowlist: vec![],
            },
            max_action_input_amount: MaxActionInputAmountConfig {
                per_mint_caps: vec![],
                per_mint_collateral_caps: vec![],
            },
        };
    }

    /// Unit-tagged amounts/durations: the config types require newtypes,
    /// not bare integers. If a future refactor swaps the inner type to
    /// `u64`, this test fails to compile.
    #[test]
    fn config_uses_newtypes_for_unit_bearing_fields() {
        let mint = Pubkey::new_unique();
        let cfg = LendingRuleConfig {
            require_fresh_state: RequireFreshStateConfig {
                max_fetch_age: DurationMs::new(1),
            },
            max_oracle_staleness: MaxOracleStalenessConfig {
                max_publish_age: DurationMs::new(1),
            },
            allowed_lending_protocols: AllowedLendingProtocolsConfig {
                allowlist: vec![],
            },
            allowed_mints: AllowedMintsConfig { allowlist: vec![] },
            max_action_input_amount: MaxActionInputAmountConfig {
                per_mint_caps: vec![(mint, UnderlyingAmount::new(42))],
                per_mint_collateral_caps: vec![(mint, CollateralTokenAmount::new(7))],
            },
        };
        let cap: UnderlyingAmount = cfg.max_action_input_amount.per_mint_caps[0].1;
        let coll_cap: CollateralTokenAmount =
            cfg.max_action_input_amount.per_mint_collateral_caps[0].1;
        let age: DurationMs = cfg.require_fresh_state.max_fetch_age;
        assert_eq!(cap.raw(), 42);
        assert_eq!(coll_cap.raw(), 7);
        assert_eq!(age.raw(), 1);
    }

    // ── Gate-sequencing / fail-fast tests ─────────────────────────────────

    #[test]
    fn deposit_happy_path_passes() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        let config = permissive_config(mint, UnderlyingAmount::new(1_000_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass
        );
    }

    #[test]
    fn repay_happy_path_passes_with_existing_debt() {
        // Build a snapshot where the obligation has a borrow on the reserve.
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();

        let borrows = vec![raw::SolendObligationLiquidityRaw {
            borrow_reserve: reserve_pk,
            borrowed_amount_wads: 1_000_000_000_000_000_000u128,
        }];
        let obl = synth_obligation(owner, mkt, 100, /*stale=*/ false, &[], &borrows);
        let res = synth_reserve(
            mkt,
            mint,
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        let inputs = SolendAssemblyInputs {
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
        };
        let snap = map_snapshot(inputs).unwrap();

        let config = permissive_config(mint, UnderlyingAmount::new(1_000_000));
        let action = ProposedAction::Repay {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(50),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass
        );
    }

    #[test]
    fn repay_without_debt_hardblocks_via_allowed_mints_cross_ref() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint); // obligation has no borrows
        let config = permissive_config(mint, UnderlyingAmount::new(1_000_000));
        let action = ProposedAction::Repay {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(10),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::AllowedMints,
                RuleRejectionDetail::RepayWithoutDebt,
            ))
        );
    }

    // ── Phase 5H — Withdraw policy tests ───────────────────────────────

    /// Build a fresh-state snapshot where the obligation has the given
    /// deposit and borrow vectors against a single reserve. Mirrors the
    /// `fresh_snapshot` helper but lets withdraw tests inject deposit /
    /// borrow positions explicitly.
    fn fresh_snapshot_with_positions(
        owner: Pubkey,
        reserve_mint: Pubkey,
        deposits: &[u64],
        borrows_wads: &[u128],
    ) -> (LendingSnapshot, Pubkey, Pubkey) {
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();

        let synth_deposits: Vec<raw::SolendObligationCollateralRaw> = deposits
            .iter()
            .map(|amt| raw::SolendObligationCollateralRaw {
                deposit_reserve: reserve_pk,
                deposited_amount: *amt,
            })
            .collect();
        let synth_borrows: Vec<raw::SolendObligationLiquidityRaw> = borrows_wads
            .iter()
            .map(|wads| raw::SolendObligationLiquidityRaw {
                borrow_reserve: reserve_pk,
                borrowed_amount_wads: *wads,
            })
            .collect();
        let obl = synth_obligation(owner, mkt, 100, false, &synth_deposits, &synth_borrows);
        let res = synth_reserve(
            mkt,
            reserve_mint,
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        let inputs = SolendAssemblyInputs {
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
        };
        let snap = map_snapshot(inputs).unwrap();
        (snap, reserve_pk, reserve_mint)
    }

    /// Build a permissive Withdraw config: same as `permissive_config`
    /// but populates `per_mint_collateral_caps` for the given mint.
    fn withdraw_permissive_config(
        allowed_mint: Pubkey,
        collateral_cap: CollateralTokenAmount,
    ) -> LendingRuleConfig {
        LendingRuleConfig {
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
                allowlist: vec![allowed_mint],
            },
            max_action_input_amount: MaxActionInputAmountConfig {
                per_mint_caps: vec![],
                per_mint_collateral_caps: vec![(allowed_mint, collateral_cap)],
            },
        }
    }

    #[test]
    fn withdraw_happy_path_passes_with_existing_deposit_and_no_debt() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _reserve_pk, _) =
            fresh_snapshot_with_positions(owner, mint, &[5_000], &[]);
        let config = withdraw_permissive_config(mint, CollateralTokenAmount::new(u64::MAX));
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass
        );
    }

    #[test]
    fn withdraw_without_deposit_hardblocks_via_allowed_mints_cross_ref() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        // No deposit on the reserve → cross-ref must fire.
        let (snap, _, _) = fresh_snapshot_with_positions(owner, mint, &[], &[]);
        let config = withdraw_permissive_config(mint, CollateralTokenAmount::new(u64::MAX));
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::AllowedMints,
                RuleRejectionDetail::WithdrawWithoutDeposit,
            ))
        );
    }

    /// A zero-amount deposit entry on the same reserve must also count
    /// as "no deposit" — the cross-ref looks for non-zero collateral.
    #[test]
    fn withdraw_with_zero_amount_deposit_entry_still_hardblocks() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _, _) = fresh_snapshot_with_positions(owner, mint, &[0], &[]);
        let config = withdraw_permissive_config(mint, CollateralTokenAmount::new(u64::MAX));
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::AllowedMints,
                RuleRejectionDetail::WithdrawWithoutDeposit,
            ))
        );
    }

    #[test]
    fn withdraw_with_existing_debt_hardblocks_via_allowed_mints_cross_ref() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        // Has a deposit AND a non-zero borrow → V1 risk-reducing-only
        // narrowed scope must HardBlock the withdraw.
        let (snap, _, _) = fresh_snapshot_with_positions(
            owner,
            mint,
            &[5_000],
            &[1_000_000_000_000_000_000u128],
        );
        let config = withdraw_permissive_config(mint, CollateralTokenAmount::new(u64::MAX));
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::AllowedMints,
                RuleRejectionDetail::WithdrawWithDebt,
            ))
        );
    }

    /// A zero-amount borrow entry on the obligation does NOT block
    /// withdraw — the cross-ref looks for non-zero outstanding debt.
    #[test]
    fn withdraw_with_zero_amount_borrow_entry_still_passes() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _, _) = fresh_snapshot_with_positions(owner, mint, &[5_000], &[0]);
        let config = withdraw_permissive_config(mint, CollateralTokenAmount::new(u64::MAX));
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass
        );
    }

    /// The cap dispatch for Withdraw uses `per_mint_collateral_caps`,
    /// not `per_mint_caps`. A config that only populates the underlying
    /// list must HardBlock Withdraw with `NoCapConfigured`.
    #[test]
    fn withdraw_with_only_underlying_cap_configured_hardblocks_with_no_cap() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _, _) = fresh_snapshot_with_positions(owner, mint, &[5_000], &[]);
        // permissive_config populates per_mint_caps but NOT
        // per_mint_collateral_caps.
        let config = permissive_config(mint, UnderlyingAmount::new(1_000_000));
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxActionInputAmount,
                RuleRejectionDetail::NoCapConfigured,
            ))
        );
    }

    /// A Withdraw whose `collateral_amount` exceeds the configured
    /// `per_mint_collateral_caps` cap HardBlocks with `AmountOverCap`.
    #[test]
    fn withdraw_collateral_amount_over_cap_hardblocks() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _, _) = fresh_snapshot_with_positions(owner, mint, &[5_000], &[]);
        // Cap at 100 cTokens, action requests 1_000.
        let config = withdraw_permissive_config(mint, CollateralTokenAmount::new(100));
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            collateral_amount: CollateralTokenAmount::new(1_000),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxActionInputAmount,
                RuleRejectionDetail::AmountOverCap,
            ))
        );
    }

    /// Deposit / Repay must continue to use `per_mint_caps` — Phase 5H's
    /// cap-dispatch refactor must NOT silently flip them to the new
    /// collateral-cap list. This test exercises a Deposit whose
    /// underlying-cap is populated and whose collateral-cap is empty;
    /// it MUST pass (i.e. the dispatcher reads from `per_mint_caps`,
    /// not `per_mint_collateral_caps`).
    #[test]
    fn deposit_cap_dispatch_unchanged_after_withdraw_addition() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        let config = permissive_config(mint, UnderlyingAmount::new(1_000_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass
        );
    }

    #[test]
    fn owner_mismatch_produces_system_invariant_failed() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        let config = permissive_config(mint, UnderlyingAmount::new(1_000_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let wrong_wallet = Pubkey::new_unique();
        let ctx = EvaluationContext {
            session_wallet: wrong_wallet,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::SystemInvariantFailed(
                SystemInvariantSubreason::OwnerMismatch,
            ))
        );
    }

    /// Gate short-circuit: system-invariant failure takes precedence even
    /// when a later rule would also fail. (Here `allowed_lending_protocols`
    /// allowlist is empty, which would otherwise produce a RuleRejected.
    /// The gate fires first and the rule never runs.)
    #[test]
    fn system_invariant_failure_short_circuits_rule_evaluation() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        let mut config = permissive_config(mint, UnderlyingAmount::new(1));
        // Make protocols allowlist empty — would otherwise HardBlock at
        // AllowedLendingProtocols.
        config.allowed_lending_protocols.allowlist.clear();
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let wrong_wallet = Pubkey::new_unique();
        let ctx = EvaluationContext {
            session_wallet: wrong_wallet,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &config, &ctx);
        assert!(matches!(
            verdict,
            LendingPolicyVerdict::HardBlock(HardBlockReason::SystemInvariantFailed(_))
        ));
    }

    /// Fail-fast: when two rules would both fail, the one that runs first
    /// determines the verdict. Here `require_fresh_state` fails (stale
    /// markers) before `allowed_mints` (would fail on empty allowlist).
    #[test]
    fn fail_fast_first_rule_failure_determines_verdict() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        // stale obligation → RequireFreshState will HardBlock.
        let obl = synth_obligation(owner, mkt, 100, /*stale=*/ true, &[], &[]);
        let res = synth_reserve(
            mkt,
            mint,
            6,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
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
                pubkey: Pubkey::from_str(
                    "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ",
                )
                .unwrap(),
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(1_000),
                publish: FeedPublishFreshness::Unknown,
            }],
            snapshot_observed_slot: ChainSlot::new(1_000),
        })
        .unwrap();

        // Deliberately empty AllowedMints allowlist so that rule would also
        // fail if reached. RequireFreshState should fire first.
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
            allowed_mints: AllowedMintsConfig { allowlist: vec![] },
            max_action_input_amount: MaxActionInputAmountConfig {
                per_mint_caps: vec![],
                per_mint_collateral_caps: vec![],
            },
        };
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &config, &ctx);
        assert_eq!(
            verdict,
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::RequireFreshState,
                RuleRejectionDetail::ProtocolNativeStale,
            ))
        );
    }

    #[test]
    fn fetch_age_exceeded_hardblocks() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();
        let obl = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let res = synth_reserve(
            mkt,
            mint,
            6,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            sentinel,
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        let snap = map_snapshot(SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obl).unwrap(),
            // Component fetched at slot 500; snapshot at slot 10_000.
            // age = 9_500 slots ≈ 3_800_000 ms. max_fetch_age = 1_000 ms =
            // 2 slots. 9_500 > 2 → FetchAgeExceeded.
            obligation_fetched_at_slot: ChainSlot::new(500),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: raw::decode_reserve(&res).unwrap(),
                fetched_at_slot: ChainSlot::new(500),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: Pubkey::new_unique(),
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(500),
                publish: FeedPublishFreshness::Unknown,
            }],
            snapshot_observed_slot: ChainSlot::new(10_000),
        })
        .unwrap();

        let mut config = permissive_config(mint, UnderlyingAmount::new(1));
        config.require_fresh_state.max_fetch_age = DurationMs::new(1_000);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::RequireFreshState,
                RuleRejectionDetail::FetchAgeExceeded,
            ))
        );
    }

    #[test]
    fn empty_oracle_feed_set_hardblocks_max_oracle_staleness() {
        // Both oracle slots are sentinels → empty OracleFeedSet →
        // MaxOracleStalenessMs HardBlocks as OracleFeedSetEmpty.
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();
        let obl = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let res = synth_reserve(
            mkt,
            mint,
            6,
            Pubkey::new_unique(),
            sentinel,
            sentinel,
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
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
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(1_000),
        })
        .unwrap();

        let config = permissive_config(mint, UnderlyingAmount::new(1));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxOracleStalenessMs,
                RuleRejectionDetail::OracleFeedSetEmpty,
            ))
        );
    }

    #[test]
    fn mint_not_in_allowlist_hardblocks() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        let mut config = permissive_config(mint, UnderlyingAmount::new(1_000));
        config.allowed_mints.allowlist.clear(); // reject all mints
        // Still needs some cap entry or MaxActionInputAmount would also reject.
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::AllowedMints,
                RuleRejectionDetail::MintNotAllowed,
            ))
        );
    }

    #[test]
    fn amount_over_cap_hardblocks() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        let config = permissive_config(mint, UnderlyingAmount::new(100));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(101),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxActionInputAmount,
                RuleRejectionDetail::AmountOverCap,
            ))
        );
    }

    #[test]
    fn protocol_not_in_allowlist_hardblocks() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        let mut config = permissive_config(mint, UnderlyingAmount::new(1_000));
        config.allowed_lending_protocols.allowlist.clear();
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::AllowedLendingProtocols,
                RuleRejectionDetail::ProtocolNotAllowed,
            ))
        );
    }

    // ── Integration: map_snapshot output feeds evaluator ─────────────────

    /// Bridge test: a snapshot produced by the formal Slice 1
    /// `map_snapshot(...)` is accepted by `evaluate_lending_policy(...)`
    /// without any type gymnastics. This confirms the Part 5 / Part 6 seam
    /// is type-complete end-to-end — no Solend raw types leak into the
    /// evaluator's inputs or outputs.
    #[test]
    fn map_snapshot_output_is_accepted_by_evaluator() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (snap, _) = fresh_snapshot(owner, mint);
        // The snapshot's type is `crate::lending::LendingSnapshot`, not
        // any `Solend*` type. This assertion is enforced by the compiler
        // via the function signature below.
        fn _accept(_: &LendingSnapshot) {}
        _accept(&snap);

        let config = permissive_config(mint, UnderlyingAmount::new(10_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(42),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &config, &ctx);
        assert_eq!(verdict, LendingPolicyVerdict::Pass);
    }

    // ── Oracle freshness semantics (Slice 2B) ─────────────────────────────
    //
    // These tests exercise `MaxOracleStalenessMs` specifically. They build
    // snapshots with a single feed whose `FeedPublishFreshness` is varied.
    // Every other rule is configured to pass so that MaxOracleStalenessMs's
    // verdict is isolated.

    /// Helper: build a snapshot with exactly one feed carrying a caller-
    /// specified publish freshness. All other rules pass.
    fn snapshot_with_feed_publish(
        owner: Pubkey,
        reserve_mint: Pubkey,
        snapshot_slot: u64,
        feed_publish: FeedPublishFreshness,
    ) -> LendingSnapshot {
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();

        let obl = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let res = synth_reserve(
            mkt,
            reserve_mint,
            6,
            Pubkey::new_unique(),
            pyth,
            sentinel, // switchboard sentinel
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );

        map_snapshot(SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obl).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(snapshot_slot),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: raw::decode_reserve(&res).unwrap(),
                fetched_at_slot: ChainSlot::new(snapshot_slot),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(snapshot_slot),
                publish: feed_publish,
            }],
            snapshot_observed_slot: ChainSlot::new(snapshot_slot),
        })
        .unwrap()
    }

    #[test]
    fn oracle_unknown_publish_freshness_hardblocks() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let snap = snapshot_with_feed_publish(
            owner,
            mint,
            1_000,
            FeedPublishFreshness::Unknown,
        );
        let config = permissive_config(mint, UnderlyingAmount::new(1_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxOracleStalenessMs,
                RuleRejectionDetail::OraclePublishFreshnessUnknown,
            ))
        );
    }

    #[test]
    fn oracle_publish_age_within_threshold_passes() {
        // Publish slot = snapshot slot → age = 0. Any positive threshold passes.
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let snap = snapshot_with_feed_publish(
            owner,
            mint,
            10_000,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
        );
        let config = permissive_config(mint, UnderlyingAmount::new(1_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass
        );
    }

    #[test]
    fn oracle_publish_age_over_threshold_hardblocks() {
        // Publish slot = 500, snapshot slot = 10_000 → age = 9_500 slots
        // ≈ 3_800_000 ms. Threshold 1_000 ms → fail.
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let snap = snapshot_with_feed_publish(
            owner,
            mint,
            10_000,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(500)),
        );
        let mut config = permissive_config(mint, UnderlyingAmount::new(1_000));
        config.max_oracle_staleness.max_publish_age = DurationMs::new(1_000);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxOracleStalenessMs,
                RuleRejectionDetail::OraclePublishAgeExceeded,
            ))
        );
    }

    /// Helper: build a snapshot with both oracle slots populated by distinct
    /// real (non-sentinel) pubkeys, each with its own publish freshness.
    fn snapshot_with_two_feeds(
        owner: Pubkey,
        reserve_mint: Pubkey,
        snapshot_slot: u64,
        pyth_publish: FeedPublishFreshness,
        swb_publish: FeedPublishFreshness,
    ) -> LendingSnapshot {
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let swb = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let swb_owner: Pubkey = "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f"
            .parse()
            .unwrap();

        let obl = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let res = synth_reserve(
            mkt,
            reserve_mint,
            6,
            Pubkey::new_unique(),
            pyth,
            swb,
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );

        map_snapshot(SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obl).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(snapshot_slot),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: raw::decode_reserve(&res).unwrap(),
                fetched_at_slot: ChainSlot::new(snapshot_slot),
            }],
            oracles: vec![
                OracleAccountInfo {
                    pubkey: pyth,
                    owner_program: pyth_owner,
                    fetched_at_slot: ChainSlot::new(snapshot_slot),
                    publish: pyth_publish,
                },
                OracleAccountInfo {
                    pubkey: swb,
                    owner_program: swb_owner,
                    fetched_at_slot: ChainSlot::new(snapshot_slot),
                    publish: swb_publish,
                },
            ],
            snapshot_observed_slot: ChainSlot::new(snapshot_slot),
        })
        .unwrap()
    }

    #[test]
    fn multi_feed_worst_feed_dominates_unknown() {
        // One fresh, one Unknown → Unknown HardBlocks (conjunction semantic).
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let snap = snapshot_with_two_feeds(
            owner,
            mint,
            10_000,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
            FeedPublishFreshness::Unknown,
        );
        let config = permissive_config(mint, UnderlyingAmount::new(1_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxOracleStalenessMs,
                RuleRejectionDetail::OraclePublishFreshnessUnknown,
            ))
        );
    }

    #[test]
    fn multi_feed_worst_feed_dominates_age() {
        // One fresh, one over threshold → OraclePublishAgeExceeded.
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let snap = snapshot_with_two_feeds(
            owner,
            mint,
            10_000,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
            FeedPublishFreshness::KnownSlot(ChainSlot::new(500)),
        );
        let mut config = permissive_config(mint, UnderlyingAmount::new(1_000));
        config.max_oracle_staleness.max_publish_age = DurationMs::new(1_000);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxOracleStalenessMs,
                RuleRejectionDetail::OraclePublishAgeExceeded,
            ))
        );
    }

    #[test]
    fn multi_feed_all_fresh_passes() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let snap = snapshot_with_two_feeds(
            owner,
            mint,
            10_000,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
            FeedPublishFreshness::KnownSlot(ChainSlot::new(9_998)),
        );
        let config = permissive_config(mint, UnderlyingAmount::new(1_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass
        );
    }

    /// Responsibility separation: when protocol-native stale markers AND
    /// oracle publish freshness are BOTH bad, RequireFreshState fires
    /// first (ordering) and MaxOracleStalenessMs does not run.
    #[test]
    fn require_fresh_state_fires_before_oracle_staleness() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();

        // Obligation: protocol-native Stale.
        let obl = synth_obligation(owner, mkt, 100, /*stale=*/ true, &[], &[]);
        let res = synth_reserve(
            mkt,
            mint,
            6,
            Pubkey::new_unique(),
            pyth,
            sentinel,
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        // Oracle: Unknown (also bad, but should not be the verdict).
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
                publish: FeedPublishFreshness::Unknown,
            }],
            snapshot_observed_slot: ChainSlot::new(1_000),
        })
        .unwrap();

        let config = permissive_config(mint, UnderlyingAmount::new(1_000));
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(1),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::RequireFreshState,
                RuleRejectionDetail::ProtocolNativeStale,
            ))
        );
    }

    // ── Slice 3A: AMM Pyth-only oracle-gate passability + negatives ──────
    //
    // These tests exercise the exact pubkeys surfaced by the Slice 2C
    // pre-Slice-3 recon run. They prove:
    //   (a) an empty first-deposit snapshot that INCLUDES the intended
    //       target reserve passes `MaxOracleStalenessMs` under current
    //       V1 oracle semantics (conjunction + Pyth-only feed set);
    //   (b) an empty first-deposit snapshot that does NOT include the
    //       target reserve is NOT oracle-passable (fails-closed on empty
    //       feed set);
    //   (c) Switchboard Unknown still HardBlocks, consistent with
    //       Slice 2B/2C;
    //   (d) protocol-native Stale on the target reserve still HardBlocks
    //       until actual refreshed bytes are re-fetched — the refresh
    //       precondition builder never mutates markers locally.

    use crate::integrations::solend::mapping::{
        map_snapshot_for_first_deposit, FirstDepositAssemblyInputs,
    };

    // Exact pubkeys from Slice 2C pre-Slice-3 recon (docs/lending_policy_
    // vocabulary.md §30.5 recon conclusion).
    const AMM_LENDING_MARKET_BS58: &str = "Au3S1ZSkGwm1fo7g3WFhkD1rcPoUXj7h5ubsGsUFqbLX";
    const AMM_RESERVE_BS58: &str = "6nb1odSYHutVAxoaQyiwPhQNTFn3nBNFdQdNCm5v9Jbp";
    const AMM_MINT_BS58: &str = "E5ndSkaB17Dm7CsD22dvcjfrYSDLCxFcMd6z8ddCk5wp";
    const AMM_PYTH_ORACLE_BS58: &str = "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX";
    const PYTH_SOLANA_RECEIVER_OWNER_BS58: &str =
        "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    fn amm_pyth_only_first_deposit_snapshot(
        session_wallet: Pubkey,
        reserve_stale: bool,
        pyth_publish: FeedPublishFreshness,
    ) -> LendingSnapshot {
        let mkt = Pubkey::from_str(AMM_LENDING_MARKET_BS58).unwrap();
        let reserve_pk = Pubkey::from_str(AMM_RESERVE_BS58).unwrap();
        let mint = Pubkey::from_str(AMM_MINT_BS58).unwrap();
        let pyth = Pubkey::from_str(AMM_PYTH_ORACLE_BS58).unwrap();
        let pyth_owner = Pubkey::from_str(PYTH_SOLANA_RECEIVER_OWNER_BS58).unwrap();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();

        // Synthetic reserve bytes with the AMM pubkeys + decimals baked in.
        // Pyth slot = real Pyth pubkey; Switchboard slot = sentinel (Pyth-only).
        let reserve_bytes = synth_reserve(
            mkt,
            mint,
            9, // AMM reserve decimals
            Pubkey::new_unique(),
            pyth,
            sentinel,
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            reserve_stale,
        );
        let reserve_raw = raw::decode_reserve(&reserve_bytes).unwrap();

        let inputs = FirstDepositAssemblyInputs {
            session_wallet,
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
                publish: pyth_publish,
            }],
            snapshot_observed_slot: ChainSlot::new(10_000),
        };
        map_snapshot_for_first_deposit(inputs).expect("AMM first-deposit mapping succeeds")
    }

    fn amm_permissive_config(mint: Pubkey) -> LendingRuleConfig {
        LendingRuleConfig {
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
                per_mint_collateral_caps: vec![],
            },
        }
    }

    #[test]
    fn amm_pyth_only_first_deposit_oracle_gate_passes() {
        // Controlled fixture: reserve is Fresh (not stale) and Pyth feed
        // is KnownSlot. This isolates the oracle gate; it does NOT claim
        // the full live deposit path is unlocked on mainnet (real mainnet
        // reserves are `stale = true` until refreshed — §66 refresh
        // precondition is a separate future step).
        let owner = Pubkey::new_unique();
        let snap = amm_pyth_only_first_deposit_snapshot(
            owner,
            /*reserve_stale=*/ false,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
        );

        // Structural assertions per prompt:
        let mint = Pubkey::from_str(AMM_MINT_BS58).unwrap();
        let reserve_pk = Pubkey::from_str(AMM_RESERVE_BS58).unwrap();
        assert_eq!(snap.reserves.len(), 1);
        assert_eq!(snap.reserves[0].identifier, reserve_pk);
        assert_eq!(snap.reserves[0].mint, mint);
        assert_eq!(snap.oracles.len(), 1);
        assert_eq!(snap.oracles[0].priced_asset, mint);
        // Sentinel excluded; exactly one feed and it is the Pyth feed.
        assert_eq!(snap.oracles[0].feeds.len(), 1);
        assert_eq!(
            snap.oracles[0].feeds[0].provider,
            OracleProvider::PythSolanaReceiver
        );
        assert_eq!(
            snap.oracles[0].feeds[0].publish,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000))
        );

        // Evaluator passes.
        let config = amm_permissive_config(mint);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::Pass,
            "oracle gate must pass for the Pyth-only AMM reserve under \
             current V1 conjunction semantics"
        );
    }

    #[test]
    fn amm_first_deposit_without_target_reserve_is_not_oracle_passable() {
        // Negative: empty first-deposit snapshot with NO target reserve
        // included. The action's mint has no oracle feed set → rule
        // HardBlocks on OracleFeedSetEmpty. Absence must not become a pass.
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::from_str(AMM_LENDING_MARKET_BS58).unwrap();
        let mint = Pubkey::from_str(AMM_MINT_BS58).unwrap();
        let snap = map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: mkt,
            target_reserves: vec![],
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(10_000),
        })
        .unwrap();

        let config = amm_permissive_config(mint);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        // This particular scenario hits AllowedMints' reserve cross-reference
        // BEFORE MaxOracleStalenessMs (rule order: fresh-state, oracle,
        // protocols, mints, amount). The empty reserves array produces an
        // OracleFeedSetEmpty HardBlock at MaxOracleStalenessMs.
        //
        // The core invariant being asserted: absence of feeds is NOT a Pass.
        let verdict = evaluate_lending_policy(&snap, &action, &config, &ctx);
        assert!(
            matches!(verdict, LendingPolicyVerdict::HardBlock(_)),
            "empty first-deposit snapshot must HardBlock, got {verdict:?}"
        );
    }

    #[test]
    fn amm_first_deposit_with_reserve_stale_still_hardblocks_on_require_fresh_state() {
        // Stale state remains a HardBlock until actual refreshed account
        // bytes are re-fetched. The refresh precondition BUILDER does not
        // fake freshness — this test proves the evaluator still sees the
        // stale marker in the underlying reserve even when a Slice 3A
        // refresh-plan builder would notionally be invoked by outer
        // wiring. (The refresh plan itself is blind to snapshots; this
        // test never calls it — it simply verifies that a stale-bytes
        // snapshot HardBlocks.)
        let owner = Pubkey::new_unique();
        let snap = amm_pyth_only_first_deposit_snapshot(
            owner,
            /*reserve_stale=*/ true,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
        );
        assert_eq!(
            snap.reserves[0].protocol_native_stale,
            StaleMarker::Stale,
            "stale markers in the underlying reserve bytes are preserved \
             verbatim through the mapping"
        );
        let mint = Pubkey::from_str(AMM_MINT_BS58).unwrap();
        let config = amm_permissive_config(mint);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::RequireFreshState,
                RuleRejectionDetail::ProtocolNativeStale,
            )),
            "stale-reserve first-deposit snapshot must HardBlock on \
             RequireFreshState until caller actually refreshes + re-fetches"
        );
    }

    #[test]
    fn snapshot_with_switchboard_unknown_feed_still_hardblocks_under_current_policy() {
        // Regression-style assertion for Slice 3A: a snapshot whose target
        // reserve carries a non-sentinel Switchboard feed reporting
        // `Unknown` still HardBlocks under the unchanged V1 conjunction
        // semantic. This is Slice 2B/2C behavior — restated here to lock
        // it against any refactor drift during Slice 3A precondition work.
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::from_str(AMM_LENDING_MARKET_BS58).unwrap();
        let reserve_pk = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let swb = Pubkey::new_unique();
        let pyth_owner = Pubkey::from_str(PYTH_SOLANA_RECEIVER_OWNER_BS58).unwrap();
        let swb_owner: Pubkey = "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f"
            .parse()
            .unwrap();

        let reserve_bytes = synth_reserve(
            mkt,
            mint,
            9,
            Pubkey::new_unique(),
            pyth,
            swb, // non-sentinel switchboard slot
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        let reserve_raw = raw::decode_reserve(&reserve_bytes).unwrap();
        let snap = map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: mkt,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(10_000),
            }],
            oracles: vec![
                OracleAccountInfo {
                    pubkey: pyth,
                    owner_program: pyth_owner,
                    fetched_at_slot: ChainSlot::new(10_000),
                    publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
                },
                OracleAccountInfo {
                    pubkey: swb,
                    owner_program: swb_owner,
                    fetched_at_slot: ChainSlot::new(10_000),
                    // Switchboard decoder is still Unknown under Slice 2C
                    // evidence.
                    publish: FeedPublishFreshness::Unknown,
                },
            ],
            snapshot_observed_slot: ChainSlot::new(10_000),
        })
        .unwrap();

        let config = amm_permissive_config(mint);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::MaxOracleStalenessMs,
                RuleRejectionDetail::OraclePublishFreshnessUnknown,
            )),
            "any non-sentinel Switchboard Unknown feed still HardBlocks \
             the oracle gate under current V1 conjunction semantics"
        );
    }

    #[test]
    fn refresh_plan_builder_does_not_mutate_stale_markers() {
        // Structural proof: building a refresh plan for a reserve does
        // NOT touch the underlying snapshot / stale markers. The plan is
        // a pure Vec<Instruction>; the evaluator still sees stale as
        // stale until the caller actually re-fetches and re-maps.
        use crate::integrations::solend::refresh::{
            build_refresh_instructions, RefreshPlanInputs, ReserveRefreshInput,
        };
        let owner = Pubkey::new_unique();
        let snap_before = amm_pyth_only_first_deposit_snapshot(
            owner,
            /*reserve_stale=*/ true,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(10_000)),
        );
        assert_eq!(snap_before.reserves[0].protocol_native_stale, StaleMarker::Stale);

        // Build a refresh plan — this operation is independent of the
        // snapshot.
        let solend_program: Pubkey = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo"
            .parse()
            .unwrap();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program,
            reserves: vec![ReserveRefreshInput {
                reserve_pubkey: snap_before.reserves[0].identifier,
                pyth_oracle: Pubkey::from_str(AMM_PYTH_ORACLE_BS58).unwrap(),
                switchboard_oracle: SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap(),
            }],
            obligation: None,
        });
        assert_eq!(plan.instructions.len(), 1);

        // The snapshot's stale marker is unchanged — the refresh plan
        // builder is a pure producer of instruction bytes; it cannot
        // and does not mutate any policy state.
        assert_eq!(
            snap_before.reserves[0].protocol_native_stale,
            StaleMarker::Stale,
            "refresh plan builder must not locally flip stale markers"
        );

        // And the evaluator still HardBlocks on the pre-refresh snapshot.
        let mint = Pubkey::from_str(AMM_MINT_BS58).unwrap();
        let config = amm_permissive_config(mint);
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: mint,
            amount: UnderlyingAmount::new(100),
        };
        let ctx = EvaluationContext {
            session_wallet: owner,
        };
        assert_eq!(
            evaluate_lending_policy(&snap_before, &action, &config, &ctx),
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(
                RuleKind::RequireFreshState,
                RuleRejectionDetail::ProtocolNativeStale,
            )),
            "evaluator still sees stale until caller actually re-fetches \
             post-refresh bytes"
        );
    }
}

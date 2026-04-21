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
//! - `ProposedAction` admits only `Deposit` and `Repay` at the type level.
//!   `Withdraw` and `Borrow` are structurally unrepresentable (§49.2).
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
use super::types::{DurationMs, UnderlyingAmount};

// ── ProposedAction ─────────────────────────────────────────────────────────

/// V1 lending action. Only risk-reducing actions are representable: §3.1.
///
/// `Withdraw` and `Borrow` are intentionally absent. They are Part 1 §3.2
/// scope-boundary blocks, enforced at the type level to prevent any V1
/// code path from constructing them. Adding a variant here is a scope
/// decision (Part 1), not an implementation change.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Deposit,
    Repay,
}

impl ProposedAction {
    pub fn protocol(&self) -> ProtocolTag {
        match self {
            ProposedAction::Deposit { protocol, .. }
            | ProposedAction::Repay { protocol, .. } => *protocol,
        }
    }

    pub fn reserve_mint(&self) -> Pubkey {
        match self {
            ProposedAction::Deposit { reserve_mint, .. }
            | ProposedAction::Repay { reserve_mint, .. } => *reserve_mint,
        }
    }

    pub fn amount(&self) -> UnderlyingAmount {
        match self {
            ProposedAction::Deposit { amount, .. }
            | ProposedAction::Repay { amount, .. } => *amount,
        }
    }

    pub fn kind(&self) -> ActionKind {
        match self {
            ProposedAction::Deposit { .. } => ActionKind::Deposit,
            ProposedAction::Repay { .. } => ActionKind::Repay,
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
    /// Per-mint caps. An action whose mint has no entry here is rejected
    /// by [`RuleRejectionDetail::NoCapConfigured`] — the conservative
    /// posture: a missing cap is not an implicit "no limit."
    pub per_mint_caps: Vec<(Pubkey, UnderlyingAmount)>,
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
    // otherwise the deposit/repay has no target reserve to apply against.
    let reserve = snapshot
        .reserves
        .iter()
        .find(|r| r.mint == mint)
        .ok_or(RuleRejectionDetail::ReserveNotInSnapshot)?;
    // Repay-specific cross-reference: the obligation must carry a borrow
    // on this reserve. Part 3A §18.4's "a Repay against a mint the
    // obligation has no debt in is rejected by this rule's cross-reference."
    if action.kind() == ActionKind::Repay {
        let has_debt = snapshot
            .obligation
            .borrows
            .iter()
            .any(|b| b.reserve == reserve.identifier);
        if !has_debt {
            return Err(RuleRejectionDetail::RepayWithoutDebt);
        }
    }
    Ok(())
}

fn max_action_input_amount(
    action: &ProposedAction,
    config: &MaxActionInputAmountConfig,
) -> Result<(), RuleRejectionDetail> {
    let mint = action.reserve_mint();
    match config.per_mint_caps.iter().find(|(m, _)| *m == mint) {
        None => Err(RuleRejectionDetail::NoCapConfigured),
        Some((_, cap)) => {
            if action.amount() > *cap {
                Err(RuleRejectionDetail::AmountOverCap)
            } else {
                Ok(())
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
    use crate::lending::{ChainSlot, FeedPublishFreshness};
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
            },
        }
    }

    // ── API shape tests (lock the seam at compile + runtime) ──────────────

    /// Pattern-matching exhaustion: if a V1 variant is ever added (e.g.
    /// `Withdraw` / `Borrow` sneaking back), this test fails to compile.
    #[test]
    fn proposed_action_is_deposit_or_repay_only() {
        fn _exhaustive(a: &ProposedAction) {
            match a {
                ProposedAction::Deposit { .. } => {}
                ProposedAction::Repay { .. } => {}
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
        _exhaustive(&d);
        _exhaustive(&r);
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
            },
        };
        let cap: UnderlyingAmount = cfg.max_action_input_amount.per_mint_caps[0].1;
        let age: DurationMs = cfg.require_fresh_state.max_fetch_age;
        assert_eq!(cap.raw(), 42);
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
}

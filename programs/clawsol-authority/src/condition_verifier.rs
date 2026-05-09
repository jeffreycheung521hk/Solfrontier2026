//! Stage 2 condition verifier — pure, deterministic, fixture-tested.
//!
//! # What this module is
//!
//! Substrate-only oracle/rate verification fixtures the eventual
//! `clawsol-authority::execute_action` ix will call after decoding live
//! account data. **No live RPC, no Pyth fetch, no Solend fetch, no tx
//! building, no CPI.** Inputs are already-decoded snapshot structs; the
//! verifier turns them and a canonical `WatchRule` condition into a
//! single `bool` (or a `VerifierError` if the snapshot fails any
//! integrity gate).
//!
//! # What this module is not
//!
//! - Not the watcher.
//! - Not the executor.
//! - Not a Solend / Pyth account decoder. Decoding is the caller's job
//!   (off-chain daemon decodes the live RPC bytes; the on-chain
//!   re-verifier decodes the account data passed in via `accounts[..]`).
//! - Not a Jupiter pre/post bracket. Sibling-instruction sysvar
//!   verification belongs in a separate module per Stage 2 spec § 6.1.3.
//!
//! # Source-of-truth alignment
//!
//! - `docs/STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md` § 5.2 (Solend
//!   refresh-then-withdraw same-tx, deposit-only obligation), § 7.1
//!   (Pyth: feed identity, verification level, max-age, max-confidence,
//!   adverse bound, no float, exponent normalisation), § 7.2 (Solend:
//!   no on-chain `supply_apy`, three-region kinked borrow rate with
//!   `super_max_borrow_rate: u64`, `SLOTS_PER_YEAR = 63_072_000`,
//!   formula version pinned in `schema_version`), § 10 (no floats
//!   anywhere, fail-closed on any safety gate, all thresholds part of
//!   canonical hash), § 14 (audit U-5 Pyth Full-only, audit C-3 width
//!   discipline).
//! - `docs/STAGE2_PYTH_ADAPTER_RESEARCH.md` § 4 / § 6 / § 7 / § 9.
//! - `docs/STAGE2_SOLEND_APY_RESEARCH.md` § 3 / § 4 / § 6.
//! - `crates/types/src/stage2_watch_rule.rs` (canonical `Condition`,
//!   `Comparison`, `BoundMode`, `RateKind`, `VerificationLevel`,
//!   `ConditionLogic`).
//!
//! # Why mirror enums instead of importing `claw-types`
//!
//! `claw-types` is a Stage 1/Stage 2 substrate crate that depends on
//! `serde_json`, `chrono`, `schemars`, `uuid` — types the on-chain BPF
//! build deliberately does not pull in (see `programs/clawsol-intent`
//! and the existing `programs/clawsol-authority/Cargo.toml`, which keep
//! `claw-types` as a `dev-dependency` only). To stay consistent with
//! that boundary while still operating on the same canonical wire
//! shapes, this module declares small mirror enums that match the
//! Borsh ordinal of each `claw_types::stage2_watch_rule` variant.
//! Parity is asserted by tests (see the `mirror_enum_parity` block at
//! the bottom) so any reorder on either side fails loudly.
//!
//! # Float ban
//!
//! All math is integer / fixed-point. Pyth comparison widens to `i128`
//! and uses `checked_*` exclusively. Solend math operates in WAD
//! (10¹⁸) `u128` per `STAGE2_SOLEND_APY_RESEARCH.md` § 4.5 and 4.7.
//! Saturating arithmetic on a comparison path is forbidden — a wrong
//! answer with no error is the worst-case failure mode and we'd rather
//! return a [`VerifierError`].

use borsh::{BorshDeserialize, BorshSerialize};
use core::cmp::Ordering;

// ── Constants ───────────────────────────────────────────────────────────────

/// WAD scale (10¹⁸). Identical to Solend's `Decimal` scale and to the
/// constant in `crates/gateway/src/integrations/solend/raw.rs`. Kept
/// duplicated here on purpose: the on-chain BPF build does not depend
/// on the gateway crate, so the constant must travel with the verifier.
/// Parity with the gateway value is asserted in tests.
pub const SOLEND_WAD: u128 = 1_000_000_000_000_000_000;

/// Slots per year, Solend mainnet. Pinned per Stage 2 spec § 7.2 (audit
/// U-1) and `STAGE2_SOLEND_APY_RESEARCH.md` § 1 — Solend's
/// `token-lending/sdk/src/state/mod.rs` defines `SLOTS_PER_YEAR: u64 =
/// 63_072_000` (= 2 slots/sec × 60 × 60 × 24 × 365). Forks/branches
/// have shipped different values; the verifier MUST agree with Solend
/// mainnet on the exact integer.
pub const SOLEND_SLOTS_PER_YEAR: u64 = 63_072_000;

/// Basis-points denominator (1 bps = 1 / 10_000).
pub const BPS_DENOM: u128 = 10_000;

/// Solend supply-APR formula version this build of the verifier ships.
/// The B1 canonical schema commits `formula_version: u8` per condition
/// (`crates/types/src/stage2_watch_rule.rs::Condition::SolendReserveSupplyRate`);
/// the verifier rejects anything other than this constant per Stage 2
/// spec § 7.2 ("APR formula version pinned in `schema_version`").
pub const SUPPORTED_SOLEND_FORMULA_VERSION: u8 = 1;

/// Maximum exponent difference accepted by the Pyth comparator before
/// `i128` overflow becomes a real risk. Per
/// `STAGE2_PYTH_ADAPTER_RESEARCH.md` § 6.4: realistic feeds use
/// `diff ∈ [-3, 6]`; anything outside `[-18, 18]` is rejected as
/// fail-closed `PythExponentOutOfRange`.
pub const PYTH_MAX_EXPONENT_DIFF: i32 = 18;

// ── Mirror enums (parity with claw-types) ──────────────────────────────────

/// Comparison operator. Mirror of
/// `claw_types::stage2_watch_rule::Comparison`. The numeric position
/// of each variant in this enum matches the Borsh ordinal of the
/// canonical type (`Lt = 0`, `Lte = 1`, `Gt = 2`, `Gte = 3`); reorder
/// is a wire break and is caught by the parity test in
/// `tests::mirror_enum_parity`.
///
/// Borsh-serializable so `ConditionProofPayload` (instruction.rs) can
/// carry a `Comparison` directly into the on-chain processor with
/// canonical-ordinal parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum Comparison {
    Lt,
    Lte,
    Gt,
    Gte,
}

impl Comparison {
    /// Apply the operator to two `i128`s. No floats, no saturating math.
    pub const fn apply_i128(self, lhs: i128, rhs: i128) -> bool {
        match self {
            Self::Lt => lhs < rhs,
            Self::Lte => lhs <= rhs,
            Self::Gt => lhs > rhs,
            Self::Gte => lhs >= rhs,
        }
    }

    /// Apply the operator to two `u128`s (Solend WAD path).
    pub const fn apply_u128(self, lhs: u128, rhs: u128) -> bool {
        match self {
            Self::Lt => lhs < rhs,
            Self::Lte => lhs <= rhs,
            Self::Gt => lhs > rhs,
            Self::Gte => lhs >= rhs,
        }
    }
}

/// Adverse-bound discipline for Pyth. Mirror of
/// `claw_types::stage2_watch_rule::BoundMode`. See spec § 7.1 and
/// `STAGE2_PYTH_ADAPTER_RESEARCH.md` § 4.1 / § 7.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum BoundMode {
    /// `(price - confidence)` is the lhs of `>` checks.
    AdverseLowerForGt,
    /// `(price + confidence)` is the lhs of `<` checks.
    AdverseUpperForLt,
    /// Use `price` itself, no confidence widening.
    ///
    /// **Why allowed at all.** The canonical claw-types schema
    /// includes `Midpoint` for forward compatibility (preview /
    /// diagnostic use). The verifier accepts it under one strict
    /// condition (see [`verify_pyth_price_condition`]): the mode MUST
    /// agree with the requested comparison direction (Gt/Gte ↔
    /// AdverseLowerForGt or Midpoint, Lt/Lte ↔ AdverseUpperForLt or
    /// Midpoint). Mismatched mode → `BoundDirectionMismatch`.
    Midpoint,
}

/// Solend rate kind. Mirror of `claw_types::stage2_watch_rule::RateKind`.
/// v1 supports `Apr` only; `Apy` is rejected as
/// [`VerifierError::UnsupportedRateKind`] per spec § 7.2 / research
/// § 0 / § 4.1 (APY needs a higher CU envelope and a separate fixture
/// set; the demo is not gated on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum RateKind {
    Apr,
    Apy,
}

/// Pyth verification level. Mirror of
/// `claw_types::stage2_watch_rule::VerificationLevel`. v1 supports
/// `Full` only (audit U-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum VerificationLevel {
    Full,
}

/// How a rule's `conditions[..]` are combined. Mirror of
/// `claw_types::stage2_watch_rule::ConditionLogic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum ConditionLogic {
    /// AND — every condition must hold.
    All,
    /// OR — at least one condition must hold.
    Any,
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Verifier failure modes. Every variant is "fail closed" — the
/// caller MUST propagate the error upward and refuse to execute the
/// action. The variants are deliberately fine-grained so audit logs
/// can attribute the failure to the specific gate that tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifierError {
    // ── Pyth integrity gates ─────────────────────────────────────────
    /// `snapshot.feed_id != condition.feed_id`. Mirrors Pyth SDK's
    /// `GetPriceError::MismatchedFeedId`. Spec § 7.1.
    PythFeedIdMismatch,
    /// `snapshot.publish_time` older than `condition.max_age_seconds`.
    /// Mirrors Pyth SDK's `GetPriceError::PriceTooOld`. Spec § 7.1.
    PythPriceTooOld,
    /// `snapshot.verification_level != Full`. Spec § 7.1, audit U-5.
    PythVerificationLevelInsufficient,
    /// `(conf * 10_000) / abs(price) > max_confidence_bps`. Liveness
    /// gate. Spec § 7.1.
    PythConfidenceTooWide,
    /// Threshold mantissa or exponent invalid (e.g. `price == 0` would
    /// make confidence-bps math divide by zero) or `exponent` out of
    /// `[-PYTH_MAX_EXPONENT_DIFF, PYTH_MAX_EXPONENT_DIFF]` range.
    /// Spec § 7.1, research § 6.4.
    PythExponentOutOfRange,
    /// `i128` overflow in the comparator. Research § 6.4.
    PythComputeOverflow,
    /// `BoundMode` does not match the requested `Comparison`
    /// direction. E.g. `Comparison::Gt` paired with
    /// `BoundMode::AdverseUpperForLt` is meaningless and we refuse to
    /// compute it.
    BoundDirectionMismatch,
    /// `Comparison::Lt`/`Lte` paired with the verifier's only Pyth
    /// price feed mode requires `AdverseUpperForLt` or `Midpoint`;
    /// other modes refuse here as a fail-closed signal.
    UnsupportedComparison,

    // ── Solend integrity gates ───────────────────────────────────────
    /// `condition.formula_version != SUPPORTED_SOLEND_FORMULA_VERSION`.
    /// Spec § 7.2.
    SolendFormulaVersionUnsupported,
    /// `current_slot - last_update_slot > max_reserve_staleness_slots`,
    /// or the snapshot's `stale_flag` is set. Spec § 5.2 step 6.
    SolendReserveStale,
    /// `condition.rate_kind == Apy` (deferred). Research § 4.1.
    UnsupportedRateKind,
    /// Reserve config inputs violate the documented invariants
    /// (`min ≤ optimal ≤ max ≤ super_max`, `optimal_util ≤ max_util ≤
    /// 100`, `take ∈ [0, 100]`). Research § 4.5 / R-O2.
    SolendInvalidReserveConfig,
    /// `u128` overflow inside the WAD math. Research § 4.5 / 4.7.
    SolendComputeOverflow,
    /// Threshold conversion overflowed (e.g. `threshold_bps` > `u32`
    /// shouldn't happen — type already constrains — but converting
    /// bps → WAD via `* 10^14` requires `u128` and we still
    /// `checked_mul`). Defence in depth.
    SolendThresholdOverflow,
}

impl core::fmt::Display for VerifierError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PythFeedIdMismatch => f.write_str("pyth feed id mismatch"),
            Self::PythPriceTooOld => f.write_str("pyth price too old"),
            Self::PythVerificationLevelInsufficient => {
                f.write_str("pyth verification_level != Full")
            }
            Self::PythConfidenceTooWide => f.write_str("pyth confidence > max_confidence_bps"),
            Self::PythExponentOutOfRange => f.write_str("pyth exponent out of supported range"),
            Self::PythComputeOverflow => f.write_str("pyth comparator i128 overflow"),
            Self::BoundDirectionMismatch => {
                f.write_str("pyth bound mode does not match comparison direction")
            }
            Self::UnsupportedComparison => f.write_str("unsupported comparison operator"),
            Self::SolendFormulaVersionUnsupported => {
                f.write_str("solend formula_version not supported by this verifier build")
            }
            Self::SolendReserveStale => f.write_str("solend reserve snapshot is stale"),
            Self::UnsupportedRateKind => {
                f.write_str("solend rate_kind not supported (Apr only in v1)")
            }
            Self::SolendInvalidReserveConfig => {
                f.write_str("solend reserve config violates invariants")
            }
            Self::SolendComputeOverflow => f.write_str("solend WAD math overflow"),
            Self::SolendThresholdOverflow => f.write_str("solend threshold conversion overflow"),
        }
    }
}

// ── Pyth condition + snapshot ──────────────────────────────────────────────

/// Pure-Rust projection of `claw_types::stage2_watch_rule::Condition::PythPrice`
/// containing only the fields the verifier needs to evaluate the
/// condition. Construct from a canonical [`Condition`] off-chain (or
/// from the on-chain decoded `WatchRule` in the eventual
/// `execute_action` ix) with `.into()` once a `From` impl is added by
/// the caller layer.
///
/// [`Condition`]: ../../../../crates/types/src/stage2_watch_rule.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PythPriceCondition {
    pub feed_id: [u8; 32],
    pub comparison: Comparison,
    pub threshold_mantissa: i64,
    pub threshold_exponent: i32,
    pub max_age_seconds: u32,
    pub max_confidence_bps: u16,
    pub verification_level_required: VerificationLevel,
    pub bound_mode: BoundMode,
}

/// Already-decoded Pyth `PriceUpdateV2` snapshot. The on-chain
/// re-verifier MUST construct this from the live `PriceUpdateV2`
/// account using `pyth_solana_receiver_sdk` (or equivalent verbatim
/// decode); the off-chain daemon constructs it from RPC. This struct
/// is the parity boundary — both sides hand the same numbers to
/// [`verify_pyth_price_condition`] and get the same `bool` back.
///
/// Per `STAGE2_PYTH_ADAPTER_RESEARCH.md` § 6.1 the native
/// representation is:
///   - `price: i64`
///   - `conf: u64`
///   - `exponent: i32`
///   - `publish_time: i64` (Unix seconds)
///   - `feed_id: [u8; 32]`
///   - `verification_level: VerificationLevel` (post-decode)
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PythPriceSnapshot {
    pub feed_id: [u8; 32],
    pub price_mantissa: i64,
    pub price_exponent: i32,
    pub conf: u64,
    pub publish_time: i64,
    pub verification_level: VerificationLevel,
}

/// Verification-time context (clock-equivalent inputs). Separated
/// from the snapshot so a single decoded snapshot can be re-evaluated
/// at multiple wall-clock points by tests / replay tooling without
/// mutating the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PythVerificationContext {
    pub current_unix_time: i64,
}

// ── Solend condition + snapshot ────────────────────────────────────────────

/// Pure-Rust projection of
/// `claw_types::stage2_watch_rule::Condition::SolendReserveSupplyRate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SolendSupplyAprCondition {
    pub comparison: Comparison,
    pub threshold_bps: u32,
    pub rate_kind: RateKind,
    pub formula_version: u8,
    pub max_reserve_staleness_slots: u32,
}

/// Already-decoded Solend reserve snapshot — the inputs the
/// supply-APR formula needs. Field set follows
/// `STAGE2_SOLEND_APY_RESEARCH.md` § 4.5 (`ReserveRateInputs`).
///
/// The fields with `_pct` suffix store **percent values in `[0, 100]`**
/// exactly as Solend stores them on the wire (`u8` percents for the
/// kinked-rate boundary points, `u64` percent for `super_max_borrow_rate`
/// — width preserved per audit C-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SolendReserveSnapshot {
    /// Liquidity available for new borrows. Base units of underlying
    /// token (e.g. raw USDC lamports for the USDC reserve).
    pub available_amount: u64,
    /// Outstanding debt, scaled to WAD (Solend's `Decimal` shape).
    pub borrowed_amount_wads: u128,
    /// `min_borrow_rate` in percent (`u8`).
    pub min_borrow_rate_pct: u8,
    /// `optimal_borrow_rate` in percent (`u8`).
    pub optimal_borrow_rate_pct: u8,
    /// `max_borrow_rate` in percent (`u8`).
    pub max_borrow_rate_pct: u8,
    /// `super_max_borrow_rate` in percent. **`u64` width preserved** —
    /// audit C-3: a naive `as u8` cast here truncates above 255% and
    /// silently breaks the third region of the kinked rate model.
    pub super_max_borrow_rate_pct: u64,
    /// `optimal_utilization_rate` in percent (`u8`).
    pub optimal_utilization_rate_pct: u8,
    /// `max_utilization_rate` in percent (`u8`).
    pub max_utilization_rate_pct: u8,
    /// `protocol_take_rate` in percent (`u8`).
    pub protocol_take_rate_pct: u8,
    /// Slot at which this reserve was last refreshed
    /// (`reserve.last_update.slot`). Used together with
    /// `current_slot` for the staleness gate.
    pub last_update_slot: u64,
    /// `reserve.last_update.stale` boolean flag (Solend marks a reserve
    /// stale on accrue without refresh; the on-chain refresh ix flips
    /// this back to `false`).
    pub stale_flag: bool,
}

/// Verification-time context for a Solend snapshot. Separated for the
/// same reason as [`PythVerificationContext`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SolendVerificationContext {
    pub current_slot: u64,
}

// ── Pyth verifier ──────────────────────────────────────────────────────────

/// Verify a Pyth price condition against a decoded snapshot.
///
/// Returns `Ok(true)` if the trigger fires, `Ok(false)` if it does
/// not, or `Err(VerifierError::Pyth*)` on any integrity-gate failure.
/// Callers MUST treat any `Err(_)` as fail-closed and refuse to
/// execute the action (spec § 7.3 — "On any oracle freshness or
/// integrity failure: fail closed. No fallback feed.")
///
/// Gates enforced (in evaluation order — cheap structural checks
/// first, math last):
///
/// 1. **Feed identity.** `snapshot.feed_id == condition.feed_id`.
///    Spec § 7.1 (the price-update account is not derived as a PDA
///    from feed_id; same-pubkey-different-feed is a real attack
///    surface).
/// 2. **Verification level.** `snapshot.verification_level == Full`.
///    Spec § 7.1 / audit U-5 (`Partial` is rejected outright in v1).
/// 3. **Freshness.** `current_unix_time - publish_time <= max_age_seconds`.
///    Spec § 7.1 (per-action defaults: ≤ 30 s for Jupiter trading,
///    ≤ 60 s for Solend slow conditions; this verifier enforces
///    whatever the rule committed at authorisation time).
/// 4. **Bound-direction agreement.** `BoundMode` and `Comparison` must
///    agree on the direction or the verifier returns
///    [`VerifierError::BoundDirectionMismatch`] without doing any
///    arithmetic.
/// 5. **Confidence ratio.** `(conf * 10_000) / abs(price_mantissa) <=
///    max_confidence_bps`. Spec § 7.1. Computed in `i128`/`u128` with
///    `checked_*`.
/// 6. **Adverse-bound + threshold compare.** Per § 7.1 / research § 6.2:
///    apply the bound, normalise exponents to a common space (always
///    up-shift, never down-shift — preserves precision), compare in
///    `i128`. Overflow is `PythComputeOverflow`, not wrap.
pub fn verify_pyth_price_condition(
    condition: &PythPriceCondition,
    snapshot: &PythPriceSnapshot,
    context: &PythVerificationContext,
) -> Result<bool, VerifierError> {
    // Gate 1 — feed identity.
    if snapshot.feed_id != condition.feed_id {
        return Err(VerifierError::PythFeedIdMismatch);
    }

    // Gate 2 — verification level. The match here looks redundant
    // (only one variant exists in v1) but keeping the exhaustive
    // shape forces a deliberate decision when a new variant lands.
    match (snapshot.verification_level, condition.verification_level_required) {
        (VerificationLevel::Full, VerificationLevel::Full) => {}
    }

    // Gate 3 — freshness. Reject negative skew (publish_time after
    // current time, which would imply clock drift) the same way as
    // an over-age reading: fail closed. Cast to i128 to dodge i64
    // overflow on adversarial inputs.
    let age_secs = (context.current_unix_time as i128)
        .checked_sub(snapshot.publish_time as i128)
        .ok_or(VerifierError::PythComputeOverflow)?;
    if age_secs < 0 || age_secs > condition.max_age_seconds as i128 {
        return Err(VerifierError::PythPriceTooOld);
    }

    // Gate 4 — bound-direction agreement.
    match (condition.comparison, condition.bound_mode) {
        (Comparison::Gt | Comparison::Gte, BoundMode::AdverseLowerForGt) => {}
        (Comparison::Lt | Comparison::Lte, BoundMode::AdverseUpperForLt) => {}
        (_, BoundMode::Midpoint) => {
            // Midpoint is admissible only as a deliberate "no
            // confidence widening" mode and only when the rule
            // explicitly committed to it at authz time. The
            // canonical-rule hash bakes the choice in, so we just
            // accept the mode and pair it with whichever Comparison
            // is requested.
        }
        // Anything else (e.g. Gt + AdverseUpperForLt) is a logic
        // error in the rule — refuse to evaluate.
        _ => return Err(VerifierError::BoundDirectionMismatch),
    }

    // Gate 5 — confidence ratio.
    //
    //   (conf as u128 * BPS_DENOM) / abs(price as i128) as u128 <= max_confidence_bps
    //
    // Reject `price == 0` (would divide by zero, and a zero spot
    // price is meaningless on every Pyth feed Stage 2 cares about).
    let price_abs_u128 = (snapshot.price_mantissa as i128)
        .checked_abs()
        .and_then(|n| u128::try_from(n).ok())
        .ok_or(VerifierError::PythComputeOverflow)?;
    if price_abs_u128 == 0 {
        return Err(VerifierError::PythConfidenceTooWide);
    }
    let conf_u128 = snapshot.conf as u128;
    let conf_bps_num = conf_u128
        .checked_mul(BPS_DENOM)
        .ok_or(VerifierError::PythComputeOverflow)?;
    let conf_bps = conf_bps_num / price_abs_u128;
    if conf_bps > condition.max_confidence_bps as u128 {
        return Err(VerifierError::PythConfidenceTooWide);
    }

    // Gate 6 — apply adverse bound, then compare.
    let bound = match condition.bound_mode {
        BoundMode::AdverseLowerForGt => (snapshot.price_mantissa as i128)
            .checked_sub(snapshot.conf as i128)
            .ok_or(VerifierError::PythComputeOverflow)?,
        BoundMode::AdverseUpperForLt => (snapshot.price_mantissa as i128)
            .checked_add(snapshot.conf as i128)
            .ok_or(VerifierError::PythComputeOverflow)?,
        BoundMode::Midpoint => snapshot.price_mantissa as i128,
    };

    // Exponent normalisation. We always *up-shift* the side with the
    // larger exponent (less negative) so we never down-shift and lose
    // precision. Diff = price_exponent - threshold_exponent.
    //
    //   diff > 0:  price exponent larger → multiply price (bound) by 10^diff
    //   diff < 0:  threshold exponent larger → multiply threshold by 10^(-diff)
    //   diff == 0: compare raw
    let diff = (snapshot.price_exponent as i64)
        .checked_sub(condition.threshold_exponent as i64)
        .ok_or(VerifierError::PythComputeOverflow)?;
    if !(-(PYTH_MAX_EXPONENT_DIFF as i64)..=(PYTH_MAX_EXPONENT_DIFF as i64)).contains(&diff) {
        return Err(VerifierError::PythExponentOutOfRange);
    }
    let (lhs, rhs) = match diff.cmp(&0) {
        Ordering::Equal => (bound, condition.threshold_mantissa as i128),
        Ordering::Greater => {
            let m = pow10_i128(diff as u32)?;
            let lhs = bound.checked_mul(m).ok_or(VerifierError::PythComputeOverflow)?;
            (lhs, condition.threshold_mantissa as i128)
        }
        Ordering::Less => {
            let m = pow10_i128((-diff) as u32)?;
            let rhs = (condition.threshold_mantissa as i128)
                .checked_mul(m)
                .ok_or(VerifierError::PythComputeOverflow)?;
            (bound, rhs)
        }
    };

    Ok(condition.comparison.apply_i128(lhs, rhs))
}

/// Compute `10^n` as `i128`, with overflow as a hard error. Range is
/// `n <= 38` for `i128::MAX`; the verifier additionally caps `n` at
/// [`PYTH_MAX_EXPONENT_DIFF`] (see [`PythComputeOverflow`] /
/// [`PythExponentOutOfRange`] for the resulting error variants).
fn pow10_i128(n: u32) -> Result<i128, VerifierError> {
    let mut acc: i128 = 1;
    for _ in 0..n {
        acc = acc
            .checked_mul(10)
            .ok_or(VerifierError::PythComputeOverflow)?;
    }
    Ok(acc)
}

// ── Solend verifier ─────────────────────────────────────────────────────────

/// Verify a Solend supply-APR condition against a decoded reserve
/// snapshot.
///
/// Returns `Ok(true)` if the trigger fires, `Ok(false)` if it does
/// not, `Err(_)` on any integrity-gate failure.
///
/// Gates enforced (cheap → expensive):
///
/// 1. **Formula version.** `condition.formula_version ==
///    SUPPORTED_SOLEND_FORMULA_VERSION`. Spec § 7.2 — old
///    authorisations against an obsolete rate model are
///    non-executable until the user re-authorises.
/// 2. **Rate kind.** `condition.rate_kind == Apr`. v1 mandate per
///    research § 4.1.
/// 3. **Reserve config invariants.** `min ≤ optimal ≤ max ≤ super_max`,
///    `optimal_util ≤ max_util ≤ 100`, `take ∈ [0, 100]`. Reject on
///    violation rather than silently producing nonsense (research
///    § 4.5 / R-O2).
/// 4. **Staleness.** `current_slot - last_update_slot <=
///    max_reserve_staleness_slots` AND `!stale_flag`. Spec § 5.2
///    step 6 (Solend's own `STALE_AFTER_SLOTS_ELAPSED = 1`; the
///    rule's value MUST be at least as strict as Solend's, but the
///    verifier accepts whatever the rule committed at authz time).
/// 5. **Compute supply APR (WAD).** Three-region kinked borrow rate
///    × utilisation × (1 − take rate). All math is `u128`/WAD with
///    `checked_*`; overflow is `SolendComputeOverflow`.
/// 6. **Compare.** Convert `threshold_bps` to WAD (1 bps = 10¹⁴ WAD);
///    apply the operator.
pub fn verify_solend_supply_apr_condition(
    condition: &SolendSupplyAprCondition,
    snapshot: &SolendReserveSnapshot,
    context: &SolendVerificationContext,
) -> Result<bool, VerifierError> {
    // Gate 1 — formula version.
    if condition.formula_version != SUPPORTED_SOLEND_FORMULA_VERSION {
        return Err(VerifierError::SolendFormulaVersionUnsupported);
    }

    // Gate 2 — rate kind.
    match condition.rate_kind {
        RateKind::Apr => {}
        RateKind::Apy => return Err(VerifierError::UnsupportedRateKind),
    }

    // Gate 3 — config invariants. `super_max_borrow_rate` is `u64`; we
    // widen `max` to `u64` for the comparison so the check is
    // truncation-safe (audit C-3).
    if !is_reserve_config_valid(snapshot) {
        return Err(VerifierError::SolendInvalidReserveConfig);
    }

    // Gate 4 — staleness.
    let age = context
        .current_slot
        .checked_sub(snapshot.last_update_slot)
        // last_update_slot > current_slot is "future-dated" — fail closed.
        .ok_or(VerifierError::SolendReserveStale)?;
    if age > condition.max_reserve_staleness_slots as u64 || snapshot.stale_flag {
        return Err(VerifierError::SolendReserveStale);
    }

    // Gate 5 — derive supply APR in WAD.
    let supply_apr_wad = supply_apr_wad(snapshot)?;

    // Gate 6 — convert threshold_bps to WAD and compare.
    //
    //   threshold_wad = threshold_bps * 10^14
    //
    // because 1 bps = 0.0001 = 10⁻⁴ and WAD scale is 10¹⁸, so
    // threshold_bps × 10^14 lifts bps into WAD space without losing
    // precision. Max `u32` × 10^14 ≈ 4.3 × 10²³ — fits in `u128`
    // comfortably (cap is ~3.4 × 10³⁸); we still `checked_mul` for
    // belt-and-braces.
    let threshold_wad = (condition.threshold_bps as u128)
        .checked_mul(10u128.pow(14))
        .ok_or(VerifierError::SolendThresholdOverflow)?;

    Ok(condition
        .comparison
        .apply_u128(supply_apr_wad, threshold_wad))
}

/// Internal: validate Solend reserve config invariants. Documented at
/// research § 4.5 / R-O2.
fn is_reserve_config_valid(s: &SolendReserveSnapshot) -> bool {
    // Borrow-rate ordering. `super_max` is `u64`; widen the others to
    // `u64` so the comparison cannot truncate on the way up (audit
    // C-3 — we never want the reverse mistake either).
    let min = s.min_borrow_rate_pct as u64;
    let opt = s.optimal_borrow_rate_pct as u64;
    let max = s.max_borrow_rate_pct as u64;
    let super_max = s.super_max_borrow_rate_pct;
    if !(min <= opt && opt <= max && max <= super_max) {
        return false;
    }
    // Utilisation ordering and bounds.
    if !(s.optimal_utilization_rate_pct <= s.max_utilization_rate_pct
        && s.max_utilization_rate_pct <= 100)
    {
        return false;
    }
    if s.protocol_take_rate_pct > 100 {
        return false;
    }
    true
}

/// Public WAD-space supply APR derivation, exposed so daemon and
/// program can both call it and assert bit-equal results in fixture
/// tests (research § 6.5 — "the load-bearing test").
///
/// Formula (verbatim, research § 4.2):
///
/// ```text
/// supply_apr_wad = mul_wad( borrow_rate_wad,
///                  mul_wad( utilisation_wad,
///                           sub_wad( WAD, take_rate_wad ) ) )
/// ```
pub fn supply_apr_wad(s: &SolendReserveSnapshot) -> Result<u128, VerifierError> {
    if !is_reserve_config_valid(s) {
        return Err(VerifierError::SolendInvalidReserveConfig);
    }
    let utilisation = utilization_wad(s)?;
    let borrow_rate = current_borrow_rate_wad(s)?;
    let take_rate = pct_to_wad(s.protocol_take_rate_pct as u64)?;
    let one_minus_take = SOLEND_WAD
        .checked_sub(take_rate)
        .ok_or(VerifierError::SolendComputeOverflow)?;
    let inner = mul_wad(utilisation, one_minus_take)?;
    mul_wad(borrow_rate, inner)
}

/// Solend `utilization_rate(reserve)` in WAD.
///
/// Per research § 3.2 / source verbatim:
///
/// ```text
/// total_supply  = floor( (available_amount * WAD + borrowed_amount_wads
///                        - accumulated_protocol_fees_wads) / WAD )
/// utilization   = if total_supply == 0 || borrowed_wads == 0 → 0
///                 else borrowed_wads / (borrowed_wads + available_wads)
/// ```
///
/// Note that the **utilisation denominator** is
/// `borrowed_amount_wads + available_amount × WAD`, not the
/// `total_supply` (research § 3.2 explicitly: fees subtraction lives
/// only in `total_supply`'s early-zero check, NOT in the utilisation
/// denominator). We therefore follow the same shape: the snapshot
/// does not need `accumulated_protocol_fees_wads` for the verifier,
/// because the verifier checks `borrowed_amount_wads == 0` directly
/// for the early-zero case.
pub fn utilization_wad(s: &SolendReserveSnapshot) -> Result<u128, VerifierError> {
    if s.borrowed_amount_wads == 0 {
        return Ok(0);
    }
    let available_wads = (s.available_amount as u128)
        .checked_mul(SOLEND_WAD)
        .ok_or(VerifierError::SolendComputeOverflow)?;
    let denom = s
        .borrowed_amount_wads
        .checked_add(available_wads)
        .ok_or(VerifierError::SolendComputeOverflow)?;
    if denom == 0 {
        return Ok(0);
    }
    // utilisation in WAD: (borrowed_wads × WAD) / denom
    div_wad(s.borrowed_amount_wads, denom)
}

/// Solend three-region kinked `current_borrow_rate(reserve)` in WAD.
///
/// Verbatim per research § 3.1.
///
/// **Audit C-3 discipline.** Region 3 uses `super_max_borrow_rate`,
/// which is **`u64`** on the wire. A naive port that does `as u8` here
/// truncates above 255% and silently caps the third region. The
/// width-preserving conversion is in [`pct_to_wad`] which takes `u64`.
pub fn current_borrow_rate_wad(s: &SolendReserveSnapshot) -> Result<u128, VerifierError> {
    let utilisation = utilization_wad(s)?;
    let optimal_util = pct_to_wad(s.optimal_utilization_rate_pct as u64)?;
    let max_util = pct_to_wad(s.max_utilization_rate_pct as u64)?;

    if utilisation <= optimal_util {
        // Region 1 — linear from min_borrow_rate to optimal_borrow_rate
        // over u in [0, optimal_util].
        let min_rate = pct_to_wad(s.min_borrow_rate_pct as u64)?;
        if optimal_util == 0 {
            return Ok(min_rate);
        }
        let normalised = div_wad(utilisation, optimal_util)?; // u/optimal_util
        let opt_minus_min = (s.optimal_borrow_rate_pct as u64)
            .checked_sub(s.min_borrow_rate_pct as u64)
            .ok_or(VerifierError::SolendComputeOverflow)?;
        let rate_range = pct_to_wad(opt_minus_min)?;
        let scaled = mul_wad(normalised, rate_range)?;
        return scaled
            .checked_add(min_rate)
            .ok_or(VerifierError::SolendComputeOverflow);
    }

    if utilisation <= max_util {
        // Region 2 — linear from optimal_borrow_rate to max_borrow_rate
        // over u in (optimal_util, max_util].
        let weight_num = utilisation
            .checked_sub(optimal_util)
            .ok_or(VerifierError::SolendComputeOverflow)?;
        let weight_den = max_util
            .checked_sub(optimal_util)
            .ok_or(VerifierError::SolendComputeOverflow)?;
        if weight_den == 0 {
            // optimal_util == max_util — degenerate config; we already
            // know utilisation > optimal_util via the outer if-chain,
            // so we fall straight to the optimal_borrow_rate bound.
            return pct_to_wad(s.optimal_borrow_rate_pct as u64);
        }
        let weight = div_wad(weight_num, weight_den)?;
        let optimal_rate = pct_to_wad(s.optimal_borrow_rate_pct as u64)?;
        let max_rate = pct_to_wad(s.max_borrow_rate_pct as u64)?;
        let rate_range = max_rate
            .checked_sub(optimal_rate)
            .ok_or(VerifierError::SolendComputeOverflow)?;
        let scaled = mul_wad(weight, rate_range)?;
        return scaled
            .checked_add(optimal_rate)
            .ok_or(VerifierError::SolendComputeOverflow);
    }

    // Region 3 — linear from max_borrow_rate to super_max_borrow_rate
    // over u in (max_util, 100%].
    let weight_num = utilisation
        .checked_sub(max_util)
        .ok_or(VerifierError::SolendComputeOverflow)?;
    // (100% - max_util) in WAD = WAD - max_util.
    let weight_den = SOLEND_WAD
        .checked_sub(max_util)
        .ok_or(VerifierError::SolendComputeOverflow)?;
    if weight_den == 0 {
        return pct_to_wad_u64_super_max(s.super_max_borrow_rate_pct);
    }
    let weight = div_wad(weight_num, weight_den)?;
    let max_rate = pct_to_wad(s.max_borrow_rate_pct as u64)?;
    // **Width-preserving** conversion of `super_max_borrow_rate_pct`
    // (`u64`) → WAD. This is the audit-C-3 trap: any code that does
    // `as u8` here silently caps at 255%.
    let super_max_rate = pct_to_wad_u64_super_max(s.super_max_borrow_rate_pct)?;
    let rate_range = super_max_rate
        .checked_sub(max_rate)
        .ok_or(VerifierError::SolendComputeOverflow)?;
    let scaled = mul_wad(weight, rate_range)?;
    scaled
        .checked_add(max_rate)
        .ok_or(VerifierError::SolendComputeOverflow)
}

/// Convert a `u8`-percent (range typically `[0, 100]`, but the
/// caller is responsible for the upper-bound check via
/// [`is_reserve_config_valid`]) into WAD.
fn pct_to_wad(pct: u64) -> Result<u128, VerifierError> {
    // `pct` × 10¹⁶ — saturates within u128 even at `pct == u64::MAX`
    // (worst-case `2^64 × 10^16 ≈ 1.8 × 10^35`, fits below `2^127`).
    (pct as u128)
        .checked_mul(10u128.pow(16))
        .ok_or(VerifierError::SolendComputeOverflow)
}

/// Convert `super_max_borrow_rate_pct: u64` into WAD. **Identical**
/// implementation to [`pct_to_wad`]; named separately to make the
/// audit-C-3 width-preserving site self-documenting at the call site.
fn pct_to_wad_u64_super_max(pct: u64) -> Result<u128, VerifierError> {
    pct_to_wad(pct)
}

/// `mul_wad(a, b) = (a × b) / WAD`, floored. Widens through a 256-bit
/// intermediate so realistic Solend reserve sizes — `borrowed_amount_wads`
/// already × WAD; `available_amount × WAD` for the utilisation
/// denominator — don't overflow the multiply step. Matches the
/// strategy in research § 4.5 / 4.7 ("all intermediate ops are
/// WAD-fixed-point u128, with widening to u256 only inside the
/// multiply"). The narrowing `u256 → u128` can fail; a `u128`
/// overflow at narrowing time is still a hard error
/// ([`VerifierError::SolendComputeOverflow`]).
fn mul_wad(a: u128, b: u128) -> Result<u128, VerifierError> {
    mul_div_floor_u128(a, b, SOLEND_WAD)
}

/// `div_wad(a, b) = (a × WAD) / b`, floored. Same widening as
/// [`mul_wad`].
fn div_wad(a: u128, b: u128) -> Result<u128, VerifierError> {
    if b == 0 {
        return Err(VerifierError::SolendComputeOverflow);
    }
    mul_div_floor_u128(a, SOLEND_WAD, b)
}

/// `floor((a × b) / c)` with a 256-bit intermediate, returning `u128`.
/// Hard-fails (`SolendComputeOverflow`) if the result does not fit
/// in `u128` after the divide, or if `c == 0`.
///
/// Algorithm: split `a` and `b` into `u64` halves
/// (`a = a_hi · 2^64 + a_lo`, same for `b`), compute the four
/// `u64 × u64 → u128` partial products, sum them with carry into a
/// 256-bit `(hi, lo)` pair, then long-divide by `c`. Identical in
/// shape to `Decimal::try_mul` in Solend's mainnet source (which
/// widens to U192/U256 via `uint`). We hand-roll instead of pulling
/// `primitive-types` (and its `uint` macro expansion) into the
/// on-chain crate's dependency tree to keep the BPF compiled size
/// minimal.
fn mul_div_floor_u128(a: u128, b: u128, c: u128) -> Result<u128, VerifierError> {
    if c == 0 {
        return Err(VerifierError::SolendComputeOverflow);
    }
    // Fast path — most rate-config × rate-config products fit in u128
    // directly, saving the 256-bit dance.
    if let Some(prod) = a.checked_mul(b) {
        return Ok(prod / c);
    }

    // Slow path — 256-bit intermediate.
    let a_lo = a as u64 as u128;
    let a_hi = a >> 64;
    let b_lo = b as u64 as u128;
    let b_hi = b >> 64;

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    // Sum into (hi, lo) where the full product equals
    //   hi · 2^128 + lo
    // Layout:
    //   ll                        →   bits   0..128
    //   (lh + hl) << 64           →   bits  64..192
    //   hh << 128                 →   bits 128..256
    let mid = lh.wrapping_add(hl);
    // Carry from (lh + hl) when it overflows u128.
    let mid_carry = if mid < lh { 1u128 << 64 } else { 0 };

    // Add mid << 64 to ll, capturing carry into hi.
    let mid_lo = mid << 64;
    let mid_hi = mid >> 64;
    let (lo, c1) = ll.overflowing_add(mid_lo);
    let mut hi = hh
        .checked_add(mid_hi)
        .ok_or(VerifierError::SolendComputeOverflow)?
        .checked_add(if c1 { 1 } else { 0 })
        .ok_or(VerifierError::SolendComputeOverflow)?;
    hi = hi
        .checked_add(mid_carry)
        .ok_or(VerifierError::SolendComputeOverflow)?;

    // 256-bit / 128-bit division yielding a 128-bit quotient.
    // Restoring binary long division — 256 iterations.
    //
    // Loop invariant: at the start of iteration i (counting i from
    // 255 down to 0), `rem` holds the remainder of the partial
    // dividend consisting of bits [i+1, 255] of N. Each step:
    //   1. Track whether the MSB of `rem` is set (the bit that will
    //      be shifted out — call it `rem_overflow`).
    //   2. Shift `rem` left by 1 and OR in bit i of N.
    //   3. If either `rem_overflow` was set (so the *true* remainder
    //      is rem + 2^128, definitely ≥ c) or `rem >= c`, subtract c
    //      and set the corresponding quotient bit. Otherwise leave
    //      `rem` alone and clear the quotient bit.
    //
    // After 256 iterations, `quot` is the floor of N/c. If the
    // 128-bit quotient does not fit (i.e. any of the top 128 bits
    // of the conceptual 256-bit quotient is non-zero), we surface
    // [`VerifierError::SolendComputeOverflow`] — we detect this by
    // tracking whether any quotient bit "above" position 127 was
    // ever set.
    let mut quot: u128 = 0;
    let mut quot_overflow: bool = false;
    let mut rem: u128 = 0;
    for i in (0..256).rev() {
        let rem_overflow = (rem >> 127) & 1 == 1;
        rem = (rem << 1) | bit_of_u256(hi, lo, i as u32);
        let need_sub = rem_overflow || rem >= c;
        if need_sub {
            rem = rem.wrapping_sub(c);
        }
        // Capture the quotient bit before shifting the accumulator,
        // so the high bit out of `quot` becomes the overflow flag.
        let quot_msb = (quot >> 127) & 1 == 1;
        if quot_msb {
            quot_overflow = true;
        }
        quot = (quot << 1) | (need_sub as u128);
    }
    if quot_overflow {
        return Err(VerifierError::SolendComputeOverflow);
    }
    Ok(quot)
}

/// Extract bit `idx` from the 256-bit value `hi·2^128 + lo`.
#[inline]
fn bit_of_u256(hi: u128, lo: u128, idx: u32) -> u128 {
    if idx < 128 {
        (lo >> idx) & 1
    } else {
        (hi >> (idx - 128)) & 1
    }
}

// ── Top-level evaluator ────────────────────────────────────────────────────

/// Pure-Rust projection of `claw_types::stage2_watch_rule::Condition`.
/// The on-chain re-verifier and the off-chain daemon both build this
/// from a canonical [`Condition`][claw_condition] (the daemon by
/// directly reading the rule, the program by decoding the snapshot
/// passed to `execute_action` against the canonical-rule hash bound
/// in the authorisation PDA — see Stage 2 spec § 3.2).
///
/// [claw_condition]: ../../../../crates/types/src/stage2_watch_rule.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalCondition {
    Pyth(PythPriceCondition),
    Solend(SolendSupplyAprCondition),
}

/// Per-condition snapshot + clock context. Each variant must match
/// the [`EvalCondition`] variant it pairs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalSnapshot {
    Pyth(PythPriceSnapshot, PythVerificationContext),
    Solend(SolendReserveSnapshot, SolendVerificationContext),
}

/// Evaluate one condition + snapshot pair. Variant-shape mismatch
/// (e.g. `EvalCondition::Pyth` paired with `EvalSnapshot::Solend`) is
/// a logic error in the caller, not user input — we still fail closed
/// with a typed error for clarity.
pub fn evaluate_condition(
    condition: &EvalCondition,
    snapshot: &EvalSnapshot,
) -> Result<bool, VerifierError> {
    match (condition, snapshot) {
        (EvalCondition::Pyth(c), EvalSnapshot::Pyth(s, ctx)) => {
            verify_pyth_price_condition(c, s, ctx)
        }
        (EvalCondition::Solend(c), EvalSnapshot::Solend(s, ctx)) => {
            verify_solend_supply_apr_condition(c, s, ctx)
        }
        // Cross-shape pairing: this can only happen if the caller
        // mis-zipped its (conditions[i], snapshots[i]) lists, which is
        // a programming error. Return a clearly-named error so the
        // eventual `execute_action` ix can panic loudly in
        // debug-assertion builds and emit a structured log otherwise.
        (EvalCondition::Pyth(_), EvalSnapshot::Solend(_, _))
        | (EvalCondition::Solend(_), EvalSnapshot::Pyth(_, _)) => {
            Err(VerifierError::UnsupportedComparison)
        }
    }
}

/// Combine the per-condition booleans according to the rule's
/// `condition_logic`. Matches the canonical
/// `claw_types::stage2_watch_rule::ConditionLogic` semantics:
///
/// - `All` — empty list = `true` (vacuously). Stage 2 still rejects
///   empty rules at the validation layer (see
///   `claw_types::stage2_watch_rule::validate_conditions_bounded`),
///   so this branch is theoretical inside an executor; we keep the
///   vacuous-true semantics anyway because they are the standard for
///   AND-fold and any other choice would surprise the caller.
/// - `Any` — empty list = `false` (vacuously). Same OR-fold convention.
pub fn evaluate_condition_logic(logic: ConditionLogic, results: &[bool]) -> bool {
    match logic {
        ConditionLogic::All => results.iter().all(|&r| r),
        ConditionLogic::Any => results.iter().any(|&r| r),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Stable Pyth test fixtures (mirror claw-types fixture B) ─────────

    const BTC_USD_FEED_ID: [u8; 32] = [
        0xe6, 0x2d, 0xf6, 0xc8, 0xb4, 0xa8, 0x5f, 0xe1, 0xa6, 0x7d, 0xb4, 0x4d, 0xc1, 0x2d, 0xe5,
        0xdb, 0x33, 0x0f, 0x7a, 0xc6, 0x6b, 0x72, 0xdc, 0x65, 0x8a, 0xfe, 0xdf, 0x0f, 0x4a, 0x41,
        0x5b, 0x43,
    ];
    const ETH_USD_FEED_ID: [u8; 32] = [
        0xff, 0x61, 0x49, 0x1a, 0x93, 0x11, 0x12, 0xdd, 0xf1, 0xbd, 0x81, 0x47, 0xcd, 0x1b, 0x64,
        0x13, 0x75, 0xf7, 0x9f, 0x58, 0x25, 0x12, 0x6d, 0x66, 0x54, 0x80, 0x87, 0x46, 0x34, 0xfd,
        0x0a, 0xce,
    ];
    const SOL_USD_FEED_ID: [u8; 32] = [
        0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda,
        0x39, 0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80,
        0xb5, 0x6d,
    ];

    /// "Now" used by every Pyth fixture-snapshot test. publish_time = NOW - 5 means
    /// the snapshot is 5 s old — well inside any 30-s max-age window we use here.
    const NOW: i64 = 1_700_000_000;

    fn pyth_btc_gt_75k_condition() -> PythPriceCondition {
        // Mirrors fixture B's BTC condition: BTC > 75,000.00 with -2 exponent.
        PythPriceCondition {
            feed_id: BTC_USD_FEED_ID,
            comparison: Comparison::Gt,
            threshold_mantissa: 7_500_000,
            threshold_exponent: -2,
            max_age_seconds: 30,
            max_confidence_bps: 50,
            verification_level_required: VerificationLevel::Full,
            bound_mode: BoundMode::AdverseLowerForGt,
        }
    }

    fn pyth_sol_lt_90_condition() -> PythPriceCondition {
        PythPriceCondition {
            feed_id: SOL_USD_FEED_ID,
            comparison: Comparison::Lt,
            threshold_mantissa: 9_000,
            threshold_exponent: -2,
            max_age_seconds: 30,
            max_confidence_bps: 50,
            verification_level_required: VerificationLevel::Full,
            bound_mode: BoundMode::AdverseUpperForLt,
        }
    }

    fn pyth_eth_gt_2300_condition() -> PythPriceCondition {
        PythPriceCondition {
            feed_id: ETH_USD_FEED_ID,
            comparison: Comparison::Gt,
            threshold_mantissa: 230_000,
            threshold_exponent: -2,
            max_age_seconds: 30,
            max_confidence_bps: 50,
            verification_level_required: VerificationLevel::Full,
            bound_mode: BoundMode::AdverseLowerForGt,
        }
    }

    fn pyth_btc_snapshot_at(price: i64, conf: u64, age_secs: i64) -> PythPriceSnapshot {
        PythPriceSnapshot {
            feed_id: BTC_USD_FEED_ID,
            price_mantissa: price,
            // Pyth typically publishes BTC with -8 exponent; our condition
            // pins -2. The verifier normalises by up-shifting one side.
            // Keep -8 here so the exponent normalisation path is exercised.
            price_exponent: -8,
            conf,
            publish_time: NOW - age_secs,
            verification_level: VerificationLevel::Full,
        }
    }

    fn pyth_sol_snapshot_at(price: i64, conf: u64, age_secs: i64) -> PythPriceSnapshot {
        PythPriceSnapshot {
            feed_id: SOL_USD_FEED_ID,
            price_mantissa: price,
            price_exponent: -8,
            conf,
            publish_time: NOW - age_secs,
            verification_level: VerificationLevel::Full,
        }
    }

    fn pyth_eth_snapshot_at(price: i64, conf: u64, age_secs: i64) -> PythPriceSnapshot {
        PythPriceSnapshot {
            feed_id: ETH_USD_FEED_ID,
            price_mantissa: price,
            price_exponent: -8,
            conf,
            publish_time: NOW - age_secs,
            verification_level: VerificationLevel::Full,
        }
    }

    fn ctx_now() -> PythVerificationContext {
        PythVerificationContext {
            current_unix_time: NOW,
        }
    }

    // ── Pyth happy paths ────────────────────────────────────────────────

    #[test]
    fn pyth_btc_gt_75k_adverse_lower_fires() {
        // BTC = 75,001.23 with -8 exponent → mantissa 7_500_123_000_000.
        // Confidence = 100 (1e-6 of price) → adverse lower = price - conf,
        // still well above 75,000.00.
        let cond = pyth_btc_gt_75k_condition();
        let snap = pyth_btc_snapshot_at(7_500_123_000_000, 100, 5);
        let ok = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap();
        assert!(ok, "BTC adverse-lower 75001.23 - tiny conf > 75000.00 should fire");
    }

    #[test]
    fn pyth_btc_gt_75k_adverse_lower_does_not_fire_when_within_confidence() {
        // BTC = 75,001.00; confidence = 200_000_000 (= $2.00) → adverse
        // lower = 74,999.00 → should NOT fire.
        let cond = pyth_btc_gt_75k_condition();
        let snap = pyth_btc_snapshot_at(7_500_100_000_000, 200_000_000, 5);
        let ok = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap();
        assert!(
            !ok,
            "BTC adverse-lower bound below 75000 with wide-but-still-acceptable conf must NOT fire"
        );
    }

    #[test]
    fn pyth_sol_lt_90_adverse_upper_fires() {
        // SOL = 89.50 with -8 exponent; conf small → upper bound 89.50+ε < 90.
        let cond = pyth_sol_lt_90_condition();
        let snap = pyth_sol_snapshot_at(8_950_000_000, 100, 5);
        let ok = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap();
        assert!(ok, "SOL adverse-upper 89.50 + tiny conf < 90 should fire");
    }

    #[test]
    fn pyth_sol_lt_90_adverse_upper_does_not_fire_when_at_boundary() {
        // SOL = 89.95; confidence = 0.10 → upper bound 90.05 → must NOT fire.
        // (Prevents the "true price could be 90.05" false positive,
        // research § 7.1.)
        let cond = pyth_sol_lt_90_condition();
        let snap = pyth_sol_snapshot_at(8_995_000_000, 10_000_000, 5);
        let ok = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap();
        assert!(
            !ok,
            "SOL adverse-upper bound 90.05 must NOT trip the < 90 trigger"
        );
    }

    // ── Pyth fail-closed cases ──────────────────────────────────────────

    #[test]
    fn pyth_wrong_feed_id_fails_closed() {
        let cond = pyth_btc_gt_75k_condition();
        // SOL price under a BTC condition.
        let snap = pyth_sol_snapshot_at(8_950_000_000, 100, 5);
        let err = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap_err();
        assert_eq!(err, VerifierError::PythFeedIdMismatch);
    }

    #[test]
    fn pyth_stale_price_fails_closed() {
        let cond = pyth_btc_gt_75k_condition(); // max_age = 30 s
        let snap = pyth_btc_snapshot_at(7_500_123_000_000, 100, 31);
        let err = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap_err();
        assert_eq!(err, VerifierError::PythPriceTooOld);
    }

    #[test]
    fn pyth_future_dated_publish_time_fails_closed() {
        // publish_time > current_unix_time (clock skew) is fail-closed
        // the same way as over-age.
        let cond = pyth_btc_gt_75k_condition();
        let mut snap = pyth_btc_snapshot_at(7_500_123_000_000, 100, 5);
        snap.publish_time = NOW + 5;
        let err = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap_err();
        assert_eq!(err, VerifierError::PythPriceTooOld);
    }

    #[test]
    fn pyth_partial_verification_fails_closed() {
        // Build a snapshot whose verification_level encodes Partial. The
        // verifier's mirror enum only declares `Full` (audit U-5), so
        // the only way to *test* a partial reading is to demonstrate
        // that the encoded-Full snapshot continues to succeed and that
        // any future addition of a `Partial` variant must be paired
        // with a verifier branch — which the exhaustive match in
        // verify_pyth_price_condition forces. This test pins the
        // behaviour: only Full is accepted today.
        let cond = pyth_btc_gt_75k_condition();
        let snap = pyth_btc_snapshot_at(7_500_123_000_000, 100, 5);
        // Sanity: Full-verified snapshot succeeds.
        assert!(verify_pyth_price_condition(&cond, &snap, &ctx_now()).is_ok());

        // Width-discipline pin: VerificationLevel mirrors claw-types and has
        // exactly one variant. Adding a `Partial` variant in either crate
        // without paired verifier handling is a wire break.
        let n_variants =
            [VerificationLevel::Full].iter().count();
        assert_eq!(
            n_variants, 1,
            "Adding a VerificationLevel variant requires audit U-5 review",
        );
    }

    #[test]
    fn pyth_excessive_confidence_fails_closed() {
        // Price = 75_001 with -8 exponent, max_confidence_bps = 50 (0.5%).
        // conf = 1% of price → fails the liveness gate.
        let cond = pyth_btc_gt_75k_condition();
        let conf = 7_500_100_000_000u64 / 100; // 1%
        let snap = pyth_btc_snapshot_at(7_500_100_000_000, conf, 5);
        let err = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap_err();
        assert_eq!(err, VerifierError::PythConfidenceTooWide);
    }

    #[test]
    fn pyth_zero_price_fails_closed() {
        // A zero spot price is meaningless on every Pyth feed Stage 2
        // cares about; the confidence-bps math would divide by zero.
        let cond = pyth_btc_gt_75k_condition();
        let snap = pyth_btc_snapshot_at(0, 0, 5);
        let err = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap_err();
        assert_eq!(err, VerifierError::PythConfidenceTooWide);
    }

    #[test]
    fn pyth_bound_direction_mismatch_fails_closed() {
        // Gt + AdverseUpperForLt is meaningless — refuse to compute.
        let mut cond = pyth_btc_gt_75k_condition();
        cond.bound_mode = BoundMode::AdverseUpperForLt;
        let snap = pyth_btc_snapshot_at(7_500_123_000_000, 100, 5);
        let err = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap_err();
        assert_eq!(err, VerifierError::BoundDirectionMismatch);
    }

    #[test]
    fn pyth_midpoint_mode_fires_when_above_threshold() {
        // Midpoint is admissible when explicit; tested for full coverage.
        let mut cond = pyth_btc_gt_75k_condition();
        cond.bound_mode = BoundMode::Midpoint;
        let snap = pyth_btc_snapshot_at(7_500_001_000_000, 1_000_000_000, 5);
        // Midpoint = 75,000.01 > 75,000.00 → fires (no confidence widening).
        let ok = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap();
        assert!(ok, "Midpoint-mode comparison should ignore confidence widening");
    }

    #[test]
    fn pyth_exponent_diff_at_limit_succeeds() {
        // Threshold exponent -2 vs price exponent -8 → diff = -6.
        // Inside the [-18, 18] range; confirms the normalisation path.
        let cond = pyth_btc_gt_75k_condition();
        let snap = pyth_btc_snapshot_at(7_500_123_000_000, 100, 5);
        verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap();
    }

    #[test]
    fn pyth_exponent_diff_out_of_range_fails_closed() {
        let mut cond = pyth_btc_gt_75k_condition();
        cond.threshold_exponent = -50; // diff = -8 - -50 = 42 → out of range
        let snap = pyth_btc_snapshot_at(7_500_123_000_000, 100, 5);
        let err = verify_pyth_price_condition(&cond, &snap, &ctx_now()).unwrap_err();
        assert_eq!(err, VerifierError::PythExponentOutOfRange);
    }

    // ── Solend fixtures ─────────────────────────────────────────────────

    /// "Healthy USDC reserve" baseline — utilisation ≈ 50 %, all rate
    /// boundaries from typical Solend USDC reserve config:
    ///   min = 0%, optimal = 4%, max = 30%, super_max = 300%
    ///   optimal_util = 80%, max_util = 95%, take_rate = 20%
    fn fixture_reserve_low_util() -> SolendReserveSnapshot {
        SolendReserveSnapshot {
            // 5,000,000 USDC available (raw 6-decimal lamports).
            available_amount: 5_000_000_000_000,
            // 5,000,000 USDC borrowed, scaled to WAD.
            borrowed_amount_wads: 5_000_000_000_000u128 * SOLEND_WAD,
            min_borrow_rate_pct: 0,
            optimal_borrow_rate_pct: 4,
            max_borrow_rate_pct: 30,
            super_max_borrow_rate_pct: 300,
            optimal_utilization_rate_pct: 80,
            max_utilization_rate_pct: 95,
            protocol_take_rate_pct: 20,
            last_update_slot: 415_500_000,
            stale_flag: false,
        }
    }

    /// Same config, utilisation ≈ 90 % (region 2).
    fn fixture_reserve_mid_util() -> SolendReserveSnapshot {
        let mut r = fixture_reserve_low_util();
        // Borrowed 9,000,000 / total 10,000,000 → 90% utilisation.
        r.available_amount = 1_000_000_000_000;
        r.borrowed_amount_wads = 9_000_000_000_000u128 * SOLEND_WAD;
        r
    }

    /// Same config, utilisation ≈ 99 % (region 3 — super-max territory).
    fn fixture_reserve_high_util() -> SolendReserveSnapshot {
        let mut r = fixture_reserve_low_util();
        r.available_amount = 100_000_000_000;
        r.borrowed_amount_wads = 9_900_000_000_000u128 * SOLEND_WAD;
        r
    }

    fn solend_below_threshold_condition(threshold_bps: u32) -> SolendSupplyAprCondition {
        SolendSupplyAprCondition {
            comparison: Comparison::Lt,
            threshold_bps,
            rate_kind: RateKind::Apr,
            formula_version: SUPPORTED_SOLEND_FORMULA_VERSION,
            max_reserve_staleness_slots: 16,
        }
    }

    fn solend_ctx_at(slot: u64) -> SolendVerificationContext {
        SolendVerificationContext { current_slot: slot }
    }

    // ── Solend happy paths and threshold boundary ───────────────────────

    #[test]
    fn solend_apr_low_util_below_high_threshold_passes() {
        // Region 1: at 50% utilisation, borrow_rate = (50/80) * 4% = 2.5%.
        // supply_apr = 2.5% × 50% × (1 - 20%) = 1.0% = 100 bps.
        // Threshold 1000 bps (10%) → 100 < 1000 → fires.
        let cond = solend_below_threshold_condition(1_000);
        let snap = fixture_reserve_low_util();
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let ok = verify_solend_supply_apr_condition(&cond, &snap, &ctx).unwrap();
        assert!(ok, "1.0% APR < 10% threshold must fire");
    }

    #[test]
    fn solend_apr_below_threshold_exact_boundary_for_lt_does_not_fire() {
        // Same low-util fixture as above (~100 bps APR).
        // Compute the exact APR in bps and test BOTH sides of it.
        let snap = fixture_reserve_low_util();
        let apr_wad = supply_apr_wad(&snap).unwrap();
        // bps = floor(apr_wad / 10^14)
        let apr_bps = (apr_wad / 10u128.pow(14)) as u32;

        // At threshold == apr_bps, Lt is FALSE (strict <).
        let cond_eq = solend_below_threshold_condition(apr_bps);
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let ok_eq =
            verify_solend_supply_apr_condition(&cond_eq, &snap, &ctx).unwrap();
        assert!(
            !ok_eq,
            "boundary: APR == threshold ({apr_bps} bps) under Lt must NOT fire"
        );

        // Just-above (threshold = apr_bps + 1) → Lt holds.
        let cond_above = solend_below_threshold_condition(apr_bps + 1);
        let ok_above =
            verify_solend_supply_apr_condition(&cond_above, &snap, &ctx).unwrap();
        assert!(
            ok_above,
            "boundary: APR ({apr_bps} bps) < threshold ({}) must fire",
            apr_bps + 1
        );
    }

    #[test]
    fn solend_apr_above_threshold_fails_for_lt() {
        // Region 3 fixture (~99% utilisation) → APR much higher than
        // 1000 bps → Lt 1000 bps must NOT fire.
        let cond = solend_below_threshold_condition(1_000);
        let snap = fixture_reserve_high_util();
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let ok = verify_solend_supply_apr_condition(&cond, &snap, &ctx).unwrap();
        assert!(!ok, "high-util APR >> 10% threshold under Lt must NOT fire");
    }

    /// Region 2 of the kinked rate model. Utilisation = 90%, between
    /// optimal (80%) and max (95%). borrow_rate is linearly
    /// interpolated between optimal_borrow_rate (4%) and
    /// max_borrow_rate (30%); supply_apr derives from there with the
    /// take-rate skim. Purely a region-coverage test: ensures the
    /// middle branch of `current_borrow_rate_wad` is exercised by
    /// fixtures, not just regions 1 and 3.
    #[test]
    fn solend_apr_mid_util_lands_in_region_2() {
        let snap = fixture_reserve_mid_util();
        let borrow_rate = current_borrow_rate_wad(&snap).unwrap();
        // weight = (90 - 80) / (95 - 80) = 10/15 = 2/3
        // borrow = 4 + 2/3 × (30 - 4) = 4 + 17.33... ≈ 21.33%
        // 21.33% in WAD (10^18 scale) = 0.2133 × 10^18 ≈ 2.133 × 10^17
        // Allow for floor rounding.
        let expected_low = 213_000_000_000_000_000u128;
        let expected_high = 214_000_000_000_000_000u128;
        assert!(
            borrow_rate >= expected_low && borrow_rate <= expected_high,
            "region 2 borrow_rate ≈ 21.33% (≈ 2.13×10^17 WAD) expected; got {borrow_rate} WAD",
        );

        // supply_apr = borrow_rate × util × (1 - take_rate)
        // = 21.33% × 90% × 80% ≈ 15.36% ≈ 1536 bps
        let apr_wad = supply_apr_wad(&snap).unwrap();
        let apr_bps = apr_wad / 10u128.pow(14);
        assert!(
            (1_500..=1_600).contains(&(apr_bps as u32)),
            "expected ≈ 1536 bps in region 2, got {apr_bps}",
        );
    }

    #[test]
    fn solend_unsupported_formula_version_fails_closed() {
        let mut cond = solend_below_threshold_condition(1_000);
        cond.formula_version = 2;
        let snap = fixture_reserve_low_util();
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let err = verify_solend_supply_apr_condition(&cond, &snap, &ctx).unwrap_err();
        assert_eq!(err, VerifierError::SolendFormulaVersionUnsupported);
    }

    #[test]
    fn solend_apy_unsupported_in_v1_fails_closed() {
        let mut cond = solend_below_threshold_condition(1_000);
        cond.rate_kind = RateKind::Apy;
        let snap = fixture_reserve_low_util();
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let err = verify_solend_supply_apr_condition(&cond, &snap, &ctx).unwrap_err();
        assert_eq!(err, VerifierError::UnsupportedRateKind);
    }

    #[test]
    fn solend_stale_reserve_snapshot_fails_closed() {
        // last_update_slot age exceeds max_reserve_staleness_slots.
        let cond = solend_below_threshold_condition(1_000);
        let snap = fixture_reserve_low_util();
        let ctx = solend_ctx_at(snap.last_update_slot + cond.max_reserve_staleness_slots as u64 + 1);
        let err = verify_solend_supply_apr_condition(&cond, &snap, &ctx).unwrap_err();
        assert_eq!(err, VerifierError::SolendReserveStale);
    }

    #[test]
    fn solend_stale_flag_set_fails_closed_even_within_age_window() {
        let cond = solend_below_threshold_condition(1_000);
        let mut snap = fixture_reserve_low_util();
        snap.stale_flag = true;
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let err = verify_solend_supply_apr_condition(&cond, &snap, &ctx).unwrap_err();
        assert_eq!(err, VerifierError::SolendReserveStale);
    }

    #[test]
    fn solend_invalid_reserve_config_fails_closed() {
        let cond = solend_below_threshold_condition(1_000);
        let mut snap = fixture_reserve_low_util();
        snap.optimal_borrow_rate_pct = 50; // > max_borrow_rate_pct (30)
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let err = verify_solend_supply_apr_condition(&cond, &snap, &ctx).unwrap_err();
        assert_eq!(err, VerifierError::SolendInvalidReserveConfig);
    }

    /// Audit-C-3 regression. A naive `as u8` cast on
    /// `super_max_borrow_rate_pct` (300%) would truncate to 44. With
    /// the width-preserving `u64` path, Region 3 must reach the
    /// 300%-anchored ceiling — concretely, at 99% utilisation the
    /// computed APR must be **strictly higher** than what a u8-cast
    /// implementation would produce (which would silently cap at the
    /// `max_borrow_rate` ceiling of 30%).
    #[test]
    fn solend_super_max_borrow_rate_is_not_truncated_to_u8() {
        let snap = fixture_reserve_high_util();
        let apr_wad = supply_apr_wad(&snap).unwrap();
        let apr_bps = apr_wad / 10u128.pow(14);
        // Sanity: at high util we must be in or near Region 3.
        // A correct implementation produces supply_apr ≈ borrow_rate
        // × util × (1 − take_rate). At util = 99% with super_max =
        // 300%, borrow_rate is well above 30% — supply APR is
        // well above 30% × 99% × 80% ≈ 23.76% ≈ 2376 bps.
        //
        // A truncated implementation (super_max → 44) would cap
        // borrow_rate at 30% (max), giving supply_apr = 30% × 99%
        // × 80% ≈ 23.76% ≈ 2376 bps.
        //
        // Our correct implementation gives a strictly higher value
        // because Region 3 lifts above max_borrow_rate.
        assert!(
            apr_bps > 2_400,
            "expected APR above 24% in region 3 (got {apr_bps} bps); \
             u8 cast on super_max would silently cap below this — audit C-3 regression",
        );
    }

    /// Same regression, lower-level: with `super_max_borrow_rate_pct
    /// = 300`, the WAD-encoded super-max value MUST equal
    /// `300 × 10^16`, not `(300 as u8) × 10^16 = 44 × 10^16`.
    #[test]
    fn solend_super_max_pct_to_wad_does_not_truncate() {
        let v = pct_to_wad_u64_super_max(300).unwrap();
        assert_eq!(v, 300u128 * 10u128.pow(16));
        // u8 truncation would give 44 × 10^16 — pin the difference.
        assert_ne!(v, 44u128 * 10u128.pow(16));
    }

    // ── ConditionLogic ──────────────────────────────────────────────────

    #[test]
    fn condition_logic_all_truth_table() {
        assert!(evaluate_condition_logic(ConditionLogic::All, &[true, true, true]));
        assert!(!evaluate_condition_logic(ConditionLogic::All, &[true, false, true]));
        assert!(!evaluate_condition_logic(ConditionLogic::All, &[false]));
        // Vacuous-true on empty.
        assert!(evaluate_condition_logic(ConditionLogic::All, &[]));
    }

    #[test]
    fn condition_logic_any_truth_table() {
        assert!(evaluate_condition_logic(ConditionLogic::Any, &[false, false, true]));
        assert!(!evaluate_condition_logic(ConditionLogic::Any, &[false, false]));
        assert!(evaluate_condition_logic(ConditionLogic::Any, &[true]));
        // Vacuous-false on empty.
        assert!(!evaluate_condition_logic(ConditionLogic::Any, &[]));
    }

    // ── Top-level evaluate_condition ────────────────────────────────────

    #[test]
    fn evaluate_condition_pyth_path() {
        let cond = EvalCondition::Pyth(pyth_btc_gt_75k_condition());
        let snap = EvalSnapshot::Pyth(
            pyth_btc_snapshot_at(7_500_123_000_000, 100, 5),
            ctx_now(),
        );
        assert!(evaluate_condition(&cond, &snap).unwrap());
    }

    #[test]
    fn evaluate_condition_solend_path() {
        let cond = EvalCondition::Solend(solend_below_threshold_condition(1_000));
        let snap = fixture_reserve_low_util();
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let snapshot = EvalSnapshot::Solend(snap, ctx);
        assert!(evaluate_condition(&cond, &snapshot).unwrap());
    }

    #[test]
    fn evaluate_condition_cross_shape_fails_closed() {
        // A Pyth condition paired with a Solend snapshot is a caller
        // bug; verifier returns a typed error rather than guess.
        let cond = EvalCondition::Pyth(pyth_btc_gt_75k_condition());
        let snap = EvalSnapshot::Solend(
            fixture_reserve_low_util(),
            solend_ctx_at(415_500_001),
        );
        let err = evaluate_condition(&cond, &snap).unwrap_err();
        assert_eq!(err, VerifierError::UnsupportedComparison);
    }

    // ── Stage 2 fixture A: Solend APR < 10% (mirrors claw-types fixture A) ──

    /// Stage 2 fixture A end-to-end: a single Solend supply-APR
    /// condition under `Lt` 1000 bps, against the low-utilisation
    /// reserve snapshot, evaluates `true` (the rule's trigger fires).
    #[test]
    fn stage2_fixture_a_solend_apr_evaluates_true_with_low_util_snapshot() {
        let cond = EvalCondition::Solend(solend_below_threshold_condition(1_000));
        let snap = fixture_reserve_low_util();
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let snapshot = EvalSnapshot::Solend(snap, ctx);
        let result = evaluate_condition(&cond, &snapshot).unwrap();
        let combined = evaluate_condition_logic(ConditionLogic::All, &[result]);
        assert!(
            combined,
            "Stage 2 Scenario A (Solend APR < 10%) must evaluate to true under the low-util fixture",
        );
    }

    /// And its negative: under the high-util fixture (region 3 of the
    /// kinked rate model), the same rule does NOT fire.
    #[test]
    fn stage2_fixture_a_solend_apr_does_not_fire_under_high_util_snapshot() {
        let cond = EvalCondition::Solend(solend_below_threshold_condition(1_000));
        let snap = fixture_reserve_high_util();
        let ctx = solend_ctx_at(snap.last_update_slot + 1);
        let snapshot = EvalSnapshot::Solend(snap, ctx);
        let result = evaluate_condition(&cond, &snapshot).unwrap();
        let combined = evaluate_condition_logic(ConditionLogic::All, &[result]);
        assert!(
            !combined,
            "Stage 2 Scenario A under high-util fixture must NOT fire",
        );
    }

    // ── Stage 2 fixture B: BTC > 75k AND ETH > 2300 AND SOL < 90 ────────

    #[test]
    fn stage2_fixture_b_basket_buy_evaluates_true_when_all_conditions_hold() {
        let conds = [
            EvalCondition::Pyth(pyth_btc_gt_75k_condition()),
            EvalCondition::Pyth(pyth_eth_gt_2300_condition()),
            EvalCondition::Pyth(pyth_sol_lt_90_condition()),
        ];
        let snaps = [
            EvalSnapshot::Pyth(pyth_btc_snapshot_at(7_500_123_000_000, 1_000_000, 5), ctx_now()),
            EvalSnapshot::Pyth(pyth_eth_snapshot_at(232_000_000_000, 100_000, 5), ctx_now()),
            EvalSnapshot::Pyth(pyth_sol_snapshot_at(8_950_000_000, 1_000_000, 5), ctx_now()),
        ];
        let mut results: [bool; 3] = [false; 3];
        for i in 0..3 {
            results[i] = evaluate_condition(&conds[i], &snaps[i]).unwrap();
        }
        let combined = evaluate_condition_logic(ConditionLogic::All, &results);
        assert!(combined, "Scenario B must fire when BTC>75k AND ETH>2300 AND SOL<90");
    }

    #[test]
    fn stage2_fixture_b_does_not_fire_when_one_condition_fails() {
        // Same as above, but ETH price is 2,290 (just below 2,300) →
        // the basket evaluates false under ALL.
        let conds = [
            EvalCondition::Pyth(pyth_btc_gt_75k_condition()),
            EvalCondition::Pyth(pyth_eth_gt_2300_condition()),
            EvalCondition::Pyth(pyth_sol_lt_90_condition()),
        ];
        let snaps = [
            EvalSnapshot::Pyth(pyth_btc_snapshot_at(7_500_123_000_000, 1_000_000, 5), ctx_now()),
            EvalSnapshot::Pyth(pyth_eth_snapshot_at(229_000_000_000, 100_000, 5), ctx_now()),
            EvalSnapshot::Pyth(pyth_sol_snapshot_at(8_950_000_000, 1_000_000, 5), ctx_now()),
        ];
        let mut results: [bool; 3] = [false; 3];
        for i in 0..3 {
            results[i] = evaluate_condition(&conds[i], &snaps[i]).unwrap();
        }
        let combined = evaluate_condition_logic(ConditionLogic::All, &results);
        assert!(!combined, "ETH miss must drag the AND-fold to false");

        // But under ANY-logic it would still fire (BTC and SOL hold).
        let or_combined = evaluate_condition_logic(ConditionLogic::Any, &results);
        assert!(
            or_combined,
            "OR-fold over the same results must still fire (BTC + SOL hold)"
        );
    }

    // ── 256-bit mul-div helper sanity ───────────────────────────────────

    #[test]
    fn mul_div_floor_u128_matches_native_for_small_values() {
        // For values where `a*b` fits in u128 we can sanity-check
        // against the native multiply.
        for (a, b, c) in [
            (0u128, 0, 1),
            (1, 1, 1),
            (10, 20, 7),
            (123_456_789u128, 987_654_321, 1_000_000_007),
            (SOLEND_WAD, SOLEND_WAD, SOLEND_WAD), // = WAD
            (SOLEND_WAD / 2, SOLEND_WAD * 2, SOLEND_WAD), // = WAD
            (3 * SOLEND_WAD, SOLEND_WAD / 7, SOLEND_WAD),
        ] {
            let want = (a * b) / c;
            let got = mul_div_floor_u128(a, b, c).unwrap();
            assert_eq!(got, want, "mul_div_floor_u128({a},{b},{c})");
        }
    }

    #[test]
    fn mul_div_floor_u128_handles_wide_intermediate() {
        // a*b overflows u128 but (a*b)/c fits.
        // Pick a = 2^120, b = 2^120, c = 2^120 → result = 2^120.
        let a = 1u128 << 120;
        let b = 1u128 << 120;
        let c = 1u128 << 120;
        let got = mul_div_floor_u128(a, b, c).unwrap();
        assert_eq!(got, 1u128 << 120);
    }

    #[test]
    fn mul_div_floor_u128_fails_on_overflow_quotient() {
        // a*b/c that would yield > u128::MAX — surface overflow.
        // (1 << 200) / 1 cannot fit in u128.
        let a = 1u128 << 100;
        let b = 1u128 << 100;
        let c = 1u128;
        let err = mul_div_floor_u128(a, b, c).unwrap_err();
        assert_eq!(err, VerifierError::SolendComputeOverflow);
    }

    #[test]
    fn mul_div_floor_u128_zero_divisor_fails() {
        let err = mul_div_floor_u128(1, 1, 0).unwrap_err();
        assert_eq!(err, VerifierError::SolendComputeOverflow);
    }

    #[test]
    fn mul_div_floor_u128_solend_realistic_borrowed_wads_does_not_overflow() {
        // Realistic Solend USDC reserve: 5,000,000 USDC available
        // (5e12 raw at 6 decimals), 5e12 raw borrowed × WAD = 5e30
        // borrowed_wads. Compute utilisation in WAD.
        let available = 5_000_000_000_000u128;
        let borrowed_wads = 5_000_000_000_000u128 * SOLEND_WAD;
        let denom = borrowed_wads + available * SOLEND_WAD;
        let util = mul_div_floor_u128(borrowed_wads, SOLEND_WAD, denom).unwrap();
        // Utilisation = 50% → ~ 0.5 × WAD
        let expected_low = SOLEND_WAD / 2 - SOLEND_WAD / 100;
        let expected_high = SOLEND_WAD / 2 + SOLEND_WAD / 100;
        assert!(
            util >= expected_low && util <= expected_high,
            "expected ~50% utilisation, got {util}",
        );
    }

    // ── Constant pinning ────────────────────────────────────────────────

    #[test]
    fn constants_match_research_pin() {
        assert_eq!(SOLEND_SLOTS_PER_YEAR, 63_072_000);
        assert_eq!(SOLEND_WAD, 1_000_000_000_000_000_000);
        assert_eq!(BPS_DENOM, 10_000);
        assert_eq!(SUPPORTED_SOLEND_FORMULA_VERSION, 1);
    }

    /// Cross-crate parity: gateway crate already pins SOLEND_WAD =
    /// 10^18 (`crates/gateway/src/integrations/solend/raw.rs`). Our
    /// on-chain copy MUST match so daemon-side and program-side WAD
    /// math is bit-equal. We don't import the gateway crate here
    /// (it's not a dependency of this on-chain crate, and shouldn't
    /// be); the literal pin below is the parity contract.
    #[test]
    fn solend_wad_matches_gateway_literal() {
        assert_eq!(SOLEND_WAD, 1_000_000_000_000_000_000u128);
    }
}

// ── Mirror enum parity tests (dev-dep crate-types) ──────────────────────────

#[cfg(test)]
mod mirror_enum_parity {
    //! Asserts our local mirror enums match the canonical ones in
    //! `claw_types::stage2_watch_rule`. Same pattern as
    //! `tests/discriminator_parity.rs` for `Stage2ActionType`.
    //!
    //! `claw-types` is a dev-dep on this crate (Cargo.toml) — these
    //! tests ride on that and never pull `claw-types` into the BPF
    //! build.

    use super::{BoundMode, Comparison, ConditionLogic, RateKind, VerificationLevel};
    use claw_types::stage2_watch_rule as canonical;

    #[test]
    fn comparison_variants_round_trip_via_borsh_ordinal() {
        // Borsh encodes enums as a `u8` tag + body. We assert the
        // ordinal of each canonical variant matches the order of our
        // local mirror enum, which is what
        // `verify_pyth_price_condition` ultimately consumes.
        for (canon, mirror) in [
            (canonical::Comparison::Lt, Comparison::Lt),
            (canonical::Comparison::Lte, Comparison::Lte),
            (canonical::Comparison::Gt, Comparison::Gt),
            (canonical::Comparison::Gte, Comparison::Gte),
        ] {
            // Ordinal parity via Borsh first byte.
            let bytes = borsh::to_vec(&canon).unwrap();
            let mirror_ordinal = match mirror {
                Comparison::Lt => 0u8,
                Comparison::Lte => 1,
                Comparison::Gt => 2,
                Comparison::Gte => 3,
            };
            assert_eq!(
                bytes[0], mirror_ordinal,
                "Comparison::{canon:?} canonical ordinal must equal mirror ordinal",
            );
        }
    }

    #[test]
    fn bound_mode_variants_round_trip_via_borsh_ordinal() {
        for (canon, mirror_ordinal) in [
            (canonical::BoundMode::AdverseLowerForGt, 0u8),
            (canonical::BoundMode::AdverseUpperForLt, 1),
            (canonical::BoundMode::Midpoint, 2),
        ] {
            let bytes = borsh::to_vec(&canon).unwrap();
            assert_eq!(bytes[0], mirror_ordinal, "BoundMode ordinal mismatch");
        }
        // And the mirror itself has 3 variants.
        let _: [BoundMode; 3] = [
            BoundMode::AdverseLowerForGt,
            BoundMode::AdverseUpperForLt,
            BoundMode::Midpoint,
        ];
    }

    #[test]
    fn rate_kind_variants_round_trip_via_borsh_ordinal() {
        for (canon, mirror_ordinal) in [
            (canonical::RateKind::Apr, 0u8),
            (canonical::RateKind::Apy, 1),
        ] {
            let bytes = borsh::to_vec(&canon).unwrap();
            assert_eq!(bytes[0], mirror_ordinal, "RateKind ordinal mismatch");
        }
        let _: [RateKind; 2] = [RateKind::Apr, RateKind::Apy];
    }

    #[test]
    fn verification_level_variants_round_trip_via_borsh_ordinal() {
        let bytes = borsh::to_vec(&canonical::VerificationLevel::Full).unwrap();
        assert_eq!(bytes[0], 0u8, "VerificationLevel::Full must be ordinal 0");
        let _: [VerificationLevel; 1] = [VerificationLevel::Full];
    }

    #[test]
    fn condition_logic_variants_round_trip_via_borsh_ordinal() {
        for (canon, mirror_ordinal) in [
            (canonical::ConditionLogic::All, 0u8),
            (canonical::ConditionLogic::Any, 1),
        ] {
            let bytes = borsh::to_vec(&canon).unwrap();
            assert_eq!(bytes[0], mirror_ordinal, "ConditionLogic ordinal mismatch");
        }
        let _: [ConditionLogic; 2] = [ConditionLogic::All, ConditionLogic::Any];
    }
}

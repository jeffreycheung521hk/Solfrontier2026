//! Stage 2 — delegated conditional execution: canonical WatchRule
//! schema + hash.
//!
//! Substrate-only. This module defines the on-the-wire shape of a
//! Stage 2 watch rule (authorize-now / execute-later) and produces a
//! deterministic SHA-256 hash over its Borsh canonical bytes. It does
//! NOT implement the executor, the watcher, the Authorization PDA, or
//! the on-chain program. Those are downstream slices.
//!
//! # Stage 1 Tail boundary
//!
//! This is NOT [`crate::canonical_intent::CanonicalIntent`]. Stage 1
//! Tail commits to a single human-approved propose-now action; Stage 2
//! commits to a longer-lived rule whose execution is delegated to a
//! daemon when conditions are met. The two have separate schemas,
//! separate version constants, and separate canonical-hash domains so
//! a Stage 1 Tail hash can never be reinterpreted as a Stage 2 rule
//! and vice versa.
//!
//! # Source-of-truth alignment
//!
//! Field shapes follow:
//!   - `docs/STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md` (top-level
//!     PDA layout — § 3.2)
//!   - `docs/STAGE2_PYTH_ADAPTER_RESEARCH.md` (Pyth condition fields —
//!     § canonical PythPriceCondition layout)
//!   - `docs/STAGE2_SOLEND_APY_RESEARCH.md` (Solend rate condition
//!     fields — § 5)
//!
//! Integer / fixed-point only. No `f64` anywhere — the hash and the
//! eventual on-chain comparator are both integer-domain by construction
//! per the specs' explicit float-ban discipline.
//!
//! # Hash construction
//!
//! ```text
//! canonical_rule_bytes(rule) = borsh::to_vec(rule)
//! canonical_rule_hash(rule)  = SHA-256(canonical_rule_bytes(rule))
//! ```
//!
//! [`WatchRule`] does NOT carry its own hash — including a self-hash
//! field would create a chicken-and-egg problem at hash time and would
//! also let a future schema accidentally hash-bind something derived
//! from a hash. The hash lives on [`WatchRuleMetadata`] instead, which
//! is the small projection downstream layers (Authorization PDA seeds,
//! UI, audit) carry around.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::canonical_intent::PubkeyBytes;

/// Stage 2 schema version, the FIRST byte of every canonical rule.
/// A future schema change MUST bump this; any consumer that pins the
/// old version sees a hash mismatch and fails closed instead of
/// silently parsing newer-shaped bytes.
pub const STAGE2_WATCH_RULE_SCHEMA_VERSION: u8 = 1;

/// Upper bound on `conditions.len()`. Keeps Borsh-decoded sizes
/// predictable and gives the on-chain comparator a small fixed
/// compute budget; eight is plenty for the demo (Scenario B is 3
/// conditions). Future slices may bump this together with a schema
/// version bump.
pub const MAX_CONDITIONS: usize = 8;

/// Lower bound on `conditions.len()`. A rule with no conditions is
/// trivially-true and almost always a programming error.
pub const MIN_CONDITIONS: usize = 1;

// ── Shared discriminators ───────────────────────────────────────────────────

/// How `conditions[..]` are combined.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ConditionLogic {
    /// AND — every condition must hold.
    All,
    /// OR — at least one condition must hold.
    Any,
}

/// Comparison operator for numeric thresholds.
///
/// `Lt` / `Gt` are the only operators the Stage 2 specs explicitly
/// reference (price > X, APR < Y); `Lte` / `Gte` are included for
/// future use.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Lt,
    Lte,
    Gt,
    Gte,
}

// ── Pyth condition ──────────────────────────────────────────────────────────

/// Pyth required verification level. v1 = `Full` only; the receiver
/// allows `Partial` updates but the Stage 2 spec § 7.1 (audit U-5)
/// requires `Full` and rejects `Partial` outright.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum VerificationLevel {
    Full,
}

/// Adverse-bound discipline for Pyth price + confidence. Per
/// `STAGE2_PYTH_ADAPTER_RESEARCH.md` § canonical PythPriceCondition:
///
///   - `AdverseLowerForGt` — `(price - confidence)` is the lhs of `>` checks
///     so a noisy upper-tail estimate cannot trip a "price above X" trigger.
///   - `AdverseUpperForLt` — `(price + confidence)` is the lhs of `<` checks
///     so a noisy lower-tail estimate cannot trip a "price below X" trigger.
///   - `Midpoint` — `price` itself, no confidence widening. Use only when
///     the user accepts confidence-band slop in their trigger.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BoundMode {
    AdverseLowerForGt,
    AdverseUpperForLt,
    Midpoint,
}

// ── Solend condition ────────────────────────────────────────────────────────

/// Which rate the comparator is checking. v1 demo = `Apr` only; `Apy`
/// is allowed but expensive (per `STAGE2_SOLEND_APY_RESEARCH.md` § 4.4
/// the APR path is one fixed-point multiply; APY needs the per-slot
/// power and an explicit higher compute-budget allocation).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RateKind {
    Apr,
    Apy,
}

// ── Conditions ──────────────────────────────────────────────────────────────

/// One condition in the rule's `conditions[..]` list.
///
/// Field order within each variant = canonical Borsh order. Every
/// reordering, addition, or removal MUST bump
/// [`STAGE2_WATCH_RULE_SCHEMA_VERSION`].
#[derive(
    Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    /// Pyth price condition. Per
    /// `STAGE2_PYTH_ADAPTER_RESEARCH.md` § canonical PythPriceCondition
    /// (lines 203-222 of the research doc).
    PythPrice {
        feed_id: [u8; 32],
        price_update_account: PubkeyBytes,
        comparison: Comparison,
        threshold_mantissa: i64,
        threshold_exponent: i32,
        max_age_seconds: u32,
        max_confidence_bps: u16,
        verification_level_required: VerificationLevel,
        bound_mode: BoundMode,
    },
    /// Solend reserve supply rate condition. Per
    /// `STAGE2_SOLEND_APY_RESEARCH.md` § 5.
    SolendReserveSupplyRate {
        reserve_pubkey: PubkeyBytes,
        lending_market: PubkeyBytes,
        solend_program_id: PubkeyBytes,
        comparison: Comparison,
        threshold_bps: u32,
        rate_kind: RateKind,
        formula_version: u8,
        max_reserve_staleness_slots: u32,
        required_refresh_same_tx: bool,
    },
}

impl Condition {
    pub const fn condition_kind(&self) -> ConditionKind {
        match self {
            Self::PythPrice { .. } => ConditionKind::PythPrice,
            Self::SolendReserveSupplyRate { .. } => ConditionKind::SolendReserveSupplyRate,
        }
    }
}

/// Discriminator for [`Condition`] (used by routing / audit).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ConditionKind {
    PythPrice,
    SolendReserveSupplyRate,
}

// ── Action ──────────────────────────────────────────────────────────────────

/// Solend withdraw mode. Single variant for v1 (per spec § 5.3, the
/// demo MUST NOT implement partial withdraws); enum reserved for
/// future modes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawMode {
    /// Withdraw the delegated wallet's full collateral position from
    /// `target_obligation`. Cap is enforced at front-door by
    /// `max_input_amount_raw`; withdraw-all on the delegated
    /// obligation is bounded by construction (spec § 5.3.1).
    WithdrawAllDelegatedPosition,
}

/// Jupiter API version pin. Per spec § 6.1.1, version MUST be pinned
/// before any tx-size measurement; v1 and v2 have materially
/// different byte budgets.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum JupiterApiVersion {
    /// `/swap/v1/quote` + `/swap/v1/swap-instructions`. Deprecated
    /// upstream; allowed only with explicit acknowledgement.
    V1,
    /// `/swap/v2/order` + `/swap/v2/build` + `/swap/v2/execute`. v2
    /// adds `tipInstruction` (Jito) and `blockhashWithMetadata`
    /// fields to the build payload.
    V2,
}

/// One action the rule may execute when its `condition_logic`
/// evaluation passes.
///
/// Field order within each variant = canonical Borsh order. Adding or
/// reordering fields MUST bump [`STAGE2_WATCH_RULE_SCHEMA_VERSION`].
#[derive(
    Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionSpec {
    /// Withdraw 100% of the delegated wallet's deposit on
    /// `target_obligation`, then sweep to `destination_wallet`.
    /// Per spec § 5.1, `target_obligation` MUST be owned by the
    /// delegated wallet (never the user's main-wallet obligation).
    SolendWithdrawAllDelegated {
        target_obligation: PubkeyBytes,
        reserve_pubkey: PubkeyBytes,
        lending_market: PubkeyBytes,
        destination_wallet: PubkeyBytes,
        withdraw_mode: WithdrawMode,
    },
    /// Buy SOL with USDC via Jupiter. The pre/post bracket described
    /// in spec § 6.1.2 is required when `require_pre_post_bracket`
    /// is `true`.
    JupiterBuySolWithUsdc {
        input_mint: PubkeyBytes,
        output_mint: PubkeyBytes,
        input_amount_raw: u64,
        /// `Some(n)` pins the floor exactly; `None` defers to
        /// `slippage_bps` at execute time. Borsh tag-byte cost is
        /// 1 byte; the option is part of the canonical bytes.
        min_output_amount_raw: Option<u64>,
        jupiter_api_version: JupiterApiVersion,
        /// `maxAccounts` hint passed to Jupiter `/quote`. Spec §
        /// 6.1.1 prescribes `≤ 48` to leave headroom for the
        /// bracket + sysvar account slots.
        max_accounts_hint: u8,
        require_pre_post_bracket: bool,
    },
}

impl ActionSpec {
    pub const fn action_type(&self) -> WatchRuleActionType {
        match self {
            Self::SolendWithdrawAllDelegated { .. } => {
                WatchRuleActionType::SolendWithdrawAllDelegated
            }
            Self::JupiterBuySolWithUsdc { .. } => {
                WatchRuleActionType::JupiterBuySolWithUsdc
            }
        }
    }
}

/// Discriminator for [`ActionSpec`] (used by metadata / routing /
/// audit). Stable wire string via [`Self::label`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum WatchRuleActionType {
    SolendWithdrawAllDelegated,
    JupiterBuySolWithUsdc,
}

impl WatchRuleActionType {
    pub const fn label(self) -> &'static str {
        match self {
            Self::SolendWithdrawAllDelegated => "solend_withdraw_all_delegated",
            Self::JupiterBuySolWithUsdc => "jupiter_buy_sol_with_usdc",
        }
    }

    /// Stable on-chain discriminator value, used by the Stage 2
    /// Authorization PDA's `allowed_action_type: u8` field.
    ///
    /// Values start at `1` so that the all-zero byte (a default-zeroed
    /// PDA buffer) never aliases to a valid action type. The values
    /// here are part of the on-chain wire shape and MUST NOT change
    /// without a Stage 2 schema-version bump in
    /// [`STAGE2_WATCH_RULE_SCHEMA_VERSION`].
    ///
    /// This helper is independent of Borsh — the canonical rule hash
    /// is computed over the [`ActionSpec`] variant inside a
    /// [`WatchRule`], not over this discriminator, so adding this
    /// method does not affect any pinned canonical-hash fixture.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::SolendWithdrawAllDelegated => 1,
            Self::JupiterBuySolWithUsdc => 2,
        }
    }

    /// Inverse of [`Self::to_u8`]. Returns `None` for any byte not in
    /// `{1, 2}` so that the on-chain comparator can fail-closed on an
    /// uninterpretable discriminator (zero-default, or a byte from a
    /// future schema this build doesn't know about).
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::SolendWithdrawAllDelegated),
            2 => Some(Self::JupiterBuySolWithUsdc),
            _ => None,
        }
    }
}

// ── WatchRule top-level ─────────────────────────────────────────────────────

/// Top-level Stage 2 watch rule.
///
/// Field order in source = canonical Borsh order. Reordering or
/// adding fields requires bumping [`STAGE2_WATCH_RULE_SCHEMA_VERSION`].
///
/// The rule does NOT carry its own hash; the hash lives on
/// [`WatchRuleMetadata`] (constructed via [`WatchRuleMetadata::from_rule`]).
#[derive(
    Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct WatchRule {
    pub schema_version: u8,
    pub rule_id: [u8; 16],
    pub user: PubkeyBytes,
    /// Daemon executor pubkey allowed to dispatch the action. The
    /// program rejects executions whose signer differs.
    pub executor: PubkeyBytes,
    /// PDA-funded delegated wallet that owns the position. Per spec
    /// § 5.1, the rule's Solend `target_obligation` MUST be owned by
    /// this wallet.
    pub delegated_wallet: PubkeyBytes,
    pub created_at_slot: u64,
    pub expires_at_slot: u64,
    /// `true` = single execution; `false` = multi-use (multi-use is
    /// explicitly NOT built in v1 — spec § 3.1).
    pub one_shot: bool,
    pub condition_logic: ConditionLogic,
    pub conditions: Vec<Condition>,
    pub action: ActionSpec,
    pub max_input_amount_raw: u64,
    pub used_amount_raw: u64,
    pub destination: PubkeyBytes,
    pub slippage_bps: u16,
}

// ── Canonical bytes + hash ──────────────────────────────────────────────────

/// Borsh-serialize the rule into its canonical byte form. Borsh's
/// deterministic-by-construction layout (no map iteration order, no
/// floats, no enum discriminant ambiguity) gives the same bytes
/// across runs and across crate consumers.
pub fn canonical_rule_bytes(rule: &WatchRule) -> Vec<u8> {
    borsh::to_vec(rule).expect(
        "Borsh serialization of WatchRule is infallible for the auto-derived layout",
    )
}

/// SHA-256 over [`canonical_rule_bytes`]. Stable across runs.
pub fn canonical_rule_hash(rule: &WatchRule) -> [u8; 32] {
    let bytes = canonical_rule_bytes(rule);
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher.finalize().into()
}

// ── Validation ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WatchRuleValidationError {
    #[error("rule expired: current_slot {current} >= expires_at_slot {expires}")]
    Expired { current: u64, expires: u64 },
    #[error("conditions empty: must contain at least {min} condition(s)")]
    NoConditions { min: usize },
    #[error("conditions over cap: got {got}, max {max}")]
    TooManyConditions { got: usize, max: usize },
    #[error("used_amount_raw {used} exceeds max_input_amount_raw {max}")]
    UsedAmountOverMax { used: u64, max: u64 },
    #[error(
        "one_shot rule has partial used_amount_raw {used} that is neither 0 nor max_input_amount_raw {max}"
    )]
    OneShotPartialAmount { used: u64, max: u64 },
}

/// Returns `Ok(())` iff `current_slot < rule.expires_at_slot`.
/// Boundary exclusive at `expires_at_slot` (mirrors Solana's
/// `last_valid_block_height` semantics).
pub fn validate_not_expired(
    rule: &WatchRule,
    current_slot: u64,
) -> Result<(), WatchRuleValidationError> {
    if current_slot < rule.expires_at_slot {
        Ok(())
    } else {
        Err(WatchRuleValidationError::Expired {
            current: current_slot,
            expires: rule.expires_at_slot,
        })
    }
}

/// Returns `Ok(())` iff `MIN_CONDITIONS <= conditions.len() <= MAX_CONDITIONS`.
pub fn validate_conditions_bounded(
    rule: &WatchRule,
) -> Result<(), WatchRuleValidationError> {
    let n = rule.conditions.len();
    if n < MIN_CONDITIONS {
        return Err(WatchRuleValidationError::NoConditions {
            min: MIN_CONDITIONS,
        });
    }
    if n > MAX_CONDITIONS {
        return Err(WatchRuleValidationError::TooManyConditions {
            got: n,
            max: MAX_CONDITIONS,
        });
    }
    Ok(())
}

/// Returns `Ok(())` iff `used_amount_raw <= max_input_amount_raw`,
/// AND (when `one_shot == true`) `used_amount_raw` is either `0`
/// (not yet executed) or `== max_input_amount_raw` (fully consumed).
///
/// The intermediate-value rejection on the `one_shot` arm is a
/// belt-and-braces guard: the program ought to enforce it on chain,
/// but a backend that constructs a one-shot rule with a partial used
/// amount is almost certainly buggy and we'd rather see it fail at
/// validation than at execution.
pub fn validate_used_amount(rule: &WatchRule) -> Result<(), WatchRuleValidationError> {
    if rule.used_amount_raw > rule.max_input_amount_raw {
        return Err(WatchRuleValidationError::UsedAmountOverMax {
            used: rule.used_amount_raw,
            max: rule.max_input_amount_raw,
        });
    }
    if rule.one_shot
        && rule.used_amount_raw != 0
        && rule.used_amount_raw != rule.max_input_amount_raw
    {
        return Err(WatchRuleValidationError::OneShotPartialAmount {
            used: rule.used_amount_raw,
            max: rule.max_input_amount_raw,
        });
    }
    Ok(())
}

// ── WatchRuleMetadata ───────────────────────────────────────────────────────

/// Lossless metadata snapshot, separate from [`WatchRule`] so the
/// canonical hash can live alongside the schema version, rule id,
/// user, executor, expiry, action type — without itself appearing
/// inside the hashed bytes (which would create a chicken-and-egg
/// problem). Use this for Authorization PDA seeds, UI rendering, and
/// audit correlation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct WatchRuleMetadata {
    pub schema_version: u8,
    pub rule_id: [u8; 16],
    pub user: PubkeyBytes,
    pub executor: PubkeyBytes,
    pub expires_at_slot: u64,
    pub action_type: WatchRuleActionType,
    pub canonical_rule_hash: [u8; 32],
}

impl WatchRuleMetadata {
    pub fn from_rule(rule: &WatchRule) -> Self {
        Self {
            schema_version: rule.schema_version,
            rule_id: rule.rule_id,
            user: rule.user,
            executor: rule.executor,
            expires_at_slot: rule.expires_at_slot,
            action_type: rule.action.action_type(),
            canonical_rule_hash: canonical_rule_hash(rule),
        }
    }

    /// Same exclusive-boundary semantics as [`validate_not_expired`].
    pub fn validate_not_expired(
        &self,
        current_slot: u64,
    ) -> Result<(), WatchRuleValidationError> {
        if current_slot < self.expires_at_slot {
            Ok(())
        } else {
            Err(WatchRuleValidationError::Expired {
                current: current_slot,
                expires: self.expires_at_slot,
            })
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_hex(b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len() * 2);
        for byte in b {
            std::fmt::Write::write_fmt(&mut s, format_args!("{byte:02x}")).unwrap();
        }
        s
    }

    fn pk(b: u8) -> PubkeyBytes {
        PubkeyBytes::new([b; 32])
    }

    fn pk_from_str(s: &str) -> PubkeyBytes {
        PubkeyBytes::from_base58(s).expect("test pubkey parses")
    }

    /// Stable rule id for fixtures.
    const FIXTURE_RULE_ID: [u8; 16] = [
        0x52, 0x55, 0x4c, 0x45, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
    ];

    /// Stable Pyth feed ids (publicly published, mainnet).
    const BTC_USD_FEED_ID: [u8; 32] = [
        0xe6, 0x2d, 0xf6, 0xc8, 0xb4, 0xa8, 0x5f, 0xe1, 0xa6, 0x7d, 0xb4, 0x4d, 0xc1,
        0x2d, 0xe5, 0xdb, 0x33, 0x0f, 0x7a, 0xc6, 0x6b, 0x72, 0xdc, 0x65, 0x8a, 0xfe,
        0xdf, 0x0f, 0x4a, 0x41, 0x5b, 0x43,
    ];
    const ETH_USD_FEED_ID: [u8; 32] = [
        0xff, 0x61, 0x49, 0x1a, 0x93, 0x11, 0x12, 0xdd, 0xf1, 0xbd, 0x81, 0x47, 0xcd,
        0x1b, 0x64, 0x13, 0x75, 0xf7, 0x9f, 0x58, 0x25, 0x12, 0x6d, 0x66, 0x54, 0x80,
        0x87, 0x46, 0x34, 0xfd, 0x0a, 0xce,
    ];
    const SOL_USD_FEED_ID: [u8; 32] = [
        0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95,
        0xd1, 0xda, 0x39, 0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c,
        0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
    ];

    const SOLEND_USDC_RESERVE: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
    const SOLEND_LENDING_MARKET: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
    const SOLEND_PROGRAM_ID: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
    const TEST_USER: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

    const FIXTURE_CREATED_AT: u64 = 415_500_000;
    const FIXTURE_EXPIRES_AT: u64 = 415_700_000;

    // ── Scenario A: Solend APR < 10% → withdraw delegated position ──────

    fn fixture_a_solend_apr_below_10() -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id: FIXTURE_RULE_ID,
            user: pk_from_str(TEST_USER),
            executor: pk(0x02),       // synthetic delegated executor
            delegated_wallet: pk(0x03), // synthetic delegated wallet
            created_at_slot: FIXTURE_CREATED_AT,
            expires_at_slot: FIXTURE_EXPIRES_AT,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![Condition::SolendReserveSupplyRate {
                reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
                lending_market: pk_from_str(SOLEND_LENDING_MARKET),
                solend_program_id: pk_from_str(SOLEND_PROGRAM_ID),
                comparison: Comparison::Lt,
                threshold_bps: 1_000, // 10.00%
                rate_kind: RateKind::Apr,
                formula_version: 1,
                max_reserve_staleness_slots: 16,
                required_refresh_same_tx: true,
            }],
            action: ActionSpec::SolendWithdrawAllDelegated {
                target_obligation: pk(0x04),
                reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
                lending_market: pk_from_str(SOLEND_LENDING_MARKET),
                destination_wallet: pk_from_str(TEST_USER),
                withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
            },
            max_input_amount_raw: 5_000_000, // 5 USDC
            used_amount_raw: 0,
            destination: pk_from_str(TEST_USER),
            slippage_bps: 0, // Solend has no slippage; field present for symmetry
        }
    }

    // ── Scenario B: BTC > 75000 AND ETH > 2300 AND SOL < 90 → buy SOL ───

    fn fixture_b_basket_buy_sol() -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id: FIXTURE_RULE_ID,
            user: pk_from_str(TEST_USER),
            executor: pk(0x02),
            delegated_wallet: pk(0x03),
            created_at_slot: FIXTURE_CREATED_AT,
            expires_at_slot: FIXTURE_EXPIRES_AT,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![
                Condition::PythPrice {
                    feed_id: BTC_USD_FEED_ID,
                    price_update_account: pk(0x10),
                    comparison: Comparison::Gt,
                    threshold_mantissa: 7_500_000, // 75000.00
                    threshold_exponent: -2,
                    max_age_seconds: 30,
                    max_confidence_bps: 50,
                    verification_level_required: VerificationLevel::Full,
                    bound_mode: BoundMode::AdverseLowerForGt,
                },
                Condition::PythPrice {
                    feed_id: ETH_USD_FEED_ID,
                    price_update_account: pk(0x11),
                    comparison: Comparison::Gt,
                    threshold_mantissa: 230_000, // 2300.00
                    threshold_exponent: -2,
                    max_age_seconds: 30,
                    max_confidence_bps: 50,
                    verification_level_required: VerificationLevel::Full,
                    bound_mode: BoundMode::AdverseLowerForGt,
                },
                Condition::PythPrice {
                    feed_id: SOL_USD_FEED_ID,
                    price_update_account: pk(0x12),
                    comparison: Comparison::Lt,
                    threshold_mantissa: 9_000, // 90.00
                    threshold_exponent: -2,
                    max_age_seconds: 30,
                    max_confidence_bps: 50,
                    verification_level_required: VerificationLevel::Full,
                    bound_mode: BoundMode::AdverseUpperForLt,
                },
            ],
            action: ActionSpec::JupiterBuySolWithUsdc {
                input_mint: pk_from_str(USDC_MINT),
                output_mint: pk_from_str(WSOL_MINT),
                input_amount_raw: 5_000_000,
                min_output_amount_raw: None,
                jupiter_api_version: JupiterApiVersion::V2,
                max_accounts_hint: 48,
                require_pre_post_bracket: true,
            },
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk_from_str(TEST_USER),
            slippage_bps: 50,
        }
    }

    // ── Stable fixture-hash assertions (Phase B1 commit-pinned) ─────────

    /// Scenario A canonical hash. Any change to schema layout, fixture
    /// contents, or Borsh encoding surfaces here.
    #[test]
    fn scenario_a_solend_apr_fixture_hash_is_stable() {
        let h = canonical_rule_hash(&fixture_a_solend_apr_below_10());
        assert_eq!(
            fmt_hex(&h),
            "5fd3a3b8cfeab17e9985826be642acdd44d726b5395bc6752f58076b97329adc"
        );
    }

    #[test]
    fn scenario_b_basket_buy_fixture_hash_is_stable() {
        let h = canonical_rule_hash(&fixture_b_basket_buy_sol());
        assert_eq!(
            fmt_hex(&h),
            "ee8a1edb91a6c7b3d3c023adbdfa9df48901ccb71b1445a630a2d1038b48b7bb"
        );
    }

    // ── Determinism + variant separation ────────────────────────────────

    #[test]
    fn same_rule_serialises_to_same_bytes() {
        let a = fixture_a_solend_apr_below_10();
        let b = fixture_a_solend_apr_below_10();
        assert_eq!(canonical_rule_bytes(&a), canonical_rule_bytes(&b));
        assert_eq!(canonical_rule_hash(&a), canonical_rule_hash(&b));
    }

    #[test]
    fn distinct_scenarios_have_distinct_hashes() {
        let h1 = canonical_rule_hash(&fixture_a_solend_apr_below_10());
        let h2 = canonical_rule_hash(&fixture_b_basket_buy_sol());
        assert_ne!(h1, h2);
    }

    #[test]
    fn schema_version_is_first_canonical_byte() {
        let bytes = canonical_rule_bytes(&fixture_a_solend_apr_below_10());
        assert_eq!(bytes[0], STAGE2_WATCH_RULE_SCHEMA_VERSION);
    }

    // ── Mutation tests — every load-bearing field is hash-included ──────

    #[test]
    fn changing_solend_threshold_changes_hash() {
        let mut m = fixture_a_solend_apr_below_10();
        if let Condition::SolendReserveSupplyRate {
            ref mut threshold_bps,
            ..
        } = m.conditions[0]
        {
            *threshold_bps = 999;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_a_solend_apr_below_10()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_pyth_threshold_mantissa_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        if let Condition::PythPrice {
            ref mut threshold_mantissa,
            ..
        } = m.conditions[0]
        {
            *threshold_mantissa += 1;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_pyth_threshold_exponent_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        if let Condition::PythPrice {
            ref mut threshold_exponent,
            ..
        } = m.conditions[0]
        {
            *threshold_exponent -= 1;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_pyth_bound_mode_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        if let Condition::PythPrice {
            ref mut bound_mode,
            ..
        } = m.conditions[0]
        {
            *bound_mode = BoundMode::Midpoint;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_pyth_max_age_seconds_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        if let Condition::PythPrice {
            ref mut max_age_seconds,
            ..
        } = m.conditions[0]
        {
            *max_age_seconds += 1;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_solend_formula_version_changes_hash() {
        let mut m = fixture_a_solend_apr_below_10();
        if let Condition::SolendReserveSupplyRate {
            ref mut formula_version,
            ..
        } = m.conditions[0]
        {
            *formula_version = 2;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_a_solend_apr_below_10()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_solend_rate_kind_changes_hash() {
        let mut m = fixture_a_solend_apr_below_10();
        if let Condition::SolendReserveSupplyRate { ref mut rate_kind, .. } = m.conditions[0]
        {
            *rate_kind = RateKind::Apy;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_a_solend_apr_below_10()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_expires_at_slot_changes_hash() {
        for fixture in [
            fixture_a_solend_apr_below_10(),
            fixture_b_basket_buy_sol(),
        ] {
            let mut bumped = fixture.clone();
            bumped.expires_at_slot += 1;
            assert_ne!(
                canonical_rule_hash(&fixture),
                canonical_rule_hash(&bumped)
            );
        }
    }

    #[test]
    fn changing_executor_changes_hash() {
        let mut m = fixture_a_solend_apr_below_10();
        m.executor = pk(0xff);
        assert_ne!(
            canonical_rule_hash(&fixture_a_solend_apr_below_10()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_destination_changes_hash() {
        let mut m = fixture_a_solend_apr_below_10();
        m.destination = pk(0xee);
        assert_ne!(
            canonical_rule_hash(&fixture_a_solend_apr_below_10()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_slippage_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        m.slippage_bps = 100;
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_action_variant_changes_hash() {
        let mut m = fixture_a_solend_apr_below_10();
        m.action = ActionSpec::JupiterBuySolWithUsdc {
            input_mint: pk_from_str(USDC_MINT),
            output_mint: pk_from_str(WSOL_MINT),
            input_amount_raw: 5_000_000,
            min_output_amount_raw: None,
            jupiter_api_version: JupiterApiVersion::V2,
            max_accounts_hint: 48,
            require_pre_post_bracket: true,
        };
        assert_ne!(
            canonical_rule_hash(&fixture_a_solend_apr_below_10()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_condition_logic_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        m.condition_logic = ConditionLogic::Any;
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_one_shot_changes_hash() {
        let mut m = fixture_a_solend_apr_below_10();
        m.one_shot = false;
        assert_ne!(
            canonical_rule_hash(&fixture_a_solend_apr_below_10()),
            canonical_rule_hash(&m)
        );
    }

    /// Reordering conditions changes the hash (we preserve order, do
    /// NOT canonical-sort). A future refactor that sorts inside the
    /// hash path would silently change the hash domain — this test
    /// fails loudly in that case.
    #[test]
    fn reordering_conditions_changes_hash() {
        let original = fixture_b_basket_buy_sol();
        let mut reordered = original.clone();
        reordered.conditions.swap(0, 2);
        assert_ne!(
            canonical_rule_hash(&original),
            canonical_rule_hash(&reordered)
        );
    }

    #[test]
    fn changing_jupiter_api_version_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        if let ActionSpec::JupiterBuySolWithUsdc {
            ref mut jupiter_api_version,
            ..
        } = m.action
        {
            *jupiter_api_version = JupiterApiVersion::V1;
        }
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    #[test]
    fn changing_min_output_amount_changes_hash() {
        let mut m = fixture_b_basket_buy_sol();
        if let ActionSpec::JupiterBuySolWithUsdc {
            ref mut min_output_amount_raw,
            ..
        } = m.action
        {
            *min_output_amount_raw = Some(123_456);
        }
        assert_ne!(
            canonical_rule_hash(&fixture_b_basket_buy_sol()),
            canonical_rule_hash(&m)
        );
    }

    // ── Borsh round-trip ────────────────────────────────────────────────

    #[test]
    fn borsh_round_trip_preserves_rule() {
        for fixture in [
            fixture_a_solend_apr_below_10(),
            fixture_b_basket_buy_sol(),
        ] {
            let bytes = canonical_rule_bytes(&fixture);
            let decoded: WatchRule = borsh::from_slice(&bytes).expect("borsh round-trip");
            assert_eq!(decoded, fixture);
        }
    }

    // ── Closed schema (deny_unknown_fields) ─────────────────────────────

    #[test]
    fn json_with_unknown_top_level_field_fails() {
        let json = serde_json::to_string(&fixture_a_solend_apr_below_10()).unwrap();
        let injected = json.replacen(
            "\"executor\":",
            "\"injected\":\"x\",\"executor\":",
            1,
        );
        assert!(
            serde_json::from_str::<WatchRule>(&injected).is_err(),
            "unknown top-level field must be rejected"
        );
    }

    #[test]
    fn json_with_unknown_condition_field_fails() {
        let json = serde_json::to_string(&fixture_b_basket_buy_sol()).unwrap();
        let injected = json.replacen(
            "\"max_confidence_bps\":50",
            "\"max_confidence_bps\":50,\"hidden_kicker\":7",
            1,
        );
        assert!(
            serde_json::from_str::<WatchRule>(&injected).is_err(),
            "unknown condition field must be rejected"
        );
    }

    #[test]
    fn json_with_unknown_action_field_fails() {
        let json = serde_json::to_string(&fixture_b_basket_buy_sol()).unwrap();
        let injected = json.replacen(
            "\"max_accounts_hint\":48",
            "\"max_accounts_hint\":48,\"sneaky\":1",
            1,
        );
        assert!(
            serde_json::from_str::<WatchRule>(&injected).is_err(),
            "unknown action field must be rejected"
        );
    }

    #[test]
    fn invalid_comparison_string_rejected() {
        let json = serde_json::to_string(&fixture_a_solend_apr_below_10()).unwrap();
        // Replace the comparison string with one not in the enum.
        let mutated = json.replacen("\"lt\"", "\"approximately\"", 1);
        assert!(
            serde_json::from_str::<WatchRule>(&mutated).is_err(),
            "comparison must be one of the closed enum variants"
        );
    }

    #[test]
    fn invalid_rate_kind_string_rejected() {
        let json = serde_json::to_string(&fixture_a_solend_apr_below_10()).unwrap();
        let mutated = json.replacen("\"apr\"", "\"weekly\"", 1);
        assert!(
            serde_json::from_str::<WatchRule>(&mutated).is_err(),
            "rate_kind must be apr or apy"
        );
    }

    // ── Validation helpers ──────────────────────────────────────────────

    #[test]
    fn validate_not_expired_boundary() {
        let r = fixture_a_solend_apr_below_10();
        assert_eq!(validate_not_expired(&r, r.expires_at_slot - 1), Ok(()));
        assert_eq!(
            validate_not_expired(&r, r.expires_at_slot),
            Err(WatchRuleValidationError::Expired {
                current: r.expires_at_slot,
                expires: r.expires_at_slot,
            })
        );
        assert_eq!(
            validate_not_expired(&r, r.expires_at_slot + 100),
            Err(WatchRuleValidationError::Expired {
                current: r.expires_at_slot + 100,
                expires: r.expires_at_slot,
            })
        );
    }

    #[test]
    fn validate_conditions_bounded_min() {
        let mut r = fixture_a_solend_apr_below_10();
        r.conditions.clear();
        assert_eq!(
            validate_conditions_bounded(&r),
            Err(WatchRuleValidationError::NoConditions {
                min: MIN_CONDITIONS,
            })
        );
    }

    #[test]
    fn validate_conditions_bounded_max() {
        let mut r = fixture_a_solend_apr_below_10();
        let one = r.conditions[0].clone();
        // Push 8 extra (one already present, so total = 9 > MAX_CONDITIONS=8).
        for _ in 0..MAX_CONDITIONS {
            r.conditions.push(one.clone());
        }
        match validate_conditions_bounded(&r) {
            Err(WatchRuleValidationError::TooManyConditions { got, max }) => {
                assert_eq!(got, MAX_CONDITIONS + 1);
                assert_eq!(max, MAX_CONDITIONS);
            }
            other => panic!("expected TooManyConditions, got {other:?}"),
        }
    }

    #[test]
    fn validate_used_amount_overflow() {
        let mut r = fixture_a_solend_apr_below_10();
        r.used_amount_raw = r.max_input_amount_raw + 1;
        assert!(matches!(
            validate_used_amount(&r),
            Err(WatchRuleValidationError::UsedAmountOverMax { .. })
        ));
    }

    #[test]
    fn validate_used_amount_one_shot_partial_rejected() {
        let mut r = fixture_a_solend_apr_below_10();
        r.used_amount_raw = r.max_input_amount_raw / 2;
        assert!(matches!(
            validate_used_amount(&r),
            Err(WatchRuleValidationError::OneShotPartialAmount { .. })
        ));
    }

    #[test]
    fn validate_used_amount_one_shot_zero_or_max_ok() {
        let mut r = fixture_a_solend_apr_below_10();
        r.used_amount_raw = 0;
        assert_eq!(validate_used_amount(&r), Ok(()));
        r.used_amount_raw = r.max_input_amount_raw;
        assert_eq!(validate_used_amount(&r), Ok(()));
    }

    // ── Metadata projection ─────────────────────────────────────────────

    #[test]
    fn metadata_from_rule_captures_all_fields() {
        for fixture in [
            fixture_a_solend_apr_below_10(),
            fixture_b_basket_buy_sol(),
        ] {
            let m = WatchRuleMetadata::from_rule(&fixture);
            assert_eq!(m.schema_version, fixture.schema_version);
            assert_eq!(m.rule_id, fixture.rule_id);
            assert_eq!(m.user, fixture.user);
            assert_eq!(m.executor, fixture.executor);
            assert_eq!(m.expires_at_slot, fixture.expires_at_slot);
            assert_eq!(m.action_type, fixture.action.action_type());
            assert_eq!(m.canonical_rule_hash, canonical_rule_hash(&fixture));
        }
    }

    #[test]
    fn metadata_validate_not_expired_matches_rule_helper() {
        let r = fixture_a_solend_apr_below_10();
        let m = WatchRuleMetadata::from_rule(&r);
        assert_eq!(
            m.validate_not_expired(r.expires_at_slot - 1),
            validate_not_expired(&r, r.expires_at_slot - 1)
        );
        assert_eq!(
            m.validate_not_expired(r.expires_at_slot),
            validate_not_expired(&r, r.expires_at_slot)
        );
    }

    #[test]
    fn metadata_json_unknown_field_fails() {
        let r = fixture_a_solend_apr_below_10();
        let m = WatchRuleMetadata::from_rule(&r);
        let json = serde_json::to_string(&m).unwrap();
        let injected = json.replacen(
            "\"action_type\":",
            "\"injected\":\"x\",\"action_type\":",
            1,
        );
        assert!(serde_json::from_str::<WatchRuleMetadata>(&injected).is_err());
    }

    // ── Action / condition kind projections ─────────────────────────────

    #[test]
    fn action_type_label_round_trip() {
        assert_eq!(
            WatchRuleActionType::SolendWithdrawAllDelegated.label(),
            "solend_withdraw_all_delegated"
        );
        assert_eq!(
            WatchRuleActionType::JupiterBuySolWithUsdc.label(),
            "jupiter_buy_sol_with_usdc"
        );
    }

    #[test]
    fn action_type_u8_round_trip() {
        for v in [
            WatchRuleActionType::SolendWithdrawAllDelegated,
            WatchRuleActionType::JupiterBuySolWithUsdc,
        ] {
            assert_eq!(WatchRuleActionType::from_u8(v.to_u8()), Some(v));
        }
        // Stage 1's ActionType discriminators (1 = SolendDeposit,
        // 2 = SolendWithdrawAll, 3 = JupiterSwap) are intentionally
        // a different namespace; bytes outside the Stage 2 set must
        // come back as None and never alias to a valid Stage 2 type.
        assert_eq!(WatchRuleActionType::from_u8(0), None);
        assert_eq!(WatchRuleActionType::from_u8(3), None);
        assert_eq!(WatchRuleActionType::from_u8(255), None);

        // Pin the on-chain wire values explicitly so any reorder /
        // renumber is loud.
        assert_eq!(WatchRuleActionType::SolendWithdrawAllDelegated.to_u8(), 1);
        assert_eq!(WatchRuleActionType::JupiterBuySolWithUsdc.to_u8(), 2);
    }

    #[test]
    fn action_action_type_round_trip() {
        assert_eq!(
            fixture_a_solend_apr_below_10().action.action_type(),
            WatchRuleActionType::SolendWithdrawAllDelegated
        );
        assert_eq!(
            fixture_b_basket_buy_sol().action.action_type(),
            WatchRuleActionType::JupiterBuySolWithUsdc
        );
    }

    #[test]
    fn condition_kind_round_trip() {
        let a = fixture_a_solend_apr_below_10();
        assert_eq!(
            a.conditions[0].condition_kind(),
            ConditionKind::SolendReserveSupplyRate
        );
        let b = fixture_b_basket_buy_sol();
        assert_eq!(b.conditions[0].condition_kind(), ConditionKind::PythPrice);
    }

    // ── Output fixture (run with --nocapture) ───────────────────────────

    /// Emits canonical JSON / Borsh bytes hex / hash hex for both
    /// scenarios so downstream agents (Anchor program, watcher,
    /// frontend) can verify their own canonical-bytes implementations.
    /// Run: `cargo test -p claw-types stage2_watch_rule::tests::print_fixtures -- --nocapture`
    #[test]
    fn print_fixtures() {
        for (label, rule) in [
            ("A_solend_apr_below_10", fixture_a_solend_apr_below_10()),
            ("B_basket_buy_sol", fixture_b_basket_buy_sol()),
        ] {
            let bytes = canonical_rule_bytes(&rule);
            let hash = canonical_rule_hash(&rule);
            let json = serde_json::to_string_pretty(&rule).unwrap();
            println!("\n=== fixture: {label} ===");
            println!("schema_version = {STAGE2_WATCH_RULE_SCHEMA_VERSION}");
            println!("canonical_json:\n{json}");
            println!("canonical_bytes_hex ({} bytes):", bytes.len());
            println!("  {}", fmt_hex(&bytes));
            println!("canonical_rule_hash_hex (32 bytes):");
            println!("  {}", fmt_hex(&hash));
        }
    }
}

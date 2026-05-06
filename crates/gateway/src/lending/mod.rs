//! Protocol-agnostic V1 lending domain types.
//!
//! This module owns the V1 unit-bearing newtypes and the normalized
//! `LendingSnapshot` shape that the future policy evaluator consumes.
//! It is the policy/risk side of the Part 5 / Part 6 seam defined in
//! `docs/lending_policy_vocabulary.md` (Parts 5A §40 – §44 and 5B §46 – §54).
//!
//! # What this module owns
//! - V1 unit newtypes: `UnderlyingAmount`, `CollateralTokenAmount`,
//!   `WadAmount`, `ChainSlot`, `DurationMs`. Compile-time unit tags
//!   per §44 / §47.1.
//! - Normalized `LendingSnapshot` and its components per §48.
//! - `ProtocolTag` enum (closed; V1 variants only).
//! - Minimal `gate` surface sufficient for read-model tests.
//!
//! # Invariants
//! - Protocol-specific types (e.g., Solend raw structs) MUST NOT appear in
//!   any public signature here. Protocol integration modules live under
//!   `crate::integrations::<protocol>` and map their raw types to the
//!   shapes in this module.
//! - This module MUST NOT import `approval_store`, `park_store`, signer
//!   internals, daemon runtime, completion dispatch, or any protocol
//!   integration submodule.
//! - Dependencies: Solana primitives (`Pubkey`) and the standard library
//!   only.

pub mod gate;
pub mod policy;
pub mod risk_budget;
pub mod snapshot;
pub mod types;

pub use gate::{SystemInvariantError, check_system_invariants};
pub use policy::{
    evaluate_lending_policy, ActionKind, AllowedLendingProtocolsConfig,
    AllowedMintsConfig, EvaluationContext, HardBlockReason, LendingPolicyVerdict,
    LendingRuleConfig, MaxActionInputAmountConfig, MaxOracleStalenessConfig,
    ProposedAction, RequireFreshStateConfig, RuleKind, RuleRejectionDetail,
    ScopeBoundarySubreason, SystemInvariantSubreason,
};
pub use risk_budget::{
    parse_ui_decimal_to_raw, raw_to_ui_decimal, RiskBudgetParseError,
    SolendDepositRiskBudgetConfig,
};
pub use snapshot::{
    ComponentFreshness, FeedPublishFreshness, LendingSnapshot, Obligation,
    ObligationBorrow, ObligationDeposit, OracleFeed, OracleFeedSet, OracleProvider,
    ProtocolTag, Reserve, ReserveLifecycleMarkers, SnapshotFreshness, StaleMarker,
};
pub use types::{ChainSlot, CollateralTokenAmount, DurationMs, UnderlyingAmount, WadAmount};

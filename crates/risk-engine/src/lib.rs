//! `claw-risk-engine` — policy evaluation and risk checks.
//!
//! # Responsibilities
//! - Load and compile `PolicySet` from TOML configuration
//! - Evaluate a `TransactionProposal` + `SimulationResult` against the policy set
//! - Return a `PolicyVerdict` with full reasoning
//! - Track per-session and per-wallet spend for rate limits
//! - Detect anomalous instruction patterns
//!
//! # Design: stateless where possible
//! The core `evaluate` function is a pure function over inputs.
//! Stateful checks (spend caps) use an internal `SpendTracker`.
//!
//! # What does NOT belong here
//! - Signing (that's `claw-wallet-engine`)
//! - RPC calls (that's `claw-solana-core`)
//! - Persistence (that's `claw-state-store`)

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod context;
pub mod errors;
pub mod policy;
pub mod rules;
pub mod spend;

pub use context::PolicyEvaluationContext;
pub use errors::RiskError;
pub use policy::PolicySet;
pub use spend::SpendTracker;

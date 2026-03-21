//! Policy types: rules and verdicts.
//!
//! The `PolicyVerdict` enum is the most important output of the risk engine.
//! It is never a boolean — every verdict carries enough context for the
//! operator to understand exactly what happened and why.

use serde::{Deserialize, Serialize};

/// The verdict produced by evaluating a transaction proposal against the
/// active policy set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum PolicyVerdict {
    /// All checks passed. The transaction may proceed automatically.
    Approved {
        rule_name: String,
    },

    /// All checks passed, but the network or rule configuration requires
    /// explicit operator confirmation before signing.
    RequiresHumanApproval {
        reason: String,
        rule_name: String,
    },

    /// A policy rule explicitly blocked this transaction.
    Rejected {
        reason: String,
        rule_name: String,
    },

    /// Simulation has not been run yet, and the active policy requires it.
    SimulationRequired,

    /// The simulation ran but returned an error; policy blocks further progress.
    SimulationFailed {
        simulation_error: String,
    },
}

impl PolicyVerdict {
    /// Returns `true` if the transaction can proceed without human input.
    pub fn is_auto_approved(&self) -> bool {
        matches!(self, PolicyVerdict::Approved { .. })
    }

    /// Returns `true` if execution is blocked (rejected or failed).
    pub fn is_blocked(&self) -> bool {
        matches!(
            self,
            PolicyVerdict::Rejected { .. } | PolicyVerdict::SimulationFailed { .. }
        )
    }

    /// Returns `true` if a human must act before execution can continue.
    pub fn requires_human(&self) -> bool {
        matches!(self, PolicyVerdict::RequiresHumanApproval { .. })
    }

    /// Returns the verdict as a short label for display and audit.
    pub fn label(&self) -> &'static str {
        match self {
            PolicyVerdict::Approved { .. }               => "approved",
            PolicyVerdict::RequiresHumanApproval { .. }  => "requires_human_approval",
            PolicyVerdict::Rejected { .. }               => "rejected",
            PolicyVerdict::SimulationRequired             => "simulation_required",
            PolicyVerdict::SimulationFailed { .. }       => "simulation_failed",
        }
    }
}

/// A single policy rule definition (loaded from TOML config).
/// Rules are evaluated in order; the first matching rule wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Unique rule name (used in verdict and audit records).
    pub name: String,

    /// Human-readable description of what this rule does.
    pub description: String,

    /// The condition that triggers this rule.
    pub condition: PolicyCondition,

    /// The verdict to issue when the condition matches.
    pub action: PolicyAction,
}

/// The condition component of a policy rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyCondition {
    /// Matches if the network is in the given list.
    NetworkIn(Vec<crate::solana::SolanaNetwork>),

    /// Matches if any instruction references a program not in the allowlist.
    ProgramNotInAllowlist,

    /// Matches if the destination pubkey is in the denylist.
    DestinationInDenylist,

    /// Matches if the estimated transaction cost (in SOL) exceeds the threshold.
    CostExceedsSol(f64),

    /// Matches if the session's cumulative spend today exceeds the cap.
    DailySpendExceedsSol(f64),

    /// Matches if simulation was not run successfully.
    SimulationNotPassed,

    /// Always matches (catch-all rule).
    Always,
}

/// The action to take when a policy condition matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Automatically approve.
    Approve,

    /// Require explicit human approval with the given reason.
    RequireHumanApproval { reason: String },

    /// Reject with the given reason.
    Reject { reason: String },
}

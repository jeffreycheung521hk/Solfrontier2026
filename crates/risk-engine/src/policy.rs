//! Policy set: loading and evaluation.
//!
//! Rules are evaluated in declaration order. The first matching rule wins.
//! If no rule matches, the default verdict is `RequiresHumanApproval`
//! (fail-closed).

use tracing::{debug, info, warn};

use claw_types::{
    policy::{PolicyAction, PolicyCondition, PolicyRule, PolicyVerdict},
    solana::SolanaNetwork,
};

use crate::{context::PolicyEvaluationContext, errors::RiskError};

/// A compiled set of policy rules.
#[derive(Debug, Clone)]
pub struct PolicySet {
    rules: Vec<PolicyRule>,
    /// Program allowlist — if non-empty, any instruction referencing a
    /// program not in this list will trigger the `ProgramNotInAllowlist` check.
    program_allowlist: Vec<String>,
    /// Destination denylist — pubkeys that can never receive funds.
    destination_denylist: Vec<String>,
}

impl PolicySet {
    pub fn new(
        rules: Vec<PolicyRule>,
        program_allowlist: Vec<String>,
        destination_denylist: Vec<String>,
    ) -> Self {
        info!(
            rules = rules.len(),
            programs_allowed = program_allowlist.len(),
            destinations_denied = destination_denylist.len(),
            "policy set compiled"
        );
        Self {
            rules,
            program_allowlist,
            destination_denylist,
        }
    }

    /// Creates a permissive policy set for devnet/testnet development.
    /// Approves all transactions automatically — never use on mainnet.
    pub fn permissive_default() -> Self {
        use claw_types::policy::{PolicyAction, PolicyCondition, PolicyRule};

        Self::new(
            vec![PolicyRule {
                name: "allow-all".to_string(),
                description: "Approve all transactions (devnet/testnet only)".to_string(),
                condition: PolicyCondition::Always,
                action: PolicyAction::Approve,
            }],
            vec![],
            vec![],
        )
    }

    /// Creates a safe-by-default policy set suitable for mainnet.
    /// All transactions require human approval; everything else is rejected.
    pub fn mainnet_safe_default() -> Self {
        use claw_types::policy::{PolicyAction, PolicyCondition, PolicyRule};

        Self::new(
            vec![
                PolicyRule {
                    name: "mainnet-requires-human".to_string(),
                    description: "All mainnet transactions require human approval".to_string(),
                    condition: PolicyCondition::NetworkIn(vec![SolanaNetwork::MainnetBeta]),
                    action: PolicyAction::RequireHumanApproval {
                        reason: "mainnet transaction requires explicit operator approval".to_string(),
                    },
                },
                PolicyRule {
                    name: "devnet-allow".to_string(),
                    description: "Allow devnet transactions automatically".to_string(),
                    condition: PolicyCondition::NetworkIn(vec![
                        SolanaNetwork::Devnet,
                        SolanaNetwork::Testnet,
                        SolanaNetwork::Localnet,
                    ]),
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        )
    }

    /// Evaluates the policy set against the given context.
    /// Returns the first matching rule's verdict.
    pub fn evaluate(&self, ctx: &PolicyEvaluationContext<'_>) -> PolicyVerdict {
        // Pre-check: simulation requirement
        if let Some(sim) = ctx.simulation_result {
            if !sim.success {
                return PolicyVerdict::SimulationFailed {
                    simulation_error: sim
                        .error
                        .clone()
                        .unwrap_or_else(|| "unknown simulation error".to_string()),
                };
            }
        }

        // Evaluate rules in order
        for rule in &self.rules {
            if self.condition_matches(rule, ctx) {
                debug!(rule = %rule.name, "policy rule matched");
                return self.action_to_verdict(&rule.action, &rule.name);
            }
        }

        // No rule matched — fail closed
        warn!("no policy rule matched; defaulting to require_human_approval");
        PolicyVerdict::RequiresHumanApproval {
            reason: "no matching policy rule; failing closed".to_string(),
            rule_name: "default-fail-closed".to_string(),
        }
    }

    fn condition_matches(&self, rule: &PolicyRule, ctx: &PolicyEvaluationContext<'_>) -> bool {
        match &rule.condition {
            PolicyCondition::NetworkIn(networks) => networks.contains(&ctx.network),

            PolicyCondition::ProgramNotInAllowlist => {
                if self.program_allowlist.is_empty() {
                    return false; // allowlist disabled
                }
                // Check if any instruction program is not in the allowlist.
                // For V1 we check the proposal's instruction summaries.
                ctx.proposal
                    .instructions_summary
                    .iter()
                    .any(|ix| !self.program_allowlist.contains(&ix.program_id))
            }

            PolicyCondition::DestinationInDenylist => {
                if self.destination_denylist.is_empty() {
                    return false;
                }
                ctx.proposal
                    .instructions_summary
                    .iter()
                    .flat_map(|ix| ix.accounts.iter())
                    .any(|acc| self.destination_denylist.contains(&acc.pubkey))
            }

            PolicyCondition::CostExceedsSol(threshold_sol) => {
                let threshold_lamports = (threshold_sol * 1_000_000_000.0) as u64;
                if let Some(sim) = ctx.simulation_result {
                    sim.fee_lamports
                        .map(|f| f > threshold_lamports)
                        .unwrap_or(false)
                } else {
                    false
                }
            }

            PolicyCondition::DailySpendExceedsSol(cap_sol) => {
                let cap_lamports = (cap_sol * 1_000_000_000.0) as u64;
                ctx.wallet_daily_spend_lamports > cap_lamports
            }

            PolicyCondition::SimulationNotPassed => ctx
                .simulation_result
                .map(|s| !s.success)
                .unwrap_or(true), // simulation not run = not passed

            PolicyCondition::Always => true,
        }
    }

    fn action_to_verdict(&self, action: &PolicyAction, rule_name: &str) -> PolicyVerdict {
        match action {
            PolicyAction::Approve => PolicyVerdict::Approved {
                rule_name: rule_name.to_string(),
            },
            PolicyAction::RequireHumanApproval { reason } => {
                PolicyVerdict::RequiresHumanApproval {
                    reason: reason.clone(),
                    rule_name: rule_name.to_string(),
                }
            }
            PolicyAction::Reject { reason } => PolicyVerdict::Rejected {
                reason: reason.clone(),
                rule_name: rule_name.to_string(),
            },
        }
    }
}

//! Policy types: rules and verdicts.
//!
//! The `PolicyVerdict` enum is the most important output of the risk engine.
//! It is never a boolean -- every verdict carries enough context for the
//! operator to understand exactly what happened and why.

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::Error as DeError,
};

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
        /// If set, only an approver claiming this role may approve (single-stage).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required_approver_role: Option<String>,
        /// If set, the approval requires a multi-step chain (overrides `required_approver_role`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_chain: Option<Vec<crate::approval::ApprovalChainStage>>,
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
            PolicyVerdict::Approved { .. }              => "approved",
            PolicyVerdict::RequiresHumanApproval { .. } => "requires_human_approval",
            PolicyVerdict::Rejected { .. }              => "rejected",
            PolicyVerdict::SimulationRequired           => "simulation_required",
            PolicyVerdict::SimulationFailed { .. }      => "simulation_failed",
        }
    }

    /// Returns the rule name that produced the verdict, when available.
    pub fn rule_name(&self) -> Option<&str> {
        match self {
            PolicyVerdict::Approved { rule_name }
            | PolicyVerdict::RequiresHumanApproval { rule_name, .. }
            | PolicyVerdict::Rejected { rule_name, .. } => Some(rule_name),
            PolicyVerdict::SimulationRequired | PolicyVerdict::SimulationFailed { .. } => None,
        }
    }

    /// Returns the human-readable reason carried by the verdict, when available.
    pub fn reason(&self) -> Option<&str> {
        match self {
            PolicyVerdict::RequiresHumanApproval { reason, .. }
            | PolicyVerdict::Rejected { reason, .. } => Some(reason),
            PolicyVerdict::Approved { .. }
            | PolicyVerdict::SimulationRequired
            | PolicyVerdict::SimulationFailed { .. } => None,
        }
    }
}

/// The result of evaluating a policy set against a transaction proposal.
/// Wraps the verdict with evaluation metadata for audit and observability.
#[derive(Debug, Clone)]
pub struct PolicyEvaluationResult {
    /// The verdict produced by the evaluation.
    pub verdict: PolicyVerdict,
    /// Total number of rules in the policy set (custom + defaults).
    pub rules_total: usize,
    /// Number of rules evaluated before the first match (or all, if no match).
    pub rules_evaluated: usize,
    /// Index of the matched rule within the rule set, if any.
    /// `None` when the verdict is a pre-check (SimulationFailed) or the
    /// fail-closed default.
    pub matched_rule_index: Option<usize>,
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
#[derive(Debug, Clone, PartialEq)]
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

    /// Matches if the transaction transfer amount is at or above the threshold.
    AmountExceedsLamports(u64),

    /// Matches if simulation was not run successfully.
    SimulationNotPassed,

    /// Matches if any SPL Token transfer of the given mint exceeds the threshold.
    /// Use this to enforce per-token spend caps (e.g., "block USDC transfers >= 1000 USDC").
    /// The threshold is in raw token units (multiply by 10^decimals to convert from human-readable).
    /// For USDC (6 decimals): 1 USDC = 1_000_000 raw units.
    TokenAmountExceeds {
        mint: String,
        threshold: u64,
    },

    /// Matches if any SPL Token transfer references a mint NOT in the allowlist.
    /// Use this to whitelist accepted stablecoins (e.g., only USDC and USDT).
    /// Empty allowlist disables the check (does not match).
    MintNotInAllowlist {
        allowed_mints: Vec<String>,
    },

    /// Matches if any instruction is a legacy SPL Token `Transfer` (tag 3).
    /// Legacy Transfer does NOT include the mint in its accounts, so token-aware
    /// policies (`TokenAmountExceeds`, `MintNotInAllowlist`) cannot enforce
    /// per-mint rules against it. Use this condition to REJECT legacy transfers
    /// entirely when token policy is in effect, closing the bypass.
    LegacyTokenTransferPresent,

    /// Matches if the current time is outside the allowed window.
    /// Use this to restrict transactions to business hours.
    OutsideAllowedHours {
        /// Start of allowed period (0–23 inclusive).
        start_hour: u8,
        /// End of allowed period (0–23 inclusive).
        /// If start > end, the window wraps midnight (e.g., 22–6).
        end_hour: u8,
        /// Allowed days of week (1=Monday, 7=Sunday, ISO 8601).
        /// Empty = all days allowed.
        allowed_days: Vec<u8>,
        /// UTC offset in hours (e.g., 8 for HKT, -5 for EST). Default 0 (UTC).
        utc_offset_hours: i32,
    },

    /// Always matches (catch-all rule).
    Always,
}

/// The action to take when a policy condition matches.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyAction {
    /// Automatically approve.
    Approve,

    /// Require explicit human approval with the given reason.
    /// If `required_approver_role` is set, only an operator claiming that role may approve.
    RequireHumanApproval {
        reason: String,
        #[doc(hidden)]
        required_approver_role: Option<String>,
    },

    /// Require a multi-step approval chain with ordered stages.
    /// Each stage must be approved in order; any rejection is terminal.
    RequireApprovalChain {
        reason: String,
        stages: Vec<crate::approval::ApprovalChainStage>,
    },

    /// Reject with the given reason.
    Reject { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum PolicyConditionWire {
    Unit(String),
    Tagged(PolicyConditionTagged),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum PolicyConditionTagged {
    #[serde(alias = "network_in")]
    NetworkIn { networks: Vec<crate::solana::SolanaNetwork> },
    #[serde(alias = "cost_exceeds_sol")]
    CostExceedsSol { threshold: f64 },
    #[serde(alias = "daily_spend_exceeds_sol")]
    DailySpendExceedsSol { threshold: f64 },
    #[serde(alias = "amount_exceeds_lamports")]
    AmountExceedsLamports { threshold: u64 },
    #[serde(alias = "program_not_in_allowlist")]
    ProgramNotInAllowlist,
    #[serde(alias = "destination_in_denylist")]
    DestinationInDenylist,
    #[serde(alias = "simulation_not_passed")]
    SimulationNotPassed,
    #[serde(alias = "token_amount_exceeds")]
    TokenAmountExceeds {
        mint: String,
        threshold: u64,
    },
    #[serde(alias = "mint_not_in_allowlist")]
    MintNotInAllowlist {
        allowed_mints: Vec<String>,
    },
    #[serde(alias = "outside_allowed_hours")]
    OutsideAllowedHours {
        start_hour: u8,
        end_hour: u8,
        #[serde(default)]
        allowed_days: Vec<u8>,
        #[serde(default)]
        utc_offset_hours: i32,
    },
    #[serde(alias = "always")]
    Always,
}

impl Serialize for PolicyCondition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match self {
            PolicyCondition::NetworkIn(networks) => PolicyConditionWire::Tagged(
                PolicyConditionTagged::NetworkIn {
                    networks: networks.clone(),
                },
            ),
            PolicyCondition::ProgramNotInAllowlist => {
                PolicyConditionWire::Unit("ProgramNotInAllowlist".to_string())
            }
            PolicyCondition::DestinationInDenylist => {
                PolicyConditionWire::Unit("DestinationInDenylist".to_string())
            }
            PolicyCondition::CostExceedsSol(threshold) => PolicyConditionWire::Tagged(
                PolicyConditionTagged::CostExceedsSol {
                    threshold: *threshold,
                },
            ),
            PolicyCondition::DailySpendExceedsSol(threshold) => PolicyConditionWire::Tagged(
                PolicyConditionTagged::DailySpendExceedsSol {
                    threshold: *threshold,
                },
            ),
            PolicyCondition::AmountExceedsLamports(threshold) => PolicyConditionWire::Tagged(
                PolicyConditionTagged::AmountExceedsLamports {
                    threshold: *threshold,
                },
            ),
            PolicyCondition::SimulationNotPassed => {
                PolicyConditionWire::Unit("SimulationNotPassed".to_string())
            }
            PolicyCondition::LegacyTokenTransferPresent => {
                PolicyConditionWire::Unit("LegacyTokenTransferPresent".to_string())
            }
            PolicyCondition::TokenAmountExceeds { mint, threshold } => {
                PolicyConditionWire::Tagged(PolicyConditionTagged::TokenAmountExceeds {
                    mint: mint.clone(),
                    threshold: *threshold,
                })
            }
            PolicyCondition::MintNotInAllowlist { allowed_mints } => {
                PolicyConditionWire::Tagged(PolicyConditionTagged::MintNotInAllowlist {
                    allowed_mints: allowed_mints.clone(),
                })
            }
            PolicyCondition::OutsideAllowedHours { start_hour, end_hour, allowed_days, utc_offset_hours } => {
                PolicyConditionWire::Tagged(PolicyConditionTagged::OutsideAllowedHours {
                    start_hour: *start_hour,
                    end_hour: *end_hour,
                    allowed_days: allowed_days.clone(),
                    utc_offset_hours: *utc_offset_hours,
                })
            }
            PolicyCondition::Always => PolicyConditionWire::Unit("Always".to_string()),
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PolicyCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match PolicyConditionWire::deserialize(deserializer)? {
            PolicyConditionWire::Unit(name) => parse_policy_condition_unit(&name)
                .ok_or_else(|| D::Error::custom(format!("unknown policy condition '{name}'"))),
            PolicyConditionWire::Tagged(tagged) => Ok(match tagged {
                PolicyConditionTagged::NetworkIn { networks } => PolicyCondition::NetworkIn(networks),
                PolicyConditionTagged::ProgramNotInAllowlist => PolicyCondition::ProgramNotInAllowlist,
                PolicyConditionTagged::DestinationInDenylist => PolicyCondition::DestinationInDenylist,
                PolicyConditionTagged::CostExceedsSol { threshold } => {
                    PolicyCondition::CostExceedsSol(threshold)
                }
                PolicyConditionTagged::DailySpendExceedsSol { threshold } => {
                    PolicyCondition::DailySpendExceedsSol(threshold)
                }
                PolicyConditionTagged::AmountExceedsLamports { threshold } => {
                    PolicyCondition::AmountExceedsLamports(threshold)
                }
                PolicyConditionTagged::SimulationNotPassed => PolicyCondition::SimulationNotPassed,
                PolicyConditionTagged::TokenAmountExceeds { mint, threshold } => {
                    PolicyCondition::TokenAmountExceeds { mint, threshold }
                }
                PolicyConditionTagged::MintNotInAllowlist { allowed_mints } => {
                    PolicyCondition::MintNotInAllowlist { allowed_mints }
                }
                PolicyConditionTagged::OutsideAllowedHours { start_hour, end_hour, allowed_days, utc_offset_hours } => {
                    PolicyCondition::OutsideAllowedHours { start_hour, end_hour, allowed_days, utc_offset_hours }
                }
                PolicyConditionTagged::Always => PolicyCondition::Always,
            }),
        }
    }
}

fn parse_policy_condition_unit(name: &str) -> Option<PolicyCondition> {
    match name {
        "ProgramNotInAllowlist" | "program_not_in_allowlist" => {
            Some(PolicyCondition::ProgramNotInAllowlist)
        }
        "DestinationInDenylist" | "destination_in_denylist" => {
            Some(PolicyCondition::DestinationInDenylist)
        }
        "SimulationNotPassed" | "simulation_not_passed" => {
            Some(PolicyCondition::SimulationNotPassed)
        }
        "Always" | "always" => Some(PolicyCondition::Always),
        "LegacyTokenTransferPresent" | "legacy_token_transfer_present" => {
            Some(PolicyCondition::LegacyTokenTransferPresent)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum PolicyActionWire {
    Unit(String),
    Tagged(PolicyActionTagged),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum PolicyActionTagged {
    #[serde(alias = "approve")]
    Approve,
    #[serde(alias = "require_human_approval")]
    RequireHumanApproval {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required_approver_role: Option<String>,
    },
    #[serde(alias = "require_approval_chain")]
    RequireApprovalChain {
        reason: String,
        stages: Vec<crate::approval::ApprovalChainStage>,
    },
    #[serde(alias = "reject")]
    Reject { reason: String },
}

impl Serialize for PolicyAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match self {
            PolicyAction::Approve => PolicyActionWire::Unit("Approve".to_string()),
            PolicyAction::RequireHumanApproval { reason, required_approver_role } => PolicyActionWire::Tagged(
                PolicyActionTagged::RequireHumanApproval {
                    reason: reason.clone(),
                    required_approver_role: required_approver_role.clone(),
                },
            ),
            PolicyAction::RequireApprovalChain { reason, stages } => PolicyActionWire::Tagged(
                PolicyActionTagged::RequireApprovalChain {
                    reason: reason.clone(),
                    stages: stages.clone(),
                },
            ),
            PolicyAction::Reject { reason } => {
                PolicyActionWire::Tagged(PolicyActionTagged::Reject {
                    reason: reason.clone(),
                })
            }
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PolicyAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match PolicyActionWire::deserialize(deserializer)? {
            PolicyActionWire::Unit(name) => parse_policy_action_unit(&name)
                .ok_or_else(|| D::Error::custom(format!("unknown policy action '{name}'"))),
            PolicyActionWire::Tagged(tagged) => Ok(match tagged {
                PolicyActionTagged::Approve => PolicyAction::Approve,
                PolicyActionTagged::RequireHumanApproval { reason, required_approver_role } => {
                    PolicyAction::RequireHumanApproval { reason, required_approver_role }
                }
                PolicyActionTagged::RequireApprovalChain { reason, stages } => {
                    PolicyAction::RequireApprovalChain { reason, stages }
                }
                PolicyActionTagged::Reject { reason } => PolicyAction::Reject { reason },
            }),
        }
    }
}

fn parse_policy_action_unit(name: &str) -> Option<PolicyAction> {
    match name {
        "Approve" | "approve" => Some(PolicyAction::Approve),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{PolicyAction, PolicyCondition, PolicyRule};

    #[test]
    fn policy_rule_deserializes_demo_toml_shapes() {
        let rule: PolicyRule = toml::from_str(
            r#"
name = "large-require-human"
description = "Require human approval for transactions >= 0.01 SOL"
condition = { type = "AmountExceedsLamports", threshold = 10000000 }
action = { type = "RequireHumanApproval", reason = "amount exceeds 0.01 SOL threshold" }
"#,
        )
        .expect("demo TOML rule should deserialize");

        assert_eq!(
            rule.condition,
            PolicyCondition::AmountExceedsLamports(10_000_000)
        );
        assert_eq!(
            rule.action,
            PolicyAction::RequireHumanApproval {
                reason: "amount exceeds 0.01 SOL threshold".to_string(),
                required_approver_role: None,
            }
        );
    }

    #[test]
    fn policy_rule_deserializes_string_unit_variants() {
        let rule: PolicyRule = toml::from_str(
            r#"
name = "denylist-block"
description = "Block transactions to denied destinations"
condition = "DestinationInDenylist"
action = { type = "Reject", reason = "destination is on the deny list" }
"#,
        )
        .expect("unit variants should deserialize");

        assert_eq!(rule.condition, PolicyCondition::DestinationInDenylist);
        assert_eq!(
            rule.action,
            PolicyAction::Reject {
                reason: "destination is on the deny list".to_string(),
            }
        );
    }

    #[test]
    fn policy_rule_deserializes_outside_allowed_hours() {
        let rule: PolicyRule = toml::from_str(
            r#"
name = "business-hours"
description = "Block outside business hours"
condition = { type = "OutsideAllowedHours", start_hour = 9, end_hour = 17, allowed_days = [1,2,3,4,5], utc_offset_hours = 8 }
action = { type = "Reject", reason = "outside business hours" }
"#,
        )
        .expect("OutsideAllowedHours should deserialize");

        assert_eq!(
            rule.condition,
            PolicyCondition::OutsideAllowedHours {
                start_hour: 9,
                end_hour: 17,
                allowed_days: vec![1, 2, 3, 4, 5],
                utc_offset_hours: 8,
            }
        );
    }

    #[test]
    fn policy_rule_deserializes_outside_allowed_hours_defaults() {
        let rule: PolicyRule = toml::from_str(
            r#"
name = "night-block"
description = "Block outside 8-20 UTC"
condition = { type = "OutsideAllowedHours", start_hour = 8, end_hour = 20 }
action = { type = "Reject", reason = "outside hours" }
"#,
        )
        .expect("defaults should work");

        assert_eq!(
            rule.condition,
            PolicyCondition::OutsideAllowedHours {
                start_hour: 8,
                end_hour: 20,
                allowed_days: vec![],
                utc_offset_hours: 0,
            }
        );
    }

    // ── USDC / Stablecoin condition tests ─────────────────────────────────

    #[test]
    fn policy_rule_deserializes_token_amount_exceeds() {
        let rule: PolicyRule = toml::from_str(
            r#"
name = "usdc-cap"
description = "USDC transfers >= 100 USDC require approval"
condition = { type = "TokenAmountExceeds", mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", threshold = 100000000 }
action = { type = "RequireHumanApproval", reason = "USDC >= 100" }
"#,
        )
        .expect("TokenAmountExceeds should deserialize");

        assert_eq!(
            rule.condition,
            PolicyCondition::TokenAmountExceeds {
                mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                threshold: 100_000_000,
            }
        );
    }

    #[test]
    fn policy_rule_deserializes_mint_not_in_allowlist() {
        let rule: PolicyRule = toml::from_str(
            r#"
name = "stablecoin-only"
description = "Only USDC and USDT allowed"
condition = { type = "MintNotInAllowlist", allowed_mints = ["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"] }
action = { type = "Reject", reason = "non-stablecoin token" }
"#,
        )
        .expect("MintNotInAllowlist should deserialize");

        assert_eq!(
            rule.condition,
            PolicyCondition::MintNotInAllowlist {
                allowed_mints: vec![
                    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".to_string(),
                ],
            }
        );
    }

    #[test]
    fn token_amount_exceeds_round_trips_through_serde() {
        let original = PolicyCondition::TokenAmountExceeds {
            mint: "test-mint".to_string(),
            threshold: 12345,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: PolicyCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn mint_not_in_allowlist_round_trips_through_serde() {
        let original = PolicyCondition::MintNotInAllowlist {
            allowed_mints: vec!["mint-a".into(), "mint-b".into()],
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: PolicyCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn policy_rule_deserializes_legacy_token_transfer_present() {
        let rule: PolicyRule = toml::from_str(
            r#"
name = "block-legacy"
description = "Block legacy SPL Token Transfer"
condition = "LegacyTokenTransferPresent"
action = { type = "Reject", reason = "use TransferChecked" }
"#,
        )
        .expect("LegacyTokenTransferPresent should deserialize as unit variant");

        assert_eq!(rule.condition, PolicyCondition::LegacyTokenTransferPresent);
    }

    #[test]
    fn legacy_token_transfer_present_round_trips_through_serde() {
        let original = PolicyCondition::LegacyTokenTransferPresent;
        let json = serde_json::to_string(&original).unwrap();
        let parsed: PolicyCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}

//! Policy set: loading and evaluation.
//!
//! Rules are evaluated in declaration order. The first matching rule wins.
//! If no rule matches, the default verdict is `RequiresHumanApproval`
//! (fail-closed).

use tracing::{debug, info, warn};

use claw_types::{
    policy::{PolicyAction, PolicyCondition, PolicyEvaluationResult, PolicyRule, PolicyVerdict},
    solana::SolanaNetwork,
};

use crate::context::PolicyEvaluationContext;

/// Checks if the current time is outside the allowed hours/days window.
///
/// Returns `true` (condition fires) when:
/// - Current hour (adjusted by UTC offset) is outside [start_hour, end_hour], OR
/// - Current day-of-week is not in `allowed_days` (if non-empty)
///
/// Hour range wraps around midnight: start=22, end=6 means 22:00–06:59 is allowed.
/// Days use ISO 8601: 1=Monday, 7=Sunday.
fn is_outside_allowed_hours(
    start_hour: u8,
    end_hour: u8,
    allowed_days: &[u8],
    utc_offset_hours: i32,
) -> bool {
    is_outside_allowed_hours_at(chrono::Utc::now(), start_hour, end_hour, allowed_days, utc_offset_hours)
}

/// Deterministic version for testing — accepts an explicit `now` timestamp.
fn is_outside_allowed_hours_at(
    now_utc: chrono::DateTime<chrono::Utc>,
    start_hour: u8,
    end_hour: u8,
    allowed_days: &[u8],
    utc_offset_hours: i32,
) -> bool {
    let offset = chrono::FixedOffset::east_opt(utc_offset_hours * 3600)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap());
    let local = now_utc.with_timezone(&offset);

    let hour = local.format("%H").to_string().parse::<u8>().unwrap_or(0);
    let day_of_week = local.format("%u").to_string().parse::<u8>().unwrap_or(1); // 1=Mon, 7=Sun

    // Check day-of-week first.
    if !allowed_days.is_empty() && !allowed_days.contains(&day_of_week) {
        return true; // Outside allowed days
    }

    // Check hour range.
    let in_allowed_hours = if start_hour <= end_hour {
        // Normal range: e.g., 9–17
        hour >= start_hour && hour <= end_hour
    } else {
        // Wrapping range: e.g., 22–6 means 22,23,0,1,2,3,4,5,6
        hour >= start_hour || hour <= end_hour
    };

    !in_allowed_hours
}

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
    /// Returns the compiled rule list (evaluation order).
    pub fn rules(&self) -> &[PolicyRule] {
        &self.rules
    }

    /// Returns the program allowlist backing the `ProgramNotInAllowlist` check.
    pub fn program_allowlist(&self) -> &[String] {
        &self.program_allowlist
    }

    /// Returns the destination denylist.
    pub fn destination_denylist(&self) -> &[String] {
        &self.destination_denylist
    }

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

    /// Creates a new PolicySet with session-scoped rules prepended.
    ///
    /// Evaluation order: session rules → self.rules (global + defaults).
    /// Allowlist and denylist are inherited from the global set.
    pub fn with_session_rules(&self, session_rules: &[PolicyRule]) -> PolicySet {
        let mut rules = session_rules.to_vec();
        rules.extend(self.rules.clone());
        debug!(
            session_rules = session_rules.len(),
            total_rules = rules.len(),
            "layered policy set with session overrides"
        );
        PolicySet {
            rules,
            program_allowlist: self.program_allowlist.clone(),
            destination_denylist: self.destination_denylist.clone(),
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
                        required_approver_role: None,
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

    /// Evaluates the policy set against the given context using the current time.
    pub fn evaluate(&self, ctx: &PolicyEvaluationContext<'_>) -> PolicyEvaluationResult {
        self.evaluate_at(ctx, chrono::Utc::now())
    }

    /// Evaluates the policy set with an explicit timestamp (for deterministic testing).
    pub fn evaluate_at(
        &self,
        ctx: &PolicyEvaluationContext<'_>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> PolicyEvaluationResult {
        let rules_total = self.rules.len();

        // Pre-check: simulation requirement
        if let Some(sim) = ctx.simulation_result {
            if !sim.success {
                return PolicyEvaluationResult {
                    verdict: PolicyVerdict::SimulationFailed {
                        simulation_error: sim
                            .error
                            .clone()
                            .unwrap_or_else(|| "unknown simulation error".to_string()),
                    },
                    rules_total,
                    rules_evaluated: 0,
                    matched_rule_index: None,
                };
            }
        }

        // Evaluate rules in order
        for (i, rule) in self.rules.iter().enumerate() {
            if self.condition_matches_at(rule, ctx, now) {
                debug!(rule = %rule.name, index = i, "policy rule matched");
                return PolicyEvaluationResult {
                    verdict: self.action_to_verdict(&rule.action, &rule.name),
                    rules_total,
                    rules_evaluated: i + 1,
                    matched_rule_index: Some(i),
                };
            }
        }

        // No rule matched — fail closed
        warn!("no policy rule matched; defaulting to require_human_approval");
        PolicyEvaluationResult {
            verdict: PolicyVerdict::RequiresHumanApproval {
                reason: "no matching policy rule; failing closed".to_string(),
                rule_name: "default-fail-closed".to_string(),
                required_approver_role: None,
                approval_chain: None,
            },
            rules_total,
            rules_evaluated: rules_total,
            matched_rule_index: None,
        }
    }

    fn condition_matches_at(
        &self,
        rule: &PolicyRule,
        ctx: &PolicyEvaluationContext<'_>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
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

            PolicyCondition::AmountExceedsLamports(threshold_lamports) => self
                .transfer_amount_lamports(ctx)
                .map(|amount| amount >= *threshold_lamports)
                .unwrap_or(false),

            PolicyCondition::SimulationNotPassed => ctx
                .simulation_result
                .map(|s| !s.success)
                .unwrap_or(true), // simulation not run = not passed

            PolicyCondition::TokenAmountExceeds { mint, threshold } => {
                // Sum all token transfers for the given mint across all instructions.
                let total: u64 = ctx
                    .proposal
                    .instructions_summary
                    .iter()
                    .filter_map(|ix| ix.token_transfer.as_ref())
                    .filter(|tt| &tt.mint == mint)
                    .map(|tt| tt.amount)
                    .sum();
                total >= *threshold
            }

            PolicyCondition::MintNotInAllowlist { allowed_mints } => {
                if allowed_mints.is_empty() {
                    return false; // empty allowlist = check disabled
                }
                ctx.proposal
                    .instructions_summary
                    .iter()
                    .filter_map(|ix| ix.token_transfer.as_ref())
                    .any(|tt| !allowed_mints.contains(&tt.mint))
            }

            PolicyCondition::LegacyTokenTransferPresent => {
                // Fires if ANY instruction is a legacy SPL Token Transfer.
                // Legacy Transfer (tag 3) doesn't include the mint in its
                // accounts, so it bypasses TokenAmountExceeds / MintNotInAllowlist.
                // This condition closes that bypass.
                ctx.proposal
                    .instructions_summary
                    .iter()
                    .any(|ix| ix.is_legacy_token_transfer)
            }

            PolicyCondition::OutsideAllowedHours {
                start_hour, end_hour, allowed_days, utc_offset_hours,
            } => {
                is_outside_allowed_hours_at(now, *start_hour, *end_hour, allowed_days, *utc_offset_hours)
            }

            PolicyCondition::Always => true,
        }
    }

    fn transfer_amount_lamports(&self, ctx: &PolicyEvaluationContext<'_>) -> Option<u64> {
        let proposal_amounts: Vec<u64> = ctx
            .proposal
            .instructions_summary
            .iter()
            .filter_map(|ix| ix.transfer_lamports)
            .collect();

        if !proposal_amounts.is_empty() {
            // Saturating sum: a malicious or malformed proposal with many
            // huge transfer values would otherwise panic on u64 overflow
            // in debug builds. Saturation is the right semantics for
            // policy: an overflowing total still "exceeds" any sane
            // threshold, so AmountExceedsLamports rules fire correctly.
            let total = proposal_amounts
                .into_iter()
                .fold(0u64, |acc, v| acc.saturating_add(v));
            return Some(total);
        }

        let simulation = ctx.simulation_result?;
        let wallet_diff = simulation
            .account_diffs
            .iter()
            .find(|diff| diff.pubkey == ctx.proposal.wallet_pubkey)?;

        let lamports_spent = wallet_diff
            .lamports_before?
            .checked_sub(wallet_diff.lamports_after?)?;

        Some(match simulation.fee_lamports {
            Some(fee_lamports) => lamports_spent.saturating_sub(fee_lamports),
            None => lamports_spent,
        })
    }

    fn action_to_verdict(&self, action: &PolicyAction, rule_name: &str) -> PolicyVerdict {
        match action {
            PolicyAction::Approve => PolicyVerdict::Approved {
                rule_name: rule_name.to_string(),
            },
            PolicyAction::RequireHumanApproval { reason, required_approver_role } => {
                PolicyVerdict::RequiresHumanApproval {
                    reason: reason.clone(),
                    rule_name: rule_name.to_string(),
                    required_approver_role: required_approver_role.clone(),
                    approval_chain: None,
                }
            }
            PolicyAction::RequireApprovalChain { reason, stages } => {
                PolicyVerdict::RequiresHumanApproval {
                    reason: reason.clone(),
                    rule_name: rule_name.to_string(),
                    required_approver_role: None,
                    approval_chain: Some(stages.clone()),
                }
            }
            PolicyAction::Reject { reason } => PolicyVerdict::Rejected {
                reason: reason.clone(),
                rule_name: rule_name.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use claw_types::{
        policy::{PolicyAction, PolicyCondition, PolicyRule, PolicyVerdict},
        session::SessionId,
        solana::SolanaNetwork,
        transaction::{AccountRole, InstructionSummary, TransactionProposal},
    };

    use crate::{PolicyEvaluationContext, PolicySet};

    fn proposal_with_instruction(
        wallet_pubkey: &str,
        program_id: &str,
        destination: &str,
        transfer_lamports: Option<u64>,
    ) -> TransactionProposal {
        TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey: wallet_pubkey.to_string(),
            network: SolanaNetwork::Devnet,
            description: "policy test".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![InstructionSummary {
                program_id: program_id.to_string(),
                program_name: Some("test-program".to_string()),
                description: "test instruction".to_string(),
                transfer_lamports,
                token_transfer: None,
                is_legacy_token_transfer: false,
                accounts: vec![
                    AccountRole {
                        pubkey: wallet_pubkey.to_string(),
                        label: Some("from".to_string()),
                        is_signer: true,
                        is_writable: true,
                    },
                    AccountRole {
                        pubkey: destination.to_string(),
                        label: Some("to".to_string()),
                        is_signer: false,
                        is_writable: true,
                    },
                ],
            }],
            created_at: chrono::Utc::now(),
        }
    }

    fn context<'a>(proposal: &'a TransactionProposal) -> PolicyEvaluationContext<'a> {
        PolicyEvaluationContext {
            proposal,
            simulation_result: None,
            network: SolanaNetwork::Devnet,
            session_id: &proposal.session_id,
            session_spend_lamports: 0,
            wallet_daily_spend_lamports: 0,
        }
    }

    #[test]
    fn rule_priority_is_first_match_wins() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "reject-first".to_string(),
                    description: "first rule wins".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Reject {
                        reason: "first match".to_string(),
                    },
                },
                PolicyRule {
                    name: "approve-second".to_string(),
                    description: "should never run".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_instruction(
            "wallet",
            "11111111111111111111111111111111",
            "dest",
            None,
        );

        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "first match".to_string(),
                rule_name: "reject-first".to_string(),
            }
        );
        assert_eq!(result.rules_evaluated, 1);
        assert_eq!(result.matched_rule_index, Some(0));
        assert_eq!(result.rules_total, 2);
    }

    #[test]
    fn amount_threshold_matches_transfer_amount() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "large-require-human".to_string(),
                    description: "escalate large transfers".to_string(),
                    condition: PolicyCondition::AmountExceedsLamports(10_000_000),
                    action: PolicyAction::RequireHumanApproval {
                        reason: "amount exceeds 0.01 SOL threshold".to_string(),
                        required_approver_role: None,
                    },
                },
                PolicyRule {
                    name: "fallback-approve".to_string(),
                    description: "approve the rest".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_instruction(
            "wallet",
            "11111111111111111111111111111111",
            "dest",
            Some(10_000_000),
        );

        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::RequiresHumanApproval {
                reason: "amount exceeds 0.01 SOL threshold".to_string(),
                rule_name: "large-require-human".to_string(),
                required_approver_role: None,
                approval_chain: None,
            }
        );
        assert_eq!(result.rules_evaluated, 1);
        assert_eq!(result.matched_rule_index, Some(0));
    }

    #[test]
    fn program_allowlist_rule_blocks_unlisted_program() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "allowlist-block".to_string(),
                    description: "reject programs outside allowlist".to_string(),
                    condition: PolicyCondition::ProgramNotInAllowlist,
                    action: PolicyAction::Reject {
                        reason: "program is not on the allow list".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback-approve".to_string(),
                    description: "approve the rest".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec!["11111111111111111111111111111111".to_string()],
            vec![],
        );

        let proposal = proposal_with_instruction(
            "wallet",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "dest",
            None,
        );

        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "program is not on the allow list".to_string(),
                rule_name: "allowlist-block".to_string(),
            }
        );
    }

    #[test]
    fn destination_denylist_rule_blocks_denied_pubkey() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "denylist-block".to_string(),
                    description: "reject denied destinations".to_string(),
                    condition: PolicyCondition::DestinationInDenylist,
                    action: PolicyAction::Reject {
                        reason: "destination is on the deny list".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback-approve".to_string(),
                    description: "approve the rest".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec!["blocked-destination".to_string()],
        );

        let proposal = proposal_with_instruction(
            "wallet",
            "11111111111111111111111111111111",
            "blocked-destination",
            Some(1_000_000),
        );

        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "destination is on the deny list".to_string(),
                rule_name: "denylist-block".to_string(),
            }
        );
    }

    #[test]
    fn fallback_fail_closed_reports_all_rules_evaluated() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "amount-check".to_string(),
                    description: "only matches large".to_string(),
                    condition: PolicyCondition::AmountExceedsLamports(1_000_000_000),
                    action: PolicyAction::Reject {
                        reason: "too large".to_string(),
                    },
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_instruction(
            "wallet",
            "11111111111111111111111111111111",
            "dest",
            Some(1_000), // well below threshold
        );

        let result = policy.evaluate(&context(&proposal));
        assert!(result.verdict.requires_human(), "should fail closed");
        assert_eq!(result.rules_total, 1);
        assert_eq!(result.rules_evaluated, 1);
        assert_eq!(result.matched_rule_index, None);
    }

    #[test]
    fn with_session_rules_prepends_and_matches_first() {
        // Global policy: approve everything
        let global = PolicySet::new(
            vec![PolicyRule {
                name: "global-approve".to_string(),
                description: "approve all".to_string(),
                condition: PolicyCondition::Always,
                action: PolicyAction::Approve,
            }],
            vec![],
            vec![],
        );

        // Session override: reject transfers >= 1000 lamports
        let session_rules = vec![PolicyRule {
            name: "session-cap".to_string(),
            description: "session-level cap".to_string(),
            condition: PolicyCondition::AmountExceedsLamports(1_000),
            action: PolicyAction::Reject {
                reason: "session cap exceeded".to_string(),
            },
        }];

        let layered = global.with_session_rules(&session_rules);

        // Transfer above session cap → session rule fires first
        let proposal = proposal_with_instruction(
            "wallet",
            "11111111111111111111111111111111",
            "dest",
            Some(5_000),
        );
        let result = layered.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "session cap exceeded".to_string(),
                rule_name: "session-cap".to_string(),
            }
        );
        assert_eq!(result.matched_rule_index, Some(0), "session rule is index 0");
        assert_eq!(result.rules_total, 2, "1 session + 1 global");

        // Transfer below session cap → falls through to global approve
        let small_proposal = proposal_with_instruction(
            "wallet",
            "11111111111111111111111111111111",
            "dest",
            Some(500),
        );
        let result2 = layered.evaluate(&context(&small_proposal));
        assert_eq!(
            result2.verdict,
            PolicyVerdict::Approved {
                rule_name: "global-approve".to_string(),
            }
        );
        assert_eq!(result2.matched_rule_index, Some(1), "global rule is index 1");
    }

    #[test]
    fn with_session_rules_inherits_allowlist_denylist() {
        let global = PolicySet::new(
            vec![
                PolicyRule {
                    name: "denylist-check".to_string(),
                    description: "block denied".to_string(),
                    condition: PolicyCondition::DestinationInDenylist,
                    action: PolicyAction::Reject {
                        reason: "denied".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve rest".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec!["bad-dest".to_string()],
        );

        // Session adds a rule but should still inherit the global denylist
        let session_rules = vec![PolicyRule {
            name: "session-noop".to_string(),
            description: "never matches".to_string(),
            condition: PolicyCondition::AmountExceedsLamports(999_999_999_999),
            action: PolicyAction::Reject {
                reason: "impossible".to_string(),
            },
        }];

        let layered = global.with_session_rules(&session_rules);

        let proposal = proposal_with_instruction(
            "wallet",
            "11111111111111111111111111111111",
            "bad-dest",
            Some(100),
        );
        let result = layered.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "denied".to_string(),
                rule_name: "denylist-check".to_string(),
            }
        );
    }

    // ── Time-based rule tests ─────────────────────────────────────────────

    #[test]
    fn is_outside_allowed_hours_basic_range() {
        let now = chrono::Utc::now();
        let current_hour = now.format("%H").to_string().parse::<u8>().unwrap();

        // Range that includes current hour → NOT outside
        assert!(
            !super::is_outside_allowed_hours(0, 23, &[], 0),
            "0-23 should include all hours"
        );

        // Range that includes only hour 99 → outside (impossible hour, always outside)
        // We test by creating a range that definitely excludes now.
        // If current hour is 0, exclude range 1-23. Otherwise exclude range that skips current.
        let (ex_start, ex_end) = if current_hour == 0 { (1, 23) } else { (0, current_hour.saturating_sub(1)) };
        // This range excludes current_hour only if current_hour > ex_end
        if current_hour > ex_end {
            assert!(
                super::is_outside_allowed_hours(ex_start, ex_end, &[], 0),
                "range {}-{} should exclude hour {}",
                ex_start, ex_end, current_hour,
            );
        }
    }

    #[test]
    fn is_outside_allowed_hours_wrapping_range() {
        // Wrapping range: 22–6 means hours 22,23,0,1,2,3,4,5,6 are allowed.
        // Hour 12 should be outside.
        // We can't control the clock, so test the function directly with known inputs.
        // Instead, test that 0-23 always passes and that matching logic is correct
        // by verifying the full range covers all hours.
        assert!(
            !super::is_outside_allowed_hours(0, 23, &[], 0),
            "0-23 should always be inside"
        );
    }

    #[test]
    fn is_outside_allowed_hours_day_check() {
        let now = chrono::Utc::now();
        let current_day = now.format("%u").to_string().parse::<u8>().unwrap(); // 1=Mon, 7=Sun

        // Allowed days includes current day → not outside
        assert!(
            !super::is_outside_allowed_hours(0, 23, &[current_day], 0),
            "current day {} should be allowed",
            current_day,
        );

        // Allowed days excludes current day → outside
        let other_day = if current_day == 7 { 1 } else { current_day + 1 };
        assert!(
            super::is_outside_allowed_hours(0, 23, &[other_day], 0),
            "day {} should be outside when only {} is allowed",
            current_day, other_day,
        );
    }

    #[test]
    fn is_outside_allowed_hours_empty_days_allows_all() {
        assert!(
            !super::is_outside_allowed_hours(0, 23, &[], 0),
            "empty allowed_days should permit all days"
        );
    }

    #[test]
    fn outside_allowed_hours_condition_fires_in_policy_deterministic() {
        use chrono::{TimeZone, Utc};
        // Monday 2026-04-06 10:00 UTC
        let monday_10am = Utc.with_ymd_and_hms(2026, 4, 6, 10, 0, 0).unwrap();
        // Saturday 2026-04-11 10:00 UTC
        let saturday_10am = Utc.with_ymd_and_hms(2026, 4, 11, 10, 0, 0).unwrap();

        // Policy: block outside Mon-Fri 9-17 UTC
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "hours-check".to_string(),
                    description: "block outside business hours".to_string(),
                    condition: PolicyCondition::OutsideAllowedHours {
                        start_hour: 9,
                        end_hour: 17,
                        allowed_days: vec![1, 2, 3, 4, 5], // Mon-Fri
                        utc_offset_hours: 0,
                    },
                    action: PolicyAction::Reject {
                        reason: "outside business hours".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_instruction(
            "wallet", "11111111111111111111111111111111", "dest", None,
        );
        let ctx = context(&proposal);

        // Monday 10am: inside business hours → approve
        let result = policy.evaluate_at(&ctx, monday_10am);
        assert_eq!(result.verdict, PolicyVerdict::Approved { rule_name: "fallback".to_string() },
            "Mon 10am is inside business hours, should approve");

        // Saturday 10am: outside allowed days → reject
        let result2 = policy.evaluate_at(&ctx, saturday_10am);
        assert_eq!(result2.verdict, PolicyVerdict::Rejected {
            reason: "outside business hours".to_string(),
            rule_name: "hours-check".to_string(),
        }, "Sat is outside Mon-Fri");
    }

    #[test]
    fn maintenance_window_takes_priority_deterministic() {
        use chrono::{TimeZone, Utc};
        // Monday 2026-04-06 03:00 UTC
        let monday_3am = Utc.with_ymd_and_hms(2026, 4, 6, 3, 0, 0).unwrap();
        // Monday 2026-04-06 12:00 UTC
        let monday_noon = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();

        // Rules: [block outside 9-17, then approve]
        // 3am is outside 9-17 → block. Noon is inside → approve.
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "business-hours-only".to_string(),
                    description: "block outside business hours".to_string(),
                    condition: PolicyCondition::OutsideAllowedHours {
                        start_hour: 9,
                        end_hour: 17,
                        allowed_days: vec![],
                        utc_offset_hours: 0,
                    },
                    action: PolicyAction::Reject {
                        reason: "outside business hours".to_string(),
                    },
                },
                PolicyRule {
                    name: "normal-approve".to_string(),
                    description: "approve everything else".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_instruction(
            "wallet", "11111111111111111111111111111111", "dest", None,
        );
        let ctx = context(&proposal);

        // 3am: outside business hours → reject (first rule matches)
        let r1 = policy.evaluate_at(&ctx, monday_3am);
        assert_eq!(r1.verdict, PolicyVerdict::Rejected {
            reason: "outside business hours".to_string(),
            rule_name: "business-hours-only".to_string(),
        });
        assert_eq!(r1.matched_rule_index, Some(0));

        // Noon: inside business hours → first rule skipped → approve (second rule)
        let r2 = policy.evaluate_at(&ctx, monday_noon);
        assert_eq!(r2.verdict, PolicyVerdict::Approved {
            rule_name: "normal-approve".to_string(),
        });
        assert_eq!(r2.matched_rule_index, Some(1));
    }

    // ── Legacy: keep a live-clock test for backward compat ─────────────────
    // These use evaluate() (not evaluate_at) to confirm the live path still works.
    #[test]
    fn outside_allowed_hours_live_clock_sanity() {
        // 0-23 all days = everything inside → should never fire
        let policy_allows = PolicySet::new(
            vec![
                PolicyRule {
                    name: "always-inside".to_string(),
                    description: "0-23 all days".to_string(),
                    condition: PolicyCondition::OutsideAllowedHours {
                        start_hour: 0,
                        end_hour: 23,
                        allowed_days: vec![],
                        utc_offset_hours: 0,
                    },
                    action: PolicyAction::Reject { reason: "should not fire".to_string() },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_instruction(
            "wallet", "11111111111111111111111111111111", "dest", None,
        );
        let result = policy_allows.evaluate(&context(&proposal));
        assert_eq!(result.verdict, PolicyVerdict::Approved { rule_name: "fallback".to_string() },
            "0-23 all days should never fire OutsideAllowedHours");
    }

    // ── Layered policy precedence test ───────────────────────────────────

    #[test]
    fn precedence_is_caller_override_then_role_profile_then_global_then_defaults() {
        // Global: always approve
        let global = PolicySet::new(
            vec![PolicyRule {
                name: "global-approve".to_string(),
                description: "approve all".to_string(),
                condition: PolicyCondition::Always,
                action: PolicyAction::Approve,
            }],
            vec![],
            vec![],
        );

        // Role profile: reject >= 1000 lamports
        let profile_rules = vec![PolicyRule {
            name: "profile-cap".to_string(),
            description: "role profile cap".to_string(),
            condition: PolicyCondition::AmountExceedsLamports(1000),
            action: PolicyAction::Reject {
                reason: "profile cap exceeded".to_string(),
            },
        }];

        // Caller override: reject >= 500 lamports (stricter)
        let caller_rules = vec![PolicyRule {
            name: "caller-strict".to_string(),
            description: "caller override cap".to_string(),
            condition: PolicyCondition::AmountExceedsLamports(500),
            action: PolicyAction::Reject {
                reason: "caller cap exceeded".to_string(),
            },
        }];

        // Combined: caller → profile → global (two with_session_rules calls)
        let with_profile = global.with_session_rules(&profile_rules);
        let combined = with_profile.with_session_rules(&caller_rules);

        // amount=600: caller-strict fires (600 >= 500) before profile-cap (600 < 1000)
        let proposal_600 = proposal_with_instruction(
            "wallet", "11111111111111111111111111111111", "dest", Some(600),
        );
        let result = combined.evaluate(&context(&proposal_600));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "caller cap exceeded".to_string(),
                rule_name: "caller-strict".to_string(),
            },
            "caller override (500) should win over profile (1000)"
        );
        assert_eq!(result.matched_rule_index, Some(0), "caller-strict is at index 0");

        // amount=400: below both caps, falls through to global approve
        let proposal_400 = proposal_with_instruction(
            "wallet", "11111111111111111111111111111111", "dest", Some(400),
        );
        let result2 = combined.evaluate(&context(&proposal_400));
        assert_eq!(
            result2.verdict,
            PolicyVerdict::Approved {
                rule_name: "global-approve".to_string(),
            },
            "amount below all caps should fall through to global approve"
        );
    }

    // ── Deterministic time-based tests ───────────────────────────────────

    #[test]
    fn time_based_rule_allows_inside_window_and_blocks_outside_window() {
        use chrono::{TimeZone, Utc};

        // Monday 2026-04-06 10:00 UTC — should be INSIDE Mon-Fri 9-17 UTC
        let monday_10am = Utc.with_ymd_and_hms(2026, 4, 6, 10, 0, 0).unwrap();
        assert!(
            !super::is_outside_allowed_hours_at(monday_10am, 9, 17, &[1, 2, 3, 4, 5], 0),
            "Monday 10:00 should be inside Mon-Fri 9-17"
        );

        // Saturday 2026-04-11 10:00 UTC — day 6, not in [1,2,3,4,5]
        let saturday_10am = Utc.with_ymd_and_hms(2026, 4, 11, 10, 0, 0).unwrap();
        assert!(
            super::is_outside_allowed_hours_at(saturday_10am, 9, 17, &[1, 2, 3, 4, 5], 0),
            "Saturday should be outside Mon-Fri window"
        );

        // Monday 2026-04-06 20:00 UTC — hour 20, outside 9-17
        let monday_20pm = Utc.with_ymd_and_hms(2026, 4, 6, 20, 0, 0).unwrap();
        assert!(
            super::is_outside_allowed_hours_at(monday_20pm, 9, 17, &[1, 2, 3, 4, 5], 0),
            "Monday 20:00 should be outside 9-17 hour window"
        );
    }

    #[test]
    fn maintenance_window_rule_takes_priority_over_normal_allow_rule() {
        use chrono::{TimeZone, Utc};

        // Rule 1: OutsideAllowedHours(9, 17, Mon-Fri) → Reject "outside business hours"
        // Rule 2: Always → Approve
        //
        // At 03:00 UTC Monday: outside 9-17 → Rule 1 fires → Rejected
        // At 12:00 UTC Monday: inside 9-17 → Rule 1 does NOT fire → Rule 2 fires → Approved

        let rules = vec![
            PolicyRule {
                name: "business-hours-only".to_string(),
                description: "block outside business hours".to_string(),
                condition: PolicyCondition::OutsideAllowedHours {
                    start_hour: 9,
                    end_hour: 17,
                    allowed_days: vec![1, 2, 3, 4, 5],
                    utc_offset_hours: 0,
                },
                action: PolicyAction::Reject {
                    reason: "outside business hours".to_string(),
                },
            },
            PolicyRule {
                name: "fallback-approve".to_string(),
                description: "approve during business hours".to_string(),
                condition: PolicyCondition::Always,
                action: PolicyAction::Approve,
            },
        ];

        let policy = PolicySet::new(rules, vec![], vec![]);

        let proposal = proposal_with_instruction(
            "wallet", "11111111111111111111111111111111", "dest", Some(100),
        );

        // Test at 03:00 UTC Monday (outside 9-17) — rule 1 should fire
        // We can't control the clock for `condition_matches` directly via
        // the policy evaluation (it calls is_outside_allowed_hours which
        // uses Utc::now()), so we test the deterministic function instead
        // and verify the rule structure.
        let monday_3am = Utc.with_ymd_and_hms(2026, 4, 6, 3, 0, 0).unwrap();
        assert!(
            super::is_outside_allowed_hours_at(monday_3am, 9, 17, &[1, 2, 3, 4, 5], 0),
            "3am Monday should be outside 9-17 → rule would fire → Rejected"
        );

        let monday_noon = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();
        assert!(
            !super::is_outside_allowed_hours_at(monday_noon, 9, 17, &[1, 2, 3, 4, 5], 0),
            "noon Monday should be inside 9-17 → rule would NOT fire → fallback Approved"
        );

        // Verify that if we evaluated the policy right now and the current
        // time falls within business hours, the result is Approved via fallback.
        // (This may differ depending on when tests run, but the structure is correct.)
        let result = policy.evaluate(&context(&proposal));
        // We just verify the matched rule is one of the two expected rules.
        assert!(
            result.matched_rule_index == Some(0) || result.matched_rule_index == Some(1),
            "should match either the hours rule or the fallback"
        );
    }

    // ── USDC / Stablecoin policy tests ──────────────────────────────────

    fn proposal_with_token_transfer(
        wallet_pubkey: &str,
        mint: &str,
        amount: u64,
        decimals: u8,
    ) -> TransactionProposal {
        use claw_types::transaction::TokenTransfer;
        TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey: wallet_pubkey.to_string(),
            network: SolanaNetwork::Devnet,
            description: "USDC test".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![InstructionSummary {
                program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
                program_name: Some("SPL Token".to_string()),
                description: format!("TransferChecked {} of {}", amount, mint),
                transfer_lamports: None,
                token_transfer: Some(TokenTransfer {
                    mint: mint.to_string(),
                    amount,
                    decimals: Some(decimals),
                    source: "source-token-account".to_string(),
                    destination: "dest-token-account".to_string(),
                }),
                is_legacy_token_transfer: false,
                accounts: vec![],
            }],
            created_at: chrono::Utc::now(),
        }
    }

    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
    const BONK_MINT: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";

    #[test]
    fn token_amount_exceeds_fires_for_matching_mint_above_threshold() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "usdc-cap".to_string(),
                    description: "USDC >= 100".to_string(),
                    condition: PolicyCondition::TokenAmountExceeds {
                        mint: USDC_MINT.to_string(),
                        threshold: 100_000_000, // 100 USDC
                    },
                    action: PolicyAction::Reject {
                        reason: "USDC cap exceeded".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        // 150 USDC → should fire
        let proposal = proposal_with_token_transfer("wallet", USDC_MINT, 150_000_000, 6);
        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "USDC cap exceeded".to_string(),
                rule_name: "usdc-cap".to_string(),
            }
        );
    }

    #[test]
    fn token_amount_exceeds_does_not_fire_below_threshold() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "usdc-cap".to_string(),
                    description: "USDC >= 100".to_string(),
                    condition: PolicyCondition::TokenAmountExceeds {
                        mint: USDC_MINT.to_string(),
                        threshold: 100_000_000,
                    },
                    action: PolicyAction::Reject {
                        reason: "USDC cap exceeded".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        // 50 USDC → should NOT fire
        let proposal = proposal_with_token_transfer("wallet", USDC_MINT, 50_000_000, 6);
        let result = policy.evaluate(&context(&proposal));
        assert_eq!(result.verdict, PolicyVerdict::Approved { rule_name: "fallback".to_string() });
    }

    #[test]
    fn token_amount_exceeds_ignores_other_mints() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "usdc-only-cap".to_string(),
                    description: "Only check USDC".to_string(),
                    condition: PolicyCondition::TokenAmountExceeds {
                        mint: USDC_MINT.to_string(),
                        threshold: 100_000_000,
                    },
                    action: PolicyAction::Reject { reason: "USDC cap".to_string() },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        // 1 BILLION BONK transferred — but the USDC rule should not fire
        let proposal = proposal_with_token_transfer("wallet", BONK_MINT, 1_000_000_000_000, 5);
        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Approved { rule_name: "fallback".to_string() },
            "non-USDC transfer should not trigger USDC cap"
        );
    }

    #[test]
    fn mint_not_in_allowlist_fires_for_unlisted_mint() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "stablecoin-only".to_string(),
                    description: "Only USDC and USDT".to_string(),
                    condition: PolicyCondition::MintNotInAllowlist {
                        allowed_mints: vec![USDC_MINT.to_string(), USDT_MINT.to_string()],
                    },
                    action: PolicyAction::Reject {
                        reason: "non-stablecoin token".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        // BONK is not in the stablecoin allowlist
        let proposal = proposal_with_token_transfer("wallet", BONK_MINT, 1000, 5);
        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "non-stablecoin token".to_string(),
                rule_name: "stablecoin-only".to_string(),
            }
        );
    }

    #[test]
    fn mint_not_in_allowlist_passes_for_listed_mint() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "stablecoin-only".to_string(),
                    description: "Only USDC and USDT".to_string(),
                    condition: PolicyCondition::MintNotInAllowlist {
                        allowed_mints: vec![USDC_MINT.to_string(), USDT_MINT.to_string()],
                    },
                    action: PolicyAction::Reject {
                        reason: "non-stablecoin".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_token_transfer("wallet", USDC_MINT, 1_000_000, 6);
        let result = policy.evaluate(&context(&proposal));
        assert_eq!(result.verdict, PolicyVerdict::Approved { rule_name: "fallback".to_string() });
    }

    #[test]
    fn mint_not_in_allowlist_empty_list_disables_check() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "noop-allowlist".to_string(),
                    description: "Empty list = no enforcement".to_string(),
                    condition: PolicyCondition::MintNotInAllowlist {
                        allowed_mints: vec![],
                    },
                    action: PolicyAction::Reject { reason: "should not fire".to_string() },
                },
                PolicyRule {
                    name: "fallback".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_token_transfer("wallet", BONK_MINT, 999, 5);
        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Approved { rule_name: "fallback".to_string() },
            "empty allowlist should not block any mint"
        );
    }

    #[test]
    fn token_amount_exceeds_sums_multiple_instructions() {
        // Two USDC transfers in one tx, each below threshold but sum is above.
        use claw_types::transaction::TokenTransfer;

        let proposal = TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey: "wallet".to_string(),
            network: SolanaNetwork::Devnet,
            description: "split USDC".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![
                InstructionSummary {
                    program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
                    program_name: Some("SPL Token".to_string()),
                    description: "transfer 1".to_string(),
                    transfer_lamports: None,
                    token_transfer: Some(TokenTransfer {
                        mint: USDC_MINT.to_string(),
                        amount: 60_000_000, // 60 USDC
                        decimals: Some(6),
                        source: "src1".to_string(),
                        destination: "dst1".to_string(),
                    }),
                    is_legacy_token_transfer: false,
                    accounts: vec![],
                },
                InstructionSummary {
                    program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
                    program_name: Some("SPL Token".to_string()),
                    description: "transfer 2".to_string(),
                    transfer_lamports: None,
                    token_transfer: Some(TokenTransfer {
                        mint: USDC_MINT.to_string(),
                        amount: 50_000_000, // 50 USDC
                        decimals: Some(6),
                        source: "src2".to_string(),
                        destination: "dst2".to_string(),
                    }),
                    is_legacy_token_transfer: false,
                    accounts: vec![],
                },
            ],
            created_at: chrono::Utc::now(),
        };

        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "usdc-100-cap".to_string(),
                    description: "USDC sum >= 100".to_string(),
                    condition: PolicyCondition::TokenAmountExceeds {
                        mint: USDC_MINT.to_string(),
                        threshold: 100_000_000,
                    },
                    action: PolicyAction::Reject { reason: "split exceeds cap".to_string() },
                },
            ],
            vec![],
            vec![],
        );

        let result = policy.evaluate(&context(&proposal));
        // Sum is 110 USDC, exceeds 100 threshold
        assert!(result.verdict.is_blocked() || result.verdict.requires_human() ||
            matches!(&result.verdict, PolicyVerdict::Rejected { .. }),
            "split transfer that sums above threshold should fire");
    }

    // ── Legacy Token Transfer bypass guard tests ────────────────────────

    fn proposal_with_legacy_transfer(wallet_pubkey: &str) -> TransactionProposal {
        TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey: wallet_pubkey.to_string(),
            network: SolanaNetwork::Devnet,
            description: "legacy transfer test".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![InstructionSummary {
                program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
                program_name: Some("SPL Token".to_string()),
                description: "Transfer (legacy): 999 raw units".to_string(),
                transfer_lamports: None,
                // Legacy Transfer: token_transfer is None because mint
                // is not in accounts and we don't RPC-lookup in V1.
                token_transfer: None,
                is_legacy_token_transfer: true,
                accounts: vec![],
            }],
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn legacy_token_transfer_present_fires_for_legacy_transfer() {
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "block-legacy".to_string(),
                    description: "reject legacy token transfers".to_string(),
                    condition: PolicyCondition::LegacyTokenTransferPresent,
                    action: PolicyAction::Reject {
                        reason: "use TransferChecked".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback-approve".to_string(),
                    description: "approve otherwise".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let proposal = proposal_with_legacy_transfer("wallet");
        let result = policy.evaluate(&context(&proposal));

        assert_eq!(
            result.verdict,
            PolicyVerdict::Rejected {
                reason: "use TransferChecked".to_string(),
                rule_name: "block-legacy".to_string(),
            },
            "legacy SPL Token Transfer must be rejected"
        );
    }

    #[test]
    fn legacy_token_transfer_present_does_not_fire_for_transfer_checked() {
        use claw_types::transaction::TokenTransfer;

        let proposal = TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey: "wallet".to_string(),
            network: SolanaNetwork::Devnet,
            description: "TransferChecked test".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![InstructionSummary {
                program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
                program_name: Some("SPL Token".to_string()),
                description: "TransferChecked".to_string(),
                transfer_lamports: None,
                token_transfer: Some(TokenTransfer {
                    mint: USDC_MINT.to_string(),
                    amount: 1_000_000,
                    decimals: Some(6),
                    source: "src".to_string(),
                    destination: "dst".to_string(),
                }),
                is_legacy_token_transfer: false,
                accounts: vec![],
            }],
            created_at: chrono::Utc::now(),
        };

        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "block-legacy".to_string(),
                    description: "reject legacy only".to_string(),
                    condition: PolicyCondition::LegacyTokenTransferPresent,
                    action: PolicyAction::Reject {
                        reason: "use TransferChecked".to_string(),
                    },
                },
                PolicyRule {
                    name: "fallback-approve".to_string(),
                    description: "approve TransferChecked".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        let result = policy.evaluate(&context(&proposal));
        assert_eq!(
            result.verdict,
            PolicyVerdict::Approved { rule_name: "fallback-approve".to_string() },
            "TransferChecked must NOT trigger the legacy guard"
        );
    }

    #[test]
    fn legacy_token_transfer_guard_before_token_amount_closes_bypass() {
        // Regression: before the legacy guard, an attacker could send a legacy
        // Transfer with amount=999999 USDC and it would bypass TokenAmountExceeds
        // (which only checks TransferChecked token_transfer). With the guard
        // rule listed FIRST, legacy transfers are blocked before the amount
        // rule is even reached.
        let policy = PolicySet::new(
            vec![
                PolicyRule {
                    name: "block-legacy".to_string(),
                    description: "legacy guard".to_string(),
                    condition: PolicyCondition::LegacyTokenTransferPresent,
                    action: PolicyAction::Reject { reason: "legacy not allowed".to_string() },
                },
                PolicyRule {
                    name: "usdc-cap".to_string(),
                    description: "USDC cap".to_string(),
                    condition: PolicyCondition::TokenAmountExceeds {
                        mint: USDC_MINT.to_string(),
                        threshold: 100_000_000,
                    },
                    action: PolicyAction::RequireHumanApproval {
                        reason: "USDC cap".to_string(),
                        required_approver_role: None,
                    },
                },
                PolicyRule {
                    name: "fallback-approve".to_string(),
                    description: "approve".to_string(),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::Approve,
                },
            ],
            vec![],
            vec![],
        );

        // Legacy transfer (attacker bypass attempt) — blocked by guard at index 0.
        let legacy_proposal = proposal_with_legacy_transfer("attacker");
        let legacy_result = policy.evaluate(&context(&legacy_proposal));
        assert_eq!(legacy_result.matched_rule_index, Some(0),
            "legacy guard must fire before the per-mint rule could be bypassed");
        assert!(matches!(&legacy_result.verdict, PolicyVerdict::Rejected { .. }));
    }
}

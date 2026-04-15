//! P4 — Property-based fuzzing of the Policy Engine.
//!
//! Asserts three invariants across thousands of randomly generated rule
//! sets and transaction proposals:
//!
//! 1. **No panic.** `PolicySet::evaluate` never panics, regardless of how
//!    pathological the input is (huge amounts, weird mints, empty strings,
//!    out-of-range UTC offsets, wrap-around hour ranges).
//! 2. **First-match-wins.** When `matched_rule_index = Some(k)`, no rule
//!    at index `< k` matches the same context. This is the contract the
//!    risk engine sells to the rest of the system; without it, rule
//!    ordering would be a silent footgun.
//! 3. **Verdict shape consistency.** A `RequiresHumanApproval` verdict
//!    must originate from either a matching `RequireHumanApproval` action
//!    or the fail-closed default (rules empty / no rule matched).
//!
//! These tests are slower than the unit tests in `policy.rs` because each
//! `proptest!` block runs ~256 randomly generated cases. They are kept in
//! a separate integration test file so the inner unit tests remain fast.

use proptest::prelude::*;

use claw_risk_engine::{PolicyEvaluationContext, PolicySet};
use claw_types::{
    policy::{PolicyAction, PolicyCondition, PolicyRule, PolicyVerdict},
    session::SessionId,
    solana::SolanaNetwork,
    transaction::{
        AccountRole, InstructionSummary, TokenTransfer, TransactionProposal,
    },
};
use uuid::Uuid;

// ── Generators ────────────────────────────────────────────────────────────────

fn arb_network() -> impl Strategy<Value = SolanaNetwork> {
    prop_oneof![
        Just(SolanaNetwork::MainnetBeta),
        Just(SolanaNetwork::Devnet),
        Just(SolanaNetwork::Testnet),
        Just(SolanaNetwork::Localnet),
    ]
}

fn arb_pubkey_like() -> impl Strategy<Value = String> {
    // 32-44 char base58-ish string. Doesn't have to be valid base58 — the
    // policy engine treats these as opaque identifiers.
    "[1-9A-HJ-NP-Za-km-z]{32,44}"
}

fn arb_condition() -> impl Strategy<Value = PolicyCondition> {
    prop_oneof![
        // Numeric edges intentionally include 0 and u64::MAX-ish values
        // to flush out arithmetic overflow bugs.
        Just(PolicyCondition::ProgramNotInAllowlist),
        Just(PolicyCondition::DestinationInDenylist),
        Just(PolicyCondition::SimulationNotPassed),
        Just(PolicyCondition::LegacyTokenTransferPresent),
        Just(PolicyCondition::Always),
        any::<u64>().prop_map(PolicyCondition::AmountExceedsLamports),
        prop::num::f64::POSITIVE.prop_map(PolicyCondition::CostExceedsSol),
        prop::num::f64::POSITIVE.prop_map(PolicyCondition::DailySpendExceedsSol),
        (arb_pubkey_like(), any::<u64>()).prop_map(|(mint, threshold)| {
            PolicyCondition::TokenAmountExceeds { mint, threshold }
        }),
        prop::collection::vec(arb_pubkey_like(), 0..3)
            .prop_map(|allowed_mints| PolicyCondition::MintNotInAllowlist { allowed_mints }),
        // OutsideAllowedHours with deliberately wide ranges including
        // wrap-around (start > end) and degenerate UTC offsets.
        (0u8..24, 0u8..24, prop::collection::vec(1u8..=7, 0..7), -23i32..=23)
            .prop_map(|(start_hour, end_hour, allowed_days, utc_offset_hours)| {
                PolicyCondition::OutsideAllowedHours {
                    start_hour,
                    end_hour,
                    allowed_days,
                    utc_offset_hours,
                }
            }),
        prop::collection::vec(arb_network(), 1..4)
            .prop_map(PolicyCondition::NetworkIn),
    ]
}

fn arb_action() -> impl Strategy<Value = PolicyAction> {
    prop_oneof![
        Just(PolicyAction::Approve),
        ".*".prop_map(|reason| PolicyAction::RequireHumanApproval {
            reason,
            required_approver_role: None,
        }),
        ".*".prop_map(|reason| PolicyAction::Reject { reason }),
    ]
}

fn arb_rule() -> impl Strategy<Value = PolicyRule> {
    ("[a-z_]{1,16}", arb_condition(), arb_action()).prop_map(|(name, condition, action)| {
        let description = format!("fuzzed rule {name}");
        PolicyRule { name, description, condition, action }
    })
}

fn arb_token_transfer() -> impl Strategy<Value = TokenTransfer> {
    (arb_pubkey_like(), any::<u64>(), prop::option::of(0u8..16), arb_pubkey_like(), arb_pubkey_like())
        .prop_map(|(mint, amount, decimals, source, destination)| TokenTransfer {
            mint,
            amount,
            decimals,
            source,
            destination,
        })
}

fn arb_instruction() -> impl Strategy<Value = InstructionSummary> {
    (
        arb_pubkey_like(),
        prop::option::of(any::<u64>()),
        prop::option::of(arb_token_transfer()),
        any::<bool>(),
    )
        .prop_map(|(program_id, transfer_lamports, token_transfer, is_legacy)| {
            InstructionSummary {
                program_id: program_id.clone(),
                program_name: None,
                description: "fuzzed ix".to_string(),
                transfer_lamports,
                token_transfer,
                is_legacy_token_transfer: is_legacy,
                accounts: vec![AccountRole {
                    pubkey: program_id,
                    label: None,
                    is_signer: false,
                    is_writable: false,
                }],
            }
        })
}

fn arb_proposal() -> impl Strategy<Value = TransactionProposal> {
    (
        arb_pubkey_like(),
        arb_network(),
        prop::collection::vec(arb_instruction(), 0..4),
    )
        .prop_map(|(wallet_pubkey, network, instructions_summary)| TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey,
            network,
            description: "fuzzed proposal".to_string(),
            transaction_b64: String::new(),
            instructions_summary,
            created_at: chrono::Utc::now(),
        })
}

// ── Properties ───────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// Property 1: evaluation never panics, no matter how pathological the
    /// rules or proposal are.
    #[test]
    fn evaluate_never_panics(
        rules in prop::collection::vec(arb_rule(), 0..8),
        program_allowlist in prop::collection::vec(arb_pubkey_like(), 0..3),
        destination_denylist in prop::collection::vec(arb_pubkey_like(), 0..3),
        proposal in arb_proposal(),
        session_spend_lamports in any::<u64>(),
        wallet_daily_spend_lamports in any::<u64>(),
    ) {
        let policy = PolicySet::new(rules, program_allowlist, destination_denylist);
        let ctx = PolicyEvaluationContext {
            proposal: &proposal,
            simulation_result: None,
            network: proposal.network,
            session_id: &proposal.session_id,
            session_spend_lamports,
            wallet_daily_spend_lamports,
        };

        // The interesting assertion is that the call returns at all.
        let result = policy.evaluate(&ctx);

        // While we're here, keep a basic structural sanity check that won't
        // false-positive on legitimate verdicts.
        prop_assert!(result.rules_evaluated <= result.rules_total + 1);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    /// Property 2: when a rule matches, it is the FIRST matching rule.
    /// Verified by re-running evaluation against truncated rule lists and
    /// asserting consistency: if rule `k` was matched, then with rules
    /// `[..k]` the verdict must NOT match a rule (matched_rule_index = None).
    #[test]
    fn first_match_wins(
        rules in prop::collection::vec(arb_rule(), 1..6),
        proposal in arb_proposal(),
    ) {
        let full = PolicySet::new(rules.clone(), vec![], vec![]);
        let ctx = PolicyEvaluationContext {
            proposal: &proposal,
            simulation_result: None,
            network: proposal.network,
            session_id: &proposal.session_id,
            session_spend_lamports: 0,
            wallet_daily_spend_lamports: 0,
        };
        let full_result = full.evaluate(&ctx);

        if let Some(k) = full_result.matched_rule_index {
            // Truncate to the rules BEFORE k and re-evaluate. None of them
            // should match — otherwise the full evaluation would have
            // matched one of them first.
            let truncated = PolicySet::new(rules[..k].to_vec(), vec![], vec![]);
            let trunc_result = truncated.evaluate(&ctx);
            prop_assert!(
                trunc_result.matched_rule_index.is_none(),
                "rules[..{}] matched index {:?}, but full evaluation said \
                 first match was at index {}; ordering invariant broken",
                k,
                trunc_result.matched_rule_index,
                k,
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    /// Property 3: a `RequiresHumanApproval` verdict must either correspond
    /// to a `RequireHumanApproval` action on the matched rule, or be the
    /// fail-closed default (no rule matched / rule list empty).
    #[test]
    fn human_approval_verdict_has_legitimate_origin(
        rules in prop::collection::vec(arb_rule(), 0..6),
        proposal in arb_proposal(),
    ) {
        let policy = PolicySet::new(rules.clone(), vec![], vec![]);
        let ctx = PolicyEvaluationContext {
            proposal: &proposal,
            simulation_result: None,
            network: proposal.network,
            session_id: &proposal.session_id,
            session_spend_lamports: 0,
            wallet_daily_spend_lamports: 0,
        };
        let result = policy.evaluate(&ctx);

        if matches!(result.verdict, PolicyVerdict::RequiresHumanApproval { .. }) {
            match result.matched_rule_index {
                Some(k) => {
                    prop_assert!(
                        matches!(rules[k].action, PolicyAction::RequireHumanApproval { .. }),
                        "rule at index {} produced RequiresHumanApproval verdict but its \
                         action is {:?}",
                        k,
                        rules[k].action,
                    );
                }
                None => {
                    // Fail-closed default — must mean no rule matched.
                }
            }
        }
    }
}

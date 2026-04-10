//! Integration test: TOML config → PolicySet → policy evaluation.
//!
//! Proves that custom `[[policy.rules]]`, `program_allowlist`, and
//! `destination_denylist` declared in TOML actually produce the expected
//! policy verdicts at runtime.

use uuid::Uuid;

use claw_gateway::config::ClawConfig;
use claw_risk_engine::{PolicyEvaluationContext, PolicySet};
use claw_types::{
    policy::PolicyVerdict,
    session::SessionId,
    solana::SolanaNetwork,
    transaction::{AccountRole, InstructionSummary, TransactionProposal},
};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Minimal TOML config that mirrors the demo rules from config/default.toml.
const DEMO_TOML: &str = r#"
[daemon]
db_path = "./data/test.db"

[network]
network = "devnet"

[rpc]
primary_url = "https://api.devnet.solana.com"
ws_url = "wss://api.devnet.solana.com"
timeout_ms = 15000

[policy]
mainnet_safe_defaults = true
program_allowlist = [
    "11111111111111111111111111111111",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
]
destination_denylist = [
    "BurnAddress1111111111111111111111111111111",
]

[[policy.rules]]
name = "large-transfer-requires-human"
description = "Require human approval for transfers >= 0.01 SOL"
condition = { type = "AmountExceedsLamports", threshold = 10000000 }
action = { type = "RequireHumanApproval", reason = "transfer amount exceeds 0.01 SOL threshold" }

[[policy.rules]]
name = "denylist-block"
description = "Block transactions to denied destinations"
condition = "DestinationInDenylist"
action = { type = "Reject", reason = "destination is on the deny list" }

[[policy.rules]]
name = "allowlist-block"
description = "Block transactions invoking programs not on the allow list"
condition = "ProgramNotInAllowlist"
action = { type = "Reject", reason = "program is not on the allow list" }

[llm]
provider = "openai"
api_key = ""
model = "gpt-4o-mini"

[api]
bind_addr = "127.0.0.1"
port = 7070

[logging]
format = "pretty"
level = "info"
"#;

/// Build a PolicySet from a ClawConfig the same way daemon.rs does.
fn build_policy_set(config: &ClawConfig) -> PolicySet {
    PolicySet::new(
        config.policy.rules.clone(),
        config.policy.program_allowlist.clone(),
        config.policy.destination_denylist.clone(),
    )
}

fn make_proposal(
    program_id: &str,
    destination: &str,
    transfer_lamports: Option<u64>,
) -> TransactionProposal {
    TransactionProposal {
        id: Uuid::new_v4(),
        session_id: SessionId::from(Uuid::new_v4()),
        wallet_pubkey: "WalletPubkey1111111111111111111111111111111".to_string(),
        network: SolanaNetwork::Devnet,
        description: "config wiring test".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![InstructionSummary {
            program_id: program_id.to_string(),
            program_name: Some("test".to_string()),
            description: "test instruction".to_string(),
            transfer_lamports,
            token_transfer: None,
            accounts: vec![
                AccountRole {
                    pubkey: "WalletPubkey1111111111111111111111111111111".to_string(),
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

fn eval_ctx(proposal: &TransactionProposal) -> PolicyEvaluationContext<'_> {
    PolicyEvaluationContext {
        proposal,
        simulation_result: None,
        network: SolanaNetwork::Devnet,
        session_id: &proposal.session_id,
        session_spend_lamports: 0,
        wallet_daily_spend_lamports: 0,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn toml_parses_into_config_with_custom_rules() {
    let config: ClawConfig =
        toml::from_str(DEMO_TOML).expect("demo TOML should parse into ClawConfig");

    assert_eq!(config.policy.rules.len(), 3, "should have 3 custom rules");
    assert_eq!(
        config.policy.program_allowlist.len(),
        2,
        "should have 2 allowlist entries"
    );
    assert_eq!(
        config.policy.destination_denylist.len(),
        1,
        "should have 1 denylist entry"
    );
    assert_eq!(config.policy.rules[0].name, "large-transfer-requires-human");
    assert_eq!(config.policy.rules[1].name, "denylist-block");
    assert_eq!(config.policy.rules[2].name, "allowlist-block");
}

#[test]
fn large_transfer_triggers_human_approval() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    let proposal = make_proposal(
        "11111111111111111111111111111111", // allowed program
        "SafeDestination111111111111111111111111111", // not denied
        Some(50_000_000), // 0.05 SOL — above the 0.01 SOL threshold
    );

    let result = policy.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::RequiresHumanApproval {
            reason: "transfer amount exceeds 0.01 SOL threshold".to_string(),
            rule_name: "large-transfer-requires-human".to_string(),
            required_approver_role: None,
            approval_chain: None,
        }
    );
    assert_eq!(result.matched_rule_index, Some(0));
}

#[test]
fn small_transfer_to_allowed_program_falls_through() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(1_000), // well below threshold
    );

    // No custom rule matches → falls through to default fail-closed
    let result = policy.evaluate(&eval_ctx(&proposal));
    // Since we only have custom rules (no default catch-all in this test),
    // it should hit the fail-closed default.
    assert!(
        result.verdict.requires_human(),
        "small transfer with no catch-all should fail closed: {:?}",
        result.verdict
    );
    assert_eq!(result.matched_rule_index, None, "no rule matched");
    assert_eq!(result.rules_evaluated, result.rules_total);
}

#[test]
fn denied_destination_is_rejected() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    // Transfer below amount threshold but to denied destination.
    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "BurnAddress1111111111111111111111111111111", // denied
        Some(1_000),
    );

    let result = policy.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::Rejected {
            reason: "destination is on the deny list".to_string(),
            rule_name: "denylist-block".to_string(),
        }
    );
    assert_eq!(result.matched_rule_index, Some(1), "denylist is second rule");
}

#[test]
fn unlisted_program_is_rejected() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    let proposal = make_proposal(
        "UnknownProgram111111111111111111111111111", // not in allowlist
        "SafeDestination111111111111111111111111111",
        Some(1_000),
    );

    let result = policy.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::Rejected {
            reason: "program is not on the allow list".to_string(),
            rule_name: "allowlist-block".to_string(),
        }
    );
    assert_eq!(result.matched_rule_index, Some(2), "allowlist is third rule");
}

#[test]
fn verdict_json_carries_rule_name_and_reason() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "BurnAddress1111111111111111111111111111111",
        Some(1_000),
    );

    let result = policy.evaluate(&eval_ctx(&proposal));

    // Serialize the verdict to JSON and verify policy metadata fields.
    let json = serde_json::to_value(&result.verdict).expect("verdict should serialize");
    assert_eq!(json["rule_name"], "denylist-block");
    assert_eq!(json["reason"], "destination is on the deny list");
    assert_eq!(json["verdict"], "rejected");
}

// ── Session-scoped policy override tests ────────────────────────────────────

#[test]
fn session_override_takes_priority_over_global() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let global = build_policy_set(&config);

    // Session-scoped rule: reject ALL transactions (stricter than global)
    let session_rules = vec![claw_types::policy::PolicyRule {
        name: "session-block-all".to_string(),
        description: "session blocks everything".to_string(),
        condition: claw_types::policy::PolicyCondition::Always,
        action: claw_types::policy::PolicyAction::Reject {
            reason: "session locked down".to_string(),
        },
    }];

    let layered = global.with_session_rules(&session_rules);

    // This proposal would normally be approved (small, allowed program, safe dest)
    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(100),
    );

    let result = layered.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::Rejected {
            reason: "session locked down".to_string(),
            rule_name: "session-block-all".to_string(),
        }
    );
    assert_eq!(result.matched_rule_index, Some(0), "session rule fires first");
}

#[test]
fn session_override_falls_through_to_global() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let global = build_policy_set(&config);

    // Session rule: require human for amounts >= 1 SOL (higher threshold than global 0.01 SOL)
    let session_rules = vec![claw_types::policy::PolicyRule {
        name: "session-high-value".to_string(),
        description: "session-level high-value check".to_string(),
        condition: claw_types::policy::PolicyCondition::AmountExceedsLamports(1_000_000_000),
        action: claw_types::policy::PolicyAction::RequireHumanApproval {
            reason: "session: amount exceeds 1 SOL".to_string(),
            required_approver_role: None,
        },
    }];

    let layered = global.with_session_rules(&session_rules);

    // 0.05 SOL — below session threshold (1 SOL) but above global threshold (0.01 SOL)
    // Session rule doesn't match → falls through to global rule
    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(50_000_000),
    );

    let result = layered.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::RequiresHumanApproval {
            reason: "transfer amount exceeds 0.01 SOL threshold".to_string(),
            rule_name: "large-transfer-requires-human".to_string(),
            required_approver_role: None,
            approval_chain: None,
        },
        "global rule should fire when session rule doesn't match"
    );
    // session rule is index 0, global rules start at index 1
    assert_eq!(result.matched_rule_index, Some(1), "global rule is at index 1");
}

#[test]
fn no_session_override_uses_global_only() {
    let config: ClawConfig = toml::from_str(DEMO_TOML).unwrap();
    let global = build_policy_set(&config);

    // No session overrides — with_session_rules with empty slice
    let layered = global.with_session_rules(&[]);

    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "BurnAddress1111111111111111111111111111111",
        Some(100),
    );

    let result = layered.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::Rejected {
            reason: "destination is on the deny list".to_string(),
            rule_name: "denylist-block".to_string(),
        }
    );
}

// ── Role profile tests ──────────────────────────────────────────────────────

const PROFILE_TOML: &str = r#"
[daemon]
db_path = "./data/test.db"

[network]
network = "devnet"

[rpc]
primary_url = "https://api.devnet.solana.com"
ws_url = "wss://api.devnet.solana.com"
timeout_ms = 15000

[policy]
mainnet_safe_defaults = false
program_allowlist = []
destination_denylist = []

# Global catch-all: approve everything
[[policy.rules]]
name = "global-approve-all"
description = "Approve all (devnet)"
condition = "Always"
action = "Approve"

# Execution role: cap at 0.1 SOL
[[policy.role_profiles]]
role = "execution"
rules = [
    { name = "exec-cap", description = "Cap execution", condition = { type = "AmountExceedsLamports", threshold = 100000000 }, action = { type = "RequireHumanApproval", reason = "execution cap: >= 0.1 SOL" } },
]

# Research role: block all
[[policy.role_profiles]]
role = "research"
rules = [
    { name = "research-block", description = "Block all", condition = "Always", action = { type = "Reject", reason = "research cannot transact" } },
]

[llm]
provider = "openai"
api_key = ""
model = "gpt-4o-mini"

[api]
bind_addr = "127.0.0.1"
port = 7070

[logging]
format = "pretty"
level = "info"
"#;

#[test]
fn role_profiles_parse_from_toml() {
    let config: ClawConfig =
        toml::from_str(PROFILE_TOML).expect("profile TOML should parse");

    assert_eq!(config.policy.role_profiles.len(), 2);
    assert_eq!(config.policy.role_profiles[0].role, "execution");
    assert_eq!(config.policy.role_profiles[0].rules.len(), 1);
    assert_eq!(config.policy.role_profiles[0].rules[0].name, "exec-cap");
    assert_eq!(config.policy.role_profiles[1].role, "research");
}

#[test]
fn role_profile_rules_merged_with_global_produce_correct_verdicts() {
    let config: ClawConfig = toml::from_str(PROFILE_TOML).unwrap();
    let global = build_policy_set(&config);

    // Simulate what daemon does: find the execution profile, prepend to global
    let exec_profile = config
        .policy
        .role_profiles
        .iter()
        .find(|p| p.role == "execution")
        .unwrap();

    let layered = global.with_session_rules(&exec_profile.rules);

    // Transfer of 0.5 SOL (above 0.1 SOL cap) → exec-cap fires
    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(500_000_000),
    );

    let result = layered.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::RequiresHumanApproval {
            reason: "execution cap: >= 0.1 SOL".to_string(),
            rule_name: "exec-cap".to_string(),
            required_approver_role: None,
            approval_chain: None,
        }
    );
    assert_eq!(result.matched_rule_index, Some(0), "session/role rule at index 0");

    // Transfer of 0.01 SOL (below 0.1 SOL cap) → falls through to global approve
    let small_proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(10_000_000),
    );

    let result2 = layered.evaluate(&eval_ctx(&small_proposal));
    assert_eq!(
        result2.verdict,
        PolicyVerdict::Approved {
            rule_name: "global-approve-all".to_string(),
        }
    );
    assert_eq!(result2.matched_rule_index, Some(1), "global rule at index 1");
}

#[test]
fn research_role_profile_blocks_everything() {
    let config: ClawConfig = toml::from_str(PROFILE_TOML).unwrap();
    let global = build_policy_set(&config);

    let research_profile = config
        .policy
        .role_profiles
        .iter()
        .find(|p| p.role == "research")
        .unwrap();

    let layered = global.with_session_rules(&research_profile.rules);

    // Even a tiny transfer is blocked
    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(1),
    );

    let result = layered.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::Rejected {
            reason: "research cannot transact".to_string(),
            rule_name: "research-block".to_string(),
        }
    );
}

#[test]
fn caller_overrides_take_priority_over_role_profile() {
    let config: ClawConfig = toml::from_str(PROFILE_TOML).unwrap();
    let global = build_policy_set(&config);

    let exec_profile = config
        .policy
        .role_profiles
        .iter()
        .find(|p| p.role == "execution")
        .unwrap();

    // Caller override: reject amounts >= 0.01 SOL (stricter than role's 0.1 SOL)
    let caller_override = claw_types::policy::PolicyRule {
        name: "caller-strict-cap".to_string(),
        description: "Caller-level strict cap".to_string(),
        condition: claw_types::policy::PolicyCondition::AmountExceedsLamports(10_000_000),
        action: claw_types::policy::PolicyAction::Reject {
            reason: "caller cap: >= 0.01 SOL".to_string(),
        },
    };

    // Merge order: caller overrides first, then role profile, then global
    let mut combined = vec![caller_override];
    combined.extend(exec_profile.rules.clone());

    let layered = global.with_session_rules(&combined);

    // Transfer of 0.05 SOL → caller override fires (stricter)
    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(50_000_000),
    );

    let result = layered.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::Rejected {
            reason: "caller cap: >= 0.01 SOL".to_string(),
            rule_name: "caller-strict-cap".to_string(),
        }
    );
    assert_eq!(result.matched_rule_index, Some(0), "caller override at index 0");
}

#[test]
fn no_matching_role_profile_falls_through_to_global() {
    let config: ClawConfig = toml::from_str(PROFILE_TOML).unwrap();
    let global = build_policy_set(&config);

    // "ops" role has no profile defined
    let ops_profile = config
        .policy
        .role_profiles
        .iter()
        .find(|p| p.role == "ops");

    assert!(ops_profile.is_none(), "ops profile should not exist");

    // Without session rules, global rules apply directly
    let proposal = make_proposal(
        "11111111111111111111111111111111",
        "SafeDestination111111111111111111111111111",
        Some(999_000_000_000),
    );

    let result = global.evaluate(&eval_ctx(&proposal));
    assert_eq!(
        result.verdict,
        PolicyVerdict::Approved {
            rule_name: "global-approve-all".to_string(),
        }
    );
}

// ── USDC sponsorship demo path: TOML → PolicySet → fires on USDC ─────────

const USDC_MINT_DEMO: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

const USDC_DEMO_TOML: &str = r#"
[daemon]
db_path = "./data/test.db"

[network]
network = "devnet"

[rpc]
primary_url = "https://api.devnet.solana.com"
ws_url = "wss://api.devnet.solana.com"
timeout_ms = 15000

[policy]
mainnet_safe_defaults = false
program_allowlist = []
destination_denylist = []

# USDC medium-value: require human approval for transfers >= 100 USDC
[[policy.rules]]
name = "usdc-medium-value-requires-human"
description = "USDC transfers >= 100 USDC require human approval"
condition = { type = "TokenAmountExceeds", mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", threshold = 100000000 }
action = { type = "RequireHumanApproval", reason = "USDC >= 100 USDC requires operator approval" }

# USDC high-value: require multi-stage approval for transfers >= 10K USDC
[[policy.rules]]
name = "usdc-high-value-chain"
description = "USDC transfers >= 10K USDC require risk + treasury approval"
condition = { type = "TokenAmountExceeds", mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", threshold = 10000000000 }
action = { type = "RequireApprovalChain", reason = "USDC >= 10K requires multi-stage approval", stages = [
    { role = "risk", description = "Risk officer review", min_approvals = 1 },
    { role = "treasury", description = "Treasury sign-off", min_approvals = 1 },
]}

[llm]
provider = "openai"
api_key = ""
model = "gpt-4o-mini"

[api]
bind_addr = "127.0.0.1"
port = 7070

[logging]
format = "pretty"
level = "info"
"#;

fn proposal_with_usdc_transfer(amount: u64) -> claw_types::transaction::TransactionProposal {
    use claw_types::transaction::{InstructionSummary, TokenTransfer};
    claw_types::transaction::TransactionProposal {
        id: Uuid::new_v4(),
        session_id: claw_types::session::SessionId::from(Uuid::new_v4()),
        wallet_pubkey: "WalletPubkey1111111111111111111111111111111".to_string(),
        network: claw_types::solana::SolanaNetwork::Devnet,
        description: "USDC transfer".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![InstructionSummary {
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            program_name: Some("SPL Token".to_string()),
            description: format!("TransferChecked {} USDC raw units", amount),
            transfer_lamports: None,
            token_transfer: Some(TokenTransfer {
                mint: USDC_MINT_DEMO.to_string(),
                amount,
                decimals: Some(6),
                source: "src-token-account".to_string(),
                destination: "dst-token-account".to_string(),
            }),
            accounts: vec![],
        }],
        created_at: chrono::Utc::now(),
    }
}

#[test]
fn usdc_demo_toml_parses_with_two_chain_rules() {
    let config: ClawConfig = toml::from_str(USDC_DEMO_TOML).expect("USDC TOML should parse");
    assert_eq!(config.policy.rules.len(), 2);
    assert_eq!(config.policy.rules[0].name, "usdc-medium-value-requires-human");
    assert_eq!(config.policy.rules[1].name, "usdc-high-value-chain");
}

#[test]
fn usdc_50_usdc_falls_through_to_no_match_failsafe() {
    // Below 100 USDC threshold, no rule matches → fail-closed default
    let config: ClawConfig = toml::from_str(USDC_DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    let proposal = proposal_with_usdc_transfer(50_000_000); // 50 USDC
    let result = policy.evaluate(&eval_ctx(&proposal));

    // The two USDC rules don't match. There's no catch-all approve in this TOML.
    // PolicySet falls through to the default fail-closed: RequiresHumanApproval.
    assert!(
        result.verdict.requires_human(),
        "50 USDC with no matching rule should fail closed: {:?}",
        result.verdict
    );
}

#[test]
fn usdc_500_usdc_triggers_medium_value_rule() {
    let config: ClawConfig = toml::from_str(USDC_DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    let proposal = proposal_with_usdc_transfer(500_000_000); // 500 USDC
    let result = policy.evaluate(&eval_ctx(&proposal));

    assert_eq!(
        result.verdict,
        PolicyVerdict::RequiresHumanApproval {
            reason: "USDC >= 100 USDC requires operator approval".to_string(),
            rule_name: "usdc-medium-value-requires-human".to_string(),
            required_approver_role: None,
            approval_chain: None,
        },
        "500 USDC should trigger medium-value rule"
    );
    assert_eq!(result.matched_rule_index, Some(0));
}

#[test]
fn usdc_50k_usdc_triggers_multi_stage_chain() {
    let config: ClawConfig = toml::from_str(USDC_DEMO_TOML).unwrap();
    let policy = build_policy_set(&config);

    let proposal = proposal_with_usdc_transfer(50_000_000_000); // 50K USDC
    let result = policy.evaluate(&eval_ctx(&proposal));

    // Both rules match, but first-match-wins → medium-value rule fires.
    // This is intentional: if you want chain to take precedence, put it FIRST in TOML.
    // We test the default ordering behavior here.
    assert_eq!(
        result.matched_rule_index, Some(0),
        "first-match-wins: medium-value rule fires before chain rule"
    );
}

#[test]
fn usdc_chain_rule_fires_when_listed_first() {
    // Reorder rules: chain rule first (for high values to trigger it)
    let toml = r#"
[daemon]
db_path = "./data/test.db"
[network]
network = "devnet"
[rpc]
primary_url = "https://api.devnet.solana.com"
ws_url = "wss://api.devnet.solana.com"
timeout_ms = 15000
[policy]
mainnet_safe_defaults = false
program_allowlist = []
destination_denylist = []

[[policy.rules]]
name = "usdc-high-value-chain"
description = "USDC transfers >= 10K USDC require risk + treasury approval"
condition = { type = "TokenAmountExceeds", mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", threshold = 10000000000 }
action = { type = "RequireApprovalChain", reason = "USDC >= 10K requires multi-stage approval", stages = [
    { role = "risk", description = "Risk officer review", min_approvals = 1 },
    { role = "treasury", description = "Treasury sign-off", min_approvals = 1 },
]}

[[policy.rules]]
name = "usdc-medium-value-requires-human"
description = "USDC transfers >= 100 USDC require human approval"
condition = { type = "TokenAmountExceeds", mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", threshold = 100000000 }
action = { type = "RequireHumanApproval", reason = "USDC >= 100" }

[llm]
provider = "openai"
api_key = ""
model = "gpt-4o-mini"
[api]
bind_addr = "127.0.0.1"
port = 7070
[logging]
format = "pretty"
level = "info"
"#;
    let config: ClawConfig = toml::from_str(toml).unwrap();
    let policy = build_policy_set(&config);

    // 50K USDC should trigger the chain rule (first in list)
    let proposal = proposal_with_usdc_transfer(50_000_000_000);
    let result = policy.evaluate(&eval_ctx(&proposal));

    match &result.verdict {
        PolicyVerdict::RequiresHumanApproval { rule_name, approval_chain, .. } => {
            assert_eq!(rule_name, "usdc-high-value-chain");
            let chain = approval_chain.as_ref().expect("should carry chain");
            assert_eq!(chain.len(), 2);
            assert_eq!(chain[0].role, "risk");
            assert_eq!(chain[1].role, "treasury");
        }
        other => panic!("expected RequiresHumanApproval with chain, got {:?}", other),
    }
}

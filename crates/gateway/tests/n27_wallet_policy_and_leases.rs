//! N27 — Wallet-specific policy overrides and lease behavior.
//!
//! Verifies that per-wallet policy rules from `WalletPolicyConfig` are compiled
//! correctly and override the global policy when prepended via `with_session_rules`.

use uuid::Uuid;

use claw_types::{
    policy::{PolicyAction, PolicyCondition, PolicyRule, PolicyVerdict},
    session::SessionId,
    solana::SolanaNetwork,
    transaction::{AccountRole, InstructionSummary, TransactionProposal},
    wallet::SignerType,
};
use claw_risk_engine::{PolicyEvaluationContext, PolicySet};
use claw_gateway::config::{WalletConfig, WalletPolicyConfig};
use claw_gateway::wallet_policy::WalletPolicyStore;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_proposal(wallet_pubkey: &str, transfer_lamports: u64) -> TransactionProposal {
    TransactionProposal {
        id: Uuid::new_v4(),
        session_id: SessionId::from(Uuid::new_v4()),
        wallet_pubkey: wallet_pubkey.to_string(),
        network: SolanaNetwork::Devnet,
        description: "wallet policy test".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![InstructionSummary {
            program_id: "11111111111111111111111111111111".to_string(),
            program_name: Some("system".to_string()),
            description: "SOL transfer".to_string(),
            transfer_lamports: Some(transfer_lamports),
            token_transfer: None,
            accounts: vec![
                AccountRole {
                    pubkey: wallet_pubkey.to_string(),
                    label: Some("from".to_string()),
                    is_signer: true,
                    is_writable: true,
                },
                AccountRole {
                    pubkey: "dest-pubkey".to_string(),
                    label: Some("to".to_string()),
                    is_signer: false,
                    is_writable: true,
                },
            ],
        }],
        created_at: chrono::Utc::now(),
    }
}

fn make_context<'a>(proposal: &'a TransactionProposal) -> PolicyEvaluationContext<'a> {
    PolicyEvaluationContext {
        proposal,
        simulation_result: None,
        network: SolanaNetwork::Devnet,
        session_id: &proposal.session_id,
        session_spend_lamports: 0,
        wallet_daily_spend_lamports: 0,
    }
}

fn wallet_config(label: &str, pubkey: &str, max_lamports: u64) -> WalletConfig {
    WalletConfig {
        label: label.to_string(),
        signer_type: SignerType::External,
        keypair_path: None,
        keypair_base58: None,
        pubkey: Some(pubkey.to_string()),
        policy: Some(WalletPolicyConfig {
            max_amount_lamports: Some(max_lamports),
            program_allowlist: vec![],
            required_approver_role: None,
            rules: vec![],
        }),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn wallet_specific_policy_overrides_global_policy() {
    // Build the WalletPolicyStore with two wallets, different caps
    let wallet_a_pubkey = "wallet-a-pubkey-11111111111";
    let wallet_b_pubkey = "wallet-b-pubkey-22222222222";

    let mut store = WalletPolicyStore::new();
    store.register(
        wallet_a_pubkey.to_string(),
        &wallet_config("Wallet A", wallet_a_pubkey, 50_000_000), // 0.05 SOL
    );
    store.register(
        wallet_b_pubkey.to_string(),
        &wallet_config("Wallet B", wallet_b_pubkey, 500_000_000), // 0.5 SOL
    );

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

    let transfer_amount = 100_000_000; // 0.1 SOL

    // --- Wallet A: 0.1 SOL exceeds 0.05 SOL cap → RequiresHumanApproval ---
    let wallet_a_rules = store.get(wallet_a_pubkey).expect("wallet A should have rules");
    let effective_a = global.with_session_rules(wallet_a_rules);
    let proposal_a = make_proposal(wallet_a_pubkey, transfer_amount);
    let result_a = effective_a.evaluate(&make_context(&proposal_a));

    assert!(
        result_a.verdict.requires_human(),
        "wallet A: 0.1 SOL exceeds 0.05 SOL cap, should require approval: {:?}",
        result_a.verdict,
    );

    // --- Wallet B: 0.1 SOL is below 0.5 SOL cap → falls through to global approve ---
    let wallet_b_rules = store.get(wallet_b_pubkey).expect("wallet B should have rules");
    let effective_b = global.with_session_rules(wallet_b_rules);
    let proposal_b = make_proposal(wallet_b_pubkey, transfer_amount);
    let result_b = effective_b.evaluate(&make_context(&proposal_b));

    assert_eq!(
        result_b.verdict,
        PolicyVerdict::Approved {
            rule_name: "global-approve".to_string(),
        },
        "wallet B: 0.1 SOL is below 0.5 SOL cap, should be auto-approved",
    );
}

#[test]
fn wallet_without_policy_has_no_rules() {
    let mut store = WalletPolicyStore::new();

    let cfg = WalletConfig {
        label: "bare-wallet".to_string(),
        signer_type: SignerType::External,
        keypair_path: None,
        keypair_base58: None,
        pubkey: Some("bare-pubkey-333".to_string()),
        policy: None, // No policy
    };
    store.register("bare-pubkey-333".to_string(), &cfg);

    assert!(store.get("bare-pubkey-333").is_none(), "no policy → no rules");
}

#[test]
fn wallet_required_approver_role_propagates() {
    let pubkey = "role-wallet-444";
    let mut store = WalletPolicyStore::new();

    let cfg = WalletConfig {
        label: "role-wallet".to_string(),
        signer_type: SignerType::External,
        keypair_path: None,
        keypair_base58: None,
        pubkey: Some(pubkey.to_string()),
        policy: Some(WalletPolicyConfig {
            max_amount_lamports: Some(100_000_000),
            program_allowlist: vec![],
            required_approver_role: Some("treasury".to_string()),
            rules: vec![],
        }),
    };
    store.register(pubkey.to_string(), &cfg);

    let rules = store.get(pubkey).expect("should have rules");
    assert!(!rules.is_empty());

    // The amount cap rule should carry the required_approver_role
    let cap_rule = &rules[0];
    match &cap_rule.action {
        PolicyAction::RequireHumanApproval { required_approver_role, .. } => {
            assert_eq!(
                required_approver_role.as_deref(),
                Some("treasury"),
                "approver role should propagate to the generated rule"
            );
        }
        other => panic!("expected RequireHumanApproval, got {:?}", other),
    }
}

#[test]
fn wallet_from_config_builds_store_correctly() {
    let pubkey = "from-config-555";
    let wallets = vec![WalletConfig {
        label: "config-wallet".to_string(),
        signer_type: SignerType::External,
        keypair_path: None,
        keypair_base58: None,
        pubkey: Some(pubkey.to_string()),
        policy: Some(WalletPolicyConfig {
            max_amount_lamports: Some(200_000_000),
            program_allowlist: vec![],
            required_approver_role: None,
            rules: vec![],
        }),
    }];

    let store = WalletPolicyStore::from_config(&wallets);

    let rules = store.get(pubkey).expect("should have rules from config");
    assert_eq!(rules.len(), 1);
    assert!(rules[0].name.contains("amount-cap"));
}

#[test]
fn expired_signing_lease_rejects_late_approval() {
    use claw_types::approval::{
        ApprovalDecision, ApprovalOutcome, ApprovalRequest, ApprovalWorkflow,
    };
    use claw_gateway::ApprovalStore;

    let session_id = SessionId::from(Uuid::new_v4());
    let request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "lease expiry test",
        PolicyVerdict::RequiresHumanApproval {
            reason: "lease test".into(),
            rule_name: "lease-rule".into(),
            required_approver_role: None,
            approval_chain: None,
        },
        claw_types::transaction::SimulationResult {
            success: true,
            error: None,
            compute_units_used: Some(1000),
            logs: vec![],
            return_data: None,
            account_diffs: vec![],
            fee_lamports: Some(5000),
        },
    );
    let request_id = request.id;

    // Create a workflow with an already-expired lease.
    let mut wf = ApprovalWorkflow::single_stage(request_id, session_id, None);
    wf.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(10));

    let store = ApprovalStore::new();
    store.register(request, wf);
    assert_eq!(store.pending_count(), 1, "request is registered");

    // Try to approve — should be rejected because the lease has expired.
    let d = ApprovalDecision::approve(request_id, Some("too late".into()));
    let (outcome, maybe_req) = store.decide(&d);

    // The store treats expired workflows as "gone" (NotFound).
    assert_eq!(
        outcome,
        ApprovalOutcome::NotFound,
        "expired workflow should reject the approval"
    );
    // The request should be consumed/removed since the workflow expired.
    assert!(maybe_req.is_some(), "expired request should be returned for cleanup");
    assert_eq!(store.pending_count(), 0, "expired request should be removed");
}

#[tokio::test]
async fn expired_lease_blocks_pending_signing_signal() {
    use claw_types::approval::{
        ApprovalRequest, ApprovalWorkflow, ApprovalWorkflowState,
    };
    use claw_gateway::{ApprovalStore, PendingSigningStore};
    use claw_types::transaction::{SimulationResult, TransactionProposal};
    use solana_sdk::{message::Message, pubkey::Pubkey, system_instruction, transaction::Transaction};

    let pending = PendingSigningStore::new();
    let request_id = Uuid::new_v4();
    let session_id = SessionId::from(Uuid::new_v4());

    // Build a minimal unsigned Solana transaction for parking.
    let from = Pubkey::new_unique();
    let to = Pubkey::new_unique();
    let ix = system_instruction::transfer(&from, &to, 1000);
    let msg = Message::new(&[ix], Some(&from));
    let tx = Transaction::new_unsigned(msg);

    let proposal = TransactionProposal {
        id: Uuid::new_v4(),
        session_id: session_id.clone(),
        wallet_pubkey: from.to_string(),
        network: SolanaNetwork::Devnet,
        description: "expired lease test".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![],
        created_at: chrono::Utc::now(),
    };
    let sim = SimulationResult {
        success: true,
        error: None,
        compute_units_used: Some(1000),
        logs: vec![],
        return_data: None,
        account_diffs: vec![],
        fee_lamports: Some(5000),
    };
    let verdict = PolicyVerdict::RequiresHumanApproval {
        reason: "lease test".into(),
        rule_name: "lease-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };

    let decision_rx = pending
        .park(request_id, proposal, &tx, sim, verdict, 100)
        .expect("park should succeed");

    // Create an already-expired workflow.
    let mut wf = ApprovalWorkflow::single_stage(request_id, session_id.clone(), Some("treasury".to_string()));
    wf.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(60));

    let req = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "expired lease request",
        PolicyVerdict::RequiresHumanApproval {
            reason: "lease test".into(),
            rule_name: "lease-rule".into(),
            required_approver_role: Some("treasury".to_string()),
            approval_chain: None,
        },
        claw_types::transaction::SimulationResult {
            success: true,
            error: None,
            compute_units_used: Some(1000),
            logs: vec![],
            return_data: None,
            account_diffs: vec![],
            fee_lamports: Some(5000),
        },
    );
    let mut req = req;
    req.id = request_id;

    let store = ApprovalStore::new();
    store.register(req, wf);

    // Try to approve — should detect expiry and return NotFound.
    let decision = claw_types::approval::ApprovalDecision::approve_as(
        request_id,
        None,
        "treasury".to_string(),
    );
    let (outcome, expired_req) = store.decide(&decision);
    assert_eq!(
        outcome,
        claw_types::approval::ApprovalOutcome::NotFound,
        "expired workflow should reject the approval"
    );
    assert!(expired_req.is_some(), "expired request should be returned for cleanup");

    // Replicate daemon wiring: on expiry detection, signal PendingSigningStore with Expired.
    let signaled = pending.signal(request_id, ApprovalWorkflowState::Expired);
    assert!(signaled, "PendingSigningStore should accept the expired signal");

    // Verify the oneshot received the Expired state.
    let state = decision_rx.await.expect("should receive expired signal");
    assert_eq!(state, ApprovalWorkflowState::Expired);
    assert!(!state.is_approved(), "expired state must NOT be approved");
}

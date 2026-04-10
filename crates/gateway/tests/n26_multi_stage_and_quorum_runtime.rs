//! N26 — Multi-stage approval chain and quorum runtime verification.
//!
//! Tests the full flow from `PolicyAction::RequireApprovalChain` through
//! policy evaluation, workflow creation, and multi-stage approval completion.

use uuid::Uuid;

use claw_types::{
    approval::{
        ApprovalChainStage, ApprovalDecision, ApprovalOutcome, ApprovalRequest,
        ApprovalStage, ApprovalWorkflow, ApprovalWorkflowState,
    },
    policy::{PolicyAction, PolicyCondition, PolicyRule, PolicyVerdict},
    session::SessionId,
    solana::SolanaNetwork,
    transaction::{AccountRole, InstructionSummary, SimulationResult, TransactionProposal},
};
use claw_risk_engine::{PolicyEvaluationContext, PolicySet};
use claw_gateway::ApprovalStore;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_proposal(wallet: &str, transfer_lamports: u64) -> TransactionProposal {
    TransactionProposal {
        id: Uuid::new_v4(),
        session_id: SessionId::from(Uuid::new_v4()),
        wallet_pubkey: wallet.to_string(),
        network: SolanaNetwork::Devnet,
        description: "multi-stage test transfer".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![InstructionSummary {
            program_id: "11111111111111111111111111111111".to_string(),
            program_name: Some("system".to_string()),
            description: "SOL transfer".to_string(),
            transfer_lamports: Some(transfer_lamports),
            token_transfer: None,
            accounts: vec![
                AccountRole {
                    pubkey: wallet.to_string(),
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

fn fake_simulation() -> SimulationResult {
    SimulationResult {
        success: true,
        error: None,
        compute_units_used: Some(1000),
        logs: vec![],
        return_data: None,
        account_diffs: vec![],
        fee_lamports: Some(5000),
    }
}

/// Builds an ApprovalWorkflow from approval chain stages (replicates signing.rs logic).
fn workflow_from_chain(
    request_id: Uuid,
    session_id: SessionId,
    chain: &[ApprovalChainStage],
) -> ApprovalWorkflow {
    let now = chrono::Utc::now();
    let stages: Vec<ApprovalStage> = chain
        .iter()
        .enumerate()
        .map(|(i, cs)| ApprovalStage {
            index: i,
            allowed_roles: vec![cs.role.clone()],
            min_approvals: cs.min_approvals,
            decisions: vec![],
        })
        .collect();
    ApprovalWorkflow {
        request_id,
        session_id,
        state: ApprovalWorkflowState::Pending,
        stages,
        created_at: now,
        updated_at: now,
        expires_at: None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn multi_step_chain_happy_path_completes_signing() {
    // Step 1: Create a PolicySet with RequireApprovalChain
    let chain_stages = vec![
        ApprovalChainStage {
            role: "risk".to_string(),
            description: "Risk review".to_string(),
            min_approvals: 1,
        },
        ApprovalChainStage {
            role: "treasury".to_string(),
            description: "Treasury sign-off".to_string(),
            min_approvals: 1,
        },
    ];

    let policy = PolicySet::new(
        vec![
            PolicyRule {
                name: "high-value-chain".to_string(),
                description: "High-value transfers need risk + treasury approval".to_string(),
                condition: PolicyCondition::AmountExceedsLamports(500_000_000), // 0.5 SOL
                action: PolicyAction::RequireApprovalChain {
                    reason: "high-value transfer".to_string(),
                    stages: chain_stages.clone(),
                },
            },
            PolicyRule {
                name: "fallback-approve".to_string(),
                description: "approve small amounts".to_string(),
                condition: PolicyCondition::Always,
                action: PolicyAction::Approve,
            },
        ],
        vec![],
        vec![],
    );

    // Step 2: Create a proposal that triggers the chain rule
    let proposal = make_proposal("test-wallet", 1_000_000_000); // 1 SOL
    let ctx = make_context(&proposal);

    // Step 3: Evaluate policy
    let result = policy.evaluate(&ctx);
    let approval_chain = match &result.verdict {
        PolicyVerdict::RequiresHumanApproval { approval_chain, reason, .. } => {
            assert!(reason.contains("high-value"), "reason: {}", reason);
            approval_chain.clone().expect("should have approval chain")
        }
        other => panic!("expected RequiresHumanApproval, got {:?}", other),
    };
    assert_eq!(approval_chain.len(), 2);
    assert_eq!(approval_chain[0].role, "risk");
    assert_eq!(approval_chain[1].role, "treasury");

    // Step 4: Create the workflow and register in ApprovalStore
    let session_id = proposal.session_id.clone();
    let request = ApprovalRequest::new(
        session_id.clone(),
        proposal.id,
        "1 SOL transfer",
        result.verdict,
        fake_simulation(),
    );
    let request_id = request.id;
    let workflow = workflow_from_chain(request_id, session_id, &approval_chain);

    let store = ApprovalStore::new();
    store.register(request, workflow);

    // Step 5: Stage 0 — risk approves
    let d1 = ApprovalDecision::approve_as(request_id, Some("risk-approved".into()), "risk".to_string());
    let (outcome1, _) = store.decide(&d1);
    assert_eq!(
        outcome1,
        ApprovalOutcome::StageAdvanced {
            completed_stage: 0,
            next_stage: 1,
            next_required_role: Some("treasury".to_string()),
        },
        "risk approval should advance to treasury stage"
    );

    // Step 6: Stage 1 — treasury approves → terminal Approved
    let d2 = ApprovalDecision::approve_as(request_id, Some("treasury-ok".into()), "treasury".to_string());
    let (outcome2, maybe_req) = store.decide(&d2);
    assert_eq!(outcome2, ApprovalOutcome::Approved);
    assert!(maybe_req.is_some(), "terminal approval should return the request");
    assert_eq!(store.pending_count(), 0);
}

#[test]
fn later_stage_cannot_approve_before_earlier_stage() {
    let session_id = SessionId::from(Uuid::new_v4());
    let chain = vec![
        ApprovalChainStage {
            role: "risk".to_string(),
            description: "Risk review".to_string(),
            min_approvals: 1,
        },
        ApprovalChainStage {
            role: "treasury".to_string(),
            description: "Treasury sign-off".to_string(),
            min_approvals: 1,
        },
    ];

    let request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "test transfer",
        PolicyVerdict::RequiresHumanApproval {
            reason: "chain test".into(),
            rule_name: "chain-rule".into(),
            required_approver_role: None,
            approval_chain: Some(chain.clone()),
        },
        fake_simulation(),
    );
    let request_id = request.id;
    let workflow = workflow_from_chain(request_id, session_id, &chain);

    let store = ApprovalStore::new();
    store.register(request, workflow);

    // Treasury tries to approve first — should get RoleMismatch because stage 0 needs "risk"
    let d1 = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
    let (outcome, _) = store.decide(&d1);

    assert!(
        matches!(outcome, ApprovalOutcome::RoleMismatch { .. }),
        "treasury should not be able to approve stage 0 (risk): {:?}",
        outcome,
    );
    assert_eq!(store.pending_count(), 1, "request stays pending");
}

#[test]
fn rejection_at_first_stage_is_terminal() {
    let session_id = SessionId::from(Uuid::new_v4());
    let chain = vec![
        ApprovalChainStage {
            role: "risk".to_string(),
            description: "Risk review".to_string(),
            min_approvals: 1,
        },
        ApprovalChainStage {
            role: "treasury".to_string(),
            description: "Treasury sign-off".to_string(),
            min_approvals: 1,
        },
    ];

    let request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "test transfer",
        PolicyVerdict::RequiresHumanApproval {
            reason: "chain test".into(),
            rule_name: "chain-rule".into(),
            required_approver_role: None,
            approval_chain: Some(chain.clone()),
        },
        fake_simulation(),
    );
    let request_id = request.id;
    let workflow = workflow_from_chain(request_id, session_id, &chain);

    let store = ApprovalStore::new();
    store.register(request, workflow);

    // Risk rejects
    let d1 = ApprovalDecision {
        request_id,
        approved: false,
        note: Some("too risky".into()),
        approver_role: Some("risk".to_string()),
        operator_id: None,
        decided_at: chrono::Utc::now(),
    };
    let (outcome, maybe_req) = store.decide(&d1);
    assert_eq!(outcome, ApprovalOutcome::Rejected);
    assert!(maybe_req.is_some());
    assert_eq!(store.pending_count(), 0);
}

#[test]
fn small_amount_below_threshold_is_auto_approved() {
    // Verify that amounts below the chain threshold fall through to the approve rule
    let policy = PolicySet::new(
        vec![
            PolicyRule {
                name: "high-value-chain".to_string(),
                description: "chain for high value".to_string(),
                condition: PolicyCondition::AmountExceedsLamports(500_000_000),
                action: PolicyAction::RequireApprovalChain {
                    reason: "high value".to_string(),
                    stages: vec![ApprovalChainStage {
                        role: "risk".to_string(),
                        description: "risk".to_string(),
                        min_approvals: 1,
                    }],
                },
            },
            PolicyRule {
                name: "auto-approve".to_string(),
                description: "approve small".to_string(),
                condition: PolicyCondition::Always,
                action: PolicyAction::Approve,
            },
        ],
        vec![],
        vec![],
    );

    let proposal = make_proposal("test-wallet", 100_000_000); // 0.1 SOL, below threshold
    let ctx = make_context(&proposal);
    let result = policy.evaluate(&ctx);

    assert_eq!(
        result.verdict,
        PolicyVerdict::Approved {
            rule_name: "auto-approve".to_string(),
        }
    );
}

#[test]
fn quorum_stage_completes_only_after_n_unique_approvals() {
    // A stage requiring min_approvals=2 should return QuorumProgress after
    // the first approval and Approved after the second from a different operator.
    let session_id = SessionId::from(Uuid::new_v4());
    let request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "quorum transfer",
        PolicyVerdict::RequiresHumanApproval {
            reason: "quorum test".into(),
            rule_name: "quorum-rule".into(),
            required_approver_role: None,
            approval_chain: None,
        },
        fake_simulation(),
    );
    let request_id = request.id;

    // Single-stage with quorum=2
    let now = chrono::Utc::now();
    let workflow = ApprovalWorkflow {
        request_id,
        session_id,
        state: ApprovalWorkflowState::Pending,
        stages: vec![ApprovalStage {
            index: 0,
            allowed_roles: vec!["treasury".to_string()],
            min_approvals: 2,
            decisions: vec![],
        }],
        created_at: now,
        updated_at: now,
        expires_at: None,
    };

    let store = ApprovalStore::new();
    store.register(request, workflow);

    // First approval from alice → QuorumProgress (1/2)
    let mut d1 = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
    d1.operator_id = Some("alice".to_string());
    let (outcome1, _) = store.decide(&d1);
    assert_eq!(
        outcome1,
        ApprovalOutcome::QuorumProgress {
            stage: 0,
            approvals_so_far: 1,
            approvals_required: 2,
        },
        "first approval should show 1/2 progress"
    );

    // Second approval from bob → Approved (terminal)
    let mut d2 = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
    d2.operator_id = Some("bob".to_string());
    let (outcome2, maybe_req) = store.decide(&d2);
    assert_eq!(outcome2, ApprovalOutcome::Approved, "second unique approval should complete quorum");
    assert!(maybe_req.is_some(), "terminal approval should return the request");
    assert_eq!(store.pending_count(), 0, "no more pending requests");
}

#[test]
fn duplicate_approval_from_same_operator_counts_once() {
    // Same operator approving twice on a quorum stage should be rejected
    // and the approval count should NOT increase.
    let session_id = SessionId::from(Uuid::new_v4());
    let request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "dedup transfer",
        PolicyVerdict::RequiresHumanApproval {
            reason: "dedup test".into(),
            rule_name: "dedup-rule".into(),
            required_approver_role: None,
            approval_chain: None,
        },
        fake_simulation(),
    );
    let request_id = request.id;

    let now = chrono::Utc::now();
    let workflow = ApprovalWorkflow {
        request_id,
        session_id,
        state: ApprovalWorkflowState::Pending,
        stages: vec![ApprovalStage {
            index: 0,
            allowed_roles: vec!["treasury".to_string()],
            min_approvals: 2,
            decisions: vec![],
        }],
        created_at: now,
        updated_at: now,
        expires_at: None,
    };

    let store = ApprovalStore::new();
    store.register(request, workflow);

    // Alice approves once → QuorumProgress (1/2)
    let mut d1 = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
    d1.operator_id = Some("alice".to_string());
    let (outcome1, _) = store.decide(&d1);
    assert!(matches!(outcome1, ApprovalOutcome::QuorumProgress { approvals_so_far: 1, .. }));

    // Alice tries to approve again → DuplicateOperator
    let mut d2 = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
    d2.operator_id = Some("alice".to_string());
    let (outcome2, _) = store.decide(&d2);
    assert!(
        matches!(outcome2, ApprovalOutcome::DuplicateOperator { .. }),
        "duplicate should be rejected: {:?}",
        outcome2,
    );

    // Verify approvals did not increase — check via workflow.
    let wf = store.get_workflow(request_id).unwrap();
    assert_eq!(
        wf.stages[0].approval_count(),
        1,
        "approval count should still be 1 after duplicate rejection"
    );
    assert_eq!(store.pending_count(), 1, "request should still be pending");
}

// ── GAP 1: Multi-stage approval → PendingSigningStore signal integration ────

#[tokio::test]
async fn multi_stage_chain_signals_pending_store_only_on_terminal() {
    use claw_gateway::PendingSigningStore;
    use claw_types::approval::ApprovalWorkflowState;
    use claw_types::transaction::TransactionProposal;
    use solana_sdk::{message::Message, pubkey::Pubkey, system_instruction, transaction::Transaction};

    // 1. Create a PendingSigningStore and park a transaction to get the oneshot rx.
    let pending_signing = PendingSigningStore::new();
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
        network: claw_types::solana::SolanaNetwork::Devnet,
        description: "multi-stage pending-store test".to_string(),
        transaction_b64: String::new(),
        instructions_summary: vec![],
        created_at: chrono::Utc::now(),
    };
    let simulation = fake_simulation();
    let verdict = claw_types::policy::PolicyVerdict::RequiresHumanApproval {
        reason: "chain".into(),
        rule_name: "chain-rule".into(),
        required_approver_role: None,
        approval_chain: None,
    };

    let decision_rx = pending_signing
        .park(request_id, proposal, &tx, simulation, verdict, 100)
        .expect("park should succeed");

    // 2. Create approval store with a multi-stage workflow (risk → treasury).
    let approval_store = ApprovalStore::new();
    let approval_request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "multi-stage chain signal test",
        claw_types::policy::PolicyVerdict::RequiresHumanApproval {
            reason: "chain".into(),
            rule_name: "chain-rule".into(),
            required_approver_role: None,
            approval_chain: None,
        },
        fake_simulation(),
    );
    // Override the request id to match the parked transaction.
    let mut req = approval_request;
    req.id = request_id;

    let now = chrono::Utc::now();
    let workflow = ApprovalWorkflow {
        request_id,
        session_id: session_id.clone(),
        state: ApprovalWorkflowState::Pending,
        stages: vec![
            ApprovalStage {
                index: 0,
                allowed_roles: vec!["risk".to_string()],
                min_approvals: 1,
                decisions: vec![],
            },
            ApprovalStage {
                index: 1,
                allowed_roles: vec!["treasury".to_string()],
                min_approvals: 1,
                decisions: vec![],
            },
        ],
        created_at: now,
        updated_at: now,
        expires_at: None,
    };
    approval_store.register(req, workflow);

    // 3. Stage 0: risk approves → StageAdvanced (NOT terminal).
    let d1 = ApprovalDecision::approve_as(request_id, None, "risk".to_string());
    let (o1, _) = approval_store.decide(&d1);
    assert!(
        matches!(o1, ApprovalOutcome::StageAdvanced { .. }),
        "stage 0 should advance: {:?}",
        o1,
    );

    // REPLICATE daemon wiring: StageAdvanced → do NOT signal PendingSigningStore.
    // Verify the oneshot has NOT been signaled by checking with a short timeout.
    let mut decision_rx = decision_rx;
    let premature = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        &mut decision_rx,
    )
    .await;
    assert!(
        premature.is_err(),
        "PendingSigningStore should NOT be signaled after StageAdvanced"
    );

    // 4. Stage 1: treasury approves → Approved (terminal).
    let d2 = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
    let (o2, maybe_req) = approval_store.decide(&d2);
    assert_eq!(o2, ApprovalOutcome::Approved);
    assert!(maybe_req.is_some(), "terminal should return the request");

    // REPLICATE daemon wiring: terminal Approved → signal PendingSigningStore.
    let signaled = pending_signing.signal(request_id, ApprovalWorkflowState::Approved);
    assert!(signaled, "PendingSigningStore should accept the signal");

    // 5. Verify the oneshot received Approved.
    let state = decision_rx.await.expect("should receive signal from oneshot");
    assert!(state.is_approved(), "received state should be Approved");
}

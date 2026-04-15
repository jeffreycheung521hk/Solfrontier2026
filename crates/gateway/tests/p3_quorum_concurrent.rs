//! P3 — Quorum approval store under concurrent operator decisions.
//!
//! Background: `ApprovalStore::decide` takes `workflows.get_mut(...)`, which
//! is a per-key exclusive lock on the underlying `DashMap`. The contract is:
//!
//! - When N operators race to approve a single quorum stage requiring
//!   M approvals (M < N), exactly the first M succeed as quorum-progressing
//!   or workflow-completing outcomes; the remaining (N - M) see either
//!   `DuplicateOperator` (if they re-submit) or `AlreadyDecided` (if the
//!   workflow already terminated).
//! - The final workflow state is `Approved`.
//! - There must be no panic, no deadlock, and no double-counted approval.
//!
//! These tests spawn real `tokio::spawn` tasks against a `Clone`-d
//! `ApprovalStore` (DashMap + Arc inside) so the concurrency is genuine,
//! not single-threaded interleaving.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use claw_types::{
    approval::{
        ApprovalDecision, ApprovalOutcome, ApprovalRequest, ApprovalStage,
        ApprovalWorkflow, ApprovalWorkflowState,
    },
    policy::PolicyVerdict,
    session::SessionId,
    transaction::SimulationResult,
};
use claw_gateway::ApprovalStore;

// ── Helpers ──────────────────────────────────────────────────────────────────

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

/// Builds a 1-stage workflow whose single stage requires `min_approvals`
/// approvals from any operator claiming the `risk` role.
fn quorum_workflow(
    request_id: Uuid,
    session_id: SessionId,
    min_approvals: u32,
) -> (ApprovalRequest, ApprovalWorkflow) {
    let now = Utc::now();
    let request = ApprovalRequest {
        id: request_id,
        session_id: session_id.clone(),
        transaction_id: Uuid::new_v4(),
        description: "concurrent quorum test".to_string(),
        policy_verdict: PolicyVerdict::RequiresHumanApproval {
            reason: "quorum required".to_string(),
            rule_name: "p3-concurrent-quorum".to_string(),
            required_approver_role: Some("risk".to_string()),
            approval_chain: None,
        },
        simulation: fake_simulation(),
        requested_at: now,
        decided: false,
        required_approver_role: Some("risk".to_string()),
    };
    let workflow = ApprovalWorkflow {
        request_id,
        session_id,
        state: ApprovalWorkflowState::Pending,
        stages: vec![ApprovalStage {
            index: 0,
            allowed_roles: vec!["risk".to_string()],
            min_approvals,
            decisions: vec![],
        }],
        created_at: now,
        updated_at: now,
        expires_at: None,
    };
    (request, workflow)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// 5 distinct operators race to approve a quorum-of-3 stage. Exactly 3
/// successful outcomes (StageAdvanced or QuorumProgress culminating in
/// Approved); the remaining 2 see AlreadyDecided. No panics, no dups in
/// the recorded decision list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quorum_3_of_5_concurrent_decisions_are_correct() {
    let store = ApprovalStore::new();
    let request_id = Uuid::new_v4();
    let session_id = SessionId::from(Uuid::new_v4());
    let (request, workflow) = quorum_workflow(request_id, session_id, 3);
    store.register(request, workflow);

    let store_arc = Arc::new(store);
    let mut handles = Vec::with_capacity(5);
    for op_idx in 0..5 {
        let store_cloned: ApprovalStore = (*store_arc).clone();
        let handle = tokio::spawn(async move {
            let decision = ApprovalDecision {
                request_id,
                approved: true,
                note: None,
                approver_role: Some("risk".to_string()),
                operator_id: Some(format!("op-{op_idx}")),
                decided_at: Utc::now(),
            };
            store_cloned.decide(&decision)
        });
        handles.push(handle);
    }

    let mut outcome_counts: HashMap<&'static str, usize> = HashMap::new();
    for h in handles {
        let (outcome, _req) = h.await.expect("task panicked");
        let label = match outcome {
            ApprovalOutcome::Approved => "approved",
            ApprovalOutcome::Rejected => "rejected",
            ApprovalOutcome::StageAdvanced { .. } => "stage_advanced",
            ApprovalOutcome::QuorumProgress { .. } => "quorum_progress",
            ApprovalOutcome::RoleMismatch { .. } => "role_mismatch",
            ApprovalOutcome::AlreadyDecided => "already_decided",
            ApprovalOutcome::DuplicateOperator { .. } => "duplicate_operator",
            ApprovalOutcome::NotFound => "not_found",
            ApprovalOutcome::Expired => "expired",
        };
        *outcome_counts.entry(label).or_insert(0) += 1;
    }

    // The first 2 decisions register as quorum_progress (1/3, 2/3).
    // The 3rd hits the threshold -> Approved (terminal).
    // The remaining 2 arrive after the workflow is terminal -> already_decided.
    assert_eq!(
        outcome_counts.get("approved").copied().unwrap_or(0),
        1,
        "exactly one decision should complete the quorum: {outcome_counts:?}"
    );
    assert_eq!(
        outcome_counts.get("quorum_progress").copied().unwrap_or(0),
        2,
        "exactly two decisions should be quorum progress: {outcome_counts:?}"
    );
    assert_eq!(
        outcome_counts.get("already_decided").copied().unwrap_or(0),
        2,
        "exactly two decisions should arrive after terminal: {outcome_counts:?}"
    );

    // Workflow is terminal Approved.
    let final_workflow = store_arc
        .get_workflow(request_id)
        .expect("workflow still present");
    assert_eq!(final_workflow.state, ApprovalWorkflowState::Approved);

    // The stage recorded exactly 3 approvals — no double-counting.
    let stage = &final_workflow.stages[0];
    assert_eq!(stage.decisions.len(), 3, "expected exactly 3 recorded decisions");
    assert!(stage.decisions.iter().all(|d| d.approved));

    // The 3 successful operators are distinct.
    let mut ids: Vec<&str> = stage
        .decisions
        .iter()
        .filter_map(|d| d.operator_id.as_deref())
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "the 3 winners must be distinct operators");
}

/// Same operator id submits 5 concurrent decisions. The dedup invariant
/// (`current_stage.has_operator_voted`) means at most one is recorded; the
/// rest see `DuplicateOperator`. Without the per-key DashMap lock this
/// would race and produce > 1 recorded approval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_operator_dedup_holds_under_concurrency() {
    let store = ApprovalStore::new();
    let request_id = Uuid::new_v4();
    let session_id = SessionId::from(Uuid::new_v4());
    let (request, workflow) = quorum_workflow(request_id, session_id, 3);
    store.register(request, workflow);

    let store_arc = Arc::new(store);
    let mut handles = Vec::with_capacity(5);
    for _ in 0..5 {
        let store_cloned: ApprovalStore = (*store_arc).clone();
        let handle = tokio::spawn(async move {
            let decision = ApprovalDecision {
                request_id,
                approved: true,
                note: None,
                approver_role: Some("risk".to_string()),
                operator_id: Some("op-spammer".to_string()),
                decided_at: Utc::now(),
            };
            store_cloned.decide(&decision)
        });
        handles.push(handle);
    }

    let mut accepted = 0usize;
    let mut duplicates = 0usize;
    for h in handles {
        let (outcome, _) = h.await.expect("task panicked");
        match outcome {
            ApprovalOutcome::QuorumProgress { .. }
            | ApprovalOutcome::StageAdvanced { .. }
            | ApprovalOutcome::Approved => accepted += 1,
            ApprovalOutcome::DuplicateOperator { .. } => duplicates += 1,
            other => panic!("unexpected outcome under dup operator race: {other:?}"),
        }
    }

    assert_eq!(accepted, 1, "only one decision from the same op should be accepted");
    assert_eq!(duplicates, 4, "the other four must be flagged as duplicates");

    let workflow = store_arc.get_workflow(request_id).expect("present");
    assert_eq!(workflow.state, ApprovalWorkflowState::Pending);
    assert_eq!(workflow.stages[0].decisions.len(), 1);
}

/// One operator approves; another concurrently rejects. Rejection is
/// immediately terminal regardless of quorum progress; the other decision
/// either lands first (single quorum progress, then rejection terminates)
/// or arrives after termination (already_decided). Either way, the final
/// state must be one of {Rejected, AlreadyDecided-on-the-approve}, never
/// a contradiction such as Approved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_approve_and_reject_never_contradicts() {
    for _ in 0..16 {
        let store = ApprovalStore::new();
        let request_id = Uuid::new_v4();
        let session_id = SessionId::from(Uuid::new_v4());
        let (request, workflow) = quorum_workflow(request_id, session_id, 3);
        store.register(request, workflow);

        let store_arc = Arc::new(store);

        let approver_store: ApprovalStore = (*store_arc).clone();
        let approver = tokio::spawn(async move {
            let d = ApprovalDecision {
                request_id,
                approved: true,
                note: None,
                approver_role: Some("risk".to_string()),
                operator_id: Some("op-yes".to_string()),
                decided_at: Utc::now(),
            };
            approver_store.decide(&d)
        });
        let rejector_store: ApprovalStore = (*store_arc).clone();
        let rejector = tokio::spawn(async move {
            let d = ApprovalDecision {
                request_id,
                approved: false,
                note: None,
                approver_role: Some("risk".to_string()),
                operator_id: Some("op-no".to_string()),
                decided_at: Utc::now(),
            };
            rejector_store.decide(&d)
        });

        let _ = approver.await.expect("approver panicked");
        let _ = rejector.await.expect("rejector panicked");

        let final_workflow = store_arc.get_workflow(request_id).expect("present");
        // A single rejection is terminal Rejected. A single approval is
        // QuorumProgress (1/3) -> still Pending. We accept exactly these
        // two states; Approved (which would require 3) is impossible.
        assert!(
            matches!(
                final_workflow.state,
                ApprovalWorkflowState::Rejected | ApprovalWorkflowState::Pending
            ),
            "unexpected final state {:?}",
            final_workflow.state
        );
        assert_ne!(final_workflow.state, ApprovalWorkflowState::Approved);
    }
}

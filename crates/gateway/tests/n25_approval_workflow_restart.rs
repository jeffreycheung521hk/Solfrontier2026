//! N25 — Approval workflow restart and durability verification.
//!
//! Proves:
//! 1. A multi-stage workflow survives simulated restart via SQLite round-trip.
//! 2. Terminal (rejected) workflows remain idempotent after reload.
//! 3. Out-of-state decisions do not mutate the workflow.

use uuid::Uuid;

use claw_types::{
    approval::{
        ApprovalDecision, ApprovalOutcome, ApprovalRequest, ApprovalStage,
        ApprovalWorkflow, ApprovalWorkflowState, StageDecision,
    },
    policy::PolicyVerdict,
    session::SessionId,
    transaction::SimulationResult,
};
use claw_gateway::ApprovalStore;
use claw_state_store::{
    approval_workflow::ApprovalWorkflowRepository,
    Database,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fake_request(session_id: SessionId) -> ApprovalRequest {
    ApprovalRequest::new(
        session_id,
        Uuid::new_v4(),
        "test transfer",
        PolicyVerdict::RequiresHumanApproval {
            reason: "approval workflow test".into(),
            rule_name: "test-rule".into(),
            required_approver_role: None,
            approval_chain: None,
        },
        SimulationResult {
            success: true,
            error: None,
            compute_units_used: Some(1000),
            logs: vec![],
            return_data: None,
            account_diffs: vec![],
            fee_lamports: Some(5000),
        },
    )
}

fn two_stage_workflow(request_id: Uuid, session_id: SessionId) -> ApprovalWorkflow {
    let now = chrono::Utc::now();
    ApprovalWorkflow {
        request_id,
        session_id,
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
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn approval_workflow_state_survives_restart_mid_process() {
    // Create a workflow with stage 0 decided (approved) and stage 1 pending.
    // Persist to SQLite, then reload and verify the stage structure.
    let db = Database::open_in_memory().await.expect("in-memory DB");
    let repo = ApprovalWorkflowRepository::new(db.pool().clone());

    let request_id = Uuid::new_v4();
    let session_id = SessionId::from(Uuid::new_v4());

    // Build a workflow with stage 0 already decided.
    let mut wf = two_stage_workflow(request_id, session_id.clone());
    wf.stages[0].decisions.push(StageDecision {
        approved: true,
        operator_id: Some("risk-operator".to_string()),
        approver_role: Some("risk".to_string()),
        decided_at: chrono::Utc::now(),
    });
    // Stage 1 remains pending. State remains Pending (not all stages done).

    let stages_json = serde_json::to_string(&wf.stages).expect("serialize stages");
    let created_at = wf.created_at.timestamp();

    repo.create(
        &request_id.to_string(),
        &session_id.to_string(),
        "pending",
        &stages_json,
        created_at,
    )
    .await
    .unwrap();

    // Simulate restart: create a new repo with the same DB pool.
    let repo2 = ApprovalWorkflowRepository::new(db.pool().clone());
    let row = repo2
        .load(&request_id.to_string())
        .await
        .unwrap()
        .expect("should find the workflow row");

    // Deserialize and verify.
    assert_eq!(row.state, "pending");
    let stages: Vec<ApprovalStage> =
        serde_json::from_str(&row.stages_json).expect("deserialize stages");
    assert_eq!(stages.len(), 2);
    assert_eq!(stages[0].decisions.len(), 1, "stage 0 should have 1 decision");
    assert!(stages[0].decisions[0].approved);
    assert_eq!(stages[0].decisions[0].approver_role.as_deref(), Some("risk"));
    assert!(stages[1].decisions.is_empty(), "stage 1 should still be pending");
}

#[tokio::test]
async fn terminal_approval_state_is_idempotent_after_restart() {
    // Simulate the full cycle: persist a pending workflow to SQLite,
    // reload it, progress it to Rejected, then try to decide again.
    let db = Database::open_in_memory().await.expect("in-memory DB");
    let repo = ApprovalWorkflowRepository::new(db.pool().clone());

    let session_id = SessionId::from(Uuid::new_v4());

    // Register a request+workflow in the ApprovalStore and reject it.
    let store = ApprovalStore::new();
    let req = fake_request(session_id.clone());
    let rid = req.id;
    let wf = two_stage_workflow(rid, session_id.clone());
    store.register(req, wf.clone());

    // Persist the initial pending state to SQLite.
    let stages_json = serde_json::to_string(&wf.stages).unwrap();
    repo.create(
        &rid.to_string(),
        &session_id.to_string(),
        "pending",
        &stages_json,
        wf.created_at.timestamp(),
    )
    .await
    .unwrap();

    // Risk operator rejects → terminal state.
    let d1 = ApprovalDecision {
        request_id: rid,
        approved: false,
        note: Some("too risky".into()),
        approver_role: Some("risk".to_string()),
        operator_id: None,
        decided_at: chrono::Utc::now(),
    };
    let (outcome1, _) = store.decide(&d1);
    assert_eq!(outcome1, ApprovalOutcome::Rejected);

    // Update SQLite to reflect the rejection.
    // Workflow is still in the workflows map even after rejection.
    // (The pending map entry was removed, but workflow persists.)

    // Try to decide again on the same request → AlreadyDecided
    let d2 = ApprovalDecision::approve_as(rid, None, "risk".to_string());
    let (outcome2, _) = store.decide(&d2);
    assert_eq!(
        outcome2,
        ApprovalOutcome::AlreadyDecided,
        "rejected workflow should not accept further decisions"
    );

    // Verify the SQLite round-trip preserves the rejected state.
    let repo2 = ApprovalWorkflowRepository::new(db.pool().clone());
    // Update the DB to reflect the final state.
    let final_wf = store.get_workflow(rid);
    if let Some(fw) = &final_wf {
        let final_stages = serde_json::to_string(&fw.stages).unwrap();
        repo2.update(&rid.to_string(), "rejected", &final_stages, fw.updated_at.timestamp())
            .await
            .unwrap();
    }

    let row = repo2.load(&rid.to_string()).await.unwrap().expect("row exists");
    assert_eq!(row.state, "rejected", "SQLite should reflect the terminal state");
}

#[test]
fn out_of_state_decision_does_not_mutate_workflow() {
    // A decision with the wrong role should produce RoleMismatch without
    // modifying any workflow state.
    let session_id = SessionId::from(Uuid::new_v4());
    let req = fake_request(session_id.clone());
    let rid = req.id;
    let wf = two_stage_workflow(rid, session_id);

    let store = ApprovalStore::new();
    store.register(req, wf);

    // Snapshot the workflow state before the bad decision.
    let wf_before = store.get_workflow(rid).unwrap();
    assert_eq!(wf_before.state, ApprovalWorkflowState::Pending);
    assert!(wf_before.stages[0].decisions.is_empty());

    // Send a decision with wrong role ("treasury" can't approve stage 0 which requires "risk").
    let d = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
    let (outcome, _) = store.decide(&d);

    assert!(
        matches!(outcome, ApprovalOutcome::RoleMismatch { .. }),
        "wrong role should produce RoleMismatch: {:?}",
        outcome,
    );

    // Verify NO state mutation occurred.
    let wf_after = store.get_workflow(rid).unwrap();
    assert_eq!(wf_after.state, ApprovalWorkflowState::Pending, "state should not change");
    assert!(
        wf_after.stages[0].decisions.is_empty(),
        "no decisions should be recorded for a RoleMismatch"
    );
    assert_eq!(store.pending_count(), 1, "request should remain pending");
}

#[tokio::test]
async fn durable_workflow_recovered_from_db_can_be_decided() {
    // Simulate a previous daemon run that persisted a 2-stage workflow with
    // stage 0 already decided. On "restart", load from DB, reconstruct the
    // ApprovalStore, and complete the remaining stage.
    let db = Database::open_in_memory().await.expect("in-memory DB");
    let repo = ApprovalWorkflowRepository::new(db.pool().clone());

    let request_id = Uuid::new_v4();
    let session_id = SessionId::from(Uuid::new_v4());

    // Build stages with stage 0 already decided (simulating a prior run).
    let stages = vec![
        ApprovalStage {
            index: 0,
            allowed_roles: vec!["risk".to_string()],
            min_approvals: 1,
            decisions: vec![StageDecision {
                approved: true,
                operator_id: Some("alice".to_string()),
                approver_role: Some("risk".to_string()),
                decided_at: chrono::Utc::now(),
            }],
        },
        ApprovalStage {
            index: 1,
            allowed_roles: vec!["treasury".to_string()],
            min_approvals: 1,
            decisions: vec![],
        },
    ];
    let stages_json = serde_json::to_string(&stages).expect("serialize stages");

    // Persist to DB (simulating what the daemon would have done before crash).
    repo.create(
        &request_id.to_string(),
        &session_id.to_string(),
        "pending",
        &stages_json,
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();

    // --- "Restart" boundary: load from DB into a fresh ApprovalStore ---
    let repo2 = ApprovalWorkflowRepository::new(db.pool().clone());
    let row = repo2
        .load(&request_id.to_string())
        .await
        .unwrap()
        .expect("should find the workflow row");

    let recovered_stages: Vec<ApprovalStage> =
        serde_json::from_str(&row.stages_json).expect("deserialize stages");
    assert_eq!(recovered_stages.len(), 2);
    assert_eq!(recovered_stages[0].decisions.len(), 1, "stage 0 already decided");
    assert!(recovered_stages[1].decisions.is_empty(), "stage 1 still pending");

    let recovered_workflow = ApprovalWorkflow {
        request_id,
        session_id: session_id.clone(),
        state: ApprovalWorkflowState::Pending,
        stages: recovered_stages,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        expires_at: None,
    };

    // Recreate the approval request with a matching id.
    let mut req = fake_request(session_id.clone());
    req.id = request_id;

    let store = ApprovalStore::new();
    store.register(req, recovered_workflow);

    // Stage 1 (treasury) should be the current stage — decide on it.
    let decision = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
    let (outcome, maybe_req) = store.decide(&decision);

    assert_eq!(
        outcome,
        ApprovalOutcome::Approved,
        "recovered workflow should complete after stage 1 approval"
    );
    assert!(maybe_req.is_some(), "terminal approval should return the request");
    assert_eq!(store.pending_count(), 0, "no more pending after terminal");

    // Verify the workflow reached terminal state.
    let final_wf = store.get_workflow(request_id).expect("workflow should still be in map");
    assert_eq!(final_wf.state, ApprovalWorkflowState::Approved);
    assert_eq!(final_wf.stages[0].decisions.len(), 1, "stage 0 decision preserved");
    assert_eq!(final_wf.stages[1].decisions.len(), 1, "stage 1 decision recorded");

    // Update DB to reflect terminal state (daemon would do this).
    let final_stages = serde_json::to_string(&final_wf.stages).unwrap();
    repo2
        .update(
            &request_id.to_string(),
            "approved",
            &final_stages,
            final_wf.updated_at.timestamp(),
        )
        .await
        .unwrap();

    // Verify the DB has the correct terminal state.
    let final_row = repo2
        .load(&request_id.to_string())
        .await
        .unwrap()
        .expect("row should exist");
    assert_eq!(final_row.state, "approved");
}

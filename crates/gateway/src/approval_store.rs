//! In-memory approval request store with workflow state machine.
//!
//! Holds pending `ApprovalRequest`s and their associated `ApprovalWorkflow`s.
//!
//! # Single-stage vs multi-stage
//!
//! - **Single-stage:** One decision → Approved/Rejected (backward compatible).
//! - **Multi-stage:** Each `POST /approve` advances one stage. The workflow
//!   transitions to Approved only when ALL stages are approved. Any rejection
//!   is terminal. The signing resume task is only signaled on terminal state.
//!
//! # Thread safety
//!
//! `ApprovalStore` is `Clone` (cheap Arc clone). The inner `DashMap` provides
//! concurrent access without a global mutex.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use claw_types::{
    approval::{
        ApprovalDecision, ApprovalOutcome, ApprovalRequest,
        ApprovalWorkflow, ApprovalWorkflowState, StageDecision,
    },
    session::SessionId,
};

/// In-memory store for pending approval requests and their workflows.
#[derive(Clone, Debug)]
pub struct ApprovalStore {
    pending:   Arc<DashMap<Uuid, ApprovalRequest>>,
    workflows: Arc<DashMap<Uuid, ApprovalWorkflow>>,
}

impl ApprovalStore {
    /// Creates an empty approval store.
    pub fn new() -> Self {
        Self {
            pending:   Arc::new(DashMap::new()),
            workflows: Arc::new(DashMap::new()),
        }
    }

    /// Registers a new approval request with its associated workflow.
    pub fn register(&self, request: ApprovalRequest, workflow: ApprovalWorkflow) -> Uuid {
        let id = request.id;
        self.pending.insert(id, request);
        self.workflows.insert(id, workflow);
        id
    }

    /// Returns a clone of the pending request for display/polling, if it exists.
    pub fn get(&self, request_id: Uuid) -> Option<ApprovalRequest> {
        self.pending.get(&request_id).map(|r| r.clone())
    }

    /// Returns a clone of the workflow for a request, if it exists.
    pub fn get_workflow(&self, request_id: Uuid) -> Option<ApprovalWorkflow> {
        self.workflows.get(&request_id).map(|w| w.clone())
    }

    /// Returns all pending (undecided) requests for a session, sorted by `requested_at`.
    pub fn pending_for_session(&self, session_id: &SessionId) -> Vec<ApprovalRequest> {
        let mut results: Vec<ApprovalRequest> = self
            .pending
            .iter()
            .filter(|entry| &entry.session_id == session_id && !entry.decided)
            .map(|entry| entry.clone())
            .collect();
        results.sort_by_key(|r| r.requested_at);
        results
    }

    /// Applies an operator decision to the approval workflow.
    ///
    /// # Outcomes
    ///
    /// - `Approved` — all stages approved, workflow terminal, request removed.
    /// - `Rejected` — stage rejected, workflow terminal, request removed.
    /// - `StageAdvanced` — stage approved but more stages remain. Request stays pending.
    /// - `RoleMismatch` — approver role doesn't match current stage. No state change.
    /// - `AlreadyDecided` — workflow already terminal.
    /// - `NotFound` — request_id unknown.
    pub fn decide(
        &self,
        decision: &ApprovalDecision,
    ) -> (ApprovalOutcome, Option<ApprovalRequest>) {
        // ── Lookup ───────────────────────────────────────────────────────────
        let Some(mut workflow_entry) = self.workflows.get_mut(&decision.request_id) else {
            return (ApprovalOutcome::NotFound, None);
        };
        let workflow = workflow_entry.value_mut();

        if workflow.state.is_terminal() {
            return (ApprovalOutcome::AlreadyDecided, None);
        }

        // ── Check lease expiry ───────────────────────────────────────────────
        if workflow.is_expired() {
            workflow.expire();
            drop(workflow_entry);

            if let Some(mut req) = self.pending.get_mut(&decision.request_id) {
                req.decided = true;
            }
            let request = self.pending.remove(&decision.request_id).map(|(_, r)| r);
            return (ApprovalOutcome::NotFound, request); // Expired → treat as gone
        }

        // ── Find the current pending stage ───────────────────────────────────
        let current_stage = match workflow.current_stage() {
            Some(s) => s,
            None => return (ApprovalOutcome::AlreadyDecided, None),
        };
        let stage_index = current_stage.index;

        // ── Role check against allowed_roles ─────────────────────────────────
        if !current_stage.allowed_roles.is_empty() {
            match &decision.approver_role {
                Some(provided) if current_stage.allowed_roles.iter().any(|r| r == provided) => {
                    // Role is in the allowed set — proceed.
                }
                other => {
                    return (
                        ApprovalOutcome::RoleMismatch {
                            required: current_stage.allowed_roles.join(", "),
                            provided: other.clone(),
                        },
                        None,
                    );
                }
            }
        }

        // ── Dedup: prevent same operator from voting twice on the same stage ──
        if let Some(ref op_id) = decision.operator_id {
            if current_stage.has_operator_voted(op_id) {
                return (
                    ApprovalOutcome::DuplicateOperator {
                        operator_id: op_id.clone(),
                        stage: stage_index,
                    },
                    None,
                );
            }
        }

        // ── Apply decision ───────────────────────────────────────────────────
        let stage_decision = StageDecision {
            approved: decision.approved,
            operator_id: decision.operator_id.clone(),
            approver_role: decision.approver_role.clone(),
            decided_at: decision.decided_at,
        };

        workflow.apply_decision(stage_decision);

        // ── Determine outcome ────────────────────────────────────────────────
        if workflow.state.is_terminal() {
            let outcome = if workflow.state == ApprovalWorkflowState::Approved {
                ApprovalOutcome::Approved
            } else {
                ApprovalOutcome::Rejected
            };

            drop(workflow_entry);

            if let Some(mut req) = self.pending.get_mut(&decision.request_id) {
                req.decided = true;
            }
            let request = self.pending.remove(&decision.request_id).map(|(_, r)| r);

            (outcome, request)
        } else {
            // Still pending — check if same stage needs more approvals (quorum)
            // or if we advanced to the next stage.
            let next_pending = workflow.current_stage();
            let next_index = next_pending.map(|s| s.index).unwrap_or(0);

            if next_index == stage_index {
                // Same stage — quorum not yet met.
                let stage = &workflow.stages[stage_index];
                let outcome = ApprovalOutcome::QuorumProgress {
                    stage: stage_index,
                    approvals_so_far: stage.approval_count(),
                    approvals_required: stage.min_approvals,
                };
                drop(workflow_entry);
                (outcome, None)
            } else {
                // Advanced to next stage.
                let next_role = next_pending
                    .and_then(|s| s.allowed_roles.first().cloned());
                let outcome = ApprovalOutcome::StageAdvanced {
                    completed_stage: stage_index,
                    next_stage: next_index,
                    next_required_role: next_role,
                };
                drop(workflow_entry);
                (outcome, None)
            }
        }
    }

    /// Peek at which session owns a pending request (P0-3: session-request binding).
    pub fn session_for_request(&self, request_id: Uuid) -> Option<SessionId> {
        self.pending
            .get(&request_id)
            .filter(|entry| !entry.decided)
            .map(|entry| entry.session_id.clone())
    }

    /// Returns the number of currently pending (undecided) requests.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Default for ApprovalStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_types::{
        approval::{ApprovalChainStage, ApprovalRequest, ApprovalWorkflow},
        policy::PolicyVerdict,
        session::SessionId,
        transaction::SimulationResult,
    };
    use uuid::Uuid;

    fn fake_request(session_id: SessionId) -> ApprovalRequest {
        ApprovalRequest::new(
            session_id,
            Uuid::new_v4(),
            "test transfer",
            PolicyVerdict::RequiresHumanApproval {
                reason: "mainnet policy".into(),
                rule_name: "test-rule".into(),
                required_approver_role: None,
                approval_chain: None,
            },
            SimulationResult {
                success: true, error: None, compute_units_used: Some(1000),
                logs: vec![], return_data: None, account_diffs: vec![], fee_lamports: Some(5000),
            },
        )
    }

    fn single_stage_workflow(request_id: Uuid, session_id: SessionId, role: Option<&str>) -> ApprovalWorkflow {
        ApprovalWorkflow::single_stage(request_id, session_id, role.map(|r| r.to_string()))
    }

    fn multi_stage_workflow(request_id: Uuid, session_id: SessionId) -> ApprovalWorkflow {
        use claw_types::approval::ApprovalStage;
        let now = chrono::Utc::now();
        ApprovalWorkflow {
            request_id,
            session_id,
            state: ApprovalWorkflowState::Pending,
            stages: vec![
                ApprovalStage { index: 0, allowed_roles: vec!["risk".to_string()], min_approvals: 1, decisions: vec![] },
                ApprovalStage { index: 1, allowed_roles: vec!["treasury".to_string()], min_approvals: 1, decisions: vec![] },
            ],
            created_at: now,
            updated_at: now,
            expires_at: None,
        }
    }

    // ── Single-stage tests (backward compat) ────────────────────────────────

    #[test]
    fn approve_pending_request() {
        let store   = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req     = fake_request(session.clone());
        let rid     = req.id;
        let wf      = single_stage_workflow(rid, session, None);
        store.register(req, wf);

        let decision = ApprovalDecision::approve(rid, Some("looks good".into()));
        let (outcome, maybe_req) = store.decide(&decision);

        assert_eq!(outcome, ApprovalOutcome::Approved);
        assert!(maybe_req.is_some());
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn reject_pending_request() {
        let store   = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req     = fake_request(session.clone());
        let rid     = req.id;
        let wf      = single_stage_workflow(rid, session, None);
        store.register(req, wf);

        let decision = ApprovalDecision::reject(rid, Some("too risky".into()));
        let (outcome, _) = store.decide(&decision);

        assert_eq!(outcome, ApprovalOutcome::Rejected);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn duplicate_decision_is_rejected() {
        let store   = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req     = fake_request(session.clone());
        let rid     = req.id;
        let wf      = single_stage_workflow(rid, session, None);
        store.register(req, wf);

        let d1 = ApprovalDecision::approve(rid, None);
        let d2 = ApprovalDecision::approve(rid, None);

        let (outcome1, _) = store.decide(&d1);
        let (outcome2, _) = store.decide(&d2);

        assert_eq!(outcome1, ApprovalOutcome::Approved);
        assert_eq!(outcome2, ApprovalOutcome::AlreadyDecided);
    }

    #[test]
    fn unknown_request_id_returns_not_found() {
        let store    = ApprovalStore::new();
        let decision = ApprovalDecision::approve(Uuid::new_v4(), None);
        let (outcome, req) = store.decide(&decision);
        assert_eq!(outcome, ApprovalOutcome::NotFound);
        assert!(req.is_none());
    }

    #[test]
    fn pending_for_session_filters_correctly() {
        let store = ApprovalStore::new();
        let s1    = SessionId::from(Uuid::new_v4());
        let s2    = SessionId::from(Uuid::new_v4());

        let r1 = fake_request(s1.clone());
        let r2 = fake_request(s1.clone());
        let r3 = fake_request(s2.clone());
        let (id1, id2, id3) = (r1.id, r2.id, r3.id);
        store.register(r1, single_stage_workflow(id1, s1.clone(), None));
        store.register(r2, single_stage_workflow(id2, s1.clone(), None));
        store.register(r3, single_stage_workflow(id3, s2.clone(), None));

        assert_eq!(store.pending_for_session(&s1).len(), 2);
        assert_eq!(store.pending_for_session(&s2).len(), 1);
    }

    // ── Approval matrix role tests ──────────────────────────────────────────

    #[test]
    fn role_match_allows_approval() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = single_stage_workflow(rid, session, Some("treasury"));
        store.register(req, wf);

        let decision = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        let (outcome, _) = store.decide(&decision);
        assert_eq!(outcome, ApprovalOutcome::Approved);
    }

    #[test]
    fn role_mismatch_rejects_decision() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = single_stage_workflow(rid, session, Some("treasury"));
        store.register(req, wf);

        let decision = ApprovalDecision::approve_as(rid, None, "risk".to_string());
        let (outcome, _) = store.decide(&decision);
        assert!(matches!(outcome, ApprovalOutcome::RoleMismatch { .. }));
        assert_eq!(store.pending_count(), 1, "request stays pending");
    }

    #[test]
    fn missing_role_on_required_request_is_rejected() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = single_stage_workflow(rid, session, Some("treasury"));
        store.register(req, wf);

        let decision = ApprovalDecision::approve(rid, None);
        let (outcome, _) = store.decide(&decision);
        assert!(matches!(outcome, ApprovalOutcome::RoleMismatch { .. }));
    }

    #[test]
    fn no_required_role_accepts_any_approver() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = single_stage_workflow(rid, session, None);
        store.register(req, wf);

        let decision = ApprovalDecision::approve(rid, None);
        let (outcome, _) = store.decide(&decision);
        assert_eq!(outcome, ApprovalOutcome::Approved);
    }

    #[test]
    fn correct_role_then_approve_succeeds() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = single_stage_workflow(rid, session, Some("risk"));
        store.register(req, wf);

        let bad = ApprovalDecision::approve_as(rid, None, "operator".to_string());
        let (outcome1, _) = store.decide(&bad);
        assert!(matches!(outcome1, ApprovalOutcome::RoleMismatch { .. }));

        let good = ApprovalDecision::approve_as(rid, None, "risk".to_string());
        let (outcome2, _) = store.decide(&good);
        assert_eq!(outcome2, ApprovalOutcome::Approved);
    }

    // ── Multi-stage approval chain tests ────────────────────────────────────

    #[test]
    fn multi_stage_first_approval_advances_stage() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = multi_stage_workflow(rid, session);
        store.register(req, wf);

        // Stage 0: risk approves
        let d1 = ApprovalDecision::approve_as(rid, None, "risk".to_string());
        let (outcome1, _) = store.decide(&d1);

        assert_eq!(
            outcome1,
            ApprovalOutcome::StageAdvanced {
                completed_stage: 0,
                next_stage: 1,
                next_required_role: Some("treasury".to_string()),
            }
        );
        assert_eq!(store.pending_count(), 1, "request still pending");
    }

    #[test]
    fn multi_stage_all_approvals_completes_workflow() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = multi_stage_workflow(rid, session);
        store.register(req, wf);

        // Stage 0: risk
        let d1 = ApprovalDecision::approve_as(rid, None, "risk".to_string());
        let (o1, _) = store.decide(&d1);
        assert!(matches!(o1, ApprovalOutcome::StageAdvanced { .. }));

        // Stage 1: treasury
        let d2 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        let (o2, req) = store.decide(&d2);
        assert_eq!(o2, ApprovalOutcome::Approved);
        assert!(req.is_some(), "request returned on terminal");
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn multi_stage_rejection_at_any_stage_is_terminal() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = multi_stage_workflow(rid, session);
        store.register(req, wf);

        // Stage 0: risk rejects
        let d1 = ApprovalDecision {
            request_id: rid,
            approved: false,
            note: Some("too risky".into()),
            approver_role: Some("risk".to_string()),
            operator_id: None,
            decided_at: chrono::Utc::now(),
        };
        let (o1, req) = store.decide(&d1);
        assert_eq!(o1, ApprovalOutcome::Rejected);
        assert!(req.is_some());
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn multi_stage_wrong_role_at_stage_is_rejected() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = multi_stage_workflow(rid, session);
        store.register(req, wf);

        // Try treasury at stage 0 (needs risk)
        let d1 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        let (o1, _) = store.decide(&d1);
        assert!(matches!(o1, ApprovalOutcome::RoleMismatch { .. }));
        assert_eq!(store.pending_count(), 1, "request stays pending");
    }

    #[test]
    fn multi_stage_workflow_state_visible_via_get_workflow() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = multi_stage_workflow(rid, session);
        store.register(req, wf);

        // Before any decision
        let wf = store.get_workflow(rid).unwrap();
        assert_eq!(wf.state, ApprovalWorkflowState::Pending);
        assert_eq!(wf.stages.len(), 2);
        assert!(wf.stages[0].decisions.is_empty());

        // After stage 0 approved
        let d1 = ApprovalDecision::approve_as(rid, None, "risk".to_string());
        store.decide(&d1);

        let wf = store.get_workflow(rid).unwrap();
        assert_eq!(wf.state, ApprovalWorkflowState::Pending);
        assert_eq!(wf.stages[0].decisions.len(), 1);
        assert!(wf.stages[1].decisions.is_empty());
    }

    // ── Quorum tests ────────────────────────────────────────────────────────

    fn quorum_workflow(request_id: Uuid, session_id: SessionId) -> ApprovalWorkflow {
        use claw_types::approval::ApprovalStage;
        let now = chrono::Utc::now();
        ApprovalWorkflow {
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
        }
    }

    #[test]
    fn quorum_first_approval_returns_progress() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = quorum_workflow(rid, session);
        store.register(req, wf);

        let mut d1 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        d1.operator_id = Some("alice".to_string());
        let (o1, _) = store.decide(&d1);

        assert_eq!(
            o1,
            ApprovalOutcome::QuorumProgress {
                stage: 0,
                approvals_so_far: 1,
                approvals_required: 2,
            }
        );
        assert_eq!(store.pending_count(), 1, "request still pending");
    }

    #[test]
    fn quorum_met_after_second_approval() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = quorum_workflow(rid, session);
        store.register(req, wf);

        let mut d1 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        d1.operator_id = Some("alice".to_string());
        store.decide(&d1);

        let mut d2 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        d2.operator_id = Some("bob".to_string());
        let (o2, req) = store.decide(&d2);

        assert_eq!(o2, ApprovalOutcome::Approved);
        assert!(req.is_some());
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn quorum_duplicate_operator_rejected() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = quorum_workflow(rid, session);
        store.register(req, wf);

        let mut d1 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        d1.operator_id = Some("alice".to_string());
        store.decide(&d1);

        // Same operator tries again
        let mut d2 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        d2.operator_id = Some("alice".to_string());
        let (o2, _) = store.decide(&d2);

        assert!(
            matches!(o2, ApprovalOutcome::DuplicateOperator { .. }),
            "duplicate operator should be rejected: {:?}",
            o2,
        );
        assert_eq!(store.pending_count(), 1, "still pending");
    }

    #[test]
    fn quorum_rejection_is_terminal_even_mid_quorum() {
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = quorum_workflow(rid, session);
        store.register(req, wf);

        // First approves
        let mut d1 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        d1.operator_id = Some("alice".to_string());
        store.decide(&d1);

        // Second rejects — terminal
        let mut d2 = ApprovalDecision {
            request_id: rid,
            approved: false,
            note: Some("veto".into()),
            approver_role: Some("treasury".to_string()),
            operator_id: Some("bob".to_string()),
            decided_at: chrono::Utc::now(),
        };
        let (o2, req) = store.decide(&d2);
        assert_eq!(o2, ApprovalOutcome::Rejected);
        assert!(req.is_some());
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn quorum_anonymous_operator_not_deduped() {
        // When operator_id is None, dedup cannot fire — each vote is anonymous.
        let store = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req = fake_request(session.clone());
        let rid = req.id;
        let wf = quorum_workflow(rid, session);
        store.register(req, wf);

        let d1 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        let (o1, _) = store.decide(&d1);
        assert!(matches!(o1, ApprovalOutcome::QuorumProgress { .. }));

        let d2 = ApprovalDecision::approve_as(rid, None, "treasury".to_string());
        let (o2, _) = store.decide(&d2);
        assert_eq!(o2, ApprovalOutcome::Approved, "anonymous votes are not deduped");
    }
}

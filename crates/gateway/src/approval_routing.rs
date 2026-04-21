//! Approval decision routing logic.
//!
//! Extracted from `GatewayApprovalHandler::decide_inner()` so it can be
//! tested directly. The daemon calls `route_approval_outcome()` after
//! `ApprovalStore::decide()` and audit logging.
//!
//! This module owns the signal→PendingSigningStore decision and alert emission.
//! The daemon owns audit logging and event publishing.

use claw_types::approval::{ApprovalOutcome, ApprovalRequest, ApprovalWorkflowState};

use crate::integrations::jupiter_park::PendingJupiterParkStore;
use crate::pending_signing::PendingSigningStore;
use crate::policy_alerting::AlertDispatcher;

/// Result of routing an approval outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingAction {
    /// Terminal state — PendingSigningStore was signaled.
    Signaled(ApprovalWorkflowState),
    /// Non-terminal — no signal sent (more stages or quorum needed).
    NoSignal,
    /// Error condition (role mismatch, duplicate, etc.) — no signal.
    Rejected,
}

/// Routes an approval outcome to PendingSigningStore and emits alerts.
///
/// Called by `GatewayApprovalHandler::decide_inner()` after audit logging.
/// This is the single source of truth for:
/// - Whether to signal PendingSigningStore (only on terminal outcomes)
/// - What alerts to emit for each outcome type
/// - What workflow state to send through the oneshot channel
pub fn route_approval_outcome(
    outcome: &ApprovalOutcome,
    request_id: uuid::Uuid,
    pending_signing: &PendingSigningStore,
    pending_jupiter_park: &PendingJupiterParkStore,
    alerts: &AlertDispatcher,
    request: Option<&ApprovalRequest>,
) -> RoutingAction {
    match outcome {
        ApprovalOutcome::StageAdvanced { .. } => RoutingAction::NoSignal,

        ApprovalOutcome::QuorumProgress { .. } => RoutingAction::NoSignal,

        ApprovalOutcome::DuplicateOperator { ref operator_id, stage } => {
            alerts.send(
                "approval_duplicate_operator",
                claw_types::alert::AlertSeverity::Warning,
                &format!("Operator '{}' duplicate vote on {} stage {}", operator_id, request_id, stage),
                serde_json::json!({ "request_id": request_id, "operator_id": operator_id, "stage": stage }),
            );
            RoutingAction::Rejected
        }

        ApprovalOutcome::RoleMismatch { ref required, ref provided } => {
            alerts.send(
                "approval_role_mismatch",
                claw_types::alert::AlertSeverity::Warning,
                &format!("Role mismatch on {}: required '{}', got '{}'",
                    request_id, required, provided.as_deref().unwrap_or("none")),
                serde_json::json!({ "request_id": request_id, "required_role": required, "provided_role": provided }),
            );
            RoutingAction::Rejected
        }

        ApprovalOutcome::AlreadyDecided | ApprovalOutcome::NotFound => RoutingAction::Rejected,

        ApprovalOutcome::Expired => {
            // Lease expired — signal both stores with Expired so their
            // respective resume tasks wake up and clean up. Each store's
            // signal() is a graceful no-op if the request_id isn't parked there.
            pending_signing.signal(request_id, ApprovalWorkflowState::Expired);
            pending_jupiter_park.signal(request_id, ApprovalWorkflowState::Expired);
            RoutingAction::Signaled(ApprovalWorkflowState::Expired)
        }

        ApprovalOutcome::Approved => {
            pending_signing.signal(request_id, ApprovalWorkflowState::Approved);
            pending_jupiter_park.signal(request_id, ApprovalWorkflowState::Approved);
            RoutingAction::Signaled(ApprovalWorkflowState::Approved)
        }

        ApprovalOutcome::Rejected => {
            // Build full-context rejection alert from the request.
            if let Some(req) = request {
                alerts.send(
                    "approval_rejected",
                    claw_types::alert::AlertSeverity::Warning,
                    &format!("Transaction {} rejected by operator", req.transaction_id),
                    serde_json::json!({
                        "request_id":      request_id,
                        "session_id":      req.session_id,
                        "transaction_id":  req.transaction_id,
                        "description":     req.description,
                        "policy_verdict":  req.policy_verdict.label(),
                        "rule_name":       req.policy_verdict.rule_name(),
                    }),
                );
            }
            pending_signing.signal(request_id, ApprovalWorkflowState::Rejected);
            pending_jupiter_park.signal(request_id, ApprovalWorkflowState::Rejected);
            RoutingAction::Signaled(ApprovalWorkflowState::Rejected)
        }
    }
}

/// Builds the rejection alert payload from a request.
/// Exposed for testing: tests can verify the exact shape matches production.
pub fn build_rejection_alert_payload(
    request_id: uuid::Uuid,
    request: &ApprovalRequest,
) -> serde_json::Value {
    serde_json::json!({
        "request_id":      request_id,
        "session_id":      request.session_id,
        "transaction_id":  request.transaction_id,
        "description":     request.description,
        "policy_verdict":  request.policy_verdict.label(),
        "rule_name":       request.policy_verdict.rule_name(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn fake_simulation() -> claw_types::transaction::SimulationResult {
        claw_types::transaction::SimulationResult {
            success: true, error: None, compute_units_used: Some(1000),
            logs: vec![], return_data: None, account_diffs: vec![], fee_lamports: Some(5000),
        }
    }

    fn fake_request(rid: Uuid, sid: claw_types::session::SessionId) -> ApprovalRequest {
        let mut req = ApprovalRequest::new(
            sid, Uuid::new_v4(), "test transfer",
            claw_types::policy::PolicyVerdict::RequiresHumanApproval {
                reason: "policy".into(), rule_name: "test-rule".into(),
                required_approver_role: None, approval_chain: None,
            },
            fake_simulation(),
        );
        req.id = rid;
        req
    }

    fn park_dummy(pending: &PendingSigningStore, rid: Uuid) -> tokio::sync::oneshot::Receiver<ApprovalWorkflowState> {
        let from = solana_sdk::pubkey::Pubkey::new_unique();
        let to = solana_sdk::pubkey::Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&from, &to, 1000);
        let msg = solana_sdk::message::Message::new(&[ix], Some(&from));
        let tx = solana_sdk::transaction::Transaction::new_unsigned(msg);
        let proposal = claw_types::transaction::TransactionProposal {
            id: Uuid::new_v4(), session_id: claw_types::session::SessionId::new(),
            wallet_pubkey: from.to_string(), network: claw_types::solana::SolanaNetwork::Devnet,
            description: "test".into(), transaction_b64: String::new(),
            instructions_summary: vec![], created_at: chrono::Utc::now(),
        };
        let sim = fake_simulation();
        let verdict = claw_types::policy::PolicyVerdict::RequiresHumanApproval {
            reason: "t".into(), rule_name: "r".into(),
            required_approver_role: None, approval_chain: None,
        };
        pending.park(rid, proposal, &tx, sim, verdict, 100).unwrap()
    }

    #[test]
    fn stage_advanced_does_not_signal() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let outcome = ApprovalOutcome::StageAdvanced {
            completed_stage: 0, next_stage: 1, next_required_role: Some("treasury".into()),
        };
        let action = route_approval_outcome(&outcome, Uuid::new_v4(), &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::NoSignal);
    }

    #[test]
    fn quorum_progress_does_not_signal() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let outcome = ApprovalOutcome::QuorumProgress {
            stage: 0, approvals_so_far: 1, approvals_required: 2,
        };
        let action = route_approval_outcome(&outcome, Uuid::new_v4(), &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::NoSignal);
    }

    #[tokio::test]
    async fn approved_signals_pending_store() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let rid = Uuid::new_v4();

        let decision_rx = park_dummy(&pending, rid);

        let action = route_approval_outcome(&ApprovalOutcome::Approved, rid, &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::Signaled(ApprovalWorkflowState::Approved));

        let state = decision_rx.await.expect("should have received signal");
        assert!(state.is_approved());
    }

    #[test]
    fn rejected_signals_pending_store_with_alert() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default(); // Disabled, but routing still fires
        let rid = Uuid::new_v4();
        let sid = claw_types::session::SessionId::new();
        let req = fake_request(rid, sid);

        let action = route_approval_outcome(&ApprovalOutcome::Rejected, rid, &pending, &PendingJupiterParkStore::new(), &alerts, Some(&req));
        assert_eq!(action, RoutingAction::Signaled(ApprovalWorkflowState::Rejected));
    }

    #[test]
    fn role_mismatch_does_not_signal() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let outcome = ApprovalOutcome::RoleMismatch {
            required: "treasury".into(), provided: Some("risk".into()),
        };
        let action = route_approval_outcome(&outcome, Uuid::new_v4(), &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::Rejected);
    }

    #[test]
    fn duplicate_operator_does_not_signal() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let outcome = ApprovalOutcome::DuplicateOperator {
            operator_id: "alice".into(), stage: 0,
        };
        let action = route_approval_outcome(&outcome, Uuid::new_v4(), &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::Rejected);
    }

    #[test]
    fn already_decided_does_not_signal() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let action = route_approval_outcome(&ApprovalOutcome::AlreadyDecided, Uuid::new_v4(), &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::Rejected);
    }

    #[test]
    fn not_found_does_not_signal() {
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let action = route_approval_outcome(&ApprovalOutcome::NotFound, Uuid::new_v4(), &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::Rejected);
    }

    #[tokio::test]
    async fn expired_signals_pending_store_with_expired_state() {
        // Lease expiry must wake the parked signing task with the Expired
        // state, not Approved/Rejected. This prevents false audit rows and
        // ensures the signing resume task cleans up correctly.
        let pending = PendingSigningStore::new();
        let alerts = AlertDispatcher::default();
        let rid = Uuid::new_v4();
        let decision_rx = park_dummy(&pending, rid);

        let action = route_approval_outcome(&ApprovalOutcome::Expired, rid, &pending, &PendingJupiterParkStore::new(), &alerts, None);
        assert_eq!(action, RoutingAction::Signaled(ApprovalWorkflowState::Expired));

        // Parked signing task receives Expired through the oneshot.
        let state = decision_rx.await.expect("should receive expired signal");
        assert_eq!(state, ApprovalWorkflowState::Expired);
        assert!(!state.is_approved(), "expired is not approved");
    }

    #[test]
    fn rejection_alert_payload_has_full_context() {
        let rid = Uuid::new_v4();
        let sid = claw_types::session::SessionId::new();
        let req = fake_request(rid, sid);

        let payload = build_rejection_alert_payload(rid, &req);

        // Verify all expected fields are present
        assert_eq!(payload["request_id"], serde_json::json!(rid));
        assert!(payload["session_id"].is_string() || payload["session_id"].is_object());
        assert!(payload["transaction_id"].is_string());
        assert_eq!(payload["description"], "test transfer");
        assert_eq!(payload["policy_verdict"], "requires_human_approval");
        assert_eq!(payload["rule_name"], "test-rule");
    }
}

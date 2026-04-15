//! Audit emission for `ApprovalHandler::decide` outcomes.
//!
//! Extracted from `GatewayApprovalHandler::decide_inner()` so the audit side
//! effect can be exercised without constructing the full daemon. The routing
//! logic (pending-signing signaling, alerts) lives in `approval_routing.rs` —
//! audit is separate because it writes to SQLite and is a pure recorder.
//!
//! Invariants enforced here:
//! - `Approved` / `Rejected` write `human_approved` / `human_rejected`
//! - `Expired` writes `approval_lease_expired` (NOT a human decision)
//! - `RoleMismatch`, `StageAdvanced`, `QuorumProgress` each write their own
//!   distinct event type — none of them counts as a human decision
//! - `AlreadyDecided`, `NotFound`, `DuplicateOperator` write nothing (by design)

use claw_state_store::{audit::AuditSeverity, AuditRepository};
use claw_types::approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use tracing::warn;

/// Writes the audit row(s) implied by a decide-outcome pair.
///
/// Must be called after `ApprovalStore::decide()` and before any pending
/// signing signaling so that the audit record is durable regardless of
/// downstream failures.
pub async fn emit_decide_audit(
    audit:         &AuditRepository,
    decision:      &ApprovalDecision,
    outcome:       &ApprovalOutcome,
    maybe_request: Option<&ApprovalRequest>,
) {
    let actor = decision.operator_id.as_deref().unwrap_or("operator");

    match outcome {
        ApprovalOutcome::RoleMismatch { required, provided } => {
            warn!(request_id = %decision.request_id, %required, ?provided, "role mismatch");
            let _ = audit.append(
                None,
                &decision.request_id.to_string(),
                "approval_role_mismatch",
                "operator",
                &serde_json::json!({
                    "request_id":    decision.request_id,
                    "required_role": required,
                    "provided_role": provided,
                }),
                AuditSeverity::Warning,
            ).await;
        }
        ApprovalOutcome::StageAdvanced { completed_stage, next_stage, next_required_role } => {
            let _ = audit.append(
                None,
                &decision.request_id.to_string(),
                "approval_stage_advanced",
                actor,
                &serde_json::json!({
                    "request_id":         decision.request_id,
                    "completed_stage":    completed_stage,
                    "next_stage":         next_stage,
                    "next_required_role": next_required_role,
                    "operator_id":        decision.operator_id,
                    "approver_role":      decision.approver_role,
                }),
                AuditSeverity::Info,
            ).await;
        }
        ApprovalOutcome::QuorumProgress { stage, approvals_so_far, approvals_required } => {
            let _ = audit.append(
                None,
                &decision.request_id.to_string(),
                "approval_quorum_progress",
                actor,
                &serde_json::json!({
                    "request_id":         decision.request_id,
                    "stage":              stage,
                    "approvals_so_far":   approvals_so_far,
                    "approvals_required": approvals_required,
                    "operator_id":        decision.operator_id,
                    "approver_role":      decision.approver_role,
                }),
                AuditSeverity::Info,
            ).await;
        }
        ApprovalOutcome::Expired => {
            warn!(
                request_id = %decision.request_id,
                "late decision rejected: approval workflow lease expired"
            );
            let _ = audit.append(
                None,
                &decision.request_id.to_string(),
                "approval_lease_expired",
                actor,
                &serde_json::json!({
                    "request_id":  decision.request_id,
                    "operator_id": decision.operator_id,
                    "reason":      "late decision arrived after lease expiry",
                }),
                AuditSeverity::Warning,
            ).await;
        }
        _ => {}
    }

    // Terminal human-decision audit — ONLY on real Approved/Rejected with a
    // returned request. Expiry / role_mismatch / duplicate / stage-advanced /
    // quorum-progress must NOT land here.
    let is_real_decision = matches!(
        outcome,
        ApprovalOutcome::Approved | ApprovalOutcome::Rejected,
    );
    if is_real_decision {
        if let Some(request) = maybe_request {
            let (event_type, severity) = if decision.approved {
                ("human_approved",  AuditSeverity::Info)
            } else {
                ("human_rejected", AuditSeverity::Warning)
            };
            let _ = audit.append(
                Some(&request.session_id.to_string()),
                &decision.request_id.to_string(),
                event_type,
                actor,
                &serde_json::json!({
                    "request_id":     decision.request_id,
                    "transaction_id": request.transaction_id,
                    "approved":       decision.approved,
                    "note":           decision.note,
                    "operator_id":    decision.operator_id,
                    "approver_role":  decision.approver_role,
                }),
                severity,
            ).await;
        }
    }
}

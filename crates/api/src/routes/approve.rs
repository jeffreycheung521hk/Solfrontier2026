//! `POST /sessions/:id/approve` — operator approval/rejection endpoint.
//!
//! Receives an `ApproveRequest` from the operator and dispatches it to the
//! `ApprovalHandler` implementation (wired in from the gateway layer).
//!
//! # State machine
//!
//! - `approved: true`  → `ApprovalOutcome::Approved`   → `202 Accepted`
//! - `approved: false` → `ApprovalOutcome::Rejected`   → `200 OK`
//! - Already decided   → `ApprovalOutcome::AlreadyDecided` → `409 Conflict`
//! - Unknown request   → `ApprovalOutcome::NotFound`   → `404 Not Found`
//! - Session not found → `404 Not Found`

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use claw_types::{
    approval::{ApprovalDecision, ApprovalOutcome},
    operator::OperatorIdentity,
    session::SessionId,
};

use crate::server::AppState;

/// Request body for `POST /sessions/:id/approve`.
#[derive(Debug, Deserialize)]
pub struct ApproveRequest {
    /// The approval request ID (from the pending request or from the `awaiting_approval` response).
    pub request_id: Uuid,

    /// `true` = approve, `false` = reject.
    pub approved:   bool,

    /// Optional operator note / justification.
    pub note:       Option<String>,

    /// The role claimed by the approver (e.g., "risk", "treasury").
    /// Required when the policy rule specifies `required_approver_role`.
    pub approver_role: Option<String>,
}

/// Response body for a successfully processed decision.
#[derive(Debug, Serialize)]
pub struct ApproveResponse {
    /// The request that was decided.
    pub request_id:      Uuid,

    /// The resulting outcome.
    pub outcome:         String,

    /// The transaction ID that was approved or rejected.
    pub transaction_id:  Uuid,

    /// Echoes the operator's note back.
    pub note:            Option<String>,
}

/// `POST /sessions/:id/approve`
///
/// Accepts or rejects a pending approval request for a session.
pub async fn submit_approval(
    State(state):    State<AppState>,
    Path(id_str):    Path<String>,
    operator:        Option<axum::Extension<OperatorIdentity>>,
    Json(req):       Json<ApproveRequest>,
) -> Response {
    // ── Parse session ID ──────────────────────────────────────────────────────
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u)  => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    // ── Validate session is active ────────────────────────────────────────────
    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "session not found or not active" }))).into_response();
    }

    // ── P0-3: Verify request belongs to this session ─────────────────────────
    match state.approval.session_for_request(req.request_id) {
        Some(ref owner) if owner != &session_id => {
            return (StatusCode::NOT_FOUND, Json(json!({
                "error": "no pending approval request with this ID for the session"
            }))).into_response();
        }
        None => {
            return (StatusCode::NOT_FOUND, Json(json!({
                "error": "no pending approval request with this ID for the session"
            }))).into_response();
        }
        Some(_) => {} // session matches — proceed
    }

    // ── Build decision and dispatch ───────────────────────────────────────────
    let identity = operator.map(|axum::Extension(id)| id);

    let mut decision = if req.approved {
        ApprovalDecision::approve(req.request_id, req.note.clone())
    } else {
        ApprovalDecision::reject(req.request_id, req.note.clone())
    };
    decision.operator_id = identity.as_ref().map(|id| id.id.to_string());

    // S1e: If operator has authenticated roles, validate the claimed role.
    // A self-declared role that isn't in the operator's registered role set is rejected.
    decision.approver_role = match &identity {
        Some(id) if !id.roles.is_empty() => {
            match &req.approver_role {
                // Body claims a role — verify operator actually holds it.
                Some(claimed) if id.roles.contains(claimed) => Some(claimed.clone()),
                // Body claims a role the operator doesn't hold — reject.
                Some(claimed) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "error":          "role_not_held",
                            "request_id":     req.request_id,
                            "claimed_role":   claimed,
                            "operator_roles": id.roles,
                            "message":        format!(
                                "Operator '{}' does not hold role '{}'. Registered roles: {:?}",
                                id.id, claimed, id.roles,
                            ),
                        })),
                    ).into_response();
                }
                // Body doesn't specify — use operator's first registered role.
                None => id.roles.first().cloned(),
            }
        }
        _ => req.approver_role.clone(), // Legacy anonymous: self-declared
    };

    let (outcome, maybe_request) = state.approval.decide(decision).await;

    match outcome {
        ApprovalOutcome::Approved => {
            let request = maybe_request.expect("Approved outcome must carry the request");
            (
                StatusCode::ACCEPTED,
                Json(ApproveResponse {
                    request_id:     req.request_id,
                    outcome:        "approved".into(),
                    transaction_id: request.transaction_id,
                    note:           req.note,
                }),
            ).into_response()
        }

        ApprovalOutcome::Rejected => {
            let request = maybe_request.expect("Rejected outcome must carry the request");
            (
                StatusCode::OK,
                Json(ApproveResponse {
                    request_id:     req.request_id,
                    outcome:        "rejected".into(),
                    transaction_id: request.transaction_id,
                    note:           req.note,
                }),
            ).into_response()
        }

        ApprovalOutcome::AlreadyDecided => (
            StatusCode::CONFLICT,
            Json(json!({
                "error":      "already_decided",
                "request_id": req.request_id,
                "message":    "This approval request has already been decided. Duplicate decisions are not allowed.",
            })),
        ).into_response(),

        ApprovalOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error":      "not_found",
                "request_id": req.request_id,
                "message":    "No pending approval request with this ID for the session.",
            })),
        ).into_response(),

        ApprovalOutcome::RoleMismatch { required, provided } => (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error":             "role_mismatch",
                "request_id":        req.request_id,
                "required_role":     required,
                "provided_role":     provided,
                "message":           format!(
                    "This request requires approver role '{}' but '{}' was provided.",
                    required,
                    provided.as_deref().unwrap_or("none"),
                ),
            })),
        ).into_response(),

        ApprovalOutcome::StageAdvanced { completed_stage, next_stage, next_required_role } => (
            StatusCode::OK,
            Json(json!({
                "status":              "stage_advanced",
                "request_id":          req.request_id,
                "completed_stage":     completed_stage,
                "next_stage":          next_stage,
                "next_required_role":  next_required_role,
                "message":             format!(
                    "Stage {} approved. Awaiting approval for stage {} (role: {}).",
                    completed_stage,
                    next_stage,
                    next_required_role.as_deref().unwrap_or("any"),
                ),
            })),
        ).into_response(),

        ApprovalOutcome::QuorumProgress { stage, approvals_so_far, approvals_required } => (
            StatusCode::OK,
            Json(json!({
                "status":              "quorum_progress",
                "request_id":          req.request_id,
                "stage":               stage,
                "approvals_so_far":    approvals_so_far,
                "approvals_required":  approvals_required,
                "message":             format!(
                    "Approval recorded for stage {}. {}/{} approvals received.",
                    stage, approvals_so_far, approvals_required,
                ),
            })),
        ).into_response(),

        ApprovalOutcome::DuplicateOperator { operator_id, stage } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error":        "duplicate_operator",
                "request_id":   req.request_id,
                "operator_id":  operator_id,
                "stage":        stage,
                "message":      format!(
                    "Operator '{}' has already voted on stage {}.",
                    operator_id, stage,
                ),
            })),
        ).into_response(),
    }
}

/// `GET /sessions/:id/approvals` — lists pending approval requests for a session.
pub async fn list_approvals(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Response {
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u)  => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "session not found" }))).into_response();
    }

    let pending = state.approval.pending_for_session(&session_id);
    Json(json!({
        "session_id": session_id,
        "pending_count": pending.len(),
        "requests": pending,
    })).into_response()
}

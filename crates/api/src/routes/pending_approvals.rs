//! `GET /pending-approvals` — every pending approval across every session.
//!
//! Each item carries both the `ApprovalRequest` and its `ApprovalWorkflow`
//! so multi-stage chains / quorum progress can be rendered without a second
//! call. Ordered by `requested_at` ascending.

use axum::{extract::State, response::{IntoResponse, Response}, Json};

use crate::state::{AppState, PendingApprovalItem};

#[derive(serde::Serialize)]
pub struct PendingApprovalsResponse {
    pub items: Vec<PendingApprovalItem>,
}

pub async fn list_pending_approvals(State(state): State<AppState>) -> Response {
    let items = state.approval.all_pending();
    Json(PendingApprovalsResponse { items }).into_response()
}

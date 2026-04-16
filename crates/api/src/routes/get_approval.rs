//! `GET /approvals/:id` — retrieve a single approval request with its workflow.
//!
//! Returns the same `PendingApprovalItem` shape used by `/pending-approvals`,
//! so the frontend can stop doing list-then-filter for individual lookups.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::state::AppState;

pub async fn get_approval(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Response {
    let Ok(request_id) = id_str.parse::<uuid::Uuid>() else {
        return (StatusCode::BAD_REQUEST, "invalid UUID").into_response();
    };

    match state.approval.get_by_id(request_id) {
        Some(item) => Json(item).into_response(),
        None => (StatusCode::NOT_FOUND, "approval not found").into_response(),
    }
}

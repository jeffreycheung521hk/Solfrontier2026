//! `GET /audit` — paged audit event feed (most-recent first).
//!
//! Query params:
//! - `limit`  (default 100, max 1000)
//! - `offset` (default 0)

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::state::{AppState, AuditRowDto};

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default = "default_limit")]
    pub limit:  i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 { 100 }

#[derive(serde::Serialize)]
pub struct AuditResponse {
    pub rows:   Vec<AuditRowDto>,
    pub limit:  i64,
    pub offset: i64,
}

pub async fn list_audit(
    State(state):  State<AppState>,
    Query(params): Query<AuditQuery>,
) -> Response {
    let limit  = params.limit.clamp(1, 1000);
    let offset = params.offset.max(0);

    match state.audit.list(limit, offset).await {
        Ok(rows) => Json(AuditResponse { rows, limit, offset }).into_response(),
        Err(e)   => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "audit_query_failed", "detail": e })),
        ).into_response(),
    }
}

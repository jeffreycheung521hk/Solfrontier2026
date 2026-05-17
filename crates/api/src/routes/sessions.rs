//! Session management API routes.
//!
//! `GET  /sessions`       — list active sessions
//! `POST /sessions`       — open a new session
//! `DELETE /sessions/:id` — close a session

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use claw_types::{agent::AgentRole, policy::PolicyRule, session::SessionId};

use crate::server::AppState;

#[derive(Debug, Deserialize)]
pub struct OpenSessionRequest {
    pub role:    AgentRole,
    pub channel: Option<String>,
    /// Optional per-session policy rules. Evaluated before global rules.
    #[serde(default)]
    pub policy_overrides: Option<Vec<PolicyRule>>,
}

#[derive(Debug, Serialize)]
pub struct OpenSessionResponse {
    pub session_id: SessionId,
    pub role:       AgentRole,
}

pub async fn list_sessions(State(state): State<AppState>) -> Response {
    let count = state.session_mgr.active_count();
    Json(json!({ "active_sessions": count })).into_response()
}

pub async fn open_session(
    State(state): State<AppState>,
    Json(req): Json<OpenSessionRequest>,
) -> Response {
    let channel = req.channel.unwrap_or_else(|| "api".to_string());
    let session_id = state.session_mgr.open(req.role, channel, req.policy_overrides);

    (
        StatusCode::CREATED,
        Json(OpenSessionResponse {
            session_id,
            role: req.role,
        }),
    )
        .into_response()
}

pub async fn close_session(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Response {
    // Parse the session ID — it's a UUID
    match id_str.parse::<uuid::Uuid>() {
        Ok(uuid) => {
            let id = SessionId::from(uuid);
            if state.session_mgr.is_active(&id) {
                state.session_mgr.close(&id, "api-requested");
                StatusCode::NO_CONTENT.into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "session not found" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid session id" })),
        )
            .into_response(),
    }
}

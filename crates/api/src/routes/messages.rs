//! Message ingress endpoint — the primary operator loop entry point.
//!
//! `POST /sessions/:id/messages` accepts an agent command, dispatches it
//! to the agent runtime, and returns the response synchronously.
//!
//! # Operator flow
//!
//! 1. Operator opens a session via `POST /sessions`
//! 2. Operator sends a command via `POST /sessions/:id/messages`
//! 3. Agent executes (ReAct loop: LLM → tools → LLM → ... → final response)
//! 4. Response is returned with tool_call_summary
//!
//! If the agent issues a transaction, the response will contain
//! `status: awaiting_approval` and the operator must POST to
//! `/sessions/:id/approve` to continue.
//!
//! # Security note
//!
//! The session ID is validated against active sessions before executing.
//! An inactive/closed session returns 404, not a stale execution.

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
    agent::{AgentCommand, AgentResponseStatus},
    session::SessionId,
};

use crate::server::AppState;

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    /// The instruction or question for the agent.
    pub text: String,
    /// Optional structured parameters the operator wants the agent to consider.
    #[serde(default)]
    pub parameters: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct SendMessageResponse {
    pub session_id:  SessionId,
    pub command_id:  Uuid,
    pub text:        String,
    pub status:      String,
    pub tool_calls:  Vec<ToolCallSummaryDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:        Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ToolCallSummaryDto {
    pub tool_name:   String,
    pub status:      String,
    pub duration_ms: u64,
}

/// `POST /sessions/:id/messages`
///
/// Executes an agent command for the given session.
pub async fn send_message(
    State(state): State<AppState>,
    Path(id_str):  Path<String>,
    Json(req):     Json<SendMessageRequest>,
) -> Response {
    // Validate + parse session ID
    let session_id = match id_str.parse::<Uuid>() {
        Ok(uuid) => SessionId::from(uuid),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid session id" })),
            )
                .into_response()
        }
    };

    // Verify session is active before doing expensive LLM call
    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or already closed" })),
        )
            .into_response();
    }

    // Build the AgentCommand
    let command = AgentCommand {
        id:          Uuid::new_v4(),
        text:        req.text,
        parameters:  req.parameters
            .into_iter()
            .map(|(k, v)| (k, v))
            .collect(),
        target_role: None,
    };

    let command_id = command.id;

    // Dispatch to agent via the MessageHandler interface.
    // This is async and may take several seconds (LLM round-trips).
    match state.message_handler.handle(&session_id, command).await {
        Ok(response) => {
            let status_str = match response.status {
                AgentResponseStatus::Completed        => "completed",
                AgentResponseStatus::AwaitingApproval => "awaiting_approval",
                AgentResponseStatus::Failed           => "failed",
                AgentResponseStatus::Cancelled        => "cancelled",
            };

            (
                StatusCode::OK,
                Json(SendMessageResponse {
                    session_id,
                    command_id,
                    text:       response.text,
                    status:     status_str.to_string(),
                    tool_calls: response.tool_calls.iter().map(|tc| ToolCallSummaryDto {
                        tool_name:   tc.tool_name.clone(),
                        status:      tc.status.clone(),
                        duration_ms: tc.duration_ms,
                    }).collect(),
                    data: response.data,
                }),
            )
                .into_response()
        }

        Err(err) => {
            tracing::error!(
                session = %session_id,
                error = %err,
                "agent execution failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "agent execution failed",
                    "detail": err
                })),
            )
                .into_response()
        }
    }
}

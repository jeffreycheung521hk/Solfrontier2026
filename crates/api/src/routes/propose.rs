//! `POST /sessions/:id/propose-transfer` — direct SOL transfer proposal.
//!
//! Bypasses the LLM agent for deterministic transaction building.
//! Builds a SOL transfer, routes through simulate → policy → signing pipeline.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use claw_types::session::SessionId;

use crate::server::AppState;

#[derive(Debug, Deserialize)]
pub struct ProposeTransferRequest {
    /// Sender public key (base58). Must be a loaded wallet.
    pub from: String,
    /// Recipient public key (base58).
    pub to: String,
    /// Amount in lamports (1 SOL = 1_000_000_000).
    pub lamports: u64,
}

pub async fn propose_transfer(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(req): Json<ProposeTransferRequest>,
) -> Response {
    let session_id = match id_str.parse::<uuid::Uuid>() {
        Ok(id) => SessionId::from(id),
        Err(_) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": "invalid session ID"
        }))).into_response(),
    };

    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "session not found or inactive"
        }))).into_response();
    }

    let proposer = match &state.propose {
        Some(p) => p,
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "error": "transaction proposal not available (no wallet loaded)"
        }))).into_response(),
    };

    match proposer.propose_transfer(&session_id, &req.from, &req.to, req.lamports).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({
            "error": e
        }))).into_response(),
    }
}

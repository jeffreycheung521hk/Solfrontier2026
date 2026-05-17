//! `POST /sessions/:id/wallet-bind-challenge` — request a challenge nonce.
//! `POST /sessions/:id/wallet-bind-confirm`   — submit signed challenge to prove ownership.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use claw_types::session::SessionId;

use crate::server::AppState;

/// Request body for creating a challenge.
#[derive(Debug, Deserialize)]
pub struct CreateChallengeRequest {
    /// The base58-encoded public key of the wallet to bind.
    pub pubkey: String,
}

/// Request body for confirming a challenge.
#[derive(Debug, Deserialize)]
pub struct ConfirmChallengeRequest {
    /// The challenge ID returned by the challenge endpoint.
    pub challenge_id: String,
    /// The base58-encoded public key of the wallet.
    pub pubkey: String,
    /// Base64-encoded ed25519 signature over the challenge message.
    pub signature_b64: String,
}

/// `POST /sessions/:id/wallet-bind-challenge`
pub async fn create_wallet_challenge(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(req): Json<CreateChallengeRequest>,
) -> Response {
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "session not found or not active" }))).into_response();
    }

    match state.wallet_challenges.create_challenge(&session_id, &req.pubkey).await {
        Ok(info) => {
            (
                StatusCode::OK,
                Json(json!({
                    "challenge_id": info.challenge_id,
                    "message": info.message,
                    "expires_at": info.expires_at,
                })),
            ).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()
        }
    }
}

/// `POST /sessions/:id/wallet-bind-confirm`
pub async fn confirm_wallet_challenge(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(req): Json<ConfirmChallengeRequest>,
) -> Response {
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "session not found or not active" }))).into_response();
    }

    match state.wallet_challenges.verify_and_bind(
        &session_id, &req.challenge_id, &req.pubkey, &req.signature_b64,
    ).await {
        Ok(verified_pubkey) => {
            (
                StatusCode::OK,
                Json(json!({
                    "session_id": session_id,
                    "pubkey": verified_pubkey,
                    "bound": true,
                    "verified": true,
                })),
            ).into_response()
        }
        Err(e) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
        }
    }
}

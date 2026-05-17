//! `POST /sessions/:id/wallet-signatures` — submit a signed transaction from an external wallet.
//! `GET  /sessions/:id/wallet-signatures` — list pending wallet signature requests.
//! `POST /sessions/:id/bind-wallet`       — bind an external wallet to a session.
//!
//! # Flow
//!
//! When the agent calls `submit_for_signing` targeting an external wallet,
//! the gateway returns `{ status: "awaiting_wallet_signature", ... }`.
//! The client retrieves the unsigned transaction and forwards it to the
//! external wallet for signing. The signed transaction is then submitted
//! back via `POST /sessions/:id/wallet-signatures`.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use claw_types::session::SessionId;

use crate::server::AppState;

/// Request body for `POST /sessions/:id/wallet-signatures`.
#[derive(Debug, Deserialize)]
pub struct SubmitWalletSignatureRequest {
    /// The wallet signature request ID (from the `awaiting_wallet_signature` response).
    pub request_id: Uuid,
    /// Base64-encoded signed transaction bytes.
    pub signed_tx_b64: String,
}

/// Response body for a processed wallet signature submission.
#[derive(Debug, Serialize)]
pub struct SubmitWalletSignatureResponse {
    /// The request ID.
    pub request_id: Uuid,
    /// Whether the signed transaction was accepted (verification passed).
    pub accepted: bool,
    /// The wallet's ed25519 signature.
    pub signature: Option<String>,
    /// On-chain transaction signature from RPC sendTransaction.
    pub tx_signature: Option<String>,
    /// Whether the transaction was submitted to the network.
    pub submitted: bool,
    /// Error message if rejected or submission failed.
    pub error: Option<String>,
    /// `true` when the wallet modified `recent_blockhash` (strict-mode rejection).
    ///
    /// This is a **retry signal**, not a verification failure:
    /// - The signed bytes were rejected because the V0 blockhash was altered.
    /// - The original approved `SwapIntent` is still valid.
    /// - The caller should trigger a fresh JIT build (e.g. by calling
    ///   `submit_jupiter_swap` again with the same parameters) and resubmit
    ///   the new transaction for signing.
    ///
    /// For any other verification failure, this field is `false`.
    pub rebuild_required: bool,
}

/// Request body for `POST /sessions/:id/bind-wallet`.
#[derive(Debug, Deserialize)]
pub struct BindWalletRequest {
    /// The base58-encoded public key of the external wallet.
    pub pubkey: String,
}

/// `POST /sessions/:id/wallet-signatures`
pub async fn submit_wallet_signature(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(req): Json<SubmitWalletSignatureRequest>,
) -> Response {
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "session not found or not active" }))).into_response();
    }

    let outcome = state
        .wallet_signatures
        .submit_signed_tx(&session_id, req.request_id, req.signed_tx_b64)
        .await;

    let status = if outcome.accepted {
        StatusCode::ACCEPTED
    } else {
        StatusCode::BAD_REQUEST
    };

    (
        status,
        Json(SubmitWalletSignatureResponse {
            request_id: outcome.request_id,
            accepted: outcome.accepted,
            signature: outcome.signature,
            tx_signature: outcome.tx_signature,
            submitted: outcome.submitted,
            error: outcome.error,
            rebuild_required: outcome.rebuild_required,
        }),
    )
        .into_response()
}

/// `GET /sessions/:id/wallet-signatures`
pub async fn list_wallet_signatures(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Response {
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "session not found" }))).into_response();
    }

    let pending = state.wallet_signatures.pending_for_session(&session_id);
    Json(json!({
        "session_id": session_id,
        "pending_count": pending.len(),
        "requests": pending,
    }))
    .into_response()
}

/// `POST /sessions/:id/bind-wallet`
pub async fn bind_wallet(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(req): Json<BindWalletRequest>,
) -> Response {
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    if !state.session_mgr.is_active(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "session not found or not active" }))).into_response();
    }

    state.wallet_signatures.bind_wallet(&session_id, &req.pubkey);

    (
        StatusCode::OK,
        Json(json!({
            "session_id": session_id,
            "pubkey": req.pubkey,
            "bound": true,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    //! Wire-shape contract tests for `POST /sessions/:id/wallet-signatures`.
    //!
    //! These tests lock the public response shape. The frontend / SDK depends on
    //! the exact field names and the backward-compatible default for
    //! `rebuild_required`. A breaking change here must update BOTH sides.
    use super::*;
    use serde_json::Value;

    fn base_response(rebuild_required: bool) -> SubmitWalletSignatureResponse {
        SubmitWalletSignatureResponse {
            request_id: Uuid::nil(),
            accepted: false,
            signature: None,
            tx_signature: None,
            submitted: false,
            error: Some("wallet modified blockhash; rebuild required".into()),
            rebuild_required,
        }
    }

    #[test]
    fn response_shape_contains_rebuild_required_field() {
        let v: Value = serde_json::to_value(base_response(true)).unwrap();
        let obj = v.as_object().expect("response must serialize to an object");

        for field in [
            "request_id",
            "accepted",
            "signature",
            "tx_signature",
            "submitted",
            "error",
            "rebuild_required",
        ] {
            assert!(
                obj.contains_key(field),
                "SubmitWalletSignatureResponse missing wire field `{field}`"
            );
        }
        assert_eq!(v["rebuild_required"], Value::Bool(true));
    }

    #[test]
    fn blockhash_modified_response_shape_is_retry_signal_not_accepted() {
        // Lock the exact combination: accepted=false + rebuild_required=true.
        // This is the semantic the persona and docs instruct clients to detect.
        let v: Value = serde_json::to_value(base_response(true)).unwrap();
        assert_eq!(v["accepted"], Value::Bool(false));
        assert_eq!(v["rebuild_required"], Value::Bool(true));
        assert_eq!(v["submitted"], Value::Bool(false));
    }

    #[test]
    fn generic_verification_failure_has_rebuild_required_false() {
        let v: Value = serde_json::to_value(base_response(false)).unwrap();
        assert_eq!(v["accepted"], Value::Bool(false));
        assert_eq!(v["rebuild_required"], Value::Bool(false));
    }

    #[test]
    fn legacy_client_payload_without_rebuild_required_deserializes() {
        // The gateway's `WalletSignatureOutcome` has `#[serde(default)]` on
        // `rebuild_required`. This test locks that guarantee so that an older
        // client response shape does not break deserialization in callers that
        // consume the outcome directly.
        use crate::state::WalletSignatureOutcome;
        let legacy = serde_json::json!({
            "request_id":   Uuid::nil(),
            "accepted":     true,
            "signature":    "sig",
            "tx_signature": null,
            "submitted":    true,
            "error":        null,
        });
        let outcome: WalletSignatureOutcome = serde_json::from_value(legacy).unwrap();
        assert!(!outcome.rebuild_required, "missing field must default to false");
    }
}

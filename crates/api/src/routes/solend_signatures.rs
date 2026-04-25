//! Phase 4C-6 — Solend-specific signing retrieval + submit endpoints.
//!
//! `GET  /sessions/:id/solend-signatures/:request_id` — serve the parked
//!     partially-signed transaction bytes + metadata to the session that
//!     owns the signing request.
//! `POST /sessions/:id/solend-signatures/:request_id` — accept the
//!     user-signed transaction back, verify + submit via the gateway's
//!     Solend submit path, and return the typed outcome.
//!
//! # Why separate from `wallet_signatures`
//!
//! The Solend flow has its own parked-artifact shape (obligation Keypair
//! lifetime, preflight metadata, grouped setup/refresh/deposit
//! instructions) and its own terminal-outcome cache
//! (`SolendSubmissionLifecycleStore`). Folding Solend into
//! `wallet_signatures` would either (a) leak Solend-specific fields into
//! the generic `WalletSignatureOutcome` that every tool consumes, or
//! (b) force the Solend signing store to impersonate
//! `ExternalWalletStore` (which lacks a Keypair slot for the obligation
//! signer). Parallel routes that share `State(AppState)` and the same
//! auth middleware are the cheaper option.
//!
//! # Session-ownership enforcement
//!
//! Both handlers require the authenticated session (the `:id` path
//! segment) to be active AND to match the signing request's owning
//! session inside the handler (`SolendSignatureHandler::retrieve` /
//! `::submit` return `NotFound` on mismatch — the two variants are
//! deliberately indistinguishable, so cross-session probes can't
//! distinguish "this id doesn't exist" from "you don't own it").

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
use crate::state::{SolendRetrievalResult, SolendSubmitResult};

/// Request body for `POST /sessions/:id/solend-signatures/:request_id`.
#[derive(Debug, Deserialize)]
pub struct SubmitSolendSignatureRequest {
    /// Base64-encoded signed (legacy) Solana `Transaction` bytes. The
    /// frontend should produce this by taking the `unsigned_tx_b64` from
    /// the GET response, co-signing in the user's wallet, and base64-
    /// encoding the resulting transaction bytes.
    pub signed_tx_b64: String,
}

fn parse_path_ids(
    session_str: &str,
    request_str: &str,
) -> Result<(SessionId, Uuid), Response> {
    let session_uuid = session_str.parse::<Uuid>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid session id" })),
        )
            .into_response()
    })?;
    let request_id = request_str.parse::<Uuid>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid request id" })),
        )
            .into_response()
    })?;
    Ok((SessionId::from(session_uuid), request_id))
}

fn require_solend_handler(state: &AppState) -> Result<(), Response> {
    if state.solend_signatures.is_some() {
        Ok(())
    } else {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "solend signature handler not wired in this daemon",
            })),
        )
            .into_response())
    }
}

/// `GET /sessions/:id/solend-signatures/:request_id`
///
/// Returns one of:
/// - `200 { status: "found", ... }` — parked and session matches
/// - `404 { status: "not_found" }` — missing OR cross-session (indistinguishable)
/// - `410 { status: "expired" }` — was there, TTL elapsed
/// - `400` — malformed ids
/// - `404` — session is not active
/// - `503` — solend handler not wired
pub async fn get_solend_signature(
    State(state): State<AppState>,
    Path((session_str, request_str)): Path<(String, String)>,
) -> Response {
    let (session_id, request_id) = match parse_path_ids(&session_str, &request_str) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response();
    }

    if let Err(resp) = require_solend_handler(&state) {
        return resp;
    }
    let handler = state.solend_signatures.as_ref().unwrap();

    let result = handler.retrieve(&session_id, request_id).await;
    match &result {
        // 200 OK for any known-good state the frontend can render.
        SolendRetrievalResult::Found { .. }
        | SolendRetrievalResult::Submitted { .. }
        | SolendRetrievalResult::Confirming { .. }
        | SolendRetrievalResult::Finalized { .. } => {
            (StatusCode::OK, Json(result)).into_response()
        }
        // 422 for semantic-level failures that require frontend action.
        SolendRetrievalResult::Failed { .. }
        | SolendRetrievalResult::Rejected { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(result)).into_response()
        }
        SolendRetrievalResult::BroadcastFailed { .. } => {
            (StatusCode::BAD_GATEWAY, Json(result)).into_response()
        }
        // 410 Gone — terminal-but-expired states (blockhash or signing TTL).
        SolendRetrievalResult::ConfirmationTimeout { .. }
        | SolendRetrievalResult::PreSubmitExpired { .. }
        | SolendRetrievalResult::Expired => (StatusCode::GONE, Json(result)).into_response(),
        SolendRetrievalResult::NotFound => {
            (StatusCode::NOT_FOUND, Json(result)).into_response()
        }
    }
}

/// `POST /sessions/:id/solend-signatures/:request_id`
///
/// Returns one of:
/// - `202 { status: "submitted", tx_signature, ... }` — broadcast accepted
/// - `200 { status: "recovered", tx_signature, ... }` — idempotent replay
/// - `404 { status: "not_found" }` — no parked entry AND no lifecycle cache
/// - `410 { status: "expired", reason }` — blockhash / TTL elapsed
/// - `422 { status: "rejected", error_type, message }` — verification fail
/// - `502 { status: "broadcast_failed", error_type, message, ... }` — verified but RPC errored
/// - `400` — malformed ids or body
/// - `404` — session is not active
/// - `503` — solend handler not wired
pub async fn submit_solend_signature(
    State(state): State<AppState>,
    Path((session_str, request_str)): Path<(String, String)>,
    Json(req): Json<SubmitSolendSignatureRequest>,
) -> Response {
    let (session_id, request_id) = match parse_path_ids(&session_str, &request_str) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response();
    }

    if let Err(resp) = require_solend_handler(&state) {
        return resp;
    }
    let handler = state.solend_signatures.as_ref().unwrap();

    let result = handler.submit(&session_id, request_id, req.signed_tx_b64).await;
    match &result {
        SolendSubmitResult::Submitted { .. } => (StatusCode::ACCEPTED, Json(result)).into_response(),
        SolendSubmitResult::Recovered { .. } => (StatusCode::OK, Json(result)).into_response(),
        SolendSubmitResult::NotFound => (StatusCode::NOT_FOUND, Json(result)).into_response(),
        SolendSubmitResult::Expired { .. } => (StatusCode::GONE, Json(result)).into_response(),
        SolendSubmitResult::Rejected { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(result)).into_response()
        }
        SolendSubmitResult::BroadcastFailed { .. } => {
            (StatusCode::BAD_GATEWAY, Json(result)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    //! Wire-shape contract tests for the Solend-signature endpoint result
    //! types. These tests lock the exposed JSON shape. A breaking change
    //! here must update the frontend AND any SDK consumers.
    use super::*;
    use serde_json::Value;

    #[test]
    fn retrieval_found_shape_includes_expected_fields() {
        let v = serde_json::to_value(SolendRetrievalResult::Found {
            signing_request_id: Uuid::nil(),
            intent_id: Uuid::nil(),
            session_wallet: "So11111111111111111111111111111111111111112".into(),
            unsigned_tx_b64: "AAAA".into(),
            obligation_signer_backend_partial: true,
            last_valid_block_height: 100,
            expires_at_unix_ms: 1_700_000_000_000,
            verified_slot: 50,
            simulation_slot: Some(51),
            units_consumed: Some(21_000),
        })
        .unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj["status"], Value::String("found".into()));
        for field in [
            "signing_request_id",
            "intent_id",
            "session_wallet",
            "unsigned_tx_b64",
            "obligation_signer_backend_partial",
            "last_valid_block_height",
            "expires_at_unix_ms",
            "verified_slot",
            "simulation_slot",
            "units_consumed",
        ] {
            assert!(obj.contains_key(field), "missing `{field}`");
        }
    }

    #[test]
    fn retrieval_not_found_shape_is_status_only() {
        let v = serde_json::to_value(SolendRetrievalResult::NotFound).unwrap();
        assert_eq!(v["status"], Value::String("not_found".into()));
    }

    #[test]
    fn retrieval_expired_shape_is_status_only() {
        let v = serde_json::to_value(SolendRetrievalResult::Expired).unwrap();
        assert_eq!(v["status"], Value::String("expired".into()));
    }

    // ── 4C-7 lifecycle retrieval shapes ──────────────────────────────────────

    #[test]
    fn retrieval_submitted_shape_includes_tx_signature() {
        let v = serde_json::to_value(SolendRetrievalResult::Submitted {
            signing_request_id: Uuid::nil(),
            intent_id: Uuid::nil(),
            tx_signature: "5Gx7...".into(),
            last_valid_block_height: 1000,
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("submitted".into()));
        assert_eq!(v["tx_signature"], Value::String("5Gx7...".into()));
    }

    #[test]
    fn retrieval_confirming_shape_includes_slot_and_tx_signature() {
        let v = serde_json::to_value(SolendRetrievalResult::Confirming {
            signing_request_id: Uuid::nil(),
            intent_id: Uuid::nil(),
            tx_signature: "5Gx7...".into(),
            slot: 123_456,
            last_valid_block_height: 1000,
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("confirming".into()));
        assert_eq!(v["slot"], Value::Number(123_456u64.into()));
    }

    #[test]
    fn retrieval_finalized_shape_is_terminal_success() {
        let v = serde_json::to_value(SolendRetrievalResult::Finalized {
            signing_request_id: Uuid::nil(),
            intent_id: Uuid::nil(),
            tx_signature: "FINALIZED-SIG".into(),
            slot: 200_000,
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("finalized".into()));
        assert_eq!(v["tx_signature"], Value::String("FINALIZED-SIG".into()));
    }

    #[test]
    fn retrieval_confirmation_timeout_requires_reproposal() {
        let v = serde_json::to_value(SolendRetrievalResult::ConfirmationTimeout {
            signing_request_id: Uuid::nil(),
            tx_signature: "TIMEOUT-SIG".into(),
            last_valid_block_height: 1000,
            current_block_height: 1100,
            requires_reproposal: true,
            reason: "height past".into(),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("confirmation_timeout".into()));
        assert_eq!(v["requires_reproposal"], Value::Bool(true));
    }

    #[test]
    fn retrieval_failed_carries_error_string() {
        let v = serde_json::to_value(SolendRetrievalResult::Failed {
            signing_request_id: Uuid::nil(),
            intent_id: Uuid::nil(),
            tx_signature: "EXEC-FAIL".into(),
            err: "InstructionError(0, Custom(42))".into(),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("failed".into()));
        assert!(v["err"].as_str().unwrap().contains("Custom(42)"));
    }

    #[test]
    fn retrieval_presubmit_expired_distinct_from_signing_ttl_expired() {
        // Two variants deliberately kept distinct on the wire:
        //  - `pre_submit_expired` — signing window elapsed; lifecycle had record
        //  - `expired`            — no record; signing-store swept
        let v_pre = serde_json::to_value(SolendRetrievalResult::PreSubmitExpired {
            reason: "lease elapsed".into(),
        })
        .unwrap();
        let v_exp = serde_json::to_value(SolendRetrievalResult::Expired).unwrap();
        assert_eq!(v_pre["status"], Value::String("pre_submit_expired".into()));
        assert_eq!(v_exp["status"], Value::String("expired".into()));
    }

    #[test]
    fn submit_submitted_shape_includes_tx_signature() {
        let v = serde_json::to_value(SolendSubmitResult::Submitted {
            signing_request_id: Uuid::nil(),
            intent_id: Uuid::nil(),
            session_wallet: "So11111111111111111111111111111111111111112".into(),
            tx_signature: "5Gx7...".into(),
            verified_slot: 50,
            last_valid_block_height: 100,
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("submitted".into()));
        assert_eq!(v["tx_signature"], Value::String("5Gx7...".into()));
    }

    #[test]
    fn submit_recovered_shape_includes_original_tx_signature() {
        let v = serde_json::to_value(SolendSubmitResult::Recovered {
            signing_request_id: Uuid::nil(),
            tx_signature: "ORIG-SIG".into(),
            recorded_at_unix_ms: 1_700_000_000_000,
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("recovered".into()));
        assert_eq!(v["tx_signature"], Value::String("ORIG-SIG".into()));
        assert_eq!(v["recorded_at_unix_ms"], Value::Number(1_700_000_000_000i64.into()));
    }

    #[test]
    fn submit_rejected_shape_preserves_error_type_and_message() {
        let v = serde_json::to_value(SolendSubmitResult::Rejected {
            error_type: "BLOCKHASH_EXPIRED".into(),
            message: "current block height exceeds last_valid_block_height".into(),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("rejected".into()));
        assert_eq!(v["error_type"], Value::String("BLOCKHASH_EXPIRED".into()));
    }

    #[test]
    fn submit_broadcast_failed_shape_preserves_signing_request_id() {
        let v = serde_json::to_value(SolendSubmitResult::BroadcastFailed {
            signing_request_id: Uuid::nil(),
            error_type: "RPC_TIMEOUT".into(),
            message: "connection reset".into(),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("broadcast_failed".into()));
        assert!(v.as_object().unwrap().contains_key("signing_request_id"));
    }
}

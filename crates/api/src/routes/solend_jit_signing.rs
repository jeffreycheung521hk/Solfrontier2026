//! Phase 6B Window 2 — Solend JIT-prepare route.
//!
//! `POST /sessions/:session_id/approvals/:approval_request_id/solend-signing/prepare`
//!
//! The prepare route turns an `Approved + JIT-ready` Solend deposit
//! into a fresh signing handoff. The frontend calls this from the
//! user's "Sign with Phantom" click handler — by deferring the
//! blockhash fetch + tx assembly to click time, the validity window
//! starts at the click instead of at approval time, which fixes the
//! manual-flow latency race observed twice on 2026-05-04.
//!
//! HTTP status mapping (mirrors the existing `solend_signatures.rs`
//! envelope discipline):
//! - `200 { status: "ready", ... }` — fresh handoff created
//! - `404 { status: "not_found" }` — approval missing OR cross-session probe
//! - `404 { status: "jit_ready_missing" }` — no JIT-ready entry (approval
//!   exists but never persisted, or lazy-swept)
//! - `422 { status: "not_approved", state }` — workflow not in Approved
//! - `422 { status: "wallet_mismatch", expected, bound }` — current
//!   binding differs from re-check capture
//! - `502 { status: "handoff_create_failed", error_type, message }` —
//!   `create_signing_handoff` errored
//! - `400` — malformed ids / body
//! - `404 { error }` — session is not active
//! - `503 { error }` — JIT prepare handler not wired
//!
//! # What this route MUST NOT do
//!
//! - Approve. The route only consumes an existing approval; it never
//!   flips state.
//! - Sign. The user's wallet signs after this route returns.
//! - Submit. The existing POST `/solend-signatures/:signing_request_id`
//!   route accepts the user-signed bytes.
//! - Broadcast. Submit owns broadcast.
//!
//! # Retry semantics
//!
//! Calling `prepare` again for the same approved request creates a
//! FRESH `signing_request_id` with a fresh blockhash — the JIT-ready
//! entry is not consumed by a successful prepare. This is how the
//! frontend recovers from an expired handoff without forcing a
//! re-approval.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use uuid::Uuid;

use claw_types::session::SessionId;

use crate::server::AppState;
use crate::state::SolendJitPrepareResult;

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
    let request_uuid = request_str.parse::<Uuid>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid approval request id" })),
        )
            .into_response()
    })?;
    Ok((SessionId::from(session_uuid), request_uuid))
}

/// `POST /sessions/:session_id/approvals/:approval_request_id/solend-signing/prepare`
pub async fn prepare_solend_signing(
    State(state): State<AppState>,
    Path((session_str, approval_str)): Path<(String, String)>,
) -> Response {
    let (session_id, approval_request_id) = match parse_path_ids(&session_str, &approval_str) {
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

    let handler = match state.solend_jit_prepare.as_ref() {
        Some(h) => h,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "solend jit prepare handler not wired in this daemon",
                })),
            )
                .into_response();
        }
    };

    let result = handler.prepare(&session_id, approval_request_id).await;
    match &result {
        SolendJitPrepareResult::Ready { .. } => (StatusCode::OK, Json(result)).into_response(),
        SolendJitPrepareResult::NotApproved { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(result)).into_response()
        }
        SolendJitPrepareResult::JitReadyMissing => {
            (StatusCode::NOT_FOUND, Json(result)).into_response()
        }
        SolendJitPrepareResult::WalletMismatch { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(result)).into_response()
        }
        SolendJitPrepareResult::HandoffCreateFailed { .. } => {
            (StatusCode::BAD_GATEWAY, Json(result)).into_response()
        }
        SolendJitPrepareResult::NotFound => {
            (StatusCode::NOT_FOUND, Json(result)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    //! Wire-shape contract tests for the prepare result envelope. A
    //! breaking change here must update the frontend AND any SDK
    //! consumers.
    use super::*;
    use claw_types::session::SessionId;
    use serde_json::Value;

    fn nil_session() -> SessionId {
        SessionId::from(Uuid::nil())
    }

    #[test]
    fn ready_shape_includes_expected_fields() {
        let v = serde_json::to_value(SolendJitPrepareResult::Ready {
            approval_request_id: Uuid::nil(),
            signing_request_id: Uuid::nil(),
            session_id: nil_session(),
            wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L".into(),
            last_valid_block_height: 393_666_166,
            verified_slot: 415_571_900,
            expires_at_unix_ms: 1_700_000_000_000,
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("ready".into()));
        for field in [
            "approval_request_id",
            "signing_request_id",
            "session_id",
            "wallet",
            "last_valid_block_height",
            "verified_slot",
            "expires_at_unix_ms",
        ] {
            assert!(v.as_object().unwrap().contains_key(field), "missing `{field}`");
        }
    }

    #[test]
    fn not_approved_shape_carries_state() {
        let v = serde_json::to_value(SolendJitPrepareResult::NotApproved {
            state: "pending".into(),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("not_approved".into()));
        assert_eq!(v["state"], Value::String("pending".into()));
    }

    #[test]
    fn jit_ready_missing_shape_is_status_only() {
        let v = serde_json::to_value(SolendJitPrepareResult::JitReadyMissing).unwrap();
        assert_eq!(v["status"], Value::String("jit_ready_missing".into()));
    }

    #[test]
    fn wallet_mismatch_shape_carries_expected_and_bound() {
        let v = serde_json::to_value(SolendJitPrepareResult::WalletMismatch {
            expected: "BPfDMm...".into(),
            bound: Some("OtherPubkey...".into()),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("wallet_mismatch".into()));
        assert_eq!(v["expected"], Value::String("BPfDMm...".into()));
        assert_eq!(v["bound"], Value::String("OtherPubkey...".into()));
    }

    #[test]
    fn wallet_mismatch_with_no_bound_serialises_bound_as_null() {
        let v = serde_json::to_value(SolendJitPrepareResult::WalletMismatch {
            expected: "BPfDMm...".into(),
            bound: None,
        })
        .unwrap();
        assert_eq!(v["bound"], Value::Null);
    }

    #[test]
    fn handoff_create_failed_shape_preserves_error_type_and_message() {
        let v = serde_json::to_value(SolendJitPrepareResult::HandoffCreateFailed {
            error_type: "BlockhashFetchFailed".into(),
            message: "RPC timeout".into(),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("handoff_create_failed".into()));
        assert_eq!(v["error_type"], Value::String("BlockhashFetchFailed".into()));
        assert_eq!(v["message"], Value::String("RPC timeout".into()));
    }

    #[test]
    fn not_found_shape_is_status_only() {
        let v = serde_json::to_value(SolendJitPrepareResult::NotFound).unwrap();
        assert_eq!(v["status"], Value::String("not_found".into()));
    }
}

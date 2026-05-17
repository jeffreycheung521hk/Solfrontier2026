//! Phase 6I-F — Solend WITHDRAW JIT-prepare route.
//!
//! `POST /sessions/:session_id/approvals/:approval_request_id/solend-withdraw-jit/prepare`
//!
//! Mirrors the deposit-side `/solend-signing/prepare` route but operates
//! against the WITHDRAW pipeline:
//!   - looks up a parked `ParkedSolendWithdrawAllIntent`
//!   - re-fetches the obligation account, re-runs the four structural
//!     re-checks (owner / lending market / non-zero USDC deposit / no
//!     borrow)
//!   - calls the production withdraw plan assembler
//!   - parks the unsigned tx in the same `SolendSigningStore` deposit uses
//!   - returns `signing_request_id` + obligation/reserve metadata
//!
//! HTTP status mapping:
//!   - `200 { status: "ready", ... }` — fresh handoff created
//!   - `404 { status: "not_found" }` — approval missing OR cross-session probe
//!   - `404 { status: "withdraw_intent_missing" }` — no parked withdraw
//!     intent under this approval id (e.g. it was a deposit approval, or
//!     resume task hasn't yet persisted, or lazy-swept)
//!   - `422 { status: "not_approved", state }` — workflow not Approved
//!   - `422 { status: "wallet_mismatch", expected, bound }` — current
//!     binding differs from intent capture
//!   - `422 { status: "recheck_blocked", reason, detail }` — fresh
//!     on-chain state failed one of the four invariants
//!   - `502 { status: "snapshot_assemble_failed", error_type, message }`
//!   - `502 { status: "plan_assembly_failed", error_type, message }`
//!   - `502 { status: "handoff_create_failed", error_type, message }`
//!   - `400` — malformed ids
//!   - `404 { error }` — session is not active
//!   - `503 { error }` — withdraw JIT prepare handler not wired
//!
//! Retry semantics: identical to deposit's prepare route. Each call
//! issues a fresh `signing_request_id` + fresh blockhash; the parked
//! withdraw intent is NOT consumed by a successful prepare so the
//! frontend can re-prepare after an expired handoff without forcing
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
use crate::state::SolendWithdrawJitPrepareResult;

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

/// `POST /sessions/:session_id/approvals/:approval_request_id/solend-withdraw-jit/prepare`
pub async fn prepare_solend_withdraw_signing(
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

    let handler = match state.solend_withdraw_jit_prepare.as_ref() {
        Some(h) => h,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "solend withdraw jit prepare handler not wired in this daemon",
                })),
            )
                .into_response();
        }
    };

    let result = handler.prepare(&session_id, approval_request_id).await;
    match &result {
        SolendWithdrawJitPrepareResult::Ready { .. } => {
            (StatusCode::OK, Json(result)).into_response()
        }
        SolendWithdrawJitPrepareResult::NotApproved { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(result)).into_response()
        }
        SolendWithdrawJitPrepareResult::WithdrawIntentMissing => {
            (StatusCode::NOT_FOUND, Json(result)).into_response()
        }
        SolendWithdrawJitPrepareResult::WalletMismatch { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(result)).into_response()
        }
        SolendWithdrawJitPrepareResult::RecheckBlocked { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(result)).into_response()
        }
        SolendWithdrawJitPrepareResult::PlanAssemblyFailed { .. }
        | SolendWithdrawJitPrepareResult::HandoffCreateFailed { .. }
        | SolendWithdrawJitPrepareResult::SnapshotAssembleFailed { .. } => {
            (StatusCode::BAD_GATEWAY, Json(result)).into_response()
        }
        SolendWithdrawJitPrepareResult::NotFound => {
            (StatusCode::NOT_FOUND, Json(result)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    //! Wire-shape contract tests. A breaking change here must update
    //! the frontend AND any SDK consumers.
    use super::*;
    use serde_json::Value;

    fn nil_session() -> SessionId {
        SessionId::from(Uuid::nil())
    }

    #[test]
    fn ready_shape_includes_expected_fields() {
        let v = serde_json::to_value(SolendWithdrawJitPrepareResult::Ready {
            approval_request_id: Uuid::nil(),
            signing_request_id: Uuid::nil(),
            session_id: nil_session(),
            wallet: "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".into(),
            obligation_pubkey: "HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV".into(),
            reserve_pubkey: "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw".into(),
            last_valid_block_height: 1_000_000,
            verified_slot: 415_500_000,
            expires_at_unix_ms: 1_700_000_000_000,
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("ready".into()));
        for field in [
            "approval_request_id",
            "signing_request_id",
            "session_id",
            "wallet",
            "obligation_pubkey",
            "reserve_pubkey",
            "last_valid_block_height",
            "verified_slot",
            "expires_at_unix_ms",
        ] {
            assert!(v.as_object().unwrap().contains_key(field), "missing `{field}`");
        }
    }

    #[test]
    fn withdraw_intent_missing_shape_is_status_only() {
        let v = serde_json::to_value(SolendWithdrawJitPrepareResult::WithdrawIntentMissing)
            .unwrap();
        assert_eq!(v["status"], Value::String("withdraw_intent_missing".into()));
    }

    #[test]
    fn recheck_blocked_shape_carries_reason_and_detail() {
        let v = serde_json::to_value(SolendWithdrawJitPrepareResult::RecheckBlocked {
            reason: "owner_mismatch".into(),
            detail: Some("fresh owner != session wallet".into()),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("recheck_blocked".into()));
        assert_eq!(v["reason"], Value::String("owner_mismatch".into()));
        assert_eq!(
            v["detail"],
            Value::String("fresh owner != session wallet".into())
        );
    }

    #[test]
    fn not_approved_shape_carries_state() {
        let v = serde_json::to_value(SolendWithdrawJitPrepareResult::NotApproved {
            state: "pending".into(),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("not_approved".into()));
        assert_eq!(v["state"], Value::String("pending".into()));
    }

    #[test]
    fn wallet_mismatch_shape_carries_expected_and_bound() {
        let v = serde_json::to_value(SolendWithdrawJitPrepareResult::WalletMismatch {
            expected: "C4QQjz...".into(),
            bound: Some("OtherKey...".into()),
        })
        .unwrap();
        assert_eq!(v["status"], Value::String("wallet_mismatch".into()));
        assert_eq!(v["expected"], Value::String("C4QQjz...".into()));
        assert_eq!(v["bound"], Value::String("OtherKey...".into()));
    }

    #[test]
    fn not_found_shape_is_status_only() {
        let v = serde_json::to_value(SolendWithdrawJitPrepareResult::NotFound).unwrap();
        assert_eq!(v["status"], Value::String("not_found".into()));
    }
}

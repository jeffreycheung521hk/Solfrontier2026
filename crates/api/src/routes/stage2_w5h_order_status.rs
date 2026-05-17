//! W5i — `GET /sessions/:id/stage2/w5h/order/:rule_id_hex`.
//!
//! Read-only handler. Frontend polls this to detect the W5i
//! auto-execution watcher's terminal status after the order has
//! reached `budget_reserved`. The endpoint:
//!
//!   1. Parses the session id and `rule_id_hex` path segments.
//!   2. Verifies the session is active.
//!   3. Refuses 503 if no `W5hOrderStatusHandler` is wired.
//!   4. Delegates to the handler and returns the typed DTO.
//!
//! No mutations, no RPC calls, no signing. Pure DB read.

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
use crate::state::W5hOrderStatusRouteOutcome;

/// Hard cap on path-segment hex length. The real value is 32 chars
/// (16-byte rule_id), so anything beyond a small margin is malformed.
const MAX_HEX_CHARS: usize = 128;

fn bad_request(reason: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": reason.into() })),
    )
        .into_response()
}

fn service_unavailable(reason: impl Into<String>) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": reason.into() })),
    )
        .into_response()
}

/// `GET /sessions/:id/stage2/w5h/order/:rule_id_hex`
pub async fn get_w5h_order_status(
    State(state): State<AppState>,
    Path((session_str, rule_id_hex)): Path<(String, String)>,
) -> Response {
    let session_id = match session_str.parse::<Uuid>() {
        Ok(u) => SessionId::from(u),
        Err(_) => return bad_request("invalid session id"),
    };
    if rule_id_hex.is_empty() {
        return bad_request("rule_id_hex must not be empty");
    }
    if rule_id_hex.chars().count() > MAX_HEX_CHARS {
        return bad_request("rule_id_hex too long");
    }

    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response();
    }

    let handler = match state.chat_order_status.as_ref() {
        Some(h) => h,
        None => {
            return service_unavailable(
                "W5h order-status handler not wired in this daemon",
            )
        }
    };

    let outcome = handler.get(&session_id, &rule_id_hex).await;
    match outcome {
        W5hOrderStatusRouteOutcome::Ok(dto) => (StatusCode::OK, Json(dto)).into_response(),
        W5hOrderStatusRouteOutcome::NotFound(r) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": r })),
        )
            .into_response(),
        W5hOrderStatusRouteOutcome::BadRequest(r) => bad_request(r),
        W5hOrderStatusRouteOutcome::SessionNotActive => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response(),
        W5hOrderStatusRouteOutcome::Disabled(r) => service_unavailable(r),
    }
}

#[cfg(test)]
mod source_guard_tests {
    #[test]
    fn no_signing_or_send_call_sites() {
        const SOURCE: &str = include_str!("stage2_w5h_order_status.rs");
        let needles = [
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "send", "Transaction("),
            format!("{}{}", "confirm", "Transaction("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_w5h_order_status.rs must not contain `{n}`"
            );
        }
    }
}

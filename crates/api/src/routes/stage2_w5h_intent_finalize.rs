//! Phase 5c-lite — `POST /sessions/:id/stage2/w5h/intent/finalize`.
//!
//! Dumb-pipe handler. Routes a frontend confirm / reject of a previously
//! emitted `DraftIntentReviewDto` to the gateway adapter, which:
//!
//!   - looks the draft up in the in-process [`DraftIntentStore`]
//!     (Phase 5c-lite — no DB row exists for the draft until this
//!     route runs),
//!   - recomputes the canonical hash from the persisted preimage and
//!     compares it byte-for-byte to the `draft_hash` the client sent,
//!   - on confirm + match: consumes the draft AND persists the
//!     `WatchRule` + `W5hFundingIntent` (the existing
//!     deterministic-path pipeline), returning the standard
//!     `funding_required` payload wrapped in a finalize-audit
//!     envelope,
//!   - on reject: drops the draft and returns
//!     `status="rejected"` (no DB row written),
//!   - on hash mismatch: does NOT consume the draft and returns 409
//!     with a typed error code; the user can retry the confirm from
//!     the same card,
//!   - on not-found / expired / already-finalized: returns the
//!     appropriate typed 404/409.
//!
//! # Status mapping
//!
//! | HTTP | When |
//! |------|------|
//! | 200 OK | `W5hIntentFinalizeRouteOutcome::Ok(_)` (confirm + persist OR explicit reject) |
//! | 400 Bad Request | invalid session id, malformed body, missing/empty fields, unknown action |
//! | 404 Not Found | session not active OR `draft_not_found_or_expired` |
//! | 409 Conflict | `draft_hash_mismatch` (do not retry without re-fetch) OR `draft_already_finalized_or_missing` |
//! | 503 Service Unavailable | no `W5hIntentFinalizeHandler` wired (daemon not configured with the chat / extractor) |

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
use crate::state::{
    W5hIntentFinalizeRequestDto, W5hIntentFinalizeRouteOutcome,
};

const MAX_DRAFT_ID_CHARS: usize = 128;
const MAX_DRAFT_HASH_CHARS: usize = 128;

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

/// `POST /sessions/:id/stage2/w5h/intent/finalize`
///
/// Body shape: [`W5hIntentFinalizeRequestDto`]. The route:
///
///   1. Parses the session id (400 on garbage).
///   2. Hard-caps the string fields and rejects empty / unknown
///      `action`.
///   3. Checks the session is active (404 if not).
///   4. Refuses with 503 if no `W5hIntentFinalizeHandler` is wired.
///   5. Delegates to the wired handler and maps the typed outcome
///      to the documented HTTP status.
pub async fn post_w5h_intent_finalize(
    State(state): State<AppState>,
    Path(session_str): Path<String>,
    Json(body): Json<W5hIntentFinalizeRequestDto>,
) -> Response {
    let session_id = match session_str.parse::<Uuid>() {
        Ok(u) => SessionId::from(u),
        Err(_) => return bad_request("invalid session id"),
    };

    if body.draft_id.is_empty() {
        return bad_request("draft_id must not be empty");
    }
    if body.draft_id.chars().count() > MAX_DRAFT_ID_CHARS {
        return bad_request("draft_id too long");
    }
    if body.draft_hash.is_empty() {
        return bad_request("draft_hash must not be empty");
    }
    if body.draft_hash.chars().count() > MAX_DRAFT_HASH_CHARS {
        return bad_request("draft_hash too long");
    }
    // Action is one of {confirm, reject}. The gateway-side handler
    // also re-checks (defense in depth), but rejecting unknown
    // strings here gives the operator a 400 with a clear reason
    // instead of a generic backend rejection.
    match body.action.as_str() {
        "confirm" | "reject" => {}
        other => {
            return bad_request(format!(
                "unknown finalize action {other:?} (expected \"confirm\" or \"reject\")"
            ));
        }
    }

    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response();
    }

    let handler = match state.chat_intent_finalize.as_ref() {
        Some(h) => h,
        None => {
            return service_unavailable(
                "W5h intent-finalize handler not wired in this daemon",
            )
        }
    };

    let outcome = handler.execute(&session_id, body).await;
    match outcome {
        W5hIntentFinalizeRouteOutcome::Ok(dto) => {
            (StatusCode::OK, Json(dto)).into_response()
        }
        W5hIntentFinalizeRouteOutcome::NotFoundOrExpired => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error_code": "draft_not_found_or_expired",
                "error": "draft not found or expired",
            })),
        )
            .into_response(),
        W5hIntentFinalizeRouteOutcome::HashMismatch { provided, backend } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error_code": "draft_hash_mismatch",
                "error": "draft_hash does not match the canonical hash for the persisted draft",
                "provided_draft_hash": provided,
                "backend_draft_hash": backend,
            })),
        )
            .into_response(),
        W5hIntentFinalizeRouteOutcome::AlreadyFinalizedOrMissing => (
            StatusCode::CONFLICT,
            Json(json!({
                "error_code": "draft_already_finalized_or_missing",
                "error": "draft already finalized or never existed",
            })),
        )
            .into_response(),
        W5hIntentFinalizeRouteOutcome::BadRequest(r) => bad_request(r),
        W5hIntentFinalizeRouteOutcome::SessionNotActive => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response(),
        W5hIntentFinalizeRouteOutcome::Disabled(r) => service_unavailable(r),
    }
}

#[cfg(test)]
mod source_guard_tests {
    //! Static guards: this module must remain a dumb pipe.

    #[test]
    fn no_panic_paths_in_handler() {
        const SOURCE: &str = include_str!("stage2_w5h_intent_finalize.rs");
        let needles = [
            format!("{}{}", ".un", "wrap()"),
            format!("{}{}", ".ex", "pect("),
            format!("{}{}", "pa", "nic!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_w5h_intent_finalize.rs must not contain `{n}`"
            );
        }
    }

    #[test]
    fn no_signing_or_send_call_sites() {
        const SOURCE: &str = include_str!("stage2_w5h_intent_finalize.rs");
        let needles = [
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "send", "Transaction("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_w5h_intent_finalize.rs must not contain `{n}`"
            );
        }
    }

    #[test]
    fn no_sleeps_or_polling_in_handler() {
        const SOURCE: &str = include_str!("stage2_w5h_intent_finalize.rs");
        let needles = [
            format!("{}{}", "tokio::time::", "sleep"),
            format!("{}{}", "std::thread::", "sleep"),
            format!("{}{}", "tokio::time::", "interval"),
            format!("{}{}", "tokio::time::", "timeout"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_w5h_intent_finalize.rs must not contain `{n}`"
            );
        }
    }
}

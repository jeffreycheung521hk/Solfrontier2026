//! W5g — `POST /sessions/:id/stage2/w5g/execute`.
//!
//! Dumb-pipe handler for the W5g controlled-wallet Solend deposit
//! execution route. It is intentionally minimal:
//!
//!  1. Parse the path segment as a `SessionId` (400 on garbage).
//!  2. Decode the JSON body into [`ChatExecuteRequestDto`] (400 on
//!     malformed body — `serde(deny_unknown_fields)` keeps the
//!     surface tight).
//!  3. Verify the session is open (404 if not).
//!  4. Delegate to the wired [`crate::state::ChatExecuteHandler`].
//!  5. Translate the typed [`ChatExecuteRouteOutcome`] back into
//!     HTTP status + DTO.
//!
//! The handler does NOT load a keypair, hit Solana RPC, or call
//! `sendTransaction`. All of that lives in the gateway-side
//! `Stage2ChatExecutor` and its `Stage2ChatExecuteSender`.
//!
//! # Status mapping
//!
//! | HTTP | When |
//! |------|------|
//! | 200 OK | `ChatExecuteRouteOutcome::Ok(_)` — the typed DTO carries the success / precheck-fail / timeout / execution-fail status |
//! | 400 Bad Request | invalid session id, malformed body, empty / oversize approval phrase |
//! | 404 Not Found | session is not active |
//! | 503 Service Unavailable | no `ChatExecuteHandler` wired (daemon not configured for W5g) |

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
use crate::state::{ChatExecuteRequestDto, ChatExecuteRouteOutcome};

/// Hard cap on the approval phrase length to bound work even if the
/// framework body limit is misconfigured upstream. The required
/// phrase is 42 chars; any payload more than ~10x that is suspicious.
const MAX_PHRASE_CHARS: usize = 512;
/// Hard cap on the hex string lengths — rule_id is 32 chars,
/// canonical hash is 64 chars; anything beyond a small margin is a
/// malformed request.
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

/// `POST /sessions/:id/stage2/w5g/execute`
///
/// Body shape: [`ChatExecuteRequestDto`]. Returns the W5g execution
/// result DTO on `Ok(_)` (200), translating only structural body /
/// session errors to a 4xx / 5xx envelope.
pub async fn post_w5g_execute(
    State(state): State<AppState>,
    Path(session_str): Path<String>,
    Json(body): Json<ChatExecuteRequestDto>,
) -> Response {
    let session_id = match session_str.parse::<Uuid>() {
        Ok(u) => SessionId::from(u),
        Err(_) => return bad_request("invalid session id"),
    };

    // Defense in depth: caps on the string fields. The body limit is
    // applied at the router seam in `server.rs`; this is a per-field
    // sanity check.
    if body.rule_id_hex.chars().count() > MAX_HEX_CHARS {
        return bad_request("rule_id_hex too long");
    }
    if body.canonical_rule_hash_hex.chars().count() > MAX_HEX_CHARS {
        return bad_request("canonical_rule_hash_hex too long");
    }
    if body.approval_phrase.is_empty() {
        return bad_request("approval_phrase must not be empty");
    }
    if body.approval_phrase.chars().count() > MAX_PHRASE_CHARS {
        return bad_request("approval_phrase too long");
    }

    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response();
    }

    let handler = match state.chat_execute.as_ref() {
        Some(h) => h,
        None => {
            return service_unavailable(
                "W5g execute handler not wired in this daemon",
            )
        }
    };

    let outcome = handler.execute(&session_id, body).await;
    match outcome {
        ChatExecuteRouteOutcome::Ok(dto) => (StatusCode::OK, Json(dto)).into_response(),
        ChatExecuteRouteOutcome::BadRequest(r) => bad_request(r),
        ChatExecuteRouteOutcome::SessionNotActive => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response(),
        ChatExecuteRouteOutcome::Disabled(r) => service_unavailable(r),
    }
}

#[cfg(test)]
mod source_guard_tests {
    //! Static guards: this module must remain a dumb pipe.

    #[test]
    fn no_panic_paths_in_handler() {
        const SOURCE: &str = include_str!("stage2_chat_execute.rs");
        let needles = [
            format!("{}{}", ".un", "wrap()"),
            format!("{}{}", ".ex", "pect("),
            format!("{}{}", "pa", "nic!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_chat_execute.rs must not contain `{n}`"
            );
        }
    }

    #[test]
    fn no_signing_or_send_call_sites() {
        const SOURCE: &str = include_str!("stage2_chat_execute.rs");
        // Needles assembled at runtime so the test source itself
        // does NOT contain the joined literals.
        let needles = [
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "send", "Transaction("),
            format!("{}{}", "confirm", "Transaction("),
            // The route MUST NOT contain literal Helius / Solend
            // base URLs. The gateway-side executor holds those.
            format!("{}{}", "api.helius.", "xyz"),
            format!("{}{}", "api.sole", "nd.fi"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_chat_execute.rs must not contain `{n}` — \
                 that work belongs in the gateway adapter"
            );
        }
    }
}

//! Phase 5D.2 — user-facing chat route (`POST /sessions/:id/chat`).
//!
//! # Dumb-pipe principle
//!
//! This handler is intentionally minimal. It is the HTTP shell around
//! [`ChatHandler::handle_chat`]. It does only the four things any HTTP
//! pipe MUST do:
//!
//!  1. Parse the path segment as a `SessionId` (400 on garbage).
//!  2. Bound and parse the JSON body (axum's `DefaultBodyLimit` is
//!     applied at the router seam in `server.rs`; oversize bodies are
//!     rejected at framework level with 413 *before* this function is
//!     even called).
//!  3. Verify the session is open (404 if not).
//!  4. Hand `(session_id, message)` to the wired
//!     [`crate::state::ChatHandler`] and translate its typed
//!     [`crate::state::ChatRouteOutcome`] back into HTTP status + DTO.
//!
//! # What this handler does NOT do
//!
//! - Does NOT call any LLM provider.
//! - Does NOT consult the registry, the dispatcher, the approval store,
//!   or the wallet/signing pipeline.
//! - Does NOT inspect, sanitize, or reformat the response DTO — the
//!   sanitation contract lives inside the gateway adapter that
//!   produced the [`crate::state::ChatResponse`].
//! - Does NOT panic. The handler has no infallible-on-error escape
//!   hatches in any path the framework can reach. Every control-plane
//!   fault flows through a typed branch.
//! - Does NOT use `500 Internal Server Error` for any *domain* failure —
//!   400 / 404 / 409 / 503 cover every variant. A 500 from this route
//!   means an axum / tokio bug above us, not a chat domain fault.
//!
//! # Status mapping
//!
//! | HTTP | When |
//! |------|------|
//! | 200 OK | `ChatRouteOutcome::Ok(ChatResponse::*)` |
//! | 409 Conflict | `ChatRouteOutcome::Conflict(ChatResponse::PendingActionExists{..})` |
//! | 400 Bad Request | invalid session id; empty / blank message; oversize body¹ |
//! | 404 Not Found | session is not active |
//! | 503 Service Unavailable | no `ChatHandler` wired, or provider is `Disabled` |
//!
//! ¹ Oversize bodies are rejected by `DefaultBodyLimit` *before* this
//! handler runs and surface as 413 Payload Too Large from axum's body
//! layer. They never reach this code.

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
use crate::state::ChatRouteOutcome;

/// Hard cap on the user-message length (in *chars*, after JSON decode).
///
/// Smaller than the framework body limit (4096 bytes) so even a
/// pathological all-multibyte payload that gets through the body cap
/// still cannot push past this. Phase 5D.2 picks 4000 chars as a hard
/// upper bound; the LLM provider's own input bound applies on top.
const MAX_MESSAGE_CHARS: usize = 4000;

/// Wire shape for `POST /sessions/:id/chat`.
///
/// Strict shape — a body containing extra fields, missing `message`,
/// or `message` as a non-string is rejected at deserialization time
/// (via `serde(deny_unknown_fields)`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRequestBody {
    /// The user's verbatim text. Trimmed of leading/trailing whitespace
    /// before being passed to the chat handler. Empty after trim → 400.
    pub message: String,
}

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

/// `POST /sessions/:id/chat`
///
/// Body: `{ "message": "<user text>" }` (max 4096 bytes total at the
/// framework body layer; max 4000 chars on `message` after JSON decode).
///
/// See module docs for the full status mapping.
pub async fn post_chat(
    State(state): State<AppState>,
    Path(session_str): Path<String>,
    Json(body): Json<ChatRequestBody>,
) -> Response {
    // ── 1. Parse session id ───────────────────────────────────────────
    let session_id = match session_str.parse::<Uuid>() {
        Ok(u) => SessionId::from(u),
        Err(_) => return bad_request("invalid session id"),
    };

    // ── 2. Validate message ───────────────────────────────────────────
    let trimmed = body.message.trim();
    if trimmed.is_empty() {
        return bad_request("message must not be empty");
    }
    if trimmed.chars().count() > MAX_MESSAGE_CHARS {
        return bad_request("message exceeds maximum length");
    }
    let message = trimmed.to_string();

    // ── 3. Verify session is open ─────────────────────────────────────
    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response();
    }

    // ── 4. Locate handler ─────────────────────────────────────────────
    let handler = match state.chat.as_ref() {
        Some(h) => h,
        None => return service_unavailable("chat handler not wired in this daemon"),
    };

    // ── 5. Dispatch (the only call that touches a provider) ───────────
    let outcome = handler.handle_chat(&session_id, message).await;

    // ── 6. Map typed outcome → HTTP status + JSON ─────────────────────
    match outcome {
        ChatRouteOutcome::Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        ChatRouteOutcome::Conflict(resp) => {
            (StatusCode::CONFLICT, Json(resp)).into_response()
        }
        ChatRouteOutcome::BadRequest(reason) => bad_request(reason),
        ChatRouteOutcome::Disabled(reason) => service_unavailable(reason),
    }
}

#[cfg(test)]
mod source_guard_tests {
    //! Static guards: this module must remain a dumb pipe.

    /// Guard: no infallible-on-error shortcuts (Result→T extractors,
    /// Option→T extractors, or unconditional aborts) anywhere in this
    /// file. The handler must surface every fault as a typed Response.
    #[test]
    fn no_panic_paths_in_handler() {
        const SOURCE: &str = include_str!("chat.rs");
        // Needles assembled at runtime so the guards do not match
        // their own descriptions.
        let needles = [
            format!("{}{}", ".un", "wrap()"),
            format!("{}{}", ".ex", "pect("),
            format!("{}{}", "pa", "nic!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/chat.rs must not contain `{n}`"
            );
        }
    }

    /// Guard: no LLM provider / signing / submit / broadcast call sites.
    /// The HTTP handler is forbidden from doing anything past calling the
    /// trait. All real work belongs in the gateway-side adapter.
    #[test]
    fn no_provider_or_signing_call_sites() {
        const SOURCE: &str = include_str!("chat.rs");
        let needles = [
            format!("{}{}{}", "var(\"", "OPENAI_API_K", "EY\")"),
            format!("{}{}{}", "var(\"", "ANTHROPIC_API_K", "EY\")"),
            format!("{}{}", "https://api.openai.", "com"),
            format!("{}{}", "https://api.anthropic.", "com"),
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "ToolDis", "patcher::"),
            format!("{}{}", "ToolReg", "istry::"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/chat.rs must not contain `{n}` — that work belongs in the gateway adapter"
            );
        }
    }
}

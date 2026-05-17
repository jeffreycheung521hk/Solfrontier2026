//! W5h-lite — `POST /sessions/:id/stage2/w5h/funding/confirm`.
//!
//! Dumb-pipe handler for the W5h funding-confirm route. After the
//! user has Phantom-signed a TransferChecked of 0.25 USDC from their
//! wallet to the controlled wallet's USDC ATA, the frontend POSTs the
//! signature here so the backend can authoritatively verify the
//! token-balance delta and flip the W5h intent to `budget_reserved`.
//!
//! W5h-lite addendum §3 — POLLING OWNERSHIP:
//!
//! This route is INTENTIONALLY fast. It does NOT sleep, poll, or hold
//! the HTTP request open waiting for the funding tx to finalize.
//! Behaviour:
//!
//!  - On `funding_pending` from the orchestrator: return 200 OK with
//!    `status: "funding_pending"`. The FRONTEND owns the retry loop
//!    (2-3 s cadence, bounded UI timeout).
//!
//!  - On terminal status: return 200 OK with the terminal DTO.
//!
//!  - Never block / never sleep / never long-poll on this path.
//!
//! # Status mapping
//!
//! | HTTP | When |
//! |------|------|
//! | 200 OK | `W5hFundingConfirmRouteOutcome::Ok(_)` (every domain status, including `funding_pending`) |
//! | 400 Bad Request | invalid session id, malformed body, empty hex / signature |
//! | 404 Not Found | session is not active |
//! | 503 Service Unavailable | no `W5hFundingConfirmHandler` wired (daemon not configured with the W5h substrate) |

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
use crate::state::{W5hFundingConfirmRequestDto, W5hFundingConfirmRouteOutcome};

/// Hard caps on string field lengths. Defensive even when the body
/// limit middleware is misconfigured upstream. The real values:
///   - rule_id_hex: 32 chars
///   - canonical fields: 0 (not in this DTO)
///   - signatures: ~88 chars (base58 of 64 bytes)
///   - wallet/ATA pubkeys: ~44 chars (base58 of 32 bytes)
const MAX_HEX_CHARS: usize = 128;
const MAX_SIG_CHARS: usize = 128;
const MAX_PUBKEY_CHARS: usize = 96;

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

/// `POST /sessions/:id/stage2/w5h/funding/confirm`
///
/// Body shape: [`W5hFundingConfirmRequestDto`]. The handler:
///
///   1. Parses the session id (400 on garbage).
///   2. Hard-caps every string field (defensive against oversize
///      bodies that slipped past the framework body limit).
///   3. Checks the session is active (404 if not).
///   4. Refuses with 503 if no `W5hFundingConfirmHandler` is wired.
///   5. Delegates to the wired handler and returns the typed DTO
///      verbatim. `funding_pending` is a normal 200 OK.
pub async fn post_w5h_funding_confirm(
    State(state): State<AppState>,
    Path(session_str): Path<String>,
    Json(body): Json<W5hFundingConfirmRequestDto>,
) -> Response {
    let session_id = match session_str.parse::<Uuid>() {
        Ok(u) => SessionId::from(u),
        Err(_) => return bad_request("invalid session id"),
    };

    if body.rule_id_hex.is_empty() {
        return bad_request("rule_id_hex must not be empty");
    }
    if body.rule_id_hex.chars().count() > MAX_HEX_CHARS {
        return bad_request("rule_id_hex too long");
    }
    if body.funding_signature.is_empty() {
        return bad_request("funding_signature must not be empty");
    }
    if body.funding_signature.chars().count() > MAX_SIG_CHARS {
        return bad_request("funding_signature too long");
    }
    for (label, val) in [
        ("user_wallet", &body.user_wallet),
        ("user_usdc_ata", &body.user_usdc_ata),
        ("controlled_wallet", &body.controlled_wallet),
        ("controlled_usdc_ata", &body.controlled_usdc_ata),
    ] {
        if val.is_empty() {
            return bad_request(format!("{label} must not be empty"));
        }
        if val.chars().count() > MAX_PUBKEY_CHARS {
            return bad_request(format!("{label} too long"));
        }
    }

    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response();
    }

    let handler = match state.chat_funding_confirm.as_ref() {
        Some(h) => h,
        None => {
            return service_unavailable(
                "W5h funding-confirm handler not wired in this daemon",
            )
        }
    };

    let outcome = handler.execute(&session_id, body).await;
    match outcome {
        W5hFundingConfirmRouteOutcome::Ok(dto) => (StatusCode::OK, Json(dto)).into_response(),
        W5hFundingConfirmRouteOutcome::BadRequest(r) => bad_request(r),
        W5hFundingConfirmRouteOutcome::SessionNotActive => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        )
            .into_response(),
        W5hFundingConfirmRouteOutcome::Disabled(r) => service_unavailable(r),
    }
}

#[cfg(test)]
mod source_guard_tests {
    //! Static guards: this module must remain a dumb pipe.

    #[test]
    fn no_panic_paths_in_handler() {
        const SOURCE: &str = include_str!("stage2_w5h_funding_confirm.rs");
        let needles = [
            format!("{}{}", ".un", "wrap()"),
            format!("{}{}", ".ex", "pect("),
            format!("{}{}", "pa", "nic!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_w5h_funding_confirm.rs must not contain `{n}`"
            );
        }
    }

    #[test]
    fn no_signing_or_send_call_sites() {
        const SOURCE: &str = include_str!("stage2_w5h_funding_confirm.rs");
        let needles = [
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "send", "Transaction("),
            format!("{}{}", "confirm", "Transaction("),
            // No literal RPC base URLs in the route — that belongs to
            // the gateway-side LiveTxFetcher.
            format!("{}{}", "api.helius.", "xyz"),
            format!("{}{}", "api.sole", "nd.fi"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_w5h_funding_confirm.rs must not contain `{n}` \
                 \u{2014} that work belongs in the gateway adapter"
            );
        }
    }

    #[test]
    fn no_sleeps_or_blocking_polling_in_handler() {
        // W5h-lite addendum §3: the HTTP route must NEVER sleep /
        // long-poll / hold the request open waiting for tx
        // finalization. Polling ownership is on the frontend.
        //
        // Needles are assembled at runtime so the test source itself
        // does NOT contain the joined literals.
        const SOURCE: &str = include_str!("stage2_w5h_funding_confirm.rs");
        let needles = [
            format!("{}{}", "tokio::time::", "sleep"),
            format!("{}{}", "std::thread::", "sleep"),
            format!("{}{}", "tokio::time::", "interval"),
            format!("{}{}", "tokio::time::", "timeout"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "routes/stage2_w5h_funding_confirm.rs must not contain `{n}` \
                 \u{2014} the route MUST return funding_pending immediately and \
                 let the frontend own polling cadence"
            );
        }
    }
}

//! `GET /sessions/:id/events` — Server-Sent Events stream for session-scoped events.
//!
//! Clients connect and receive a stream of `GatewayEvent`s serialized as JSON,
//! filtered to the requested session.
//!
//! # Durability disclaimer
//!
//! The underlying event bus is a `tokio::sync::broadcast` channel with capacity
//! 1024. Events are NOT persisted. If a client connects after an event was
//! emitted, or falls behind the broadcast window, events are silently dropped.
//! Clients should reconnect and poll the audit log for historical events if needed.
//!
//! # Usage
//!
//!   GET /sessions/<session_id>/events
//!   Accept: text/event-stream
//!
//! Each event is a `data:` field containing a JSON-serialized `GatewayEvent`.
//! A `heartbeat` comment is emitted every 30 seconds to keep the connection alive.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures::stream::{self};
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use claw_types::{events::GatewayEvent, session::SessionId};

use crate::server::AppState;

/// `GET /sessions/:id/events`
///
/// Returns an SSE stream of `GatewayEvent`s scoped to the given session.
/// The stream ends when the client disconnects or the session is closed.
pub async fn session_events(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Response {
    // ── Parse session ID ──────────────────────────────────────────────────────
    let session_uuid = match id_str.parse::<Uuid>() {
        Ok(u)  => u,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid session id" }))).into_response(),
    };
    let session_id = SessionId::from(session_uuid);

    // ── Validate session ──────────────────────────────────────────────────────
    if !state.session_mgr.is_active(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found or not active" })),
        ).into_response();
    }

    // ── Subscribe to the event bus ────────────────────────────────────────────
    let rx = state.events.subscribe();

    // ── Build the SSE stream ──────────────────────────────────────────────────
    let stream = stream::unfold(
        (rx, session_id),
        |(mut rx, sid)| async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // Filter to events relevant to this session.
                        if !event_matches_session(&event, &sid) {
                            // Not for this session — keep polling.
                            continue;
                        }

                        let data = match serde_json::to_string(&event) {
                            Ok(s)  => s,
                            Err(e) => {
                                // Serialization failure should never happen for our types,
                                // but emit an error event rather than silently dropping.
                                format!("{{\"error\":\"serialization_failed\",\"details\":\"{e}\"}}")
                            }
                        };

                        let sse_event = Event::default().data(data);
                        return Some((Ok::<_, Infallible>(sse_event), (rx, sid)));
                    }

                    Err(RecvError::Lagged(n)) => {
                        // Client fell behind the broadcast window. Emit a warning event
                        // and continue — do NOT terminate the stream.
                        let lag_event = Event::default()
                            .event("lag_warning")
                            .data(format!("{{\"dropped_events\":{n}}}"));
                        return Some((Ok(lag_event), (rx, sid)));
                    }

                    Err(RecvError::Closed) => {
                        // The event bus was shut down (daemon is stopping). End stream.
                        return None;
                    }
                }
            }
        },
    );

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("heartbeat"),
        )
        .into_response()
}

/// Returns `true` if this event should be forwarded to the session-scoped stream.
///
/// Currently forwards all approval, transaction, and agent lifecycle events for
/// the given session. Session-global system events are not forwarded.
fn event_matches_session(event: &GatewayEvent, session_id: &SessionId) -> bool {
    match event {
        GatewayEvent::AgentTaskStarted(e)    => &e.session_id == session_id,
        GatewayEvent::AgentTaskCompleted(e)  => &e.session_id == session_id,
        GatewayEvent::AgentTaskFailed(e)     => &e.session_id == session_id,
        GatewayEvent::ToolInvoked(e)         => &e.session_id == session_id,
        GatewayEvent::ToolCompleted(e)       => &e.session_id == session_id,
        GatewayEvent::ToolFailed(e)          => &e.session_id == session_id,
        GatewayEvent::TransactionProposed(e) => &e.session_id == session_id,
        GatewayEvent::TransactionSimulated(e)=> &e.session_id == session_id,
        GatewayEvent::PolicyEvaluated(e)     => &e.session_id == session_id,
        GatewayEvent::ApprovalRequested(e)   => &e.session_id == session_id,
        GatewayEvent::ApprovalReceived(e)    => &e.session_id == session_id,
        GatewayEvent::TransactionSigned(e)   => &e.session_id == session_id,
        GatewayEvent::TransactionSent(e)     => &e.session_id == session_id,
        GatewayEvent::TransactionSubmitted(e)=> &e.session_id == session_id,
        GatewayEvent::TransactionConfirmed(e)=> &e.session_id == session_id,
        GatewayEvent::TransactionFinalized(e)=> &e.session_id == session_id,
        GatewayEvent::TransactionFailed(e)   => &e.session_id == session_id,
        GatewayEvent::TransactionExecutionFailed(e) => &e.session_id == session_id,
        GatewayEvent::TransactionDropped(e)  => &e.session_id == session_id,
        GatewayEvent::TransactionExpired(e)  => &e.session_id == session_id,
        GatewayEvent::WalletSignatureRequested(e) => &e.session_id == session_id,
        GatewayEvent::WalletSignatureReceived(e)  => &e.session_id == session_id,
        GatewayEvent::WalletSignatureRejected(e)  => &e.session_id == session_id,
        GatewayEvent::WalletSignatureExpired(e)   => &e.session_id == session_id,
        GatewayEvent::SessionOpened(e)       => &e.session_id == session_id,
        GatewayEvent::SessionStateChanged(e) => &e.session_id == session_id,
        GatewayEvent::SessionClosed(e)       => &e.session_id == session_id,
        // System-wide events are not session-scoped.
        GatewayEvent::SolanaEvent(_)         => false,
        GatewayEvent::AlertEmitted(_)        => false,
        GatewayEvent::HealthCheckCompleted(_)=> false,
        GatewayEvent::ConfigReloaded(_)      => false,
        GatewayEvent::DaemonShuttingDown     => true, // forward shutdown to all
    }
}

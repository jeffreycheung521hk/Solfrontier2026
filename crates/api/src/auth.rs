//! Local bearer-token authentication middleware.
//!
//! The API server runs on localhost only. It is protected by a bearer token
//! that is generated at daemon startup and stored in the state store.
//! The `claw` CLI reads the token from `~/.config/claw/token` (written
//! by the daemon at startup).
//!
//! **What belongs here:** token extraction, constant-time comparison,
//! rejection response formatting.
//!
//! **What does NOT belong here:** session logic, policy evaluation,
//! or any Solana code.

use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;

/// The shared auth token state injected into the router.
#[derive(Clone)]
pub struct AuthToken(Arc<String>);

impl AuthToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(Arc::new(token.into()))
    }

    /// Returns the expected token value.
    pub fn value(&self) -> &str {
        &self.0
    }
}

/// Axum middleware that validates the `Authorization: Bearer <token>` header.
///
/// Requests without a valid token receive `401 Unauthorized`.
/// The comparison uses constant-time equality to prevent timing attacks.
pub async fn require_bearer_token(
    axum::extract::State(expected): axum::extract::State<AuthToken>,
    request: Request,
    next: Next,
) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(token, expected.value()) => {
            next.run(request).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized", "message": "valid Bearer token required" })),
        )
            .into_response(),
    }
}

/// Constant-time string comparison to prevent timing side-channels.
fn constant_time_eq(a: &str, b: &str) -> bool {
    use std::hint::black_box;
    if a.len() != b.len() {
        return false;
    }
    let result = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes().iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    black_box(result) == 0
}

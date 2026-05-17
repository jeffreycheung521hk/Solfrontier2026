//! Local bearer-token authentication middleware.
//!
//! The API server runs on localhost only. It is protected by bearer tokens.
//! Each token maps to an `OperatorIdentity` via the `OperatorRegistry`.
//!
//! # Operator resolution
//!
//! When `[[operators]]` are configured in TOML, each operator has their own
//! token and role set. The middleware resolves the token to an identity and
//! injects it into request extensions, making it available to route handlers.
//!
//! When no operators are configured (legacy single-token mode), the fallback
//! `CLAW_API_TOKEN` creates an anonymous operator with all-role access.
//!
//! **What belongs here:** token extraction, identity resolution,
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

use claw_types::operator::OperatorIdentity;

/// The shared auth token state injected into the router.
///
/// In legacy mode (no `[[operators]]` config), holds a single shared token.
/// In operator mode, this is unused — `OperatorRegistry` handles resolution.
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

/// Registry mapping bearer tokens to operator identities.
///
/// Thread-safe and cheap to clone (Arc-wrapped).
/// When empty, falls back to `AuthToken` for legacy single-token auth.
#[derive(Clone, Debug)]
pub struct OperatorRegistry {
    /// Token → OperatorIdentity mapping.
    operators: Arc<Vec<(String, OperatorIdentity)>>,
}

impl OperatorRegistry {
    /// Creates an empty registry (legacy mode — falls back to AuthToken).
    pub fn new() -> Self {
        Self {
            operators: Arc::new(Vec::new()),
        }
    }

    /// Creates a registry from a list of (token, identity) pairs.
    pub fn from_operators(operators: Vec<(String, OperatorIdentity)>) -> Self {
        Self {
            operators: Arc::new(operators),
        }
    }

    /// Returns true if no operators are registered (legacy mode).
    pub fn is_empty(&self) -> bool {
        self.operators.is_empty()
    }

    /// Resolves a bearer token to an operator identity.
    /// Uses constant-time comparison for each registered token.
    pub fn resolve(&self, token: &str) -> Option<OperatorIdentity> {
        self.operators
            .iter()
            .find(|(t, _)| constant_time_eq(t, token))
            .map(|(_, identity)| identity.clone())
    }
}

impl Default for OperatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Combined auth state for the middleware.
#[derive(Clone)]
pub struct AuthState {
    pub token: AuthToken,
    pub registry: OperatorRegistry,
}

/// Axum middleware that validates the bearer token and resolves operator identity.
///
/// Also accepts `?token=<value>` as a query parameter fallback for SSE
/// endpoints, since `EventSource` does not support custom request headers.
///
/// Resolved `OperatorIdentity` is injected into request extensions.
/// Route handlers can extract it via `request.extensions().get::<OperatorIdentity>()`.
pub async fn require_bearer_token(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let provided = extract_token(&request);

    // Try operator registry first (if configured).
    if !auth.registry.is_empty() {
        if let Some(ref token) = provided {
            if let Some(identity) = auth.registry.resolve(token) {
                request.extensions_mut().insert(identity);
                return next.run(request).await;
            }
        }
        // Token not found in registry — unauthorized.
        return unauthorized_response();
    }

    // Legacy fallback: single shared token → anonymous operator.
    match provided {
        Some(ref token) if constant_time_eq(token, auth.token.value()) => {
            request.extensions_mut().insert(OperatorIdentity::anonymous());
            next.run(request).await
        }
        _ => unauthorized_response(),
    }
}

/// Extracts the bearer token from the request (header or query param).
fn extract_token(request: &Request) -> Option<String> {
    // Try Authorization header first (preferred).
    let from_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    // Fallback: ?token= query parameter (for EventSource/SSE).
    let from_query = request
        .uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|pair| pair.strip_prefix("token="))
                .map(|v| v.to_string())
        });

    from_header.or(from_query)
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized", "message": "valid Bearer token required" })),
    )
        .into_response()
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

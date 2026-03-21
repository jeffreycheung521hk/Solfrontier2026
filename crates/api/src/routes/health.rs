//! `GET /health` — system health endpoint.
//!
//! Runs all registered health checks and returns aggregated status.
//! Returns `200 OK` when healthy, `503 Service Unavailable` when any
//! component is unhealthy.
//!
//! This endpoint is intentionally NOT protected by the auth middleware
//! so that load balancers and process supervisors can check it freely.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use claw_observability::{HealthRegistry, HealthStatus};

/// Handler state: just needs the health registry.
pub async fn health_handler(State(registry): State<HealthRegistry>) -> Response {
    let health = registry.check_all().await;

    let status_code = match health.status {
        HealthStatus::Healthy  => StatusCode::OK,
        HealthStatus::Degraded => StatusCode::OK, // degraded is still "up"
        HealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
    };

    (status_code, Json(health)).into_response()
}

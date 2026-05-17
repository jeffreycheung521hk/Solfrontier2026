//! `POST /debug/seed-demo` — inject a realistic snapshot for demos.
//!
//! Only available when the daemon is started with `CLAW_ENABLE_DEMO_SEED=1`.
//! The endpoint is idempotent-ish: calling it repeatedly appends more rows.
//! Not for production. Not for tests. Strictly for sponsor demos / screenshots.

use axum::{extract::State, http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::json;

use crate::state::AppState;

pub async fn seed_demo(State(state): State<AppState>) -> Response {
    let Some(seeder) = &state.demo_seeder else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "demo_seed_disabled",
                "hint": "start the daemon with CLAW_ENABLE_DEMO_SEED=1 to enable"
            })),
        ).into_response();
    };

    match seeder.seed().await {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(e)     => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "seed_failed", "detail": e })),
        ).into_response(),
    }
}

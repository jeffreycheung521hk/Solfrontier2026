//! `GET /policy/rules` — current global policy rules.
//!
//! Returns the rule list in evaluation order. Session/per-wallet overrides
//! are *prepended* at evaluation time; this endpoint only exposes the
//! global baseline.

use axum::{extract::State, response::{IntoResponse, Response}, Json};

use crate::state::AppState;

/// Response body for `GET /policy/rules`.
#[derive(serde::Serialize)]
pub struct PolicyRulesResponse {
    /// Rule list in evaluation order (first-match-wins).
    pub rules: Vec<claw_types::policy::PolicyRule>,
}

pub async fn list_policy_rules(State(state): State<AppState>) -> Response {
    Json(PolicyRulesResponse { rules: state.policy.rules() }).into_response()
}

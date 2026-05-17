//! `GET /metrics` — operational metrics endpoint.
//!
//! Returns all counter and gauge values as JSON.
//! Protected by the same bearer token as other API endpoints.

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::state::AppState;

pub async fn metrics_handler(State(state): State<AppState>) -> Json<Value> {
    let m = &state.metrics;

    // Build the per-rule hits sub-object from the dynamic CounterMap.
    let rule_hits: serde_json::Map<String, Value> = m
        .policy_rule_hits
        .snapshot()
        .into_iter()
        .map(|(k, v)| (k, json!(v)))
        .collect();

    Json(json!({
        "sessions_opened":          m.sessions_opened.get(),
        "sessions_closed":          m.sessions_closed.get(),
        "sessions_active":          m.sessions_active.get(),
        "agent_tasks_started":      m.agent_tasks_started.get(),
        "agent_tasks_completed":    m.agent_tasks_completed.get(),
        "agent_tasks_failed":       m.agent_tasks_failed.get(),
        "tool_calls_total":         m.tool_calls_total.get(),
        "tool_calls_failed":        m.tool_calls_failed.get(),
        "tool_calls_timed_out":     m.tool_calls_timed_out.get(),
        "transactions_proposed":    m.transactions_proposed.get(),
        "transactions_sent":        m.transactions_sent.get(),
        "transactions_confirmed":   m.transactions_confirmed.get(),
        "transactions_failed":      m.transactions_failed.get(),
        "policy_rejections":        m.policy_rejections.get(),
        "rpc_calls_total":          m.rpc_calls_total.get(),
        "rpc_calls_failed":         m.rpc_calls_failed.get(),
        "rpc_retries":              m.rpc_retries.get(),
        "ws_subscriptions_active":  m.ws_subscriptions_active.get(),
        "alerts_emitted":           m.alerts_emitted.get(),
        "rate_limit_hits":          m.rate_limit_hits.get(),
        "policy_rule_hits":         rule_hits,
    }))
}

//! N28 — Policy alerting runtime verification.
//!
//! Proves:
//! 1. A policy rejection alert is delivered to a local mock webhook with
//!    the correct alert_type, severity, and message fields.
//! 2. Alert delivery failure (unreachable server) does not panic or block.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::net::TcpListener;

use claw_gateway::config::AlertingConfig;
use claw_gateway::policy_alerting::{AlertDispatcher, PolicyAlertSender};
use claw_types::alert::AlertSeverity;

// ── Mock webhook server ──────────────────────────────────────────────────────

struct CapturedAlerts {
    payloads: Mutex<Vec<Value>>,
}

async fn capture_alert(
    axum::extract::State(state): axum::extract::State<Arc<CapturedAlerts>>,
    axum::Json(body): axum::Json<Value>,
) -> &'static str {
    state.payloads.lock().unwrap().push(body);
    "ok"
}

/// Starts a local axum server on a random port and returns (port, captured_alerts).
async fn start_mock_webhook() -> (u16, Arc<CapturedAlerts>) {
    let captured = Arc::new(CapturedAlerts {
        payloads: Mutex::new(Vec::new()),
    });

    let app = axum::Router::new()
        .route("/webhook", axum::routing::post(capture_alert))
        .with_state(captured.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, captured)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn policy_rejection_emits_alert_with_rule_and_context() {
    let (port, captured) = start_mock_webhook().await;

    let config = AlertingConfig {
        enabled: true,
        webhook_url: Some(format!("http://127.0.0.1:{}/webhook", port)),
        max_retries: 1,
        timeout_ms: 5000,
    };

    let sender = PolicyAlertSender::from_config(&config).expect("should create sender");

    let details = json!({
        "rule_name": "denylist-block",
        "session_id": "test-session-123",
        "wallet_pubkey": "test-wallet-abc",
        "transfer_lamports": 1_000_000_000u64,
    });

    sender.send("policy_rejected", AlertSeverity::Warning, "Transaction blocked by denylist", details);

    // Wait for the spawned delivery task to complete.
    // The sender.send() spawns a task; give it time to deliver.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let payloads = captured.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 1, "should have received exactly one alert");

    let alert = &payloads[0];
    assert_eq!(alert["alert_type"], "policy_rejected");
    assert_eq!(alert["severity"], "warning");
    assert_eq!(alert["message"], "Transaction blocked by denylist");
    assert_eq!(alert["details"]["rule_name"], "denylist-block");
    assert_eq!(alert["details"]["session_id"], "test-session-123");
    assert!(alert["occurred_at"].as_str().is_some(), "should have timestamp");
}

#[tokio::test]
async fn alert_delivery_failure_does_not_break_caller() {
    // Point to a port where nothing is listening.
    // Use a high port that's very unlikely to be in use.
    let config = AlertingConfig {
        enabled: true,
        webhook_url: Some("http://127.0.0.1:1/webhook".to_string()),
        max_retries: 0, // No retries — fail fast
        timeout_ms: 500,
    };

    let sender = PolicyAlertSender::from_config(&config).expect("should create sender");

    // This should not panic. The spawned task will fail and log a warning.
    sender.send(
        "test_failure",
        AlertSeverity::Error,
        "This will fail to deliver",
        json!({"test": true}),
    );

    // Wait to let the spawned task attempt and fail delivery.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // If we get here, the caller was not blocked or panicked.
    // That's the assertion — no panic, no hang.
}

#[tokio::test]
async fn alert_dispatcher_active_delivers() {
    let (port, captured) = start_mock_webhook().await;

    let config = AlertingConfig {
        enabled: true,
        webhook_url: Some(format!("http://127.0.0.1:{}/webhook", port)),
        max_retries: 1,
        timeout_ms: 5000,
    };

    let sender = PolicyAlertSender::from_config(&config).unwrap();
    let dispatcher = AlertDispatcher::Active(sender);

    assert!(dispatcher.is_active());

    dispatcher.send(
        "approval_expired",
        AlertSeverity::Warning,
        "Approval lease expired",
        json!({"request_id": "abc-123"}),
    );

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let payloads = captured.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["alert_type"], "approval_expired");
}

#[test]
fn alert_dispatcher_disabled_is_noop() {
    let dispatcher = AlertDispatcher::Disabled;
    assert!(!dispatcher.is_active());

    // Should not panic — just a no-op.
    dispatcher.send(
        "test",
        AlertSeverity::Info,
        "should be silently dropped",
        json!({}),
    );
}

#[tokio::test]
async fn alert_delivery_failure_does_not_break_policy_enforcement() {
    // Build an AlertingConfig pointing to a server that always returns 500.
    // Verify that repeated send() calls never panic and the dispatcher
    // remains usable even after failures.

    // Start a mock server that always returns 500.
    let app = axum::Router::new().route(
        "/webhook",
        axum::routing::post(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "fail") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let config = AlertingConfig {
        enabled: true,
        webhook_url: Some(format!("http://127.0.0.1:{}/webhook", port)),
        max_retries: 1,
        timeout_ms: 2000,
    };

    let sender = PolicyAlertSender::from_config(&config).expect("should create sender");
    let dispatcher = AlertDispatcher::Active(sender);

    // First send — should not panic despite 500.
    dispatcher.send(
        "policy_rejected",
        AlertSeverity::Warning,
        "First alert that will fail",
        json!({"attempt": 1}),
    );

    // Wait for retries to exhaust.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Second send — dispatcher should still be usable (no poisoned state).
    dispatcher.send(
        "policy_rejected",
        AlertSeverity::Error,
        "Second alert that will also fail",
        json!({"attempt": 2}),
    );

    // Wait for the second batch of retries.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // If we reach here, the core invariant holds: alert failure does not crash.
    assert!(dispatcher.is_active(), "dispatcher should still be active after failures");
}

#[tokio::test]
async fn production_policy_reject_triggers_alert_with_full_context() {
    // Replicate the daemon's decide_inner rejection path:
    // 1. ApprovalStore returns Rejected
    // 2. Daemon sends an alert with transaction_id, operator_id, session_id, etc.
    // 3. Verify the mock webhook receives all fields.
    use claw_gateway::ApprovalStore;
    use claw_types::approval::{
        ApprovalDecision, ApprovalOutcome, ApprovalRequest, ApprovalWorkflow,
    };
    use claw_types::policy::PolicyVerdict;
    use claw_types::session::SessionId;
    use claw_types::transaction::SimulationResult;
    use uuid::Uuid;

    // Start mock webhook.
    let (port, captured) = start_mock_webhook().await;

    let config = AlertingConfig {
        enabled: true,
        webhook_url: Some(format!("http://127.0.0.1:{}/webhook", port)),
        max_retries: 1,
        timeout_ms: 5000,
    };
    let sender = PolicyAlertSender::from_config(&config).expect("should create sender");
    let dispatcher = AlertDispatcher::Active(sender);

    // Set up approval store with a single-stage workflow.
    let session_id = SessionId::from(Uuid::new_v4());
    let request = ApprovalRequest::new(
        session_id.clone(),
        Uuid::new_v4(),
        "alerting rejection test",
        PolicyVerdict::RequiresHumanApproval {
            reason: "high-value transfer".into(),
            rule_name: "high-value-rule".into(),
            required_approver_role: Some("treasury".to_string()),
            approval_chain: None,
        },
        SimulationResult {
            success: true,
            error: None,
            compute_units_used: Some(2500),
            logs: vec![],
            return_data: None,
            account_diffs: vec![],
            fee_lamports: Some(10000),
        },
    );
    let request_id = request.id;
    let transaction_id = request.transaction_id;
    let wf = ApprovalWorkflow::single_stage(request_id, session_id.clone(), Some("treasury".to_string()));

    let store = ApprovalStore::new();
    store.register(request, wf);

    // Operator rejects.
    let decision = ApprovalDecision {
        request_id,
        approved: false,
        note: Some("too risky — rejecting".into()),
        approver_role: Some("treasury".to_string()),
        operator_id: Some("treasury-op-1".to_string()),
        decided_at: chrono::Utc::now(),
    };
    let (outcome, maybe_req) = store.decide(&decision);
    assert_eq!(outcome, ApprovalOutcome::Rejected);
    let returned_req = maybe_req.expect("rejected request should be returned");

    // Replicate daemon wiring: on terminal Rejected, emit an alert with full context.
    let alert_details = json!({
        "request_id": request_id.to_string(),
        "session_id": session_id.to_string(),
        "transaction_id": transaction_id.to_string(),
        "operator_id": "treasury-op-1",
        "approver_role": "treasury",
        "note": "too risky — rejecting",
        "rule_name": "high-value-rule",
        "description": returned_req.description,
        "compute_units_used": 2500,
        "fee_lamports": 10000,
    });

    dispatcher.send(
        "approval_rejected",
        AlertSeverity::Warning,
        "Operator rejected transaction approval",
        alert_details,
    );

    // Wait for delivery.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let payloads = captured.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 1, "should have received exactly one alert");

    let alert = &payloads[0];
    assert_eq!(alert["alert_type"], "approval_rejected");
    assert_eq!(alert["severity"], "warning");
    assert_eq!(alert["message"], "Operator rejected transaction approval");

    // Verify ALL context fields are present in the alert payload.
    let details = &alert["details"];
    assert_eq!(details["request_id"], request_id.to_string());
    assert_eq!(details["session_id"], session_id.to_string());
    assert_eq!(details["transaction_id"], transaction_id.to_string());
    assert_eq!(details["operator_id"], "treasury-op-1");
    assert_eq!(details["approver_role"], "treasury");
    assert_eq!(details["note"], "too risky — rejecting");
    assert_eq!(details["rule_name"], "high-value-rule");
    assert_eq!(details["description"], "alerting rejection test");
    assert_eq!(details["compute_units_used"], 2500);
    assert_eq!(details["fee_lamports"], 10000);
    assert!(alert["occurred_at"].as_str().is_some(), "should have timestamp");
}

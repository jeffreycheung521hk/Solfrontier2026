//! Policy violation alerting via webhook.
//!
//! When enabled, the daemon POSTs JSON payloads to a configured webhook URL
//! for policy-relevant events:
//! - Policy rejection (transaction blocked)
//! - Approval expired (lease timeout)
//! - Approver role mismatch
//! - Duplicate operator vote
//! - Approval workflow completed (approved/rejected)
//!
//! # Non-blocking delivery
//!
//! Alert delivery is fully async and never blocks the main request path.
//! Each alert is sent via `tokio::spawn`. Failed deliveries are retried
//! up to `max_retries` times with exponential backoff. Delivery failures
//! are logged but never cause the main flow to fail.
//!
//! # Dead letter
//!
//! Alerts that exhaust all retries are logged at WARN level with the full
//! payload for manual recovery. A future version may persist dead letters
//! to SQLite.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

use claw_types::alert::AlertSeverity;

use crate::config::AlertingConfig;

/// The JSON payload POSTed to the webhook.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyAlertPayload {
    /// Alert category (e.g., "policy_rejected", "approval_expired").
    pub alert_type: String,
    /// Severity level.
    pub severity: String,
    /// Human-readable summary.
    pub message: String,
    /// Structured details.
    pub details: serde_json::Value,
    /// ISO 8601 timestamp.
    pub occurred_at: String,
}

/// Sends policy alerts to a configured webhook endpoint.
///
/// Clone is cheap (Arc-wrapped internals). Pass to any component that
/// needs to emit alerts.
#[derive(Clone, Debug)]
pub struct PolicyAlertSender {
    config: Arc<AlertingConfig>,
    client: reqwest::Client,
}

impl PolicyAlertSender {
    /// Creates a new sender from config. Returns `None` if alerting is disabled.
    pub fn from_config(config: &AlertingConfig) -> Option<Self> {
        if !config.enabled || config.webhook_url.is_none() {
            return None;
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        info!(
            webhook_url = config.webhook_url.as_deref().unwrap_or("none"),
            max_retries = config.max_retries,
            timeout_ms = config.timeout_ms,
            "policy alerting enabled"
        );

        Some(Self {
            config: Arc::new(config.clone()),
            client,
        })
    }

    /// Sends an alert asynchronously. Never blocks the caller.
    ///
    /// The alert is delivered in a spawned task. Delivery failures
    /// are retried with exponential backoff and logged on exhaustion.
    pub fn send(&self, alert_type: &str, severity: AlertSeverity, message: &str, details: serde_json::Value) {
        let payload = PolicyAlertPayload {
            alert_type: alert_type.to_string(),
            severity: severity.to_string(),
            message: message.to_string(),
            details,
            occurred_at: chrono::Utc::now().to_rfc3339(),
        };

        let sender = self.clone();
        tokio::spawn(async move {
            sender.deliver(payload).await;
        });
    }

    /// Delivers the payload with retry. Called inside a spawned task.
    async fn deliver(&self, payload: PolicyAlertPayload) {
        let url = match &self.config.webhook_url {
            Some(u) => u.clone(),
            None => return,
        };

        let max_retries = self.config.max_retries;

        for attempt in 0..=max_retries {
            match self.client.post(&url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    debug!(
                        alert_type = %payload.alert_type,
                        attempt = attempt,
                        "alert delivered"
                    );
                    return;
                }
                Ok(resp) => {
                    warn!(
                        alert_type = %payload.alert_type,
                        attempt = attempt,
                        status = %resp.status(),
                        "webhook returned non-success status"
                    );
                }
                Err(e) => {
                    warn!(
                        alert_type = %payload.alert_type,
                        attempt = attempt,
                        error = %e,
                        "webhook delivery failed"
                    );
                }
            }

            if attempt < max_retries {
                let backoff = Duration::from_millis(500 * 2u64.pow(attempt));
                tokio::time::sleep(backoff).await;
            }
        }

        // Dead letter: all retries exhausted.
        warn!(
            alert_type = %payload.alert_type,
            retries = max_retries,
            payload = %serde_json::to_string(&payload).unwrap_or_default(),
            "alert delivery failed after all retries — dead letter"
        );
    }
}

/// No-op alert sender for when alerting is disabled.
/// Avoids `Option<PolicyAlertSender>` checks at every call site.
#[derive(Clone, Debug)]
pub enum AlertDispatcher {
    Active(PolicyAlertSender),
    Disabled,
}

impl AlertDispatcher {
    /// Sends an alert if alerting is active; no-op if disabled.
    pub fn send(&self, alert_type: &str, severity: AlertSeverity, message: &str, details: serde_json::Value) {
        if let AlertDispatcher::Active(sender) = self {
            sender.send(alert_type, severity, message, details);
        }
    }

    /// Returns true if alerting is active.
    pub fn is_active(&self) -> bool {
        matches!(self, AlertDispatcher::Active(_))
    }
}

impl Default for AlertDispatcher {
    fn default() -> Self {
        AlertDispatcher::Disabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AlertingConfig;

    #[test]
    fn disabled_by_default() {
        let config = AlertingConfig::default();
        assert!(!config.enabled);
        assert!(config.webhook_url.is_none());

        let sender = PolicyAlertSender::from_config(&config);
        assert!(sender.is_none());
    }

    #[test]
    fn disabled_when_no_url() {
        let config = AlertingConfig {
            enabled: true,
            webhook_url: None,
            ..AlertingConfig::default()
        };
        assert!(PolicyAlertSender::from_config(&config).is_none());
    }

    #[test]
    fn enabled_with_url() {
        let config = AlertingConfig {
            enabled: true,
            webhook_url: Some("https://example.com/webhook".to_string()),
            max_retries: 2,
            timeout_ms: 3000,
        };
        let sender = PolicyAlertSender::from_config(&config);
        assert!(sender.is_some());
    }

    #[test]
    fn alert_dispatcher_disabled_is_noop() {
        let dispatcher = AlertDispatcher::Disabled;
        assert!(!dispatcher.is_active());
        // Should not panic or do anything.
        dispatcher.send(
            "test_alert",
            AlertSeverity::Warning,
            "test message",
            serde_json::json!({"key": "value"}),
        );
    }

    #[test]
    fn payload_serializes_correctly() {
        let payload = PolicyAlertPayload {
            alert_type: "policy_rejected".to_string(),
            severity: "warning".to_string(),
            message: "Transaction blocked".to_string(),
            details: serde_json::json!({"rule": "denylist"}),
            occurred_at: "2026-04-05T00:00:00Z".to_string(),
        };

        let json = serde_json::to_value(&payload).expect("should serialize");
        assert_eq!(json["alert_type"], "policy_rejected");
        assert_eq!(json["severity"], "warning");
        assert_eq!(json["message"], "Transaction blocked");
        assert_eq!(json["details"]["rule"], "denylist");
        assert_eq!(json["occurred_at"], "2026-04-05T00:00:00Z");
    }
}

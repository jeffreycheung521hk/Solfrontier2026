//! Alert types emitted by the risk engine and monitoring subsystem.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::session::SessionId;

/// An alert emitted when a monitoring condition is triggered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id:          Uuid,
    /// The session that was active when the alert fired, if any.
    pub session_id:  Option<SessionId>,
    pub alert_type:  AlertType,
    pub severity:    AlertSeverity,
    /// Human-readable summary.
    pub message:     String,
    /// Structured details (e.g., the account, transaction signature, etc.).
    pub payload:     Value,
    pub occurred_at: DateTime<Utc>,
    pub acknowledged: bool,
}

/// The category of alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertType {
    /// An unexpected account balance change was detected.
    AccountBalanceChanged,
    /// A transaction from/to a monitored address was detected.
    TransactionDetected,
    /// A suspicious instruction pattern was detected.
    SuspiciousInstruction,
    /// A monitored program account changed.
    ProgramAccountChanged,
    /// The policy engine blocked a transaction.
    PolicyRejection,
    /// A Solana RPC endpoint failed health checks.
    RpcEndpointUnhealthy,
    /// A websocket subscription was lost and could not reconnect.
    SubscriptionLost,
    /// The system-level (daemon) health degraded.
    SystemHealth,
}

/// Alert severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    /// Informational; no action required.
    Info,
    /// Something to be aware of; may require investigation.
    Warning,
    /// Action required; something is actively wrong.
    Error,
    /// Immediate attention required; potential fund loss.
    Critical,
}

impl std::fmt::Display for AlertSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlertSeverity::Info     => write!(f, "info"),
            AlertSeverity::Warning  => write!(f, "warning"),
            AlertSeverity::Error    => write!(f, "error"),
            AlertSeverity::Critical => write!(f, "critical"),
        }
    }
}

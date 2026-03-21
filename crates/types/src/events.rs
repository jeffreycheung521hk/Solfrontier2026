//! The gateway's internal event bus types.
//!
//! `GatewayEvent` is the single enum that flows through the broadcast channel.
//! Subscribers filter by pattern-matching on variants. Every significant
//! action in the system produces at least one event.
//!
//! Design principle: events are facts, not commands. They describe what
//! happened, not what should happen next. Reactions to events are the
//! responsibility of event consumers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::{AgentRole, AgentResponseStatus};
use crate::alert::Alert;
use crate::policy::PolicyVerdict;
use crate::session::{SessionId, SessionState};
use crate::solana::SolanaEvent;
use crate::transaction::TransactionStatus;

/// The top-level event enum for the gateway's event bus.
///
/// The bus is a `tokio::sync::broadcast` channel. Slow consumers will
/// be dropped from the broadcast (lagged). Each consumer should maintain
/// its own buffer if it cannot keep up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum GatewayEvent {
    // ── Session lifecycle ────────────────────────────────────────────────────
    SessionOpened(SessionOpenedEvent),
    SessionStateChanged(SessionStateChangedEvent),
    SessionClosed(SessionClosedEvent),

    // ── Agent lifecycle ──────────────────────────────────────────────────────
    AgentTaskStarted(AgentTaskEvent),
    AgentTaskCompleted(AgentTaskEvent),
    AgentTaskFailed(AgentTaskFailedEvent),

    // ── Tool execution ───────────────────────────────────────────────────────
    ToolInvoked(ToolLifecycleEvent),
    ToolCompleted(ToolLifecycleEvent),
    ToolFailed(ToolLifecycleEvent),

    // ── Transaction pipeline ─────────────────────────────────────────────────
    TransactionProposed(TransactionLifecycleEvent),
    TransactionSimulated(TransactionLifecycleEvent),
    PolicyEvaluated(PolicyEvaluatedEvent),
    ApprovalRequested(ApprovalLifecycleEvent),
    ApprovalReceived(ApprovalLifecycleEvent),
    TransactionSigned(TransactionLifecycleEvent),
    TransactionSent(TransactionLifecycleEvent),
    TransactionConfirmed(TransactionLifecycleEvent),
    TransactionFailed(TransactionFailedEvent),

    // ── External wallet signing ──────────────────────────────────────────────
    /// Emitted when a transaction is awaiting an external wallet's signature.
    WalletSignatureRequested(WalletSignatureEvent),
    /// Emitted when a signed transaction is received from an external wallet.
    WalletSignatureReceived(WalletSignatureEvent),
    /// Emitted when a wallet-signed transaction fails verification.
    WalletSignatureRejected(WalletSignatureEvent),
    /// Emitted when a pending wallet signature request expires (TTL exceeded).
    WalletSignatureExpired(WalletSignatureEvent),

    // ── Solana chain events ──────────────────────────────────────────────────
    SolanaEvent(SolanaEvent),

    // ── System events ────────────────────────────────────────────────────────
    AlertEmitted(Alert),
    HealthCheckCompleted(HealthCheckEvent),
    ConfigReloaded(ConfigReloadedEvent),
    DaemonShuttingDown,
}

/// An event carries a standard header so all consumers can correlate and
/// timestamp events without needing to inspect inner types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventHeader {
    pub id:             Uuid,
    pub correlation_id: Uuid,
    pub session_id:     Option<SessionId>,
    pub occurred_at:    DateTime<Utc>,
}

impl EventHeader {
    pub fn new(session_id: Option<SessionId>) -> Self {
        let id = Uuid::new_v4();
        Self {
            id,
            correlation_id: id,
            session_id,
            occurred_at: chrono::Utc::now(),
        }
    }

    pub fn with_correlation(mut self, correlation_id: Uuid) -> Self {
        self.correlation_id = correlation_id;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOpenedEvent {
    pub header:     EventHeader,
    pub session_id: SessionId,
    pub agent_role: AgentRole,
    pub channel:    String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStateChangedEvent {
    pub header:     EventHeader,
    pub session_id: SessionId,
    pub old_state:  SessionState,
    pub new_state:  SessionState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClosedEvent {
    pub header:     EventHeader,
    pub session_id: SessionId,
    pub reason:     String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTaskEvent {
    pub header:         EventHeader,
    pub session_id:     SessionId,
    pub task_id:        Uuid,
    pub agent_role:     AgentRole,
    pub command_text:   String,
    pub status:         AgentResponseStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTaskFailedEvent {
    pub header:     EventHeader,
    pub session_id: SessionId,
    pub task_id:    Uuid,
    pub agent_role: AgentRole,
    pub error:      String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolLifecycleEvent {
    pub header:      EventHeader,
    pub session_id:  SessionId,
    pub tool_name:   String,
    pub trace_id:    Uuid,
    pub duration_ms: Option<u64>,
    pub error:       Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionLifecycleEvent {
    pub header:          EventHeader,
    pub session_id:      SessionId,
    pub transaction_id:  Uuid,
    pub wallet_pubkey:   String,
    pub status:          TransactionStatus,
    pub signature:       Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyEvaluatedEvent {
    pub header:         EventHeader,
    pub session_id:     SessionId,
    pub transaction_id: Uuid,
    pub verdict:        PolicyVerdict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalLifecycleEvent {
    pub header:          EventHeader,
    pub session_id:      SessionId,
    pub request_id:      Uuid,
    pub transaction_id:  Uuid,
    pub approved:        Option<bool>, // None when the request is first emitted
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionFailedEvent {
    pub header:          EventHeader,
    pub session_id:      SessionId,
    pub transaction_id:  Uuid,
    pub wallet_pubkey:   String,
    pub error:           String,
    pub at_stage:        TransactionStatus,
}

/// Event for external wallet signature lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletSignatureEvent {
    pub header:          EventHeader,
    pub session_id:      SessionId,
    pub request_id:      Uuid,
    pub transaction_id:  Uuid,
    pub wallet_pubkey:   String,
    pub error:           Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckEvent {
    pub header:    EventHeader,
    pub component: String,
    pub healthy:   bool,
    pub message:   Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigReloadedEvent {
    pub header:          EventHeader,
    pub changed_sections: Vec<String>,
}

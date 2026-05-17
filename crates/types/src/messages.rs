//! The unified message schema for all channel adapters.
//!
//! Every channel (CLI, Telegram, HTTP, etc.) translates its native format
//! into `InboundMessage` before passing it to the gateway. The gateway
//! responds with `OutboundMessage`. This normalization means the agent
//! runtime and tool system never need to know which channel they're
//! talking through.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::{AgentCommand, AgentResponse};
use crate::alert::Alert;
use crate::session::SessionId;
use crate::transaction::{TransactionProposal, SimulationResult};
use crate::policy::PolicyVerdict;

/// An inbound message arriving from a channel into the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id:             Uuid,
    /// Links this message to a causally related chain of events.
    pub correlation_id: Uuid,
    pub session_id:     SessionId,
    /// Which channel adapter delivered this message.
    pub channel:        String,
    pub received_at:    DateTime<Utc>,
    pub content:        MessageContent,
    pub auth:           AuthContext,
}

impl InboundMessage {
    pub fn new(session_id: SessionId, channel: impl Into<String>, content: MessageContent) -> Self {
        let id = Uuid::new_v4();
        Self {
            id,
            correlation_id: id, // by default the message is its own correlation root
            session_id,
            channel: channel.into(),
            received_at: chrono::Utc::now(),
            content,
            auth: AuthContext::Local,
        }
    }
}

/// The content variant of an inbound message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    /// A user-facing text command or question.
    Command(AgentCommand),
    /// A response to an approval request.
    Approval(ApprovalResponse),
    /// A system-level control message (e.g., shutdown, reload config).
    SystemControl(SystemControlMessage),
}

/// Authentication context for an inbound message.
/// V1: Local-only (the operator running the CLI). Future: token-based, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthContext {
    /// The message came from the local operator (CLI or local API).
    Local,
    /// The message was authenticated with a bearer token.
    Token { token_id: String },
}

/// An outbound message sent from the gateway to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id:             Uuid,
    pub correlation_id: Uuid,
    pub session_id:     SessionId,
    pub sent_at:        DateTime<Utc>,
    pub content:        OutboundContent,
}

/// The content variant of an outbound message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundContent {
    /// A plain-text or markdown response to the operator.
    Text(String),
    /// A structured agent response (includes tool call summary).
    AgentResponse(AgentResponse),
    /// A request for the operator to approve or reject a transaction.
    ApprovalRequest(TransactionApprovalRequest),
    /// A monitoring alert.
    Alert(Alert),
    /// A partial output chunk from a streaming tool.
    StreamChunk(StreamChunk),
    /// An error response.
    Error(ErrorResponse),
}

/// A human-facing approval request for a transaction.
/// The operator must respond with an `ApprovalResponse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionApprovalRequest {
    /// Unique ID for this approval request.
    pub request_id:       Uuid,
    pub proposal:         TransactionProposal,
    pub simulation_result: Option<SimulationResult>,
    pub policy_verdict:   PolicyVerdict,
    /// Plain-language explanation of what this transaction will do.
    pub explanation:      String,
    /// Time (ms) the operator has to respond before the request expires.
    pub expires_in_ms:    u64,
}

/// The operator's response to an approval request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: Uuid,
    pub approved:   bool,
    pub note:       Option<String>,
}

/// A partial output chunk for streaming tool responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub sequence: u32,
    pub tool_name: String,
    pub text:     String,
    pub is_final: bool,
}

/// A structured error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code:    String,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

/// System control messages from the operator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemControlMessage {
    Shutdown,
    ReloadConfig,
    ListSessions,
    KillSession { session_id: SessionId },
    ListSubscriptions,
    ListWallets,
}

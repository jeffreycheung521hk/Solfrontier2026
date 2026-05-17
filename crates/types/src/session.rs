//! Session identity and lifecycle types.
//!
//! A `Session` is the top-level isolation unit. Every agent run, tool call,
//! and transaction is scoped to a session. Sessions are created by channel
//! adapters and destroyed when the channel closes or via explicit termination.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

use crate::agent::AgentRole;

/// Opaque session identifier. Use `SessionId::new()` to create.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Creates a new, random session ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the underlying UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for SessionId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

/// The lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Session is active and accepting commands.
    Active,
    /// Session is being shut down gracefully.
    Closing,
    /// Session has ended. This is a terminal state.
    Closed,
    /// Session is in a degraded state (e.g., LLM unavailable).
    Degraded,
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionState::Active => write!(f, "active"),
            SessionState::Closing => write!(f, "closing"),
            SessionState::Closed => write!(f, "closed"),
            SessionState::Degraded => write!(f, "degraded"),
        }
    }
}

/// A summary view of a session, suitable for API responses and audit records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id:          SessionId,
    pub state:       SessionState,
    pub agent_role:  AgentRole,
    pub channel:     String,
    pub created_at:  DateTime<Utc>,
    pub closed_at:   Option<DateTime<Utc>>,
    pub message_count: u64,
    pub tool_call_count: u64,
}

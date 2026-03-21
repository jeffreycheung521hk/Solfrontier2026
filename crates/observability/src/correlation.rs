//! Correlation ID propagation.
//!
//! Every significant operation carries a `CorrelationId` that links it
//! back to the originating inbound message. This enables tracing a complete
//! request chain across the audit log, tool traces, and Solana transaction
//! records.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// A correlation ID that links events and records across a causal chain.
/// Typically set from `InboundMessage.correlation_id` at the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CorrelationId(Uuid);

impl CorrelationId {
    /// Creates a new root correlation ID (for a new inbound message).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps an existing UUID as a correlation ID.
    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for CorrelationId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

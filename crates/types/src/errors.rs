//! Top-level error hierarchy for ClawSolana.
//!
//! Each crate defines its own fine-grained error enum using `thiserror`,
//! then maps into `ClawError` at crate boundaries when errors cross layers.
//! This keeps internal error detail rich while keeping the inter-crate
//! contract stable.

use thiserror::Error;

/// The top-level error type returned across crate boundaries.
#[derive(Debug, Error)]
pub enum ClawError {
    /// Solana RPC-layer failure.
    #[error("RPC error: {0}")]
    Rpc(String),

    /// Transaction simulation returned an error.
    #[error("simulation failed: {reason}")]
    SimulationFailed { reason: String },

    /// Transaction was blocked by the policy engine.
    #[error("policy rejected transaction: {reason}")]
    PolicyRejected { reason: String },

    /// Signing was requested but no valid signer is available.
    #[error("signing failed: {0}")]
    SigningFailed(String),

    /// Human approval is required before proceeding.
    #[error("human approval required")]
    HumanApprovalRequired,

    /// Human rejected the proposed action.
    #[error("human rejected action: {reason}")]
    HumanRejected { reason: String },

    /// A requested session does not exist or has been closed.
    #[error("session not found: {session_id}")]
    SessionNotFound { session_id: String },

    /// A requested tool does not exist in the registry.
    #[error("tool not found: {name}")]
    ToolNotFound { name: String },

    /// The caller lacks the required capability.
    #[error("permission denied: requires capability {capability}")]
    PermissionDenied { capability: String },

    /// Tool input failed JSON Schema validation.
    #[error("invalid tool input: {reason}")]
    InvalidToolInput { reason: String },

    /// A tool call timed out.
    #[error("tool timeout: {tool_name} exceeded {timeout_ms}ms")]
    ToolTimeout { tool_name: String, timeout_ms: u64 },

    /// Persistence-layer failure.
    #[error("storage error: {0}")]
    Storage(String),

    /// Configuration is invalid or missing required fields.
    #[error("configuration error: {0}")]
    Config(String),

    /// LLM provider returned an error.
    #[error("LLM error: {0}")]
    Llm(String),

    /// A channel-level communication error.
    #[error("channel error: {0}")]
    Channel(String),

    /// Catch-all for unexpected internal errors.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ClawError {
    /// Returns `true` if this error represents a security boundary violation
    /// that should always be logged at WARN or higher.
    pub fn is_security_relevant(&self) -> bool {
        matches!(
            self,
            ClawError::PolicyRejected { .. }
                | ClawError::PermissionDenied { .. }
                | ClawError::HumanRejected { .. }
                | ClawError::SigningFailed(_)
        )
    }
}

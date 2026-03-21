//! Tool system error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {name}")]
    NotFound { name: String },

    #[error("invalid input: {reason}")]
    InvalidInput { reason: String },

    /// The dispatcher's capability set does not include all required capabilities.
    #[error("permission denied for tool '{tool_name}': missing capabilities: {}", missing_capabilities.join(", "))]
    PermissionDenied {
        tool_name:            String,
        missing_capabilities: Vec<String>,
    },

    #[error("tool timed out after {ms}ms")]
    Timeout { ms: u64 },

    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Solana error: {0}")]
    Solana(#[from] claw_solana_core::errors::SolanaError),
}

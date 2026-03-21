//! Agent runtime error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("LLM error: {0}")]
    Llm(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("session error: {0}")]
    Session(String),

    #[error("context window exceeded")]
    ContextWindowExceeded,

    #[error("max iterations exceeded")]
    MaxIterationsExceeded,

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("task cancelled")]
    Cancelled,
}

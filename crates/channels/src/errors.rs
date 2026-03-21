//! Channel error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("channel closed")]
    Closed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("channel {id} not found")]
    NotFound { id: String },
}

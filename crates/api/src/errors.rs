//! API error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("config error: {0}")]
    Config(String),

    #[error("failed to bind address: {0}")]
    Bind(String),

    #[error("internal server error: {0}")]
    Internal(String),
}

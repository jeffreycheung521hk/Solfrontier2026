//! Gateway error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("startup error: {0}")]
    Startup(String),

    #[error("session error: {0}")]
    Session(String),
}

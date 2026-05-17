//! Solana-core error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SolanaError {
    #[error("RPC client error: {0}")]
    Client(String),

    #[error("RPC pool exhausted — all endpoints unhealthy")]
    PoolExhausted,

    #[error("invalid pubkey: {0}")]
    InvalidPubkey(String),

    #[error("account not found: {pubkey}")]
    AccountNotFound { pubkey: String },

    #[error("simulation error: {0}")]
    Simulation(String),

    #[error("transaction error: {inner}")]
    Transaction { inner: String },

    #[error("blockhash expired or unavailable")]
    BlockhashUnavailable,

    #[error("account decode error: {0}")]
    Decode(String),

    #[error("websocket subscription error: {0}")]
    Subscription(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("timeout after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
}

impl From<solana_client::client_error::ClientError> for SolanaError {
    fn from(e: solana_client::client_error::ClientError) -> Self {
        SolanaError::Client(e.to_string())
    }
}

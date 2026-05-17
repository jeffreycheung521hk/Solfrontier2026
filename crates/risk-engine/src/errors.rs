//! Risk engine error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiskError {
    #[error("policy configuration error: {0}")]
    Config(String),

    #[error("evaluation error: {0}")]
    Evaluation(String),
}

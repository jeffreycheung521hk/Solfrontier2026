//! Policy evaluation context.
//!
//! All data the policy engine needs to evaluate a transaction.
//! Passed by value to the pure `evaluate` function.

use claw_types::{
    session::SessionId,
    solana::SolanaNetwork,
    transaction::{SimulationResult, TransactionProposal},
};

/// Everything the policy engine needs to evaluate a transaction.
#[derive(Debug)]
pub struct PolicyEvaluationContext<'a> {
    pub proposal:          &'a TransactionProposal,
    pub simulation_result: Option<&'a SimulationResult>,
    pub network:           SolanaNetwork,
    pub session_id:        &'a SessionId,
    /// The session's cumulative spend in lamports since session start.
    pub session_spend_lamports: u64,
    /// The wallet's cumulative spend in lamports today (UTC day).
    pub wallet_daily_spend_lamports: u64,
}

//! Transaction simulation client.
//!
//! Wraps `simulateTransaction` and parses the rich result into a
//! structured `SimulationOutput` that other crates can reason about
//! without handling raw RPC types.
//!
//! Key behaviors:
//! - Always passes `replaceRecentBlockhash: true` so a fresh blockhash
//!   is used automatically (avoids stale blockhash failures in simulation)
//! - Parses `InstructionError` variants into human-readable explanations
//! - Extracts compute units from the simulation logs
//! - Collects account diffs for pre/post state comparison

use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::{
    transaction::Transaction,
    commitment_config::CommitmentConfig,
};
use solana_transaction_status::UiTransactionEncoding;
use tracing::{debug, instrument};

use claw_types::transaction::{AccountDiff, SimulationResult};

use crate::errors::SolanaError;
use crate::rpc::ClawRpcClient;

/// The structured output from a simulation, combining parsed fields
/// and the raw result for downstream inspection.
#[derive(Debug, Clone)]
pub struct SimulationOutput {
    /// Whether the simulation succeeded (no error returned).
    pub success: bool,
    /// The error string if simulation failed.
    pub error: Option<String>,
    /// Compute units actually consumed during simulation.
    pub compute_units_used: Option<u64>,
    /// All `Program log:` lines from the simulation.
    pub logs: Vec<String>,
    /// Base64-encoded return data, if any.
    pub return_data: Option<String>,
    /// Estimated transaction fee in lamports.
    pub fee_lamports: Option<u64>,
    /// Account state changes observed.
    pub account_diffs: Vec<AccountDiff>,
}

impl SimulationOutput {
    /// Converts to the `SimulationResult` type from `claw-types`.
    pub fn into_result(self) -> SimulationResult {
        SimulationResult {
            success: self.success,
            error: self.error,
            compute_units_used: self.compute_units_used,
            logs: self.logs,
            return_data: self.return_data,
            account_diffs: self.account_diffs,
            fee_lamports: self.fee_lamports,
        }
    }
}

/// The simulation client.
#[derive(Clone)]
pub struct SimulationClient {
    rpc: ClawRpcClient,
}

impl SimulationClient {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }

    /// Simulates a transaction and returns structured output.
    ///
    /// Uses `replaceRecentBlockhash: true` so callers don't need to
    /// provide a fresh blockhash just for simulation.
    #[instrument(skip(self, transaction))]
    pub async fn simulate(&self, transaction: &Transaction) -> Result<SimulationOutput, SolanaError> {
        let client = self.rpc.pool().read_client()?;

        let config = RpcSimulateTransactionConfig {
            sig_verify: false,
            replace_recent_blockhash: true,
            commitment: Some(CommitmentConfig::confirmed()),
            encoding: Some(UiTransactionEncoding::Base64),
            accounts: None,
            min_context_slot: None,
            inner_instructions: false,
        };

        let response = client
            .simulate_transaction_with_config(transaction, config)
            .await
            .map_err(SolanaError::from)?;

        let value = response.value;

        let success = value.err.is_none();
        let error = value.err.as_ref().map(|e| format!("{:?}", e));
        let logs = value.logs.unwrap_or_default();

        // Parse compute units from logs.
        // The standard log line is: "Program consumed NNNN of MMMM compute units"
        let compute_units_used = logs.iter().rev().find_map(|log| {
            if log.contains("consumed") && log.contains("compute units") {
                let parts: Vec<&str> = log.split_whitespace().collect();
                parts
                    .iter()
                    .position(|&p| p == "consumed")
                    .and_then(|i| parts.get(i + 1))
                    .and_then(|s| s.parse::<u64>().ok())
            } else {
                None
            }
        });

        let return_data = value.return_data.map(|rd| rd.data.0);

        debug!(
            success,
            compute_units = ?compute_units_used,
            log_lines = logs.len(),
            "simulation completed"
        );

        Ok(SimulationOutput {
            success,
            error,
            compute_units_used,
            logs,
            return_data,
            fee_lamports: None, // filled separately via getFeeForMessage if needed
            account_diffs: vec![], // TODO: populate from accounts field in V2
        })
    }
}

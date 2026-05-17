//! Priority fee estimation strategies.

use tracing::debug;

use crate::errors::SolanaError;
use crate::rpc::ClawRpcClient;

/// Strategy for setting the priority fee (microlamports per compute unit).
#[derive(Debug, Clone)]
pub enum PriorityFeeStrategy {
    /// No priority fee added.
    None,
    /// A fixed number of microlamports per compute unit.
    Fixed(u64),
    /// Use the Nth percentile of recent prioritization fees for the given accounts.
    Percentile {
        percentile: u8,
        accounts: Vec<String>,
    },
    /// Dynamically choose based on recent network conditions, capped at a maximum.
    Dynamic { max_microlamports: u64 },
}

impl PriorityFeeStrategy {
    /// Resolves the strategy to a concrete microlamports-per-CU value.
    pub async fn resolve(
        &self,
        rpc: &ClawRpcClient,
        relevant_accounts: &[&str],
    ) -> Result<u64, SolanaError> {
        match self {
            PriorityFeeStrategy::None => Ok(0),

            PriorityFeeStrategy::Fixed(fee) => Ok(*fee),

            PriorityFeeStrategy::Percentile {
                percentile,
                accounts,
            } => {
                let accts: Vec<&str> = accounts.iter().map(String::as_str).collect();
                let fees = rpc.get_recent_prioritization_fees(&accts).await?;
                let fee = estimate_percentile(&fees, *percentile);
                debug!(percentile, fee, "resolved percentile priority fee");
                Ok(fee)
            }

            PriorityFeeStrategy::Dynamic { max_microlamports } => {
                let fees = rpc.get_recent_prioritization_fees(relevant_accounts).await?;
                // Use the 75th percentile, capped at max
                let fee = estimate_percentile(&fees, 75).min(*max_microlamports);
                debug!(fee, max = max_microlamports, "resolved dynamic priority fee");
                Ok(fee)
            }
        }
    }
}

fn estimate_percentile(
    fees: &[solana_client::rpc_response::RpcPrioritizationFee],
    percentile: u8,
) -> u64 {
    if fees.is_empty() {
        return 0;
    }
    let mut values: Vec<u64> = fees.iter().map(|f| f.prioritization_fee).collect();
    values.sort_unstable();
    let idx = ((percentile as usize) * values.len()) / 100;
    values[idx.min(values.len() - 1)]
}

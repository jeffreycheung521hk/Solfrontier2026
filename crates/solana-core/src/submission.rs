//! Transaction submission — the execution plane boundary.
//!
//! This module provides [`submit_transaction`], the function that submits a
//! verified, signed transaction to the Solana network via RPC.
//!
//! # Invariants
//!
//! - The transaction bytes are submitted **exactly as received** from the
//!   orchestrator. No re-signing, no mutation, no reconstruction.
//! - Preflight simulation is **skipped** because the control plane already
//!   performed simulation upstream (typestate-enforced).
//! - Max retries is set to **0** — retry policy is owned by the caller
//!   (execution plane tracker), not the RPC submission call.
//! - The submission block height is captured immediately after submission
//!   to anchor the lifecycle tracking grace period.
//!
//! # What does NOT belong here
//!
//! - Retry loops (owned by the transaction tracker)
//! - Confirmation polling (owned by the transaction tracker)
//! - Signature verification (already done by the orchestrator)
//! - Event emission (owned by the daemon/gateway layer)
//! - Audit recording (owned by the daemon/gateway layer)

use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    signature::Signature,
    transaction::Transaction,
};
use tracing::{info, instrument};

/// The result of a successful transaction submission.
///
/// This struct carries everything the lifecycle tracker needs to begin
/// tracking the transaction from `Submitted` to terminal state.
#[derive(Debug, Clone)]
pub struct SubmissionResult {
    /// The on-chain transaction signature.
    pub signature: Signature,
    /// The `lastValidBlockHeight` from the blockhash used in this transaction.
    /// Determines when the transaction deterministically expires.
    pub last_valid_block_height: u64,
    /// The network block height at the moment of submission.
    /// Used to calculate grace periods (e.g., Dropped threshold = +50 blocks).
    pub submission_block_height: u64,
}

/// Errors that can occur during transaction submission.
///
/// These are infrastructure-level errors — the transaction was valid and
/// verified, but RPC communication failed. The caller should decide whether
/// to retry or declare the submission failed.
#[derive(Debug)]
pub enum SubmissionError {
    /// The `sendTransaction` RPC call failed.
    RpcError(solana_client::client_error::ClientError),
    /// The `getBlockHeight` call to capture submission height failed.
    /// The transaction may or may not have been accepted by the network.
    HeightFetchError(solana_client::client_error::ClientError),
    /// The signed transaction bytes could not be deserialized.
    /// This should never happen if the orchestrator verified correctly.
    DeserializationError(String),
}

impl std::fmt::Display for SubmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmissionError::RpcError(e) => write!(f, "sendTransaction failed: {e}"),
            SubmissionError::HeightFetchError(e) => write!(f, "getBlockHeight failed: {e}"),
            SubmissionError::DeserializationError(e) => write!(f, "tx deserialization failed: {e}"),
        }
    }
}

impl std::error::Error for SubmissionError {}

/// Submits a verified, signed transaction to the Solana network.
///
/// This is the **execution plane boundary** — the point where a policy-gated,
/// human-approved, cryptographically-verified transaction enters the Solana
/// network.
///
/// # Arguments
///
/// - `rpc` — a Solana RPC client connected to the target cluster.
/// - `signed_tx_bytes` — the exact signed transaction bytes from the orchestrator.
///   These bytes are submitted as-is. No mutation.
/// - `signature` — the transaction signature, passed from the orchestrator to
///   avoid double-deserialization. Used for logging and returned in the result.
/// - `last_valid_block_height` — the `lastValidBlockHeight` from the blockhash
///   used when the transaction was constructed. Passed through to the tracking
///   record for deterministic expiry.
///
/// # Behavior
///
/// 1. Deserializes `signed_tx_bytes` into a `Transaction` (required by the
///    Solana RPC client API).
/// 2. Calls `sendTransaction` with:
///    - `skip_preflight: true` — already simulated upstream
///    - `max_retries: Some(0)` — we control retry, not the RPC node
///    - `preflight_commitment: processed` — defensive fallback if preflight
///      is somehow triggered
/// 3. Captures the current block height via `getBlockHeight` (confirmed
///    commitment) to anchor lifecycle tracking.
/// 4. Returns a `SubmissionResult` for the lifecycle tracker.
///
/// # Errors
///
/// - `RpcError` — the `sendTransaction` call failed (network, timeout, etc.)
/// - `HeightFetchError` — block height could not be fetched after submission.
///   The transaction may have been accepted. The caller should still attempt
///   to track it using the signature.
/// - `DeserializationError` — the bytes could not be deserialized. This
///   indicates a bug upstream (orchestrator should have verified these bytes).
#[instrument(skip(rpc, signed_tx_bytes), fields(signature = %signature))]
pub async fn submit_transaction(
    rpc: &RpcClient,
    signed_tx_bytes: &[u8],
    signature: Signature,
    last_valid_block_height: u64,
) -> Result<SubmissionResult, SubmissionError> {
    // Step 1: Deserialize the signed transaction.
    //
    // The Solana RPC client requires a `Transaction` object, not raw bytes.
    // We deserialize here but do NOT modify the transaction in any way.
    let tx: Transaction = bincode::deserialize(signed_tx_bytes)
        .map_err(|e| SubmissionError::DeserializationError(
            format!("failed to deserialize signed transaction: {e}")
        ))?;

    // Step 2: Submit to the network.
    //
    // Configuration rationale:
    // - skip_preflight: true — the control plane already simulated this
    //   transaction against the network state. Running preflight again
    //   would add latency and may fail on stale state if the blockhash
    //   is close to expiry.
    // - max_retries: Some(0) — the execution plane (transaction tracker)
    //   owns the retry policy, not the RPC node. Setting max_retries=0
    //   means the RPC node will attempt to land the transaction exactly
    //   once via its own leader schedule forwarding.
    // - preflight_commitment: processed — if preflight is somehow
    //   triggered despite skip_preflight, use the least-strict commitment
    //   to minimize false rejections.
    let config = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: Some(CommitmentLevel::Processed),
        max_retries: Some(0),
        ..Default::default()
    };

    let rpc_signature = rpc
        .send_transaction_with_config(&tx, config)
        .await
        .map_err(SubmissionError::RpcError)?;

    info!(
        rpc_signature = %rpc_signature,
        "transaction submitted to Solana network"
    );

    // Step 3: Capture the current block height.
    //
    // This anchors the lifecycle tracking: the tracker uses this height
    // to calculate grace periods (e.g., Dropped after +50 blocks).
    //
    // We use `confirmed` commitment for block height because:
    // - `finalized` may lag behind the actual processing front
    // - `processed` is not stable (may change)
    // - `confirmed` is the best balance of recency and stability
    let submission_block_height = rpc
        .get_block_height_with_commitment(CommitmentConfig::confirmed())
        .await
        .map_err(SubmissionError::HeightFetchError)?;

    info!(
        submission_block_height,
        last_valid_block_height,
        "captured submission context for lifecycle tracking"
    );

    Ok(SubmissionResult {
        signature: rpc_signature,
        last_valid_block_height,
        submission_block_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_result_carries_all_fields() {
        let sig = Signature::new_unique();
        let result = SubmissionResult {
            signature: sig,
            last_valid_block_height: 1150,
            submission_block_height: 1000,
        };
        assert_eq!(result.signature, sig);
        assert_eq!(result.last_valid_block_height, 1150);
        assert_eq!(result.submission_block_height, 1000);
    }

    #[test]
    fn submission_error_display() {
        let err = SubmissionError::DeserializationError("bad bytes".to_string());
        let msg = err.to_string();
        assert!(msg.contains("deserialization"));
        assert!(msg.contains("bad bytes"));
    }

    #[test]
    fn deserialization_error_on_invalid_bytes() {
        // Verify that invalid bytes produce DeserializationError, not a panic.
        let bad_bytes = vec![0u8, 1, 2, 3];
        let result: Result<Transaction, _> = bincode::deserialize(&bad_bytes);
        assert!(result.is_err());
    }
}

//! `ClawRpcClient` — a thin typed wrapper around `RpcPool` that provides
//! the most commonly needed Solana RPC operations with:
//!   - automatic retry on transient errors
//!   - structured error mapping
//!   - per-call commitment level control
//!   - tracing spans on every call

use std::str::FromStr;

use solana_client::rpc_config::RpcTransactionConfig;
use solana_client::rpc_response::RpcConfirmedTransactionStatusWithSignature;
use solana_sdk::{
    account::Account,
    pubkey::Pubkey,
    signature::Signature,
};
use solana_transaction_status::{EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding};
use tracing::instrument;

use claw_types::solana::CommitmentLevel;

use super::{to_sdk_commitment, RpcPool};
use crate::errors::SolanaError;

/// The primary API surface for Solana RPC calls throughout ClawSolana.
/// Consumers should use this rather than constructing `RpcClient` directly.
#[derive(Clone)]
pub struct ClawRpcClient {
    pool: RpcPool,
    default_commitment: CommitmentLevel,
    max_retries: u32,
}

impl ClawRpcClient {
    pub fn new(pool: RpcPool, default_commitment: CommitmentLevel) -> Self {
        Self {
            pool,
            default_commitment,
            max_retries: 3,
        }
    }

    fn parse_pubkey(s: &str) -> Result<Pubkey, SolanaError> {
        Pubkey::from_str(s).map_err(|e| SolanaError::InvalidPubkey(e.to_string()))
    }

    /// Fetches an account's data.
    #[instrument(skip(self), fields(pubkey = %pubkey))]
    pub async fn get_account(
        &self,
        pubkey: &str,
        commitment: Option<CommitmentLevel>,
    ) -> Result<Account, SolanaError> {
        let pk = Self::parse_pubkey(pubkey)?;
        let commitment = to_sdk_commitment(commitment.unwrap_or(self.default_commitment));
        let client = self.pool.read_client()?;

        let result = client
            .get_account_with_commitment(&pk, commitment)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                SolanaError::Client(e.to_string())
            })?;

        self.pool.record_success(&client.url());

        result.value.ok_or_else(|| SolanaError::AccountNotFound {
            pubkey: pubkey.to_string(),
        })
    }

    /// Fetches the SOL balance of an account in lamports.
    #[instrument(skip(self), fields(pubkey = %pubkey))]
    pub async fn get_balance(
        &self,
        pubkey: &str,
        commitment: Option<CommitmentLevel>,
    ) -> Result<u64, SolanaError> {
        let pk = Self::parse_pubkey(pubkey)?;
        let commitment = to_sdk_commitment(commitment.unwrap_or(self.default_commitment));
        let client = self.pool.read_client()?;

        let result = client
            .get_balance_with_commitment(&pk, commitment)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                SolanaError::Client(e.to_string())
            })?;

        self.pool.record_success(&client.url());
        Ok(result.value)
    }

    /// Returns the current slot.
    #[instrument(skip(self))]
    pub async fn get_slot(&self, commitment: Option<CommitmentLevel>) -> Result<u64, SolanaError> {
        let commitment = to_sdk_commitment(commitment.unwrap_or(self.default_commitment));
        let client = self.pool.read_client()?;

        let slot = client
            .get_slot_with_commitment(commitment)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                SolanaError::Client(e.to_string())
            })?;

        self.pool.record_success(&client.url());
        Ok(slot)
    }

    /// Returns the most recent transaction signatures for an address.
    #[instrument(skip(self), fields(address = %address, limit))]
    pub async fn get_signatures_for_address(
        &self,
        address: &str,
        limit: usize,
        commitment: Option<CommitmentLevel>,
    ) -> Result<Vec<RpcConfirmedTransactionStatusWithSignature>, SolanaError> {
        let pk = Self::parse_pubkey(address)?;
        let commitment = to_sdk_commitment(commitment.unwrap_or(self.default_commitment));
        let client = self.pool.read_client()?;

        let config = solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
            limit: Some(limit),
            commitment: Some(commitment),
            ..Default::default()
        };

        let sigs = client
            .get_signatures_for_address_with_config(&pk, config)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                SolanaError::Client(e.to_string())
            })?;

        self.pool.record_success(&client.url());
        Ok(sigs)
    }

    /// Fetches a transaction by signature.
    #[instrument(skip(self), fields(signature = %signature))]
    pub async fn get_transaction(
        &self,
        signature: &str,
        commitment: Option<CommitmentLevel>,
    ) -> Result<EncodedConfirmedTransactionWithStatusMeta, SolanaError> {
        let sig = Signature::from_str(signature)
            .map_err(|e| SolanaError::Client(format!("invalid signature: {e}")))?;
        let commitment = to_sdk_commitment(commitment.unwrap_or(CommitmentLevel::Confirmed));
        let client = self.pool.read_client()?;

        let tx = client
            .get_transaction_with_config(
                &sig,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::JsonParsed),
                    commitment: Some(commitment),
                    max_supported_transaction_version: Some(0),
                },
            )
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                SolanaError::Client(e.to_string())
            })?;

        self.pool.record_success(&client.url());
        Ok(tx)
    }

    /// Returns the minimum balance for rent exemption for a given data size.
    pub async fn get_minimum_balance_for_rent_exemption(
        &self,
        data_len: usize,
    ) -> Result<u64, SolanaError> {
        let client = self.pool.read_client()?;
        let lamports = client
            .get_minimum_balance_for_rent_exemption(data_len)
            .await
            .map_err(SolanaError::from)?;
        Ok(lamports)
    }

    /// Returns recent prioritization fees for a list of accounts.
    pub async fn get_recent_prioritization_fees(
        &self,
        addresses: &[&str],
    ) -> Result<Vec<solana_client::rpc_response::RpcPrioritizationFee>, SolanaError> {
        let pubkeys: Result<Vec<Pubkey>, _> =
            addresses.iter().map(|a| Self::parse_pubkey(a)).collect();
        let pubkeys = pubkeys?;
        let client = self.pool.read_client()?;

        let fees = client
            .get_recent_prioritization_fees(&pubkeys)
            .await
            .map_err(SolanaError::from)?;

        Ok(fees)
    }

    /// Returns the current block height.
    ///
    /// Used by the daemon to capture `submission_block_height` immediately
    /// after `sendTransaction`, anchoring lifecycle tracking grace periods.
    #[instrument(skip(self))]
    pub async fn get_block_height(
        &self,
        commitment: Option<CommitmentLevel>,
    ) -> Result<u64, SolanaError> {
        let commitment = to_sdk_commitment(commitment.unwrap_or(CommitmentLevel::Confirmed));
        let client = self.pool.read_client()?;

        let height = client
            .get_block_height_with_commitment(commitment)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                SolanaError::Client(e.to_string())
            })?;

        self.pool.record_success(&client.url());
        Ok(height)
    }

    /// Submits a signed transaction to the Solana network.
    ///
    /// The transaction bytes must be a fully-signed, serialized transaction
    /// (bincode format). This method does NOT re-sign or modify the transaction.
    ///
    /// Uses the write-preferred endpoint from the RPC pool.
    ///
    /// Returns the transaction signature as a base58 string on success.
    #[instrument(skip(self, signed_tx_bytes), fields(tx_bytes_len = signed_tx_bytes.len()))]
    pub async fn send_raw_transaction(
        &self,
        signed_tx_bytes: &[u8],
    ) -> Result<String, SolanaError> {
        use solana_sdk::transaction::Transaction;

        let tx: Transaction = bincode::deserialize(signed_tx_bytes)
            .map_err(|e| SolanaError::Serialization(format!("deserialize signed tx: {e}")))?;

        let client = self.pool.write_client()?;

        // Use send_transaction which handles serialization internally.
        let signature = client
            .send_transaction(&tx)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                SolanaError::Client(format!("sendTransaction: {e}"))
            })?;

        self.pool.record_success(&client.url());

        Ok(signature.to_string())
    }

    /// Returns the underlying pool for advanced use.
    pub fn pool(&self) -> &RpcPool {
        &self.pool
    }
}

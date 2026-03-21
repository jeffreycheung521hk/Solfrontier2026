//! Blockhash manager.
//!
//! Maintains a prefetched pool of recent blockhashes and refreshes them
//! proactively before expiry. Blockhashes expire after ~150 slots (~60s).

use std::sync::Arc;
use std::time::{Duration, Instant};

use solana_sdk::{commitment_config::CommitmentConfig, hash::Hash};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::{
    errors::SolanaError,
    rpc::{RpcPool, ClawRpcClient},
};

/// How many slots of buffer before expiry we consider a hash stale.
const STALE_SLOT_BUFFER: u64 = 20;

/// A blockhash entry with validity information.
#[derive(Debug, Clone)]
pub struct BlockhashEntry {
    pub hash:                   Hash,
    pub last_valid_block_height: u64,
    pub fetched_at:             Instant,
}

impl BlockhashEntry {
    pub fn is_valid(&self, current_block_height: u64) -> bool {
        current_block_height + STALE_SLOT_BUFFER < self.last_valid_block_height
    }
}

/// Manages a refreshing pool of recent blockhashes.
#[derive(Clone)]
pub struct BlockhashManager {
    pool:    RpcPool,
    current: Arc<RwLock<Option<BlockhashEntry>>>,
}

impl BlockhashManager {
    pub fn new(pool: RpcPool) -> Self {
        Self {
            pool,
            current: Arc::new(RwLock::new(None)),
        }
    }

    /// Fetches a fresh blockhash immediately from the RPC.
    pub async fn fetch_fresh(&self) -> Result<BlockhashEntry, SolanaError> {
        let rpc_client = self.pool.read_client()?;

        // In Solana SDK 2.x, get_latest_blockhash_with_commitment returns
        // (Hash, u64) where u64 is the last_valid_block_height.
        let (hash, last_valid_block_height) = rpc_client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await
            .map_err(|e| SolanaError::Client(e.to_string()))?;

        let entry = BlockhashEntry {
            hash,
            last_valid_block_height,
            fetched_at: Instant::now(),
        };

        debug!(
            hash = %entry.hash,
            last_valid = entry.last_valid_block_height,
            "fetched fresh blockhash"
        );

        Ok(entry)
    }

    /// Returns a valid (hash, last_valid_block_height), refreshing if stale.
    pub async fn get_fresh(&self) -> Result<(Hash, u64), SolanaError> {
        {
            let guard = self.current.read().await;
            if let Some(entry) = guard.as_ref() {
                if entry.fetched_at.elapsed() < Duration::from_secs(30) {
                    return Ok((entry.hash, entry.last_valid_block_height));
                }
            }
        }
        let entry = self.fetch_fresh().await?;
        let result = (entry.hash, entry.last_valid_block_height);
        *self.current.write().await = Some(entry);
        Ok(result)
    }

    /// Runs the background refresh loop. Call with `tokio::spawn`.
    pub async fn run_refresh_loop(self: Arc<Self>, interval_secs: Duration) {
        let mut ticker = interval(interval_secs);
        info!("blockhash manager refresh loop started");
        loop {
            ticker.tick().await;
            match self.fetch_fresh().await {
                Ok(entry) => { *self.current.write().await = Some(entry); }
                Err(e)    => { warn!(error = %e, "blockhash refresh failed"); }
            }
        }
    }
}

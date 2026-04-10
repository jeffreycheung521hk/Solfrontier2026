//! Transaction lifecycle tracker — polling worker.
//!
//! Feeds the pure state machine ([`calculate_next_state`]) with RPC
//! observations to drive transactions from `Submitted` to a terminal state.
//!
//! # Architecture
//!
//! ```text
//! TransactionTracker
//!   ├─ DashMap<Signature, TrackingRecord>   (in-memory tracking state)
//!   ├─ RpcPool                              (batched getSignatureStatuses)
//!   └─ run() loop                           (adaptive polling)
//!         │
//!         ├── getBlockHeight(finalized)
//!         ├── collect eligible signatures (adaptive tier)
//!         ├── getSignatureStatuses(batch, max 256)
//!         ├── for each result:
//!         │     construct RpcObservation
//!         │     new_state = calculate_next_state(current, obs, height)
//!         │     update DashMap (monotonic, idempotent)
//!         └── remove terminal entries (Finalized, Failed, Expired+buffer)
//! ```
//!
//! # What does NOT belong here
//!
//! - Database persistence (future layer)
//! - Event emission (future layer)
//! - WebSocket subscriptions (future layer)
//! - Retry / rebroadcast policy (B3: implemented via `RetryPolicy`)

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use solana_sdk::signature::Signature;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::rpc::RpcPool;
use crate::tx_lifecycle::{
    TxState, TrackingRecord, RpcObservation,
    calculate_next_state, should_continue_tracking,
};

/// A state change observed by the tracker.
///
/// Sent to the `LifecyclePersister` via an mpsc channel for durable
/// persistence, audit recording, and event emission. This keeps
/// the RPC polling loop non-blocking.
#[derive(Debug, Clone)]
pub struct StateChange {
    /// The on-chain transaction signature.
    pub signature: String,
    /// Internal transaction ID.
    pub transaction_id: Uuid,
    /// Orchestrator request ID.
    pub request_id: Uuid,
    /// Session that originated this transaction.
    pub session_id: String,
    /// Wallet pubkey that signed.
    pub wallet_pubkey: String,
    /// The previous state.
    pub from: TxState,
    /// The new state.
    pub to: TxState,
    /// Current network block height at the time of observation.
    pub block_height: u64,
}

// ── Adaptive polling tiers ─────────────────────────────────────────────────

/// Polling tier based on block distance from submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollTier {
    /// 0–30 blocks: poll every tick (2s).
    Hot,
    /// 30–100 blocks: poll every ~5s (skip 1 tick).
    Warm,
    /// 100–300 blocks: poll every ~10s (skip 4 ticks).
    Cool,
    /// >300 blocks: poll every ~30s (skip 14 ticks).
    ExpiryWatch,
}

impl PollTier {
    /// Determines the polling tier based on block distance from submission.
    fn from_block_distance(distance: u64) -> Self {
        match distance {
            0..=30 => PollTier::Hot,
            31..=100 => PollTier::Warm,
            101..=300 => PollTier::Cool,
            _ => PollTier::ExpiryWatch,
        }
    }

    /// Returns how many ticks to skip between polls for this tier.
    /// Base tick is 2 seconds.
    fn skip_ticks(self) -> u64 {
        match self {
            PollTier::Hot => 0,         // every 2s
            PollTier::Warm => 1,        // every 4s
            PollTier::Cool => 4,        // every 10s
            PollTier::ExpiryWatch => 14, // every 30s
        }
    }
}

// ── TransactionTracker ─────────────────────────────────────────────────────

/// Maximum number of signatures per `getSignatureStatuses` RPC call.
const MAX_BATCH_SIZE: usize = 256;

/// Base polling interval for the tracker loop.
const BASE_TICK: Duration = Duration::from_secs(2);

// ── Retry / rebroadcast policy (B3) ──────────────────────────────────────

/// Configuration for automatic rebroadcast of dropped transactions.
///
/// When a transaction transitions to `Dropped` (not observed after the grace
/// period but blockhash is still valid), the tracker can resubmit the same
/// signed bytes to the RPC. Solana deduplicates by signature, so this is safe.
///
/// `Expired` (blockhash invalid) is always terminal — no retry, no re-sign in V1.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of rebroadcast attempts per transaction.
    pub max_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_retries: 3 }
    }
}

/// In-memory transaction lifecycle tracker.
///
/// Tracks submitted transactions by polling `getSignatureStatuses` and
/// feeding observations into the pure [`calculate_next_state`] function.
///
/// # Concurrency
///
/// The `DashMap` allows concurrent `track()` calls while the background
/// worker is running. The worker collects signatures, drops all DashMap
/// references before awaiting RPC, then selectively updates entries.
pub struct TransactionTracker {
    rpc_pool: Arc<RpcPool>,
    tracking: Arc<DashMap<Signature, TrackingRecord>>,
    /// Channel for emitting state changes to the LifecyclePersister.
    /// If no persister is connected (None), state changes are only in-memory.
    change_tx: Option<mpsc::UnboundedSender<StateChange>>,
    /// B3: Retry policy for rebroadcasting dropped transactions.
    retry_policy: RetryPolicy,
}

impl TransactionTracker {
    /// Creates a new tracker backed by the given RPC pool.
    pub fn new(rpc_pool: Arc<RpcPool>) -> Self {
        Self {
            rpc_pool,
            tracking: Arc::new(DashMap::new()),
            change_tx: None,
            retry_policy: RetryPolicy::default(),
        }
    }

    /// Creates a tracker with a state change channel for the LifecyclePersister.
    pub fn with_change_channel(
        rpc_pool: Arc<RpcPool>,
        change_tx: mpsc::UnboundedSender<StateChange>,
    ) -> Self {
        Self {
            rpc_pool,
            tracking: Arc::new(DashMap::new()),
            change_tx: Some(change_tx),
            retry_policy: RetryPolicy::default(),
        }
    }

    /// Sets the retry policy for this tracker.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Begin tracking a newly submitted transaction.
    ///
    /// The transaction starts in `Submitted` state and will be polled
    /// by the background worker until it reaches a terminal state.
    ///
    /// If the signature is already tracked, this is a no-op (idempotent).
    pub fn track(
        &self,
        signature: Signature,
        transaction_id: Uuid,
        request_id: Uuid,
        session_id: String,
        wallet_pubkey: String,
        last_valid_block_height: u64,
        submission_block_height: u64,
        signed_tx_bytes: Option<Vec<u8>>,
    ) {
        // Idempotent: don't overwrite an existing record.
        if self.tracking.contains_key(&signature) {
            debug!(
                signature = %signature,
                "signature already tracked — skipping duplicate"
            );
            return;
        }

        let record = TrackingRecord::with_bytes(
            signature.to_string(),
            transaction_id,
            request_id,
            session_id,
            wallet_pubkey,
            last_valid_block_height,
            submission_block_height,
            signed_tx_bytes,
        );

        self.tracking.insert(signature, record);

        info!(
            signature = %signature,
            last_valid_block_height,
            submission_block_height,
            "tracking new transaction"
        );
    }

    /// Returns the number of actively tracked transactions.
    pub fn active_count(&self) -> usize {
        self.tracking.len()
    }

    /// Returns a snapshot of the current state for a tracked signature.
    pub fn get_state(&self, signature: &Signature) -> Option<TxState> {
        self.tracking.get(signature).map(|r| r.state.clone())
    }

    /// Returns a clone of the full tracking record for a signature.
    pub fn get_record(&self, signature: &Signature) -> Option<TrackingRecord> {
        self.tracking.get(signature).map(|r| r.clone())
    }

    /// Runs the polling loop. This should be spawned as a background task:
    ///
    /// ```ignore
    /// tokio::spawn(tracker.run());
    /// ```
    ///
    /// The loop runs until the task is cancelled (e.g., on shutdown).
    pub async fn run(self: Arc<Self>) {
        let mut tick_counter: u64 = 0;
        let mut interval = tokio::time::interval(BASE_TICK);

        info!("transaction tracker started");

        loop {
            interval.tick().await;
            tick_counter = tick_counter.wrapping_add(1);

            if self.tracking.is_empty() {
                continue;
            }

            // Step 1: Get current finalized block height.
            let current_height = match self.fetch_block_height().await {
                Some(h) => h,
                None => continue, // RPC error — skip this tick, try next.
            };

            // Step 2: Collect signatures eligible for polling this tick.
            // CRITICAL: Collect into a Vec and drop all DashMap references
            // BEFORE any await point.
            let eligible: Vec<(Signature, u64, u64)> = self.tracking
                .iter()
                .filter_map(|entry| {
                    let sig = *entry.key();
                    let record = entry.value();
                    let distance = current_height.saturating_sub(record.submission_block_height);
                    let tier = PollTier::from_block_distance(distance);

                    // Adaptive: only poll if this tick aligns with the tier's frequency.
                    if tick_counter % (tier.skip_ticks() + 1) != 0 {
                        return None;
                    }

                    Some((sig, record.last_valid_block_height, record.submission_block_height))
                })
                .collect();

            if eligible.is_empty() {
                continue;
            }

            // Step 3: Batch RPC calls (max 256 per call).
            for chunk in eligible.chunks(MAX_BATCH_SIZE) {
                let sigs: Vec<Signature> = chunk.iter().map(|(s, _, _)| *s).collect();

                let statuses = match self.fetch_signature_statuses(&sigs).await {
                    Some(s) => s,
                    None => continue, // RPC error — skip this batch.
                };

                // Step 4: Apply observations to state machine.
                for (i, sig) in sigs.iter().enumerate() {
                    let observation = Self::status_to_observation(statuses.get(i).and_then(|s| s.as_ref()));

                    // Look up the record, compute new state, update if changed.
                    if let Some(mut entry) = self.tracking.get_mut(sig) {
                        let record = entry.value();
                        let new_state = calculate_next_state(
                            &record.state,
                            &observation,
                            current_height,
                            record.submission_block_height,
                            record.last_valid_block_height,
                        );

                        if new_state != record.state {
                            let from_state = record.state.clone();

                            debug!(
                                signature = %sig,
                                from = %from_state,
                                to = %new_state,
                                "transaction state transition"
                            );

                            // Emit state change to persister (non-blocking).
                            if let Some(ref tx) = self.change_tx {
                                let _ = tx.send(StateChange {
                                    signature: record.signature.clone(),
                                    transaction_id: record.transaction_id,
                                    request_id: record.request_id,
                                    session_id: record.session_id.clone(),
                                    wallet_pubkey: record.wallet_pubkey.clone(),
                                    from: from_state,
                                    to: new_state.clone(),
                                    block_height: current_height,
                                });
                            }

                            let record = entry.value_mut();
                            // Update slot from observation if transitioning to a slotted state.
                            if let Some(slot) = new_state.slot() {
                                record.slot = Some(slot);
                            }
                            // Update err if transitioning to Failed.
                            if let TxState::Failed { ref err } = new_state {
                                record.err = Some(err.clone());
                            }
                            record.state = new_state;
                        }
                    }
                }
            }

            // Step 4b (B3): Rebroadcast dropped transactions if retries remain.
            // Collect candidates first (no DashMap refs held across await).
            let rebroadcast_candidates: Vec<(Signature, Vec<u8>)> = self.tracking
                .iter()
                .filter_map(|entry| {
                    let record = entry.value();
                    if record.state == TxState::Dropped
                        && record.retry_count < self.retry_policy.max_retries
                        && record.signed_tx_bytes.is_some()
                        && current_height <= record.last_valid_block_height
                    {
                        Some((*entry.key(), record.signed_tx_bytes.clone().unwrap()))
                    } else {
                        None
                    }
                })
                .collect();

            for (sig, tx_bytes) in &rebroadcast_candidates {
                match self.rebroadcast(tx_bytes).await {
                    Ok(_) => {
                        if let Some(mut entry) = self.tracking.get_mut(sig) {
                            let record = entry.value_mut();
                            record.retry_count += 1;
                            // Reset to Submitted so the grace period restarts.
                            record.state = TxState::Submitted;
                            record.submission_block_height = current_height;
                            info!(
                                signature = %sig,
                                retry = record.retry_count,
                                max = self.retry_policy.max_retries,
                                "rebroadcast dropped transaction"
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(mut entry) = self.tracking.get_mut(sig) {
                            entry.value_mut().retry_count += 1;
                        }
                        warn!(
                            signature = %sig,
                            error = %e,
                            "rebroadcast failed"
                        );
                    }
                }
            }

            // Step 5: Remove entries that should no longer be tracked.
            let to_remove: Vec<Signature> = self.tracking
                .iter()
                .filter(|entry| !should_continue_tracking(entry.value(), current_height))
                .map(|entry| *entry.key())
                .collect();

            for sig in &to_remove {
                if let Some((_, record)) = self.tracking.remove(sig) {
                    info!(
                        signature = %sig,
                        final_state = %record.state,
                        "stopped tracking transaction"
                    );
                }
            }

            if !to_remove.is_empty() {
                debug!(
                    removed = to_remove.len(),
                    remaining = self.tracking.len(),
                    "tracker cleanup complete"
                );
            }
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    /// Fetches the current finalized block height from the RPC pool.
    /// Returns `None` on error (caller should skip this tick).
    async fn fetch_block_height(&self) -> Option<u64> {
        let client = match self.rpc_pool.read_client() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "RPC pool exhausted — skipping tracker tick");
                return None;
            }
        };

        match client
            .get_block_height_with_commitment(
                solana_sdk::commitment_config::CommitmentConfig::finalized(),
            )
            .await
        {
            Ok(h) => {
                self.rpc_pool.record_success(&client.url());
                Some(h)
            }
            Err(e) => {
                self.rpc_pool.record_failure(&client.url());
                warn!(error = %e, "failed to fetch block height — skipping tick");
                None
            }
        }
    }

    /// Fetches signature statuses in a single batched RPC call.
    /// Returns `None` on error (caller should skip this batch).
    async fn fetch_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Option<Vec<Option<solana_transaction_status::TransactionStatus>>> {
        let client = match self.rpc_pool.read_client() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "RPC pool exhausted — skipping status fetch");
                return None;
            }
        };

        match client.get_signature_statuses(signatures).await {
            Ok(response) => {
                self.rpc_pool.record_success(&client.url());
                Some(response.value)
            }
            Err(e) => {
                self.rpc_pool.record_failure(&client.url());
                warn!(
                    error = %e,
                    batch_size = signatures.len(),
                    "getSignatureStatuses failed — skipping batch"
                );
                None
            }
        }
    }

    /// Converts a Solana `TransactionStatus` into our pure `RpcObservation`.
    fn status_to_observation(
        status: Option<&solana_transaction_status::TransactionStatus>,
    ) -> RpcObservation {
        match status {
            None => RpcObservation {
                status: None,
                err: None,
                slot: None,
            },
            Some(s) => RpcObservation {
                status: s.confirmation_status.as_ref().map(|cs| {
                    // TransactionConfirmationStatus is an enum; convert to string.
                    match cs {
                        solana_transaction_status::TransactionConfirmationStatus::Processed => {
                            "processed".to_string()
                        }
                        solana_transaction_status::TransactionConfirmationStatus::Confirmed => {
                            "confirmed".to_string()
                        }
                        solana_transaction_status::TransactionConfirmationStatus::Finalized => {
                            "finalized".to_string()
                        }
                    }
                }),
                err: s.err.as_ref().map(|e| format!("{e:?}")),
                slot: Some(s.slot),
            },
        }
    }

    /// B3: Rebroadcast signed transaction bytes to the RPC.
    ///
    /// Uses the same `sendTransaction` config as the original submission:
    /// skip_preflight=true, max_retries=0 (we manage retries ourselves).
    /// Solana deduplicates by signature, so resubmitting identical bytes is safe.
    async fn rebroadcast(&self, signed_tx_bytes: &[u8]) -> Result<(), String> {
        let tx: solana_sdk::transaction::Transaction = bincode::deserialize(signed_tx_bytes)
            .map_err(|e| format!("failed to deserialize tx for rebroadcast: {e}"))?;

        let client = self.rpc_pool.write_client()
            .or_else(|_| self.rpc_pool.read_client())
            .map_err(|e| format!("RPC pool exhausted: {e}"))?;

        let config = solana_client::rpc_config::RpcSendTransactionConfig {
            skip_preflight: true,
            max_retries: Some(0),
            ..Default::default()
        };

        match client.send_transaction_with_config(&tx, config).await {
            Ok(_sig) => {
                self.rpc_pool.record_success(&client.url());
                Ok(())
            }
            Err(e) => {
                self.rpc_pool.record_failure(&client.url());
                Err(format!("sendTransaction failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_tier_hot() {
        assert_eq!(PollTier::from_block_distance(0), PollTier::Hot);
        assert_eq!(PollTier::from_block_distance(15), PollTier::Hot);
        assert_eq!(PollTier::from_block_distance(30), PollTier::Hot);
    }

    #[test]
    fn poll_tier_warm() {
        assert_eq!(PollTier::from_block_distance(31), PollTier::Warm);
        assert_eq!(PollTier::from_block_distance(100), PollTier::Warm);
    }

    #[test]
    fn poll_tier_cool() {
        assert_eq!(PollTier::from_block_distance(101), PollTier::Cool);
        assert_eq!(PollTier::from_block_distance(300), PollTier::Cool);
    }

    #[test]
    fn poll_tier_expiry_watch() {
        assert_eq!(PollTier::from_block_distance(301), PollTier::ExpiryWatch);
        assert_eq!(PollTier::from_block_distance(1000), PollTier::ExpiryWatch);
    }

    #[test]
    fn skip_ticks_values() {
        assert_eq!(PollTier::Hot.skip_ticks(), 0);
        assert_eq!(PollTier::Warm.skip_ticks(), 1);
        assert_eq!(PollTier::Cool.skip_ticks(), 4);
        assert_eq!(PollTier::ExpiryWatch.skip_ticks(), 14);
    }

    #[test]
    fn null_status_to_observation() {
        let obs = TransactionTracker::status_to_observation(None);
        assert!(obs.is_null());
        assert!(!obs.has_error());
        assert_eq!(obs.slot, None);
    }

    #[test]
    fn confirmed_status_to_observation() {
        use solana_transaction_status::{TransactionConfirmationStatus, TransactionStatus};

        let status = TransactionStatus {
            slot: 42,
            confirmations: Some(10),
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
        };

        let obs = TransactionTracker::status_to_observation(Some(&status));
        assert_eq!(obs.status, Some("confirmed".to_string()));
        assert_eq!(obs.err, None);
        assert_eq!(obs.slot, Some(42));
    }

    #[test]
    fn finalized_status_to_observation() {
        use solana_transaction_status::{TransactionConfirmationStatus, TransactionStatus};

        let status = TransactionStatus {
            slot: 99,
            confirmations: None,
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Finalized),
        };

        let obs = TransactionTracker::status_to_observation(Some(&status));
        assert_eq!(obs.status, Some("finalized".to_string()));
        assert_eq!(obs.slot, Some(99));
    }

    #[test]
    fn failed_status_to_observation() {
        use solana_transaction_status::{TransactionConfirmationStatus, TransactionStatus};
        use solana_sdk::transaction::TransactionError;

        let err = TransactionError::InsufficientFundsForFee;
        let status = TransactionStatus {
            slot: 50,
            confirmations: Some(5),
            status: Err(err.clone()),
            err: Some(err),
            confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
        };

        let obs = TransactionTracker::status_to_observation(Some(&status));
        assert_eq!(obs.status, Some("confirmed".to_string()));
        assert!(obs.err.is_some());
        assert!(obs.err.as_ref().unwrap().contains("InsufficientFundsForFee"));
        assert_eq!(obs.slot, Some(50));
    }

    #[test]
    fn processed_status_to_observation() {
        use solana_transaction_status::{TransactionConfirmationStatus, TransactionStatus};

        let status = TransactionStatus {
            slot: 10,
            confirmations: Some(0),
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Processed),
        };

        let obs = TransactionTracker::status_to_observation(Some(&status));
        assert_eq!(obs.status, Some("processed".to_string()));
        assert_eq!(obs.err, None);
        assert_eq!(obs.slot, Some(10));
    }
}

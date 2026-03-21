//! Parking store for transactions awaiting human approval.
//!
//! # Design
//!
//! When `TransactionReviewPipeline::sign()` returns `PipelineResult::AwaitingApproval`,
//! the pipeline has already completed simulation and policy evaluation. The caller
//! (a Tokio task spawned by `GatewayMessageHandler`) parks the resumable state here
//! and blocks on a oneshot receiver. When the operator approves or rejects via
//! `POST /sessions/:id/approve`, `GatewayApprovalHandler::decide_inner()` sends the
//! decision through the oneshot, waking the waiting task which then calls
//! `pipeline.sign()` a second time under `ApprovalMode::RequireHuman` (overriding
//! the Automatic mode) or drops the pipeline with a rejection error.
//!
//! # What is parked
//!
//! - The `TransactionProposal` (metadata only, small).
//! - The raw `Transaction` bytes (bincode-serialized). The blockhash was set during
//!   `simulate()`. Because Solana blockhashes expire after ~90 seconds (150 slots),
//!   the operator approval window is effectively bounded to that window. Stale
//!   blockhashes cause the signing to fail, which is the correct and safe behaviour.
//! - The `SimulationResult` (already captured in the original `ApprovedTransaction`).
//! - The `PolicyVerdict` (to reconstruct the `ApprovedTransaction`).
//! - A `tokio::sync::oneshot::Sender<bool>` so the approval handler can wake the
//!   parked task with the operator's decision.
//!
//! # Durability
//!
//! **Explicitly non-durable (V1).**
//! The store is in-memory (DashMap). Parked approvals are lost on daemon restart.
//! On restart, operators must re-submit the original agent command. A future version
//! should checkpoint the serialized transaction to SQLite and resume via a restart-
//! aware pending-approval query. This is documented as a known V1 limitation.
//!
//! # Thread safety
//!
//! `PendingSigningStore` is `Clone` (cheap Arc clone). Concurrent access is safe.

use std::sync::Arc;

use bincode;
use dashmap::DashMap;
use solana_sdk::transaction::Transaction;
use tokio::sync::oneshot;
use uuid::Uuid;

use claw_types::{
    policy::PolicyVerdict,
    transaction::{SimulationResult, TransactionProposal},
};

/// The data parked for a single pending-approval transaction.
pub struct ParkedApproval {
    /// The original proposal (metadata + id).
    pub proposal: TransactionProposal,

    /// The Solana transaction serialized to bytes.
    /// The blockhash is already set from the `simulate()` stage.
    /// Deserialization reconstructs the `Transaction` for signing.
    pub tx_bytes: Vec<u8>,

    /// The simulation result from the first run-through.
    /// Used to reconstitute `SimulatedTransaction::new_unchecked()`.
    pub simulation: SimulationResult,

    /// The policy verdict that passed (must be `RequiresHumanApproval` or `Approved`).
    /// Used to reconstitute `ApprovedTransaction::new_unchecked()`.
    pub policy_verdict: PolicyVerdict,

    /// Sender half of the oneshot that wakes the parked sign task.
    /// `true` = operator approved, `false` = operator rejected.
    /// `None` after the decision has been consumed (prevent double-fire).
    pub decision_tx: Option<oneshot::Sender<bool>>,
}

/// In-memory store for transactions parked awaiting operator approval.
///
/// Keyed by `ApprovalRequest.id` (the same UUID used in the API).
#[derive(Clone)]
pub struct PendingSigningStore {
    inner: Arc<DashMap<Uuid, ParkedApproval>>,
}

impl PendingSigningStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Parks a transaction and returns a oneshot receiver the caller should await.
    ///
    /// The caller (the sign task) blocks on this receiver. When the operator
    /// decides, `signal()` sends `true` or `false` through the channel.
    pub fn park(
        &self,
        request_id: Uuid,
        proposal: TransactionProposal,
        tx: &Transaction,
        simulation: SimulationResult,
        policy_verdict: PolicyVerdict,
    ) -> Result<oneshot::Receiver<bool>, String> {
        let tx_bytes = bincode::serialize(tx)
            .map_err(|e| format!("failed to serialize parked transaction: {e}"))?;

        let (decision_tx, decision_rx) = oneshot::channel();

        self.inner.insert(
            request_id,
            ParkedApproval {
                proposal,
                tx_bytes,
                simulation,
                policy_verdict,
                decision_tx: Some(decision_tx),
            },
        );

        Ok(decision_rx)
    }

    /// Signals the parked task with the operator's decision.
    ///
    /// Returns `true` if the signal was delivered, `false` if the entry was not
    /// found (already decided or expired).
    pub fn signal(&self, request_id: Uuid, approved: bool) -> bool {
        if let Some(mut entry) = self.inner.get_mut(&request_id) {
            if let Some(tx) = entry.decision_tx.take() {
                // If send fails the receiver dropped (task gone); still clean up.
                let _ = tx.send(approved);
                return true;
            }
        }
        false
    }

    /// Removes and returns the parked approval for reconstruction.
    ///
    /// Called by the sign task after it has been woken by `signal()` with `approved = true`.
    pub fn take(&self, request_id: &Uuid) -> Option<ParkedApproval> {
        self.inner.remove(request_id).map(|(_, v)| v)
    }

    /// Removes a parked approval without reconstructing (used on rejection/expiry).
    pub fn remove(&self, request_id: &Uuid) {
        self.inner.remove(request_id);
    }

    /// Returns the number of currently parked approvals.
    pub fn parked_count(&self) -> usize {
        self.inner.len()
    }

    /// Returns all currently parked request IDs.
    /// Used for startup recovery to spawn resume tasks.
    pub fn all_parked_ids(&self) -> Vec<Uuid> {
        self.inner.iter().map(|entry| *entry.key()).collect()
    }

    /// Takes the oneshot decision receiver from a parked entry without removing it.
    /// Used during startup recovery to extract the receiver for a resume task.
    /// Returns `None` if the entry doesn't exist or the receiver was already taken.
    pub fn take_decision_rx(&self, request_id: &Uuid) -> Option<oneshot::Receiver<bool>> {
        let mut entry = self.inner.get_mut(request_id)?;
        let tx = entry.decision_tx.take()?;
        // We already took the sender; we need to create a new channel.
        // The sender was consumed, so we re-create a fresh channel
        // and put the new sender back in the entry.
        let (new_tx, new_rx) = oneshot::channel();
        entry.decision_tx = Some(new_tx);
        // Drop the old sender — it's not wired to anything.
        drop(tx);
        Some(new_rx)
    }

    /// Returns a clone of the proposal for a parked entry.
    /// Used during startup recovery to pass context to resume tasks.
    pub fn get_proposal(&self, request_id: &Uuid) -> Option<TransactionProposal> {
        self.inner.get(request_id).map(|entry| entry.proposal.clone())
    }
}

impl Default for PendingSigningStore {
    fn default() -> Self {
        Self::new()
    }
}

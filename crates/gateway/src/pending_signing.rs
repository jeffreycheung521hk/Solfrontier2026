//! Parking store for transactions awaiting human approval.
//!
//! # Design
//!
//! When `TransactionReviewPipeline::sign()` returns `PipelineResult::AwaitingApproval`,
//! the pipeline has already completed simulation and policy evaluation. The caller
//! parks the resumable state here and blocks on a oneshot receiver. When the operator
//! approves or rejects via `POST /sessions/:id/approve`, `GatewayApprovalHandler`
//! sends the workflow terminal state through the oneshot, waking the waiting task.
//!
//! # Signaling (Step 4)
//!
//! The oneshot carries `ApprovalWorkflowState` (not a raw `bool`). This is the
//! foundation for multi-step approval chains (Step 5) — the resume task matches
//! on the workflow state to decide whether to sign, reject, or handle expiry.
//!
//! # Durability
//!
//! **Explicitly non-durable (V1).**
//! The store is in-memory (DashMap). Parked approvals are lost on daemon restart.
//! On restart, operators must re-submit the original agent command. The durable
//! `ApprovalWorkflow` in SQLite records the lifecycle for audit purposes.
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
    approval::ApprovalWorkflowState,
    policy::PolicyVerdict,
    transaction::{SimulationResult, TransactionProposal},
};

/// The data parked for a single pending-approval transaction.
pub struct ParkedApproval {
    /// The original proposal (metadata + id).
    pub proposal: TransactionProposal,

    /// The Solana transaction serialized to bytes.
    pub tx_bytes: Vec<u8>,

    /// The simulation result from the first run-through.
    pub simulation: SimulationResult,

    /// The policy verdict that passed (must be `RequiresHumanApproval` or `Approved`).
    pub policy_verdict: PolicyVerdict,

    /// The `lastValidBlockHeight` from the blockhash used in this transaction.
    pub last_valid_block_height: u64,

    /// Sender half of the oneshot that wakes the parked sign task.
    /// Carries `ApprovalWorkflowState` — the terminal state of the workflow.
    /// `None` after the decision has been consumed (prevent double-fire).
    pub decision_tx: Option<oneshot::Sender<ApprovalWorkflowState>>,
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
    /// decides, `signal()` sends the terminal `ApprovalWorkflowState`.
    pub fn park(
        &self,
        request_id: Uuid,
        proposal: TransactionProposal,
        tx: &Transaction,
        simulation: SimulationResult,
        policy_verdict: PolicyVerdict,
        last_valid_block_height: u64,
    ) -> Result<oneshot::Receiver<ApprovalWorkflowState>, String> {
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
                last_valid_block_height,
                decision_tx: Some(decision_tx),
            },
        );

        Ok(decision_rx)
    }

    /// Signals the parked task with the workflow's terminal state.
    ///
    /// Returns `true` if the signal was delivered, `false` if the entry was not
    /// found (already decided or expired).
    pub fn signal(&self, request_id: Uuid, state: ApprovalWorkflowState) -> bool {
        if let Some(mut entry) = self.inner.get_mut(&request_id) {
            if let Some(tx) = entry.decision_tx.take() {
                let _ = tx.send(state);
                return true;
            }
        }
        false
    }

    /// Removes and returns the parked approval for reconstruction.
    ///
    /// Called by the sign task after it has been woken by `signal()` with `Approved`.
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
    pub fn all_parked_ids(&self) -> Vec<Uuid> {
        self.inner.iter().map(|entry| *entry.key()).collect()
    }

    /// Takes the oneshot decision receiver from a parked entry without removing it.
    /// Used during startup recovery to extract the receiver for a resume task.
    pub fn take_decision_rx(&self, request_id: &Uuid) -> Option<oneshot::Receiver<ApprovalWorkflowState>> {
        let mut entry = self.inner.get_mut(request_id)?;
        let tx = entry.decision_tx.take()?;
        let (new_tx, new_rx) = oneshot::channel();
        entry.decision_tx = Some(new_tx);
        drop(tx);
        Some(new_rx)
    }

    /// Returns a clone of the proposal for a parked entry.
    pub fn get_proposal(&self, request_id: &Uuid) -> Option<TransactionProposal> {
        self.inner.get(request_id).map(|entry| entry.proposal.clone())
    }
}

impl Default for PendingSigningStore {
    fn default() -> Self {
        Self::new()
    }
}

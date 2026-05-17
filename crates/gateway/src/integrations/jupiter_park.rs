//! In-memory parking store for Jupiter swap intents awaiting approval.
//!
//! Mirrors the shape of `PendingSigningStore` but for intents (no tx bytes).
//! The key is the `ApprovalRequest.id` — matching the shape of the signing
//! store so that the single approval-routing code path can signal both stores
//! with the same request_id without caring which one owns it.
//!
//! # Why a separate store?
//!
//! The signing store parks finalized `Transaction` bytes after policy eval
//! for local sign-and-submit. A Jupiter intent has no transaction yet — it
//! is built JIT, AFTER the operator approves. That makes the signing store's
//! parked shape (tx bytes, blockhash) wrong for Jupiter intents.
//!
//! # Signal semantics
//!
//! `signal(request_id, state)` delivers the terminal `ApprovalWorkflowState`
//! through a oneshot channel to the resume task. If no entry is parked for
//! the given `request_id`, `signal` is a graceful no-op (returns `false`) —
//! matching `PendingSigningStore::signal` so the approval router can fire
//! both signals unconditionally.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::oneshot;
use uuid::Uuid;

use claw_types::{approval::ApprovalWorkflowState, swap::SwapIntent};

/// Data parked for a single Jupiter intent awaiting a decision.
pub struct ParkedJupiterIntent {
    /// The durable intent.
    pub intent: SwapIntent,
    /// Sender half of the oneshot. `None` after the decision has been consumed.
    pub decision_tx: Option<oneshot::Sender<ApprovalWorkflowState>>,
}

/// In-memory store for Jupiter intents awaiting operator approval.
#[derive(Clone)]
pub struct PendingJupiterParkStore {
    inner: Arc<DashMap<Uuid, ParkedJupiterIntent>>,
}

impl PendingJupiterParkStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Park an intent and return the oneshot receiver the resume task awaits.
    pub fn park(
        &self,
        request_id: Uuid,
        intent: SwapIntent,
    ) -> oneshot::Receiver<ApprovalWorkflowState> {
        let (decision_tx, decision_rx) = oneshot::channel();
        self.inner.insert(
            request_id,
            ParkedJupiterIntent {
                intent,
                decision_tx: Some(decision_tx),
            },
        );
        decision_rx
    }

    /// Deliver the terminal workflow state to the parked resume task.
    /// Returns `true` if the signal was delivered, `false` if no entry exists.
    pub fn signal(&self, request_id: Uuid, state: ApprovalWorkflowState) -> bool {
        if let Some(mut entry) = self.inner.get_mut(&request_id) {
            if let Some(tx) = entry.decision_tx.take() {
                let _ = tx.send(state);
                return true;
            }
        }
        false
    }

    /// Remove an entry without signaling (e.g., cleanup after resume completes).
    pub fn remove(&self, request_id: &Uuid) {
        self.inner.remove(request_id);
    }

    /// Number of currently parked intents.
    pub fn parked_count(&self) -> usize {
        self.inner.len()
    }

    /// Whether a given request_id is parked here.
    pub fn contains(&self, request_id: &Uuid) -> bool {
        self.inner.contains_key(request_id)
    }
}

impl Default for PendingJupiterParkStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_types::{session::SessionId, solana::SolanaNetwork};

    fn test_intent() -> SwapIntent {
        SwapIntent::new(
            SessionId::from(Uuid::new_v4()),
            "wallet_pk",
            SolanaNetwork::Devnet,
            "mint_in",
            "mint_out",
            100,
            50,
            "test",
        )
    }

    #[tokio::test]
    async fn park_then_signal_delivers_state() {
        let store = PendingJupiterParkStore::new();
        let request_id = Uuid::new_v4();
        let rx = store.park(request_id, test_intent());

        let delivered = store.signal(request_id, ApprovalWorkflowState::Approved);
        assert!(delivered);

        let state = rx.await.unwrap();
        assert_eq!(state, ApprovalWorkflowState::Approved);
    }

    #[tokio::test]
    async fn signal_for_unknown_request_is_graceful_no_op() {
        let store = PendingJupiterParkStore::new();
        let delivered = store.signal(Uuid::new_v4(), ApprovalWorkflowState::Approved);
        assert!(!delivered);
    }

    #[tokio::test]
    async fn double_signal_only_fires_once() {
        let store = PendingJupiterParkStore::new();
        let request_id = Uuid::new_v4();
        let _rx = store.park(request_id, test_intent());

        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
        assert!(!store.signal(request_id, ApprovalWorkflowState::Approved));
    }

    #[tokio::test]
    async fn signal_rejected_and_expired_both_deliver() {
        let store = PendingJupiterParkStore::new();

        let rid1 = Uuid::new_v4();
        let rx1 = store.park(rid1, test_intent());
        store.signal(rid1, ApprovalWorkflowState::Rejected);
        assert_eq!(rx1.await.unwrap(), ApprovalWorkflowState::Rejected);

        let rid2 = Uuid::new_v4();
        let rx2 = store.park(rid2, test_intent());
        store.signal(rid2, ApprovalWorkflowState::Expired);
        assert_eq!(rx2.await.unwrap(), ApprovalWorkflowState::Expired);
    }

    #[test]
    fn parked_count_tracks_park_and_remove() {
        let store = PendingJupiterParkStore::new();
        let r1 = Uuid::new_v4();
        let r2 = Uuid::new_v4();
        let _rx1 = store.park(r1, test_intent());
        let _rx2 = store.park(r2, test_intent());
        assert_eq!(store.parked_count(), 2);
        store.remove(&r1);
        assert_eq!(store.parked_count(), 1);
        assert!(store.contains(&r2));
        assert!(!store.contains(&r1));
    }
}

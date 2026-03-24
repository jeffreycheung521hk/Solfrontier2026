//! In-memory approval request store.
//!
//! Holds pending `ApprovalRequest`s keyed by their `request_id`.
//! Approval requests are one-time-actionable: once `decide()` has been
//! called for a given `request_id`, any subsequent call returns
//! `ApprovalOutcome::AlreadyDecided`.
//!
//! # Durability
//!
//! This store is in-memory only. Pending approvals are lost on daemon
//! restart. For production use, approvals should be checkpointed to SQLite.
//! This is documented as a known V1 limitation.
//!
//! # Thread safety
//!
//! `ApprovalStore` is `Clone` (cheap Arc clone). The inner `DashMap` provides
//! concurrent access without a global mutex.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use claw_types::{
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    session::SessionId,
};

/// In-memory store for pending approval requests.
///
/// The store enforces the one-time-actionable invariant:
/// each `request_id` can receive exactly one decision.
#[derive(Clone, Debug)]
pub struct ApprovalStore {
    pending: Arc<DashMap<Uuid, ApprovalRequest>>,
}

impl ApprovalStore {
    /// Creates an empty approval store.
    pub fn new() -> Self {
        Self {
            pending: Arc::new(DashMap::new()),
        }
    }

    /// Registers a new approval request.
    ///
    /// Returns the `request_id` for use in the API response.
    pub fn register(&self, request: ApprovalRequest) -> Uuid {
        let id = request.id;
        self.pending.insert(id, request);
        id
    }

    /// Returns a clone of the pending request for display/polling, if it exists.
    pub fn get(&self, request_id: Uuid) -> Option<ApprovalRequest> {
        self.pending.get(&request_id).map(|r| r.clone())
    }

    /// Returns all pending (undecided) requests for a session, sorted by `requested_at`.
    pub fn pending_for_session(&self, session_id: &SessionId) -> Vec<ApprovalRequest> {
        let mut results: Vec<ApprovalRequest> = self
            .pending
            .iter()
            .filter(|entry| &entry.session_id == session_id && !entry.decided)
            .map(|entry| entry.clone())
            .collect();
        results.sort_by_key(|r| r.requested_at);
        results
    }

    /// Applies an operator decision to a pending request.
    ///
    /// # Invariants
    ///
    /// - `NotFound` if the `request_id` is unknown.
    /// - `AlreadyDecided` if the request was already acted on (idempotency guard).
    /// - `Approved` / `Rejected` on success — the request is marked `decided = true`
    ///   and removed from the pending map.
    ///
    /// The decided request is returned on success so the caller can resume
    /// the appropriate pipeline path.
    pub fn decide(
        &self,
        decision: &ApprovalDecision,
    ) -> (ApprovalOutcome, Option<ApprovalRequest>) {
        let Some(mut entry) = self.pending.get_mut(&decision.request_id) else {
            return (ApprovalOutcome::NotFound, None);
        };

        if entry.decided {
            return (ApprovalOutcome::AlreadyDecided, None);
        }

        entry.decided = true;
        let request = entry.clone();
        drop(entry); // release dashmap lock before remove

        // Remove from pending map — it has been decided.
        self.pending.remove(&decision.request_id);

        let outcome = if decision.approved {
            ApprovalOutcome::Approved
        } else {
            ApprovalOutcome::Rejected
        };

        (outcome, Some(request))
    }

    /// Peek at which session owns a pending request (P0-3: session-request binding).
    ///
    /// Returns `Some(session_id)` if the request exists and is undecided.
    /// Does NOT consume or modify the entry.
    pub fn session_for_request(&self, request_id: Uuid) -> Option<SessionId> {
        self.pending
            .get(&request_id)
            .filter(|entry| !entry.decided)
            .map(|entry| entry.session_id.clone())
    }

    /// Returns the number of currently pending (undecided) requests.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Default for ApprovalStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_types::{
        approval::ApprovalRequest,
        policy::PolicyVerdict,
        session::SessionId,
        transaction::SimulationResult,
    };
    use uuid::Uuid;

    fn fake_request(session_id: SessionId) -> ApprovalRequest {
        ApprovalRequest::new(
            session_id,
            Uuid::new_v4(),
            "test transfer",
            PolicyVerdict::RequiresHumanApproval { reason: "mainnet policy".into(), rule_name: "test-rule".into() },
            SimulationResult {
                success: true, error: None, compute_units_used: Some(1000),
                logs: vec![], return_data: None, account_diffs: vec![], fee_lamports: Some(5000),
            },
        )
    }

    #[test]
    fn approve_pending_request() {
        let store   = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req     = fake_request(session);
        let rid     = req.id;
        store.register(req);

        let decision = ApprovalDecision::approve(rid, Some("looks good".into()));
        let (outcome, maybe_req) = store.decide(&decision);

        assert_eq!(outcome, ApprovalOutcome::Approved);
        assert!(maybe_req.is_some());
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn reject_pending_request() {
        let store   = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req     = fake_request(session);
        let rid     = req.id;
        store.register(req);

        let decision = ApprovalDecision::reject(rid, Some("too risky".into()));
        let (outcome, _) = store.decide(&decision);

        assert_eq!(outcome, ApprovalOutcome::Rejected);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn duplicate_decision_is_rejected() {
        let store   = ApprovalStore::new();
        let session = SessionId::from(Uuid::new_v4());
        let req     = fake_request(session);
        let rid     = req.id;
        store.register(req);

        let d1 = ApprovalDecision::approve(rid, None);
        let d2 = ApprovalDecision::approve(rid, None);

        let (outcome1, _) = store.decide(&d1);
        let (outcome2, _) = store.decide(&d2);

        assert_eq!(outcome1, ApprovalOutcome::Approved);
        assert_eq!(outcome2, ApprovalOutcome::NotFound, "second decision should be NotFound after removal");
    }

    #[test]
    fn unknown_request_id_returns_not_found() {
        let store    = ApprovalStore::new();
        let decision = ApprovalDecision::approve(Uuid::new_v4(), None);
        let (outcome, req) = store.decide(&decision);
        assert_eq!(outcome, ApprovalOutcome::NotFound);
        assert!(req.is_none());
    }

    #[test]
    fn pending_for_session_filters_correctly() {
        let store = ApprovalStore::new();
        let s1    = SessionId::from(Uuid::new_v4());
        let s2    = SessionId::from(Uuid::new_v4());

        store.register(fake_request(s1.clone()));
        store.register(fake_request(s1.clone()));
        store.register(fake_request(s2.clone()));

        let pending_s1 = store.pending_for_session(&s1);
        assert_eq!(pending_s1.len(), 2);

        let pending_s2 = store.pending_for_session(&s2);
        assert_eq!(pending_s2.len(), 1);
    }
}

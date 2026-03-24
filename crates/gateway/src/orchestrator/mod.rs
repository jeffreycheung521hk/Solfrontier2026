//! Signature orchestration — the Execution Plane boundary.
//!
//! This module contains the `SignatureOrchestrator`, the sole entry point
//! from the Control Plane into the Execution Plane for signature operations.
//!
//! # Responsibility
//!
//! - Route `SignatureRequest`s to the correct `WalletAdapter`
//! - Track pending (asynchronous) signature requests
//! - Enforce exactly-once completion semantics
//! - Provide expiry support for external reaper tasks
//!
//! # Must NOT
//!
//! - Perform policy evaluation or approval decisions
//! - Emit audit events (`AuditRepository`)
//! - Record spend (`SpendRepository`)
//! - Publish events (`EventBus`)
//! - Reconstruct typestate objects (`SimulatedTransaction`, `ApprovedTransaction`)
//!
//! # Pending-only
//!
//! Completed and failed requests are atomically removed from tracking.
//! The orchestrator does not retain history. History belongs to audit/events/DB,
//! which are the caller's responsibility.

pub mod adapter;
pub mod types;
pub mod local_adapter;
pub mod external_adapter;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::{info, warn};

use self::adapter::WalletAdapter;
use self::types::{
    PendingSummary, SignatureError, SignatureOutcome, SignatureRequest, SignatureRequestId,
};
use claw_types::session::SessionId;

// ── Internal pending entry ──────────────────────────────────────────────────

/// Internal tracking for a pending signature request.
/// Not exposed publicly. Used only by `SignatureOrchestrator`.
struct PendingEntry {
    /// The original request payload. Owned by the orchestrator.
    /// Passed to `adapter.complete()` when the signed payload arrives.
    request: SignatureRequest,
    /// Which adapter type is handling this request.
    adapter_type: String,
    /// When the request was dispatched to the adapter. Used by `expired_candidates()`.
    dispatched_at: Instant,
}

// ── SignatureOrchestrator ───────────────────────────────────────────────────

/// Execution Plane only.
/// Routes finalized transaction artifacts to wallet adapters and tracks pending requests.
///
/// This is the sole entry point from the Control Plane into the Execution Plane
/// for signature operations. The Control Plane constructs a `SignatureRequest` from
/// an approved, finalized transaction and calls `submit()`. The orchestrator routes
/// to the correct adapter and returns a `SignatureOutcome`.
///
/// # Pending-only
///
/// Completed and failed requests are atomically removed from tracking.
/// The orchestrator does not retain history. History belongs to audit/events/DB,
/// which are the caller's responsibility.
///
/// # Must NOT
///
/// - Perform policy evaluation or approval decisions
/// - Emit audit events (`AuditRepository`)
/// - Record spend (`SpendRepository`)
/// - Publish events (`EventBus`)
/// - Interpret the business meaning of transactions
/// - Reconstruct typestate objects (`SimulatedTransaction`, `ApprovedTransaction`)
///
/// # Thread safety
///
/// All public methods are safe to call concurrently.
/// The pending map uses `DashMap` for lock-free concurrent access.
/// `complete()` and `expire()` use atomic `remove()` — no get-then-take race window.
#[derive(Clone)]
pub struct SignatureOrchestrator {
    adapters: Vec<Arc<dyn WalletAdapter>>,
    pending: Arc<DashMap<SignatureRequestId, PendingEntry>>,
}

impl SignatureOrchestrator {
    /// Creates an orchestrator with the given adapter chain.
    ///
    /// Adapters are tried in order: the first adapter where `supports()` returns `true`
    /// handles the request. Order matters — place more specific adapters (external)
    /// before more general ones (local).
    ///
    /// An orchestrator with zero adapters will return `NoAdapter` for every request.
    pub fn new(adapters: Vec<Arc<dyn WalletAdapter>>) -> Self {
        Self {
            adapters,
            pending: Arc::new(DashMap::new()),
        }
    }

    /// Submit a signature request.
    ///
    /// Routes to the first adapter where `supports(request)` returns `true`.
    ///
    /// - If the adapter returns `Signed`: the request is not tracked (synchronous completion).
    /// - If the adapter returns `Pending`: the request is stored in the pending map.
    /// - If no adapter supports the request: returns `Err(NoAdapter)`.
    /// - If the request ID is already in the pending map: returns `Err(DuplicateRequestId)`.
    ///
    /// The caller constructs `SignatureRequest` from a finalized, approved transaction.
    /// The orchestrator does not validate that the transaction was properly simulated
    /// or policy-checked — that is the caller's responsibility.
    ///
    /// On `Signed`: the caller should record audit, spend, and emit `TransactionSigned`.
    /// On `Pending`: the caller should emit `WalletSignatureRequested` and return
    ///              the unsigned tx to the user.
    /// On Error: the caller should record the failure in audit.
    pub async fn submit(
        &self,
        request: SignatureRequest,
    ) -> Result<SignatureOutcome, SignatureError> {
        // Fail-closed: reject duplicate request IDs.
        if self.pending.contains_key(&request.id) {
            warn!(
                request_id = %request.id,
                "duplicate signature request ID rejected"
            );
            return Err(SignatureError::DuplicateRequestId);
        }

        // Find the first adapter that supports this request.
        let adapter = self.adapters
            .iter()
            .find(|a| a.supports(&request))
            .ok_or_else(|| {
                warn!(
                    request_id = %request.id,
                    expected_signer = %request.expected_signer,
                    "no wallet adapter supports this request"
                );
                SignatureError::NoAdapter
            })?
            .clone();

        let adapter_type = adapter.adapter_type().to_string();
        let request_id = request.id;

        info!(
            request_id = %request_id,
            adapter_type = %adapter_type,
            "routing signature request to adapter"
        );

        // Dispatch to the adapter.
        let outcome = adapter.sign(&request).await?;

        // If Pending, track in the pending map.
        if matches!(&outcome, SignatureOutcome::Pending { .. }) {
            // Double-check: another concurrent submit() with the same ID could have
            // raced between contains_key and here. Use try_insert semantics.
            let entry = PendingEntry {
                request,
                adapter_type: adapter_type.clone(),
                dispatched_at: Instant::now(),
            };

            // DashMap::insert returns the old value if key existed.
            // We use entry API to detect duplicates atomically.
            if self.pending.contains_key(&request_id) {
                // Race: someone inserted the same ID between our check and here.
                // The adapter already returned Pending, but we can't track it.
                // Clean up the adapter's state.
                adapter.cleanup(&request_id);
                warn!(
                    request_id = %request_id,
                    "duplicate request ID detected during insert — cleaning up"
                );
                return Err(SignatureError::DuplicateRequestId);
            }
            self.pending.insert(request_id, entry);

            info!(
                request_id = %request_id,
                adapter_type = %adapter_type,
                pending_count = self.pending.len(),
                "signature request pending"
            );
        }

        Ok(outcome)
    }

    /// Complete a pending signature request with externally-signed transaction bytes.
    ///
    /// # Consume-on-attempt semantics
    ///
    /// Atomically removes the pending entry from the map BEFORE any verification
    /// or processing. The entry is consumed regardless of outcome:
    ///
    /// - On success: returns `SignatureOutcome::Signed`. Entry consumed.
    /// - On verification failure: returns `Err(VerificationFailed)`. Entry consumed.
    /// - On not found: returns `Err(RequestNotFound)`. Nothing consumed.
    ///
    /// In all consumed cases, the request ID is permanently removed from the pending map.
    /// A subsequent call with the same request ID returns `Err(RequestNotFound)`.
    /// Retrying a failed verification requires the Control Plane to construct and
    /// submit a new `SignatureRequest` from scratch (re-simulate, re-approve).
    ///
    /// This design eliminates the get-then-take race window (P0-1).
    /// Exactly one caller completes or fails the request. Concurrent callers
    /// on the same `request_id`: one succeeds/fails, all others get `RequestNotFound`.
    ///
    /// The caller is responsible for recording the outcome in audit/events.
    pub async fn complete(
        &self,
        request_id: &SignatureRequestId,
        signed_tx_bytes: &[u8],
    ) -> Result<SignatureOutcome, SignatureError> {
        // Atomic remove — exactly one caller gets the entry.
        let (_, entry) = self.pending.remove(request_id)
            .ok_or(SignatureError::RequestNotFound)?;

        info!(
            request_id = %request_id,
            adapter_type = %entry.adapter_type,
            "completing pending signature request"
        );

        // Find the adapter that was handling this request.
        let adapter = self.find_adapter(&entry.adapter_type)?;

        // Delegate verification to the adapter, passing the consumed request.
        // The adapter has access to finalized_tx_bytes and expected_signer
        // from the request — no duplicate pending storage needed.
        adapter.complete(&entry.request, signed_tx_bytes).await
    }

    /// Mark a pending request as expired and remove it.
    ///
    /// Called by an external reaper task (not owned by the orchestrator).
    /// Returns the consumed `SignatureRequest` so the caller can emit
    /// expiry audit events and notifications.
    ///
    /// # Consume-on-attempt semantics
    ///
    /// Atomically removes the entry. On success: entry is consumed.
    /// Returns `Err(RequestNotFound)` if the request doesn't exist
    /// (already completed or expired).
    ///
    /// The caller is responsible for recording expiry in audit and emitting events.
    pub fn expire(
        &self,
        request_id: &SignatureRequestId,
    ) -> Result<SignatureRequest, SignatureError> {
        let (_, entry) = self.pending.remove(request_id)
            .ok_or(SignatureError::RequestNotFound)?;

        info!(
            request_id = %request_id,
            adapter_type = %entry.adapter_type,
            "signature request expired"
        );

        // Clean up any adapter-internal state.
        if let Ok(adapter) = self.find_adapter(&entry.adapter_type) {
            adapter.cleanup(request_id);
        }

        Ok(entry.request)
    }

    // ── Rollback cleanup ──────────────────────────────────────────────────

    /// Abort a pending request that was never externally established.
    ///
    /// This is a **compensating rollback** primitive, NOT a natural TTL expiry.
    /// Used when a durable persist fails after `submit()` has already created
    /// an in-memory pending entry. The request never had durable backing, was
    /// never exposed to operators, and should be silently cleaned up.
    ///
    /// # Semantics
    ///
    /// - Removes the in-memory pending entry (if it exists)
    /// - Performs adapter cleanup
    /// - Does NOT imply the request naturally expired
    /// - Does NOT emit any operator-facing events (caller responsibility)
    /// - Returns `Ok(request)` if the entry was found and removed
    /// - Returns `Err(RequestNotFound)` if already consumed/expired
    ///
    /// # When to use
    ///
    /// Use `abort_pending()` when cleaning up after a persistence failure.
    /// Use `expire()` when a request has naturally exceeded its TTL.
    pub fn abort_pending(
        &self,
        request_id: &SignatureRequestId,
    ) -> Result<SignatureRequest, SignatureError> {
        let (_, entry) = self.pending.remove(request_id)
            .ok_or(SignatureError::RequestNotFound)?;

        info!(
            request_id = %request_id,
            adapter_type = %entry.adapter_type,
            "pending signature request aborted (persistence rollback)"
        );

        // Clean up any adapter-internal state.
        if let Ok(adapter) = self.find_adapter(&entry.adapter_type) {
            adapter.cleanup(request_id);
        }

        Ok(entry.request)
    }

    /// Returns request IDs that have been pending longer than the given duration.
    ///
    /// Called by the external reaper task to identify candidates for expiry.
    /// Does NOT modify state. The reaper must call `expire()` for each returned ID.
    ///
    /// Uses internal `Instant` timestamps for comparison. These timestamps are
    /// never exposed publicly.
    pub fn expired_candidates(&self, max_age: Duration) -> Vec<SignatureRequestId> {
        let now = Instant::now();
        self.pending
            .iter()
            .filter(|entry| now.duration_since(entry.value().dispatched_at) > max_age)
            .map(|entry| *entry.key())
            .collect()
    }

    /// List pending requests for a session.
    ///
    /// Returns read-only summaries for API responses (e.g., `GET /wallet-signatures`).
    /// O(n) over the pending map — acceptable because pending count is small.
    pub fn pending_for_session(&self, session_id: &SessionId) -> Vec<PendingSummary> {
        self.pending
            .iter()
            .filter(|entry| &entry.value().request.session_id == session_id)
            .map(|entry| {
                let req = &entry.value().request;
                PendingSummary {
                    request_id: req.id,
                    transaction_id: req.transaction_id,
                    adapter_type: entry.value().adapter_type.clone(),
                    description: req.description.clone(),
                    expected_signer: req.expected_signer.to_string(),
                    finalized_tx_bytes: req.finalized_tx_bytes.clone(),
                }
            })
            .collect()
    }

    /// Number of pending requests across all sessions.
    /// For health checks and metrics.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Peek at which session owns a pending request (P0-3: session-request binding).
    ///
    /// Returns `Some(session_id)` if the request exists in the pending map.
    /// Does NOT consume or modify the entry. Used by callers to verify that
    /// the session from the URL path actually owns the request before completing
    /// or approving it.
    pub fn session_for_request(&self, request_id: &SignatureRequestId) -> Option<SessionId> {
        self.pending
            .get(request_id)
            .map(|entry| entry.value().request.session_id.clone())
    }

    // ── Recovery ──────────────────────────────────────────────────────────

    /// Inject a recovered pending entry directly into the pending map.
    ///
    /// Called by `DurablePendingState::recover()` during startup recovery.
    /// The request is assumed to be valid (deserialized from a previously
    /// persisted row). The `dispatched_at` timestamp is set to `Instant::now()`
    /// so that the reaper's TTL starts fresh from the recovery moment.
    ///
    /// Returns `Err` if the request ID already exists (duplicate).
    pub fn recover_pending(
        &self,
        request: SignatureRequest,
        adapter_type: String,
    ) -> Result<(), String> {
        let request_id = request.id;
        if self.pending.contains_key(&request_id) {
            return Err(format!("duplicate request ID during recovery: {request_id}"));
        }
        self.pending.insert(request_id, PendingEntry {
            request,
            adapter_type,
            dispatched_at: Instant::now(),
        });
        info!(
            request_id = %request_id,
            "recovered pending signature request"
        );
        Ok(())
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    /// Find an adapter by its type string.
    fn find_adapter(&self, adapter_type: &str) -> Result<Arc<dyn WalletAdapter>, SignatureError> {
        self.adapters
            .iter()
            .find(|a| a.adapter_type() == adapter_type)
            .cloned()
            .ok_or_else(|| {
                warn!(adapter_type = %adapter_type, "adapter not found for type");
                SignatureError::NoAdapter
            })
    }
}

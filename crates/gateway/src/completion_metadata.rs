//! Caller-side metadata stash for signature completion.
//!
//! When the signing tool submits a request through the orchestrator and
//! gets back `Pending`, it stashes the fee_lamports here. When the
//! wallet signature handler later calls `orchestrator.complete()`, it
//! looks up and consumes this metadata for spend recording.
//!
//! This is a Control Plane artifact — it carries simulation-derived
//! data that the Execution Plane (orchestrator) deliberately excludes.
//!
//! # Lifecycle
//!
//! - Populated by `SubmitForSigningTool` on `SignatureOutcome::Pending`
//! - Populated by `resume_after_approval` on `SignatureOutcome::Pending`
//! - Consumed by `GatewayWalletSignatureHandler::submit_signed_tx_inner`
//! - Removed on expiry by the reaper task

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

/// Metadata stashed by the Control Plane for use at completion time.
pub struct CompletionMeta {
    /// Fee from simulation result. Used for spend recording.
    pub fee_lamports: Option<u64>,
}

/// Thread-safe, in-memory store for completion metadata.
/// Keyed by the orchestrator's `SignatureRequestId` UUID.
#[derive(Clone)]
pub struct CompletionMetadataStore {
    inner: Arc<DashMap<Uuid, CompletionMeta>>,
}

impl CompletionMetadataStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Stash metadata for a pending signature request.
    pub fn insert(&self, request_id: Uuid, meta: CompletionMeta) {
        self.inner.insert(request_id, meta);
    }

    /// Consume metadata for a completed/rejected/expired request.
    pub fn take(&self, request_id: &Uuid) -> Option<CompletionMeta> {
        self.inner.remove(request_id).map(|(_, v)| v)
    }

    /// Remove without consuming (for cleanup).
    pub fn remove(&self, request_id: &Uuid) {
        self.inner.remove(request_id);
    }
}

impl Default for CompletionMetadataStore {
    fn default() -> Self {
        Self::new()
    }
}

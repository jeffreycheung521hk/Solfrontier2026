//! Phase 6B — Just-in-time signing handoff prep store.
//!
//! # Why this exists
//!
//! Phase 4C-4 wired the resume task to call `create_signing_handoff` the
//! moment the operator approved a Solend deposit. That partial-signed
//! transaction's blockhash starts ageing immediately. By the time a
//! human reads + clicks Approve in Phantom (~30-50 s of irreducible
//! cognition), Solana's ~150-block validity window often elapses and
//! the daemon's `pre_submit_expired` gate refuses to broadcast (correct
//! safety behaviour, but never reaches a finalised tx).
//!
//! The fix moves handoff creation from approval-time to Sign-click-time
//! ("just in time"). The resume task still re-assembles, re-evaluates
//! policy, and runs structural preflight — but instead of immediately
//! signing the obligation slot with a locked blockhash, it persists the
//! preflight-passing plan into this store keyed by `approval_request_id`.
//! The new POST `.../solend-signing/prepare` HTTP route then calls
//! `create_signing_handoff` on demand with a fresh blockhash; the
//! frontend triggers it from the Sign-with-Phantom click handler.
//!
//! # What this store stores
//!
//! `SolendJitReady` carries the data `create_signing_handoff` needs:
//!
//!   - `plan: SolendDepositTxPlan` — pure unsigned plan from
//!     `assemble_solend_deposit_tx_plan`. No blockhash, no signatures.
//!   - `preflight: SolendPreflightOutcome::Passed` — only Passed
//!     variants are persisted; non-Passed paths never reach this store.
//!   - `session_id` / `intent_id` / `session_wallet` — correlation +
//!     ownership; the prepare route checks `session_id` matches the
//!     authenticated session and the bound wallet still matches.
//!   - `verified_slot` — fresh-snapshot observed slot; for audit only.
//!   - `expires_at` — TTL bound (matches `approval_lease_seconds`); the
//!     entry is treated as missing past expiry. Lazy sweep on `get`.
//!
//! # What this store does NOT store
//!
//! No transaction bytes. No blockhash. No signatures. No `Keypair`. No
//! `last_valid_block_height`. Those are all execution-time facts and
//! belong to `SolendSigningStore` (created on-demand by the prepare
//! route via `create_signing_handoff`).
//!
//! # Concurrency
//!
//! `Arc<DashMap<Uuid, Entry>>`. Same lock discipline as `SolendParkStore`
//! and `SolendSigningStore`: never-await critical sections; safe in async.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use uuid::Uuid;

use claw_types::session::SessionId;

use crate::integrations::solend_preflight::SolendPreflightOutcome;
use crate::integrations::solend_submit::SolendAuditSink;
use crate::integrations::solend_signing::{RecentBlockhashProvider, SolendSigningStore};
use crate::integrations::solend_tx_plan::SolendDepositTxPlan;

/// Data parked for a single Solend deposit awaiting just-in-time signing.
///
/// All fields are propose/preflight-time facts. No execution-time fact
/// (blockhash, tx bytes, signer, signatures) is present by design.
#[derive(Clone)]
pub struct SolendJitReady {
    /// The tool's in-memory intent id (same value as the parked
    /// `ParkedSolendDepositIntent.intent_id`).
    pub intent_id: Uuid,
    /// The session that owns this approval. The prepare route compares
    /// this against the authenticated session and against the approval
    /// workflow's `session_id`.
    pub session_id: SessionId,
    /// The session-bound wallet at approve time. The prepare route
    /// re-checks the current binding still matches this value before
    /// running the handoff.
    pub session_wallet: Pubkey,
    /// Pure unsigned plan from `assemble_solend_deposit_tx_plan`.
    /// Re-used verbatim by `create_signing_handoff` on each prepare
    /// call — the obligation Keypair + blockhash are generated fresh
    /// inside that function.
    pub plan: SolendDepositTxPlan,
    /// Preflight outcome — must be `Passed` (caller enforces).
    pub preflight: SolendPreflightOutcome,
    /// Fresh-snapshot observed slot at re-check time. Audit only.
    pub verified_slot: u64,
    /// When this entry was created (resume-task completion time).
    pub created_at: DateTime<Utc>,
    /// Wall-clock TTL. Matches `approval_lease_seconds` so the JIT-ready
    /// window mirrors the approval window.
    pub expires_at: DateTime<Utc>,
}

/// Cloneable in-memory store for JIT-ready Solend deposit entries.
#[derive(Clone)]
pub struct SolendJitReadyStore {
    inner: Arc<DashMap<Uuid, SolendJitReady>>,
}

impl Default for SolendJitReadyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SolendJitReadyStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Insert (or replace) a JIT-ready entry keyed by approval request id.
    pub fn put(&self, approval_request_id: Uuid, ready: SolendJitReady) {
        self.inner.insert(approval_request_id, ready);
    }

    /// Read a non-expired entry. Expired entries are opportunistically
    /// removed; the caller sees `None` either way.
    pub fn get(&self, approval_request_id: Uuid) -> Option<SolendJitReady> {
        let now = Utc::now();
        let entry_expired = self
            .inner
            .get(&approval_request_id)
            .map(|e| e.expires_at <= now)
            .unwrap_or(true);
        if entry_expired {
            self.inner.remove(&approval_request_id);
            return None;
        }
        self.inner.get(&approval_request_id).map(|e| e.clone())
    }

    /// Remove an entry unconditionally. Returns whether an entry was
    /// present. Used by the prepare-endpoint after a successful handoff
    /// is created (single-shot semantics) and by terminal-decision
    /// cleanup paths.
    pub fn remove(&self, approval_request_id: Uuid) -> bool {
        self.inner.remove(&approval_request_id).is_some()
    }

    /// Sweep expired entries on demand. No background task.
    pub fn cleanup_expired(&self) -> usize {
        let now = Utc::now();
        let mut removed = 0usize;
        let expired: Vec<Uuid> = self
            .inner
            .iter()
            .filter(|e| e.value().expires_at <= now)
            .map(|e| *e.key())
            .collect();
        for k in expired {
            if self.inner.remove(&k).is_some() {
                removed += 1;
            }
        }
        removed
    }

    /// Test/diagnostic helper: number of live (not necessarily
    /// non-expired) entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Dependency bundle the resume task needs to persist a JIT-ready
/// entry. Mirrors the shape of `SolendSigningDeps` so production wiring
/// is a 1:1 substitution.
#[derive(Clone)]
pub struct SolendJitReadyDeps {
    pub store: SolendJitReadyStore,
    pub audit: Arc<dyn SolendAuditSink>,
    /// TTL for the JIT-ready entry. Production passes
    /// `config.policy.approval_lease_seconds` so the JIT-ready window
    /// aligns with the approval window.
    pub jit_ready_lease_seconds: u64,
}

/// Dependency bundle the new prepare HTTP handler needs. Bundles the
/// Solend signing infrastructure so the route can call
/// `create_signing_handoff` on demand with a fresh blockhash.
#[derive(Clone)]
pub struct SolendJitPrepareDeps {
    pub jit_ready_store: SolendJitReadyStore,
    pub signing_store: SolendSigningStore,
    pub blockhash_provider: Arc<dyn RecentBlockhashProvider>,
    pub audit: Arc<dyn SolendAuditSink>,
    pub signing_lease_seconds: u64,
}

// Unit-test note: the store's mechanics (DashMap put/get/remove + lazy
// TTL sweep) are minimal and exercised by integration tests through
// the resume-task path (`solend_park` tests) and the new prepare-route
// tests, which both construct real `SolendDepositTxPlan` /
// `SolendPreflightOutcome::Passed` artifacts via the production
// assemblers. Inline unit tests here would only re-prove DashMap.

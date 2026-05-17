//! Phase 4C-6 — Solend submission lifecycle cache (UX recovery).
//!
//! # What this module owns
//!
//! A small, Solend-specific in-memory cache keyed by
//! `signing_request_id`. It records the TERMINAL outcome of a submit
//! attempt — success or typed failure — so a session that loses the
//! broadcast response (network flake, browser tab crash, duplicate
//! click) can re-query its own `signing_request_id` and receive the
//! original signature string / typed rejection instead of a bare 404.
//!
//! # Why a separate store (rather than extending SolendSigningStore)
//!
//! `SolendSigningStore` holds the PRE-submit parked artifact (tx bytes,
//! obligation Keypair if any). At submit time the parked entry is
//! consumed atomically — the slot is gone before broadcast even runs.
//! Storing the post-submit outcome in the same map would require
//! resurrecting a consumed slot, which reintroduces the replay surface
//! the atomic consume was designed to close. A separate map keyed by
//! the same `request_id` (but holding only a terminal outcome + session
//! ownership + expiry) sidesteps that.
//!
//! Confirmation polling (getting `confirmed` / `finalized` chain
//! status) is NOT this module's job — that lives on the existing
//! `LifecyclePersister` + `TransactionTrackingRepository` path already
//! used by Jupiter and the A-slice execute flow. This module only
//! remembers *that we broadcast*, and *what signature came back*, so
//! the HTTP layer can return the right thing on a duplicate request.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT broadcast. No RPC calls at all.
//! - Does NOT poll signature status. No `confirm_transaction`,
//!   no `get_signature_statuses`.
//! - Does NOT issue a new signing handoff on recovery; the recovered
//!   entry is terminal either way (Submitted → signature string;
//!   Rejected/Expired/BroadcastFailed → typed error the UI surfaces).
//! - Does NOT accept lookups from sessions that don't own the record.
//!   Session-mismatched lookups return `None` (miss) and do NOT sweep
//!   the entry, matching `SolendSigningStore::summary_for_session`.
//! - Does NOT persist across process restart. Recovery is
//!   best-effort within the lifetime of this daemon; a restarted
//!   daemon that has already broadcast simply won't have the cache,
//!   but the chain still has the transaction and the audit log still
//!   has the terminal event. Users can recover via signature lookup.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use uuid::Uuid;

use claw_types::session::SessionId;

/// Outcome of a submit attempt + on-chain confirmation state, cached
/// for UX recovery and polled by the Phase 4C-7 Solend confirmation
/// tracker.
///
/// # State progression (non-terminal states marked †)
///
/// 1. `Submitted` †             — RPC accepted the tx; network outcome unknown.
/// 2. `Confirming` †            — RPC reports confirmation_status = "confirmed" (supermajority).
/// 3. `Finalized`               — RPC reports confirmation_status = "finalized"; **terminal success**.
/// 4. `ExecutionFailed`         — RPC reports non-null err; **terminal failure**.
/// 5. `ConfirmationTimeout`     — current block height exceeded `last_valid_block_height` while unobserved; **terminal failure**.
///
/// The pre-confirmation terminal variants `Rejected`, `BroadcastFailed`,
/// and `Expired` are retained unchanged — they reflect outcomes of the
/// 4C-5 submit pipeline itself, BEFORE any on-chain confirmation.
///
/// Monotonic progress is enforced by
/// [`SolendSubmissionLifecycleStore::transition`] — terminal states
/// never regress, and pre-confirmation outcomes cannot be overwritten
/// by confirmation-tracker observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedSubmitOutcome {
    /// Broadcast accepted, no on-chain confirmation yet. **Non-terminal.**
    Submitted {
        tx_signature: String,
        intent_id: Uuid,
        session_wallet: Pubkey,
        verified_slot: u64,
        last_valid_block_height: u64,
    },
    /// Phase 4C-7 — `getSignatureStatuses` returned `confirmation_status = "confirmed"`.
    /// The transaction is in a block voted on by a supermajority.
    /// **Non-terminal** — may still be rolled back in a rare fork; we
    /// only mark terminal success on Finalized.
    Confirming {
        tx_signature: String,
        slot: u64,
        intent_id: Uuid,
        session_wallet: Pubkey,
        last_valid_block_height: u64,
    },
    /// Phase 4C-7 — `getSignatureStatuses` returned `confirmation_status = "finalized"`.
    /// The containing block is rooted. **Terminal success.**
    Finalized {
        tx_signature: String,
        slot: u64,
        intent_id: Uuid,
        session_wallet: Pubkey,
        last_valid_block_height: u64,
    },
    /// Phase 4C-7 — `getSignatureStatuses` returned a non-null `err`.
    /// The transaction landed on-chain but execution failed.
    /// **Terminal failure.**
    ExecutionFailed {
        tx_signature: String,
        intent_id: Uuid,
        session_wallet: Pubkey,
        err: String,
    },
    /// Phase 4C-7 — current block height exceeded `last_valid_block_height`
    /// while the tx was still unobserved. **Terminal failure.** Requires
    /// a new signing handoff (re-propose) if the user wants to try again.
    ConfirmationTimeout {
        tx_signature: String,
        last_valid_block_height: u64,
        current_block_height: u64,
        reason: String,
    },
    /// Pre-4C-7: blockhash / signing TTL elapsed before submit.
    Expired {
        reason: String,
    },
    /// Pre-4C-7: verification steps A–F failed.
    Rejected {
        error_type: String,
        message: String,
    },
    /// Pre-4C-7: verification passed; broadcast itself failed.
    BroadcastFailed {
        error_type: String,
        message: String,
    },
}

impl RecordedSubmitOutcome {
    /// Monotonic-progress precedence. Higher value = more terminal.
    ///
    /// Pre-confirmation terminals (`Rejected`, `BroadcastFailed`,
    /// `Expired`) are hard-locked at precedence 100 so a
    /// confirmation-tracker observation cannot overwrite them. Terminal
    /// confirmation states (`Finalized`, `ExecutionFailed`,
    /// `ConfirmationTimeout`) rank higher than `Confirming` which
    /// ranks higher than `Submitted`.
    pub fn precedence(&self) -> u32 {
        match self {
            RecordedSubmitOutcome::Submitted { .. } => 10,
            RecordedSubmitOutcome::Confirming { .. } => 20,
            RecordedSubmitOutcome::Finalized { .. } => 90,
            RecordedSubmitOutcome::ExecutionFailed { .. } => 90,
            RecordedSubmitOutcome::ConfirmationTimeout { .. } => 90,
            RecordedSubmitOutcome::Expired { .. }
            | RecordedSubmitOutcome::Rejected { .. }
            | RecordedSubmitOutcome::BroadcastFailed { .. } => 100,
        }
    }

    /// `true` when this outcome is a hard terminal that the tracker
    /// MUST NOT overwrite. `Submitted` and `Confirming` are NOT
    /// terminal — polling continues until one of the terminal variants
    /// is observed.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RecordedSubmitOutcome::Finalized { .. }
                | RecordedSubmitOutcome::ExecutionFailed { .. }
                | RecordedSubmitOutcome::ConfirmationTimeout { .. }
                | RecordedSubmitOutcome::Expired { .. }
                | RecordedSubmitOutcome::Rejected { .. }
                | RecordedSubmitOutcome::BroadcastFailed { .. }
        )
    }

    /// `true` iff this outcome represents successful finalization.
    pub fn is_success(&self) -> bool {
        matches!(self, RecordedSubmitOutcome::Finalized { .. })
    }

    /// Best-effort extraction of the on-chain signature string, if any
    /// variant carries one.
    pub fn tx_signature(&self) -> Option<&str> {
        match self {
            RecordedSubmitOutcome::Submitted { tx_signature, .. }
            | RecordedSubmitOutcome::Confirming { tx_signature, .. }
            | RecordedSubmitOutcome::Finalized { tx_signature, .. }
            | RecordedSubmitOutcome::ExecutionFailed { tx_signature, .. }
            | RecordedSubmitOutcome::ConfirmationTimeout { tx_signature, .. } => Some(tx_signature),
            RecordedSubmitOutcome::Expired { .. }
            | RecordedSubmitOutcome::Rejected { .. }
            | RecordedSubmitOutcome::BroadcastFailed { .. } => None,
        }
    }
}

/// One cached lifecycle record. `session_id` is the owner of the
/// original signing request; retrieval requires an exact session
/// match. `expires_at` is the cache TTL — distinct from the broadcast's
/// chain-side `last_valid_block_height`. Exceeding this TTL only means
/// the UX-recovery cache drops the record; the on-chain outcome is
/// unaffected.
pub struct RecordedSubmission {
    pub session_id: SessionId,
    pub outcome: RecordedSubmitOutcome,
    pub recorded_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Result of a recovery lookup. `session_id` is already matched inside
/// [`SolendSubmissionLifecycleStore::lookup_for_session`]; callers
/// never need to re-check it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredSubmission {
    pub signing_request_id: Uuid,
    pub outcome: RecordedSubmitOutcome,
    pub recorded_at: DateTime<Utc>,
}

/// In-memory Solend submission-outcome cache. DashMap-backed for
/// non-awaiting lock discipline (matches `SolendSigningStore` and
/// `SolendParkStore`).
///
/// **Idempotent record.** If `record()` is called twice with the same
/// `signing_request_id`, the first write wins. This is the correct
/// shape for UX recovery: once a broadcast has returned a signature,
/// that signature is authoritative and must not be overwritten by a
/// later misrouted attempt. The same applies for terminal failures.
///
/// **Lazy TTL.** `lookup_for_session` sweeps expired entries on read,
/// matching the rest of the Solend-side store ecosystem. Explicit
/// `cleanup_expired()` is available for operator-initiated sweeps.
#[derive(Clone)]
pub struct SolendSubmissionLifecycleStore {
    inner: Arc<DashMap<Uuid, RecordedSubmission>>,
    recovery_ttl: Duration,
}

impl SolendSubmissionLifecycleStore {
    /// Construct with the given recovery TTL. Production wires this
    /// from config (recommended: same as the signing lease, or
    /// slightly longer so a retry after the signing window expires
    /// still sees the cached terminal state rather than a blank 404).
    pub fn new(recovery_ttl_seconds: u64) -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            recovery_ttl: Duration::seconds(recovery_ttl_seconds as i64),
        }
    }

    /// Record an initial outcome. **First write wins** — subsequent
    /// calls with the same `signing_request_id` return `false` and do
    /// NOT mutate the stored record.
    ///
    /// Returns `true` iff this call actually inserted a new record.
    ///
    /// For Phase 4C-7 lifecycle advances (Submitted → Confirming →
    /// Finalized / ExecutionFailed / ConfirmationTimeout), use
    /// [`Self::transition`] instead. `record` is for the initial submit
    /// outcome only.
    pub fn record(
        &self,
        signing_request_id: Uuid,
        session_id: SessionId,
        outcome: RecordedSubmitOutcome,
    ) -> bool {
        let now = Utc::now();
        let expires_at = now + self.recovery_ttl;
        // DashMap::entry().or_insert_with() is atomic at the shard
        // level, which is exactly the property we need for first-write-
        // wins. Checking `is_vacant()` before insert is racy.
        let mut inserted = false;
        self.inner
            .entry(signing_request_id)
            .or_insert_with(|| {
                inserted = true;
                RecordedSubmission {
                    session_id,
                    outcome,
                    recorded_at: now,
                    expires_at,
                }
            });
        inserted
    }

    /// Phase 4C-7 — advance a Submitted/Confirming entry to a later
    /// state. **Monotonic**: returns `false` when the incoming outcome
    /// does not rank strictly higher than the stored one. Hard
    /// terminals (Finalized / ExecutionFailed / ConfirmationTimeout /
    /// Rejected / Expired / BroadcastFailed) are never overwritten.
    ///
    /// Enforces session-scoping: a mismatched `session_id` returns
    /// `false` even if the request_id exists.
    ///
    /// Returns `true` iff the stored outcome was actually replaced.
    pub fn transition(
        &self,
        signing_request_id: Uuid,
        session_id: &SessionId,
        new_outcome: RecordedSubmitOutcome,
    ) -> bool {
        let mut entry = match self.inner.get_mut(&signing_request_id) {
            Some(e) => e,
            None => return false,
        };
        if &entry.session_id != session_id {
            return false;
        }
        if entry.outcome.is_terminal() {
            return false;
        }
        if new_outcome.precedence() <= entry.outcome.precedence() {
            return false;
        }
        entry.outcome = new_outcome;
        true
    }

    /// Diagnostic check: `true` iff the stored outcome for this
    /// request_id is a hard terminal (does not sweep expired entries;
    /// does not enforce session-scoping — this is a lightweight check
    /// used by the confirmation tracker to avoid re-enqueuing a
    /// signature whose submit path already produced a terminal result
    /// pre-confirmation, OR whose confirmation has already landed).
    pub fn outcome_is_terminal(&self, signing_request_id: Uuid) -> bool {
        self.inner
            .get(&signing_request_id)
            .map(|e| e.outcome.is_terminal())
            .unwrap_or(false)
    }

    /// Snapshot of every non-expired non-terminal entry (Submitted /
    /// Confirming) — used by boot-up recovery to re-enqueue signatures
    /// into the Solend confirmation tracker after handler construction.
    ///
    /// Returns `(signing_request_id, session_id, outcome_clone)` so the
    /// caller can rebuild the tracker context without holding any
    /// DashMap guard.
    pub fn non_terminal_snapshot(&self) -> Vec<(Uuid, SessionId, RecordedSubmitOutcome)> {
        let now = Utc::now();
        self.inner
            .iter()
            .filter(|e| e.expires_at > now && !e.outcome.is_terminal())
            .map(|e| (*e.key(), e.session_id.clone(), e.outcome.clone()))
            .collect()
    }

    /// Session-scoped lookup. Returns `Some(record)` only when the
    /// cached record exists, has not expired, AND its `session_id`
    /// matches. Mismatched sessions receive `None` without sweeping
    /// the legitimate owner's entry.
    pub fn lookup_for_session(
        &self,
        session_id: &SessionId,
        signing_request_id: Uuid,
    ) -> Option<RecoveredSubmission> {
        let now = Utc::now();
        let entry = self.inner.get(&signing_request_id)?;
        if entry.expires_at <= now {
            drop(entry);
            self.inner.remove(&signing_request_id);
            return None;
        }
        if &entry.session_id != session_id {
            return None;
        }
        Some(RecoveredSubmission {
            signing_request_id,
            outcome: entry.outcome.clone(),
            recorded_at: entry.recorded_at,
        })
    }

    /// Whether a record exists for this id (regardless of session /
    /// expiration). Diagnostic; routes should prefer
    /// [`Self::lookup_for_session`].
    pub fn contains(&self, signing_request_id: &Uuid) -> bool {
        self.inner.contains_key(signing_request_id)
    }

    /// Cached record count, including un-swept expired entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Explicit sweep. Returns the number of entries removed.
    pub fn cleanup_expired(&self) -> usize {
        let now = Utc::now();
        let expired: Vec<Uuid> = self
            .inner
            .iter()
            .filter(|e| e.expires_at <= now)
            .map(|e| *e.key())
            .collect();
        let removed = expired.len();
        for rid in expired {
            self.inner.remove(&rid);
        }
        removed
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_submitted() -> RecordedSubmitOutcome {
        RecordedSubmitOutcome::Submitted {
            tx_signature: "5Gx7...".to_string(),
            intent_id: Uuid::new_v4(),
            session_wallet: Pubkey::new_unique(),
            verified_slot: 100,
            last_valid_block_height: 200,
        }
    }

    #[test]
    fn record_is_first_write_wins() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let session = SessionId::new();

        let first = RecordedSubmitOutcome::Submitted {
            tx_signature: "FIRST".to_string(),
            intent_id: Uuid::new_v4(),
            session_wallet: Pubkey::new_unique(),
            verified_slot: 1,
            last_valid_block_height: 2,
        };
        let second = RecordedSubmitOutcome::Rejected {
            error_type: "SHOULD_BE_IGNORED".to_string(),
            message: "second write must not clobber first".to_string(),
        };

        assert!(store.record(rid, session.clone(), first.clone()));
        assert!(!store.record(rid, session.clone(), second.clone()));

        let recovered = store.lookup_for_session(&session, rid).unwrap();
        assert_eq!(recovered.outcome, first);
    }

    #[test]
    fn lookup_for_session_rejects_wrong_session_without_sweeping() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        let attacker = SessionId::new();
        assert!(store.record(rid, owner.clone(), rec_submitted()));

        assert!(store.lookup_for_session(&attacker, rid).is_none());
        assert!(store.contains(&rid), "entry not swept by cross-session miss");
        assert!(store.lookup_for_session(&owner, rid).is_some());
    }

    #[test]
    fn expired_entries_sweep_on_lookup() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        assert!(store.record(rid, owner.clone(), rec_submitted()));

        {
            let mut entry = store.inner.get_mut(&rid).unwrap();
            entry.expires_at = Utc::now() - Duration::seconds(1);
        }
        assert!(store.lookup_for_session(&owner, rid).is_none());
        assert!(!store.contains(&rid));
    }

    #[test]
    fn cleanup_expired_removes_only_past_ttl() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let alive = Uuid::new_v4();
        let dead = Uuid::new_v4();
        let owner = SessionId::new();
        store.record(alive, owner.clone(), rec_submitted());
        store.record(dead, owner.clone(), rec_submitted());
        {
            let mut e = store.inner.get_mut(&dead).unwrap();
            e.expires_at = Utc::now() - Duration::seconds(1);
        }
        assert_eq!(store.cleanup_expired(), 1);
        assert!(store.contains(&alive));
        assert!(!store.contains(&dead));
    }

    #[test]
    fn record_preserves_variant_details() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        let br = RecordedSubmitOutcome::BroadcastFailed {
            error_type: "RPC_TIMEOUT".to_string(),
            message: "connection reset".to_string(),
        };
        store.record(rid, owner.clone(), br.clone());
        let got = store.lookup_for_session(&owner, rid).unwrap();
        assert_eq!(got.outcome, br);
    }

    // ── 4C-7 transition tests ──────────────────────────────────────────────

    fn rec_confirming(sig: &str, slot: u64) -> RecordedSubmitOutcome {
        RecordedSubmitOutcome::Confirming {
            tx_signature: sig.to_string(),
            slot,
            intent_id: Uuid::new_v4(),
            session_wallet: Pubkey::new_unique(),
            last_valid_block_height: 200,
        }
    }

    fn rec_finalized(sig: &str, slot: u64) -> RecordedSubmitOutcome {
        RecordedSubmitOutcome::Finalized {
            tx_signature: sig.to_string(),
            slot,
            intent_id: Uuid::new_v4(),
            session_wallet: Pubkey::new_unique(),
            last_valid_block_height: 200,
        }
    }

    #[test]
    fn transition_advances_submitted_to_confirming() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        assert!(store.record(rid, owner.clone(), rec_submitted()));
        assert!(store.transition(rid, &owner, rec_confirming("S", 10)));
        assert!(matches!(
            store.lookup_for_session(&owner, rid).unwrap().outcome,
            RecordedSubmitOutcome::Confirming { .. }
        ));
    }

    #[test]
    fn transition_advances_confirming_to_finalized_and_locks() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        store.record(rid, owner.clone(), rec_submitted());
        assert!(store.transition(rid, &owner, rec_confirming("S", 10)));
        assert!(store.transition(rid, &owner, rec_finalized("S", 11)));

        // Terminal — no further transition accepted.
        assert!(!store.transition(rid, &owner, rec_confirming("S", 12)));
        let got = store.lookup_for_session(&owner, rid).unwrap();
        assert!(matches!(got.outcome, RecordedSubmitOutcome::Finalized { .. }));
        assert!(got.outcome.is_success());
    }

    #[test]
    fn transition_refuses_backward_moves() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        store.record(rid, owner.clone(), rec_submitted());
        store.transition(rid, &owner, rec_confirming("S", 10));
        // Try to regress to Submitted.
        let backward = RecordedSubmitOutcome::Submitted {
            tx_signature: "BACKWARD".to_string(),
            intent_id: Uuid::new_v4(),
            session_wallet: Pubkey::new_unique(),
            verified_slot: 5,
            last_valid_block_height: 200,
        };
        assert!(!store.transition(rid, &owner, backward));
        assert!(matches!(
            store.lookup_for_session(&owner, rid).unwrap().outcome,
            RecordedSubmitOutcome::Confirming { .. }
        ));
    }

    #[test]
    fn transition_cannot_overwrite_pre_confirmation_terminal() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        let br = RecordedSubmitOutcome::BroadcastFailed {
            error_type: "RPC_TIMEOUT".to_string(),
            message: "network".to_string(),
        };
        store.record(rid, owner.clone(), br);
        // Even a Finalized observation should NOT overwrite a pre-confirmation terminal.
        assert!(!store.transition(rid, &owner, rec_finalized("S", 1)));
        assert!(matches!(
            store.lookup_for_session(&owner, rid).unwrap().outcome,
            RecordedSubmitOutcome::BroadcastFailed { .. }
        ));
    }

    #[test]
    fn transition_refuses_cross_session() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let rid = Uuid::new_v4();
        let owner = SessionId::new();
        let attacker = SessionId::new();
        store.record(rid, owner.clone(), rec_submitted());
        assert!(!store.transition(rid, &attacker, rec_finalized("S", 1)));
        assert!(matches!(
            store.lookup_for_session(&owner, rid).unwrap().outcome,
            RecordedSubmitOutcome::Submitted { .. }
        ));
    }

    #[test]
    fn non_terminal_snapshot_excludes_terminal_and_expired() {
        let store = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid_sub = Uuid::new_v4();
        let rid_conf = Uuid::new_v4();
        let rid_final = Uuid::new_v4();
        let rid_rej = Uuid::new_v4();
        let rid_expired_cache = Uuid::new_v4();

        store.record(rid_sub, owner.clone(), rec_submitted());
        store.record(rid_conf, owner.clone(), rec_confirming("A", 1));
        store.record(rid_final, owner.clone(), rec_finalized("B", 2));
        store.record(
            rid_rej,
            owner.clone(),
            RecordedSubmitOutcome::Rejected {
                error_type: "X".into(),
                message: "y".into(),
            },
        );
        store.record(rid_expired_cache, owner.clone(), rec_submitted());
        // Expire the cache TTL on the last one.
        {
            let mut e = store.inner.get_mut(&rid_expired_cache).unwrap();
            e.expires_at = Utc::now() - Duration::seconds(1);
        }

        let snap = store.non_terminal_snapshot();
        let ids: Vec<Uuid> = snap.iter().map(|(r, _, _)| *r).collect();
        assert!(ids.contains(&rid_sub));
        assert!(ids.contains(&rid_conf));
        assert!(!ids.contains(&rid_final), "terminal Finalized excluded");
        assert!(!ids.contains(&rid_rej), "terminal Rejected excluded");
        assert!(
            !ids.contains(&rid_expired_cache),
            "cache-expired entry excluded"
        );
    }

    #[test]
    fn precedence_and_is_terminal_are_consistent() {
        assert!(!rec_submitted().is_terminal());
        assert!(!rec_confirming("A", 1).is_terminal());
        assert!(rec_finalized("A", 1).is_terminal());
        assert!(rec_finalized("A", 1).is_success());
        assert!(RecordedSubmitOutcome::ExecutionFailed {
            tx_signature: "X".into(),
            intent_id: Uuid::new_v4(),
            session_wallet: Pubkey::new_unique(),
            err: "Custom(42)".into(),
        }
        .is_terminal());
        assert!(RecordedSubmitOutcome::ConfirmationTimeout {
            tx_signature: "X".into(),
            last_valid_block_height: 100,
            current_block_height: 200,
            reason: "blockhash past".into(),
        }
        .is_terminal());
    }

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("solend_lifecycle.rs");
        // Needles use `.` method-call shape so the scanner never matches
        // its own source — including doc-comment mentions like
        // `getSignatureStatuses` which describe the tracker contract.
        let needles = [
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", ".send_raw_", "transaction("),
            format!("{}{}", ".send_raw_v0_", "transaction("),
            format!("{}{}", ".confirm_", "transaction("),
            format!("{}{}", ".get_signature_", "statuses("),
            format!("{}{}", "Versioned", "Transaction"),
            format!("{}{}", "Message", "V0"),
            format!("{}{}", "AddressLookup", "Table"),
            format!("{}{}", "tokio::", "spawn("),
        ];
        for n in &needles {
            assert!(!SOURCE.contains(n), "solend_lifecycle.rs must not contain `{n}`");
        }
    }
}

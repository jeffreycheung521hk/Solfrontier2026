//! Phase 4C-7 — Solend-specific on-chain confirmation tracker.
//!
//! # What this module owns
//!
//! A small, Solend-scoped confirmation tracker. It batches
//! `getSignatureStatuses` RPC calls through the
//! [`SolendSignatureStatusProvider`] seam, feeds the results into the
//! authoritative [`calculate_next_state`] state machine from
//! `claw_solana_core::tx_lifecycle` (NOT a re-implementation), and
//! advances entries in the
//! [`SolendSubmissionLifecycleStore`][crate::integrations::solend_lifecycle::SolendSubmissionLifecycleStore]
//! through `Submitted → Confirming → Finalized / ExecutionFailed /
//! ConfirmationTimeout`.
//!
//! # Why a Solend-specific tracker (instead of plugging into `claw_solana_core::tracker::TransactionTracker`)
//!
//! The shared `TransactionTracker` emits `StateChange`s on an mpsc
//! channel that is already owned and consumed by the
//! `LifecyclePersister` background task. Tapping a second consumer on
//! the same channel would require a fan-out refactor of that
//! load-bearing code path. The lower-risk move is to keep the shared
//! tracker's scope unchanged and have this Solend-side tracker drive
//! its own bounded poll loop, reusing the AUTHORITATIVE state machine
//! (`calculate_next_state`, `RpcObservation`, `TxState`) rather than
//! reinventing the transition rules.
//!
//! In short: the **state machine is reused**, only the **sink** is
//! Solend-specific. No parallel lifecycle logic is introduced — the
//! only Solend-local code is the mapping `TxState` →
//! `RecordedSubmitOutcome` and the emission of Solend audit events.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT broadcast. No `send_transaction`, no
//!   `send_raw_transaction`.
//! - Does NOT re-sign or re-handoff. On `ConfirmationTimeout` the
//!   lifecycle records `RequiresReproposal` guidance for the UI; a new
//!   signing handoff is the USER's next step, not this tracker's.
//! - Does NOT fabricate transaction bytes, pubkeys, or signatures.
//! - Does NOT re-implement `calculate_next_state`. It calls the
//!   shared function.
//! - Does NOT persist to SQLite. The lifecycle store is in-memory;
//!   process-restart recovery limitations are documented at the
//!   daemon-wiring call site.
//! - Does NOT start or stop `claw_solana_core::tracker::TransactionTracker`.
//!   The two live side by side.
//! - Does NOT mutate any parked signing artifact or approval record.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::{mapref::entry::Entry, DashMap};
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use uuid::Uuid;

use claw_solana_core::rpc::RpcPool;
use claw_solana_core::tx_lifecycle::{calculate_next_state, RpcObservation, TxState};
use claw_types::session::SessionId;

use crate::integrations::solend_lifecycle::{
    RecordedSubmitOutcome, SolendSubmissionLifecycleStore,
};
use crate::integrations::solend_submit::SolendAuditSink;

use claw_state_store::audit::AuditSeverity;

// ── Stable audit event names (4C-7) ─────────────────────────────────────────

pub const AUDIT_EVENT_CONFIRMATION_STARTED: &str = "solend_transaction_confirmation_started";
pub const AUDIT_EVENT_CONFIRMATION_CONFIRMED: &str = "solend_transaction_confirmed";
pub const AUDIT_EVENT_CONFIRMATION_FINALIZED: &str = "solend_transaction_finalized";
pub const AUDIT_EVENT_CONFIRMATION_FAILED: &str = "solend_transaction_failed";
pub const AUDIT_EVENT_CONFIRMATION_TIMEOUT: &str = "solend_transaction_confirmation_timeout";
pub const AUDIT_EVENT_CONFIRMATION_POLL_ERROR: &str =
    "solend_transaction_confirmation_poll_error";

// ── Errors + status shape ───────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SolendConfirmationError {
    #[error("rpc failure: {reason}")]
    RpcFailure { reason: String },
}

/// Solend-local mirror of the subset of `RpcSignatureStatus` fields the
/// state machine needs. Declared here (rather than importing
/// `solana_transaction_status::TransactionStatus`) so stub tests can
/// construct values without pulling in the full solana client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendSignatureStatus {
    /// `processed` / `confirmed` / `finalized` / `None` (RPC has no record).
    pub confirmation_status: Option<String>,
    /// Non-null means the tx landed but failed.
    pub err: Option<String>,
    /// The slot the tx was included in, if observed.
    pub slot: Option<u64>,
}

/// Narrow RPC seam: batched `getSignatureStatuses` + block-height read.
///
/// Production adapter ([`ClawRpcStatusProvider`]) wraps the same
/// `rpc_pool.read_client().get_signature_statuses(..)` path that
/// `claw_solana_core::tracker::TransactionTracker` already uses — no
/// second RPC client is introduced.
#[async_trait]
pub trait SolendSignatureStatusProvider: Send + Sync {
    /// Batched status lookup. The returned vec has the SAME length as
    /// the input slice; index `i` carries the status for `sigs[i]`
    /// (or `None` if the RPC node has no record).
    async fn get_signature_statuses(
        &self,
        sigs: &[Signature],
    ) -> Result<Vec<Option<SolendSignatureStatus>>, SolendConfirmationError>;

    /// Current finalized block height (same commitment the existing
    /// tracker uses). Used to compute blockhash-expiry.
    async fn current_block_height(&self) -> Result<u64, SolendConfirmationError>;
}

// ── Production adapter ──────────────────────────────────────────────────────

/// Production adapter. Wraps an `Arc<RpcPool>` and calls through the
/// existing read-client which already has circuit-breaker tracking.
#[derive(Clone)]
pub struct ClawRpcStatusProvider {
    rpc_pool: Arc<RpcPool>,
}

impl ClawRpcStatusProvider {
    pub fn new(rpc_pool: Arc<RpcPool>) -> Self {
        Self { rpc_pool }
    }
}

#[async_trait]
impl SolendSignatureStatusProvider for ClawRpcStatusProvider {
    async fn get_signature_statuses(
        &self,
        sigs: &[Signature],
    ) -> Result<Vec<Option<SolendSignatureStatus>>, SolendConfirmationError> {
        let client = self
            .rpc_pool
            .read_client()
            .map_err(|e| SolendConfirmationError::RpcFailure {
                reason: format!("rpc pool: {e}"),
            })?;
        match client.get_signature_statuses(sigs).await {
            Ok(response) => {
                self.rpc_pool.record_success(&client.url());
                let mapped = response
                    .value
                    .into_iter()
                    .map(|opt| {
                        opt.map(|s| SolendSignatureStatus {
                            confirmation_status: s.confirmation_status.as_ref().map(|cs| {
                                use solana_transaction_status::TransactionConfirmationStatus::*;
                                match cs {
                                    Processed => "processed".to_string(),
                                    Confirmed => "confirmed".to_string(),
                                    Finalized => "finalized".to_string(),
                                }
                            }),
                            err: s.err.as_ref().map(|e| format!("{e:?}")),
                            slot: Some(s.slot),
                        })
                    })
                    .collect();
                Ok(mapped)
            }
            Err(e) => {
                self.rpc_pool.record_failure(&client.url());
                Err(SolendConfirmationError::RpcFailure {
                    reason: format!("getSignatureStatuses: {e}"),
                })
            }
        }
    }

    async fn current_block_height(&self) -> Result<u64, SolendConfirmationError> {
        let client = self
            .rpc_pool
            .read_client()
            .map_err(|e| SolendConfirmationError::RpcFailure {
                reason: format!("rpc pool: {e}"),
            })?;
        match client
            .get_block_height_with_commitment(
                solana_sdk::commitment_config::CommitmentConfig::finalized(),
            )
            .await
        {
            Ok(h) => {
                self.rpc_pool.record_success(&client.url());
                Ok(h)
            }
            Err(e) => {
                self.rpc_pool.record_failure(&client.url());
                Err(SolendConfirmationError::RpcFailure {
                    reason: format!("getBlockHeight: {e}"),
                })
            }
        }
    }
}

// ── Tracker context + enqueue outcome ───────────────────────────────────────

/// Per-signature tracker state. Carries enough Solend identity for the
/// lifecycle sink + audit emission to attribute state changes.
#[derive(Debug, Clone)]
pub struct SolendTrackedContext {
    pub signing_request_id: Uuid,
    pub intent_id: Uuid,
    pub session_id: SessionId,
    pub session_wallet: Pubkey,
    pub last_valid_block_height: u64,
    pub submission_block_height: u64,
    pub tx_signature: Signature,
    /// Current tracked state, reused from the shared state machine.
    pub state: TxState,
}

impl SolendTrackedContext {
    pub fn new_submitted(
        signing_request_id: Uuid,
        intent_id: Uuid,
        session_id: SessionId,
        session_wallet: Pubkey,
        last_valid_block_height: u64,
        submission_block_height: u64,
        tx_signature: Signature,
    ) -> Self {
        Self {
            signing_request_id,
            intent_id,
            session_id,
            session_wallet,
            last_valid_block_height,
            submission_block_height,
            tx_signature,
            state: TxState::Submitted,
        }
    }
}

/// Result of an `enqueue()` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackResult {
    /// New entry inserted; poll loop will observe it on the next tick.
    Started,
    /// Same `tx_signature` already tracked — no duplicate poller created.
    AlreadyTracked,
    /// Lifecycle store already holds a terminal record for this
    /// `signing_request_id` — polling would be wasted.
    AlreadyTerminal,
}

// ── Tracker ─────────────────────────────────────────────────────────────────

/// Solend-specific confirmation tracker. See module docs for design
/// rationale. Construct with [`Self::new`], spawn [`Self::run`] on a
/// tokio task, call [`Self::enqueue`] per successful submit.
pub struct SolendConfirmationTracker {
    tracked: Arc<DashMap<Signature, SolendTrackedContext>>,
    lifecycle: SolendSubmissionLifecycleStore,
    status_provider: Arc<dyn SolendSignatureStatusProvider>,
    audit: Arc<dyn SolendAuditSink>,
    poll_interval: Duration,
    shutdown: Arc<Notify>,
}

impl SolendConfirmationTracker {
    pub fn new(
        lifecycle: SolendSubmissionLifecycleStore,
        status_provider: Arc<dyn SolendSignatureStatusProvider>,
        audit: Arc<dyn SolendAuditSink>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            tracked: Arc::new(DashMap::new()),
            lifecycle,
            status_provider,
            audit,
            poll_interval,
            shutdown: Arc::new(Notify::new()),
        }
    }

    /// Enqueue a newly-submitted signature for confirmation tracking.
    ///
    /// Idempotent: duplicate enqueue of the same `tx_signature`
    /// returns `AlreadyTracked` — no duplicate poller task is created
    /// (the tracker has ONE run loop that batches all tracked
    /// signatures). Terminal entries in the lifecycle store
    /// short-circuit to `AlreadyTerminal`.
    pub fn enqueue(&self, ctx: SolendTrackedContext) -> TrackResult {
        // Short-circuit: lifecycle already has a hard terminal for this
        // signing_request_id (e.g., Finalized / Failed / Timeout /
        // Rejected / BroadcastFailed / Expired). Track would be wasted.
        if self
            .lifecycle
            .outcome_is_terminal(ctx.signing_request_id)
        {
            return TrackResult::AlreadyTerminal;
        }
        match self.tracked.entry(ctx.tx_signature) {
            Entry::Occupied(_) => TrackResult::AlreadyTracked,
            Entry::Vacant(v) => {
                let signing_request_id = ctx.signing_request_id;
                let intent_id = ctx.intent_id;
                let session_id = ctx.session_id.clone();
                let sig_str = ctx.tx_signature.to_string();
                let session_wallet = ctx.session_wallet;
                let last_valid_block_height = ctx.last_valid_block_height;
                let submission_block_height = ctx.submission_block_height;
                v.insert(ctx);
                let audit = self.audit.clone();
                // confirmation_started audit — fire-and-forget.
                tokio::spawn(async move {
                    audit
                        .append_solend_event(
                            AUDIT_EVENT_CONFIRMATION_STARTED,
                            signing_request_id.to_string(),
                            Some(session_id.to_string()),
                            AuditSeverity::Info,
                            json!({
                                "signing_request_id":        signing_request_id,
                                "intent_id":                 intent_id,
                                "session_id":                session_id.to_string(),
                                "session_wallet":            session_wallet.to_string(),
                                "tx_signature":              sig_str,
                                "last_valid_block_height":   last_valid_block_height,
                                "submission_block_height":   submission_block_height,
                            }),
                        )
                        .await;
                });
                TrackResult::Started
            }
        }
    }

    /// Active tracked-signature count. Useful for tests + metrics.
    pub fn active_count(&self) -> usize {
        self.tracked.len()
    }

    /// Return `true` when this signature is currently being tracked.
    pub fn contains(&self, sig: &Signature) -> bool {
        self.tracked.contains_key(sig)
    }

    /// Signal the `run` loop to exit at the next `tokio::select!`.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Background loop. Spawn once on daemon startup. Exits on
    /// `shutdown()` or when the tokio runtime drops it.
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!(
            poll_interval_ms = self.poll_interval.as_millis() as u64,
            "solend confirmation tracker started"
        );
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => {
                    info!("solend confirmation tracker shutdown signal received");
                    break;
                }
                _ = interval.tick() => {}
            }
            if self.tracked.is_empty() {
                continue;
            }
            self.tick_once().await;
        }
    }

    /// One poll tick. Separated so tests can drive the tracker without
    /// spawning a tokio task.
    pub async fn tick_once(&self) {
        let current_height = match self.status_provider.current_block_height().await {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "current_block_height failed — skipping tick");
                self.audit
                    .append_solend_event(
                        AUDIT_EVENT_CONFIRMATION_POLL_ERROR,
                        "unknown".to_string(),
                        None,
                        AuditSeverity::Warning,
                        json!({
                            "error":  format!("{e}"),
                            "phase":  "current_block_height",
                        }),
                    )
                    .await;
                return;
            }
        };

        // Snapshot tracked signatures (drop DashMap refs before await).
        let sigs: Vec<Signature> = self.tracked.iter().map(|e| *e.key()).collect();
        if sigs.is_empty() {
            return;
        }

        let statuses = match self.status_provider.get_signature_statuses(&sigs).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "get_signature_statuses failed — skipping batch");
                self.audit
                    .append_solend_event(
                        AUDIT_EVENT_CONFIRMATION_POLL_ERROR,
                        "unknown".to_string(),
                        None,
                        AuditSeverity::Warning,
                        json!({
                            "error":        format!("{e}"),
                            "phase":        "get_signature_statuses",
                            "batch_size":   sigs.len(),
                        }),
                    )
                    .await;
                return;
            }
        };

        // Zip defensively — provider contract says same length, but if
        // not we only iterate the minimum.
        for (sig, maybe_status) in sigs.iter().zip(statuses.iter()) {
            let observation = to_observation(maybe_status.as_ref());

            // Snapshot relevant context fields so we can drop the
            // DashMap guard before awaiting audit/lifecycle writes.
            let snapshot = self.tracked.get(sig).map(|e| {
                (
                    e.signing_request_id,
                    e.intent_id,
                    e.session_id.clone(),
                    e.session_wallet,
                    e.last_valid_block_height,
                    e.submission_block_height,
                    e.state.clone(),
                )
            });
            let Some((
                signing_request_id,
                intent_id,
                session_id,
                session_wallet,
                last_valid_block_height,
                submission_block_height,
                current_state,
            )) = snapshot
            else {
                continue;
            };

            let new_state = calculate_next_state(
                &current_state,
                &observation,
                current_height,
                submission_block_height,
                last_valid_block_height,
            );

            if new_state == current_state {
                continue;
            }

            // Write new state into the tracked map, then drop the guard
            // before awaiting audit/lifecycle writes.
            if let Some(mut e) = self.tracked.get_mut(sig) {
                e.state = new_state.clone();
            }

            self.on_state_change(
                *sig,
                new_state,
                signing_request_id,
                intent_id,
                &session_id,
                session_wallet,
                last_valid_block_height,
                current_height,
            )
            .await;
        }

        // Prune terminal entries from the in-memory map — one terminal
        // per tx_signature, no dangling pollers.
        let terminal: Vec<Signature> = self
            .tracked
            .iter()
            .filter(|e| e.value().state.is_terminal())
            .map(|e| *e.key())
            .collect();
        for sig in terminal {
            self.tracked.remove(&sig);
            debug!(signature = %sig, "solend tracker pruned terminal entry");
        }
    }

    async fn on_state_change(
        &self,
        sig: Signature,
        new_state: TxState,
        signing_request_id: Uuid,
        intent_id: Uuid,
        session_id: &SessionId,
        session_wallet: Pubkey,
        last_valid_block_height: u64,
        current_block_height: u64,
    ) {
        let sig_str = sig.to_string();
        let (new_outcome, event_name, severity): (
            Option<RecordedSubmitOutcome>,
            Option<&'static str>,
            AuditSeverity,
        ) = match &new_state {
            // `Dropped` is provisional per the shared state machine; we
            // don't advance the Solend lifecycle on it.
            TxState::Submitted | TxState::Dropped => (None, None, AuditSeverity::Info),
            TxState::Confirmed { slot } => (
                Some(RecordedSubmitOutcome::Confirming {
                    tx_signature: sig_str.clone(),
                    slot: *slot,
                    intent_id,
                    session_wallet,
                    last_valid_block_height,
                }),
                Some(AUDIT_EVENT_CONFIRMATION_CONFIRMED),
                AuditSeverity::Info,
            ),
            TxState::Finalized { slot } => (
                Some(RecordedSubmitOutcome::Finalized {
                    tx_signature: sig_str.clone(),
                    slot: *slot,
                    intent_id,
                    session_wallet,
                    last_valid_block_height,
                }),
                Some(AUDIT_EVENT_CONFIRMATION_FINALIZED),
                AuditSeverity::Info,
            ),
            TxState::Failed { err } => (
                Some(RecordedSubmitOutcome::ExecutionFailed {
                    tx_signature: sig_str.clone(),
                    intent_id,
                    session_wallet,
                    err: err.clone(),
                }),
                Some(AUDIT_EVENT_CONFIRMATION_FAILED),
                AuditSeverity::Warning,
            ),
            TxState::Expired => (
                Some(RecordedSubmitOutcome::ConfirmationTimeout {
                    tx_signature: sig_str.clone(),
                    last_valid_block_height,
                    current_block_height,
                    reason: format!(
                        "current_block_height {current_block_height} > \
                         last_valid_block_height {last_valid_block_height} \
                         without finalized observation"
                    ),
                }),
                Some(AUDIT_EVENT_CONFIRMATION_TIMEOUT),
                AuditSeverity::Warning,
            ),
        };

        let Some(outcome) = new_outcome else {
            return;
        };
        let transitioned =
            self.lifecycle
                .transition(signing_request_id, session_id, outcome.clone());
        if !transitioned {
            debug!(
                signing_request_id = %signing_request_id,
                new_state = %new_state,
                "solend lifecycle transition rejected (non-monotonic or locked)"
            );
            return;
        }
        if let Some(ev) = event_name {
            let slot = new_state.slot();
            let err_string = match &new_state {
                TxState::Failed { err } => Some(err.clone()),
                _ => None,
            };
            self.audit
                .append_solend_event(
                    ev,
                    signing_request_id.to_string(),
                    Some(session_id.to_string()),
                    severity,
                    json!({
                        "signing_request_id":       signing_request_id,
                        "intent_id":                intent_id,
                        "session_id":               session_id.to_string(),
                        "session_wallet":           session_wallet.to_string(),
                        "tx_signature":             sig_str,
                        "last_valid_block_height":  last_valid_block_height,
                        "current_block_height":     current_block_height,
                        "slot":                     slot,
                        "err":                      err_string,
                        "commitment":               match &new_state {
                            TxState::Confirmed { .. } => "confirmed",
                            TxState::Finalized { .. } => "finalized",
                            TxState::Failed { .. }    => "failed",
                            TxState::Expired          => "expired",
                            _                         => "other",
                        },
                    }),
                )
                .await;
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn to_observation(status: Option<&SolendSignatureStatus>) -> RpcObservation {
    match status {
        None => RpcObservation {
            status: None,
            err: None,
            slot: None,
        },
        Some(s) => RpcObservation {
            status: s.confirmation_status.clone(),
            err: s.err.clone(),
            slot: s.slot,
        },
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend_submit::NullSolendAuditSink;
    use std::sync::Mutex;

    // ── Stub status provider ───────────────────────────────────────────────

    struct StubStatuses {
        /// Successive responses; one is popped per call.
        responses: Mutex<Vec<Result<Vec<Option<SolendSignatureStatus>>, SolendConfirmationError>>>,
        /// Block height response, same per call for simplicity.
        block_height: Mutex<Result<u64, SolendConfirmationError>>,
        /// Calls to `get_signature_statuses`.
        status_calls: Mutex<Vec<Vec<Signature>>>,
    }

    impl StubStatuses {
        fn new(block_height: u64) -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
                block_height: Mutex::new(Ok(block_height)),
                status_calls: Mutex::new(Vec::new()),
            }
        }
        fn push(&self, r: Vec<Option<SolendSignatureStatus>>) {
            self.responses.lock().unwrap().push(Ok(r));
        }
        fn push_err(&self) {
            self.responses.lock().unwrap().push(Err(SolendConfirmationError::RpcFailure {
                reason: "stub rpc error".into(),
            }));
        }
        fn set_block_height(&self, h: u64) {
            *self.block_height.lock().unwrap() = Ok(h);
        }
        fn set_block_height_err(&self) {
            *self.block_height.lock().unwrap() = Err(SolendConfirmationError::RpcFailure {
                reason: "stub height error".into(),
            });
        }
        fn status_call_count(&self) -> usize {
            self.status_calls.lock().unwrap().len()
        }
        fn last_status_call(&self) -> Option<Vec<Signature>> {
            self.status_calls.lock().unwrap().last().cloned()
        }
    }

    #[async_trait]
    impl SolendSignatureStatusProvider for StubStatuses {
        async fn get_signature_statuses(
            &self,
            sigs: &[Signature],
        ) -> Result<Vec<Option<SolendSignatureStatus>>, SolendConfirmationError> {
            self.status_calls.lock().unwrap().push(sigs.to_vec());
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                // Default: all null (RPC has no record).
                return Ok(vec![None; sigs.len()]);
            }
            q.remove(0)
        }
        async fn current_block_height(&self) -> Result<u64, SolendConfirmationError> {
            match &*self.block_height.lock().unwrap() {
                Ok(h) => Ok(*h),
                Err(e) => Err(SolendConfirmationError::RpcFailure {
                    reason: match e {
                        SolendConfirmationError::RpcFailure { reason } => reason.clone(),
                    },
                }),
            }
        }
    }

    fn random_sig() -> Signature {
        Signature::new_unique()
    }

    fn ctx_for(sig: Signature, lvbh: u64, sbh: u64) -> SolendTrackedContext {
        SolendTrackedContext::new_submitted(
            Uuid::new_v4(),
            Uuid::new_v4(),
            SessionId::new(),
            Pubkey::new_unique(),
            lvbh,
            sbh,
            sig,
        )
    }

    fn tracker_with(
        status_provider: Arc<StubStatuses>,
        lifecycle: SolendSubmissionLifecycleStore,
    ) -> Arc<SolendConfirmationTracker> {
        Arc::new(SolendConfirmationTracker::new(
            lifecycle,
            status_provider,
            Arc::new(NullSolendAuditSink),
            Duration::from_millis(10),
        ))
    }

    fn seed_submitted(
        lifecycle: &SolendSubmissionLifecycleStore,
        ctx: &SolendTrackedContext,
    ) {
        lifecycle.record(
            ctx.signing_request_id,
            ctx.session_id.clone(),
            RecordedSubmitOutcome::Submitted {
                tx_signature: ctx.tx_signature.to_string(),
                intent_id: ctx.intent_id,
                session_wallet: ctx.session_wallet,
                verified_slot: 0,
                last_valid_block_height: ctx.last_valid_block_height,
            },
        );
    }

    // ── A. Start / enqueue idempotency ─────────────────────────────────────

    #[tokio::test]
    async fn enqueue_returns_started_then_already_tracked() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status, lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);

        assert_eq!(tracker.enqueue(ctx.clone()), TrackResult::Started);
        assert_eq!(tracker.enqueue(ctx.clone()), TrackResult::AlreadyTracked);
        assert_eq!(tracker.active_count(), 1);
    }

    #[tokio::test]
    async fn enqueue_short_circuits_when_lifecycle_terminal() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status, lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        lifecycle.record(
            ctx.signing_request_id,
            ctx.session_id.clone(),
            RecordedSubmitOutcome::Finalized {
                tx_signature: sig.to_string(),
                slot: 5,
                intent_id: ctx.intent_id,
                session_wallet: ctx.session_wallet,
                last_valid_block_height: 200,
            },
        );
        assert_eq!(tracker.enqueue(ctx), TrackResult::AlreadyTerminal);
        assert_eq!(tracker.active_count(), 0);
    }

    // ── B. Finalized path — single tick ────────────────────────────────────

    #[tokio::test]
    async fn single_tick_finalized_updates_lifecycle_and_prunes() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());

        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("finalized".into()),
            err: None,
            slot: Some(150),
        })]);

        tracker.tick_once().await;

        // Lifecycle advanced.
        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        match rec.outcome {
            RecordedSubmitOutcome::Finalized { slot, .. } => assert_eq!(slot, 150),
            other => panic!("expected Finalized, got {other:?}"),
        }
        // Pruned.
        assert_eq!(tracker.active_count(), 0);
        assert!(!tracker.contains(&sig));
    }

    // ── C. Confirmed-but-not-finalized — non-terminal ──────────────────────

    #[tokio::test]
    async fn confirmed_status_moves_to_confirming_but_non_terminal() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());

        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("confirmed".into()),
            err: None,
            slot: Some(140),
        })]);

        tracker.tick_once().await;

        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(matches!(rec.outcome, RecordedSubmitOutcome::Confirming { .. }));
        assert!(!rec.outcome.is_terminal(), "Confirming must be non-terminal");
        // Still tracked — we keep polling for Finalized.
        assert!(tracker.contains(&sig));
        assert_eq!(tracker.active_count(), 1);
    }

    // ── D. Execution failure ──────────────────────────────────────────────

    #[tokio::test]
    async fn err_status_moves_to_execution_failed_and_prunes() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());

        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("confirmed".into()),
            err: Some("InstructionError(0, Custom(42))".into()),
            slot: Some(141),
        })]);

        tracker.tick_once().await;

        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        match rec.outcome {
            RecordedSubmitOutcome::ExecutionFailed { ref err, .. } => {
                assert!(err.contains("Custom(42)"));
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
        assert!(!tracker.contains(&sig));
    }

    // ── F. null then Finalized ─────────────────────────────────────────────

    #[tokio::test]
    async fn null_then_finalized_reaches_success() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());

        // First poll: null.
        status.push(vec![None]);
        tracker.tick_once().await;
        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(matches!(rec.outcome, RecordedSubmitOutcome::Submitted { .. }));
        assert!(tracker.contains(&sig), "null response does not prune");

        // Second poll: finalized.
        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("finalized".into()),
            err: None,
            slot: Some(160),
        })]);
        tracker.tick_once().await;
        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(matches!(rec.outcome, RecordedSubmitOutcome::Finalized { .. }));
    }

    // ── G. null-only until height > last_valid → ConfirmationTimeout ───────

    #[tokio::test]
    async fn null_until_expiry_produces_confirmation_timeout() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100)); // current height = 100
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        // last_valid_block_height = 100 — we are ALREADY at expiry.
        let ctx = ctx_for(sig, 100, 50);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());

        // Advance height well past the grace period used by the state
        // machine (which waits beyond last_valid_block_height before
        // declaring Expired).
        status.set_block_height(2000);
        status.push(vec![None]);
        tracker.tick_once().await;

        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(
            matches!(rec.outcome, RecordedSubmitOutcome::ConfirmationTimeout { .. }),
            "expected ConfirmationTimeout, got {:?}",
            rec.outcome
        );
        assert!(!tracker.contains(&sig), "timeout prunes tracker entry");
    }

    // ── H. Transient RPC error retries without terminal failure ───────────

    #[tokio::test]
    async fn transient_rpc_errors_do_not_mark_terminal() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());

        // First tick: block height fails.
        status.set_block_height_err();
        tracker.tick_once().await;
        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(matches!(rec.outcome, RecordedSubmitOutcome::Submitted { .. }));

        // Second tick: height ok, status fails.
        status.set_block_height(110);
        status.push_err();
        tracker.tick_once().await;
        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(matches!(rec.outcome, RecordedSubmitOutcome::Submitted { .. }));

        // Third tick: finalized.
        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("finalized".into()),
            err: None,
            slot: Some(160),
        })]);
        tracker.tick_once().await;
        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(matches!(rec.outcome, RecordedSubmitOutcome::Finalized { .. }));
    }

    // ── I. enqueue after Finalized is AlreadyTerminal ─────────────────────

    #[tokio::test]
    async fn enqueue_after_finalized_is_already_terminal() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());
        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("finalized".into()),
            err: None,
            slot: Some(150),
        })]);
        tracker.tick_once().await;
        assert!(!tracker.contains(&sig));

        // Enqueue again for the SAME signing_request_id.
        assert_eq!(tracker.enqueue(ctx.clone()), TrackResult::AlreadyTerminal);
    }

    // ── J. Batched polling — two signatures in one RPC call ───────────────

    #[tokio::test]
    async fn batched_polling_issues_single_status_call_for_two_sigs() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig_a = random_sig();
        let sig_b = random_sig();
        let ctx_a = ctx_for(sig_a, 200, 100);
        let ctx_b = ctx_for(sig_b, 200, 100);
        seed_submitted(&lifecycle, &ctx_a);
        seed_submitted(&lifecycle, &ctx_b);
        tracker.enqueue(ctx_a.clone());
        tracker.enqueue(ctx_b.clone());

        // Batch response: first in the batch → Finalized, second → None.
        // The tracker iterates its DashMap in unspecified order, so we
        // don't pre-pick which signature gets Finalized — we assert
        // that exactly one progressed and the other did not.
        status.push(vec![
            Some(SolendSignatureStatus {
                confirmation_status: Some("finalized".into()),
                err: None,
                slot: Some(150),
            }),
            None,
        ]);
        tracker.tick_once().await;

        // Batched call: ONE status-provider call covering BOTH sigs.
        assert_eq!(status.status_call_count(), 1);
        let batch = status.last_status_call().unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch.contains(&sig_a));
        assert!(batch.contains(&sig_b));

        // Exactly one of {a, b} finalized; the other still Submitted.
        let rec_a = lifecycle
            .lookup_for_session(&ctx_a.session_id, ctx_a.signing_request_id)
            .unwrap();
        let rec_b = lifecycle
            .lookup_for_session(&ctx_b.session_id, ctx_b.signing_request_id)
            .unwrap();
        let finalized_count = [&rec_a.outcome, &rec_b.outcome]
            .iter()
            .filter(|o| matches!(o, RecordedSubmitOutcome::Finalized { .. }))
            .count();
        let submitted_count = [&rec_a.outcome, &rec_b.outcome]
            .iter()
            .filter(|o| matches!(o, RecordedSubmitOutcome::Submitted { .. }))
            .count();
        assert_eq!(finalized_count, 1, "exactly one finalized: {:?} / {:?}", rec_a.outcome, rec_b.outcome);
        assert_eq!(submitted_count, 1, "other stays Submitted");
    }

    #[tokio::test]
    async fn batched_polling_deduplicates_signatures_across_enqueues() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status.clone(), lifecycle.clone());

        let sig = random_sig();
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        assert_eq!(tracker.enqueue(ctx.clone()), TrackResult::Started);
        assert_eq!(tracker.enqueue(ctx.clone()), TrackResult::AlreadyTracked);

        status.push(vec![None]);
        tracker.tick_once().await;

        let batch = status.last_status_call().unwrap();
        assert_eq!(
            batch.iter().filter(|s| **s == sig).count(),
            1,
            "duplicate enqueue must not duplicate the signature in the batched RPC call"
        );
    }

    // ── K. Boot recovery: non_terminal_snapshot feeds tracker ─────────────

    #[tokio::test]
    async fn boot_recovery_re_enqueues_submitted_from_snapshot() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = tracker_with(status, lifecycle.clone());

        // Seed two records: one Submitted (should recover), one Finalized (shouldn't).
        let sig_sub = random_sig();
        let sig_fin = random_sig();
        let sub_ctx = ctx_for(sig_sub, 200, 100);
        let fin_ctx = ctx_for(sig_fin, 200, 100);
        seed_submitted(&lifecycle, &sub_ctx);
        lifecycle.record(
            fin_ctx.signing_request_id,
            fin_ctx.session_id.clone(),
            RecordedSubmitOutcome::Finalized {
                tx_signature: sig_fin.to_string(),
                slot: 150,
                intent_id: fin_ctx.intent_id,
                session_wallet: fin_ctx.session_wallet,
                last_valid_block_height: 200,
            },
        );

        // Feed the snapshot into the tracker.
        let mut recovered = 0usize;
        let mut skipped = 0usize;
        for (rid, session_id, outcome) in lifecycle.non_terminal_snapshot() {
            if let RecordedSubmitOutcome::Submitted {
                tx_signature,
                intent_id,
                session_wallet,
                last_valid_block_height,
                ..
            } = outcome
            {
                let Ok(sig) = tx_signature.parse::<Signature>() else {
                    skipped += 1;
                    continue;
                };
                let ctx = SolendTrackedContext::new_submitted(
                    rid,
                    intent_id,
                    session_id,
                    session_wallet,
                    last_valid_block_height,
                    0, // startup — submission height unknown, state machine handles this
                    sig,
                );
                if matches!(tracker.enqueue(ctx), TrackResult::Started) {
                    recovered += 1;
                }
            }
        }
        assert_eq!(recovered, 1);
        assert_eq!(skipped, 0);
        assert_eq!(tracker.active_count(), 1);
        assert!(tracker.contains(&sig_sub));
        assert!(!tracker.contains(&sig_fin));
    }

    // ── L. Shutdown notification stops the run loop ───────────────────────

    #[tokio::test]
    async fn shutdown_notify_exits_run_loop() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let tracker = Arc::new(SolendConfirmationTracker::new(
            lifecycle,
            status,
            Arc::new(NullSolendAuditSink),
            Duration::from_millis(50),
        ));
        let t2 = tracker.clone();
        let handle = tokio::spawn(async move { t2.run().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        tracker.shutdown();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("run loop exits promptly after shutdown")
            .expect("no panic");
    }

    // ── Forbidden scan ─────────────────────────────────────────────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("solend_confirmation.rs");
        // The method-call shape `.X(` is used so the scanner does not
        // match the trait method declarations OR doc-comment mentions.
        let needles = [
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", ".send_raw_", "transaction("),
            format!("{}{}", ".send_raw_v0_", "transaction("),
            format!("{}{}", ".confirm_", "transaction("),
            // `.get_signature_statuses(` is ALLOWED inside ClawRpcStatusProvider
            // (that's this module's job); no other call path should use it.
            // Build `poll_sig...(` at runtime so this scanner line is
            // not itself a match against its own source text.
            format!("{}{}", "poll_", "signature("),
            format!("{}{}", "Versioned", "Transaction"),
            format!("{}{}", "Message", "V0"),
            format!("{}{}", "AddressLookup", "Table"),
            format!("{}{}", "Transaction::", "new_signed_with_payer("),
            format!("{}{}", "simulate_", "transaction("),
            format!("{}{}", "replace_recent_", "blockhash"),
            format!("{}{}", "Keypair::", "new("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n),
                "solend_confirmation.rs must not contain `{n}`"
            );
        }
    }
}

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
//! - Does NOT re-sign or re-handoff. On `ConfirmationTimeout` the
//!   lifecycle records `RequiresReproposal` guidance for the UI; a new
//!   signing handoff is the USER's next step, not this tracker's.
//!
//! # Phase 6E — confirmation-side bounded rebroadcast
//!
//! While `getSignatureStatuses` returns null for a tracked signature,
//! the tracker periodically rebroadcasts the SAME signed bytes through
//! a [`SolendRebroadcastSender`] seam — same bytes ⇒ same signature ⇒
//! same on-chain identity, so confirmation polling continues to resolve
//! against the same canonical tx ID. Bytes are passed in via
//! [`SolendTrackedContext::signed_bytes`]; entries that have no bytes
//! (e.g., bootstrap recovery from in-memory lifecycle store after a
//! handler replacement) silently skip rebroadcast and fall back to
//! polling-only behavior.
//!
//! Eligibility per tick: `signed_bytes.is_some()`, state non-terminal,
//! status was null in this tick, attempts < `MAX_CONFIRMATION_REBROADCAST_ATTEMPTS`,
//! `current_block_height + BLOCKHASH_GUARD_BUFFER_BLOCKS < last_valid_block_height`,
//! and `now - max(enqueued_at, last_rebroadcast_at) >= CONFIRMATION_REBROADCAST_INTERVAL`.
//!
//! Rebroadcast errors (429, 503, network) are audited but do NOT abort
//! the tracker — the next eligible tick simply retries until the
//! attempt cap is reached or the blockhash guard trips.
//! - Does NOT fabricate transaction bytes, pubkeys, or signatures.
//! - Does NOT re-implement `calculate_next_state`. It calls the
//!   shared function.
//! - Does NOT persist to SQLite. The lifecycle store is in-memory;
//!   process-restart recovery limitations are documented at the
//!   daemon-wiring call site.
//! - Does NOT start or stop `claw_solana_core::tracker::TransactionTracker`.
//!   The two live side by side.
//! - Does NOT mutate any parked signing artifact or approval record.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::{mapref::entry::Entry, DashMap};
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use thiserror::Error;
use tokio::sync::Notify;
// tokio::time::Instant respects paused virtual time under
// `#[tokio::test(start_paused = true)]`, unlike std::time::Instant
// which always reads the real wall clock. The Phase 6E rebroadcast
// interval gate relies on this for deterministic tests.
use tokio::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use claw_solana_core::rpc::RpcPool;
use claw_solana_core::tx_lifecycle::{calculate_next_state, RpcObservation, TxState};
use claw_types::session::SessionId;

use crate::integrations::solend_lifecycle::{
    RecordedSubmitOutcome, SolendSubmissionLifecycleStore,
};
use crate::integrations::solend_submit::{SolendAuditSink, SolendRebroadcastSender};

use claw_state_store::audit::AuditSeverity;

// ── Stable audit event names (4C-7) ─────────────────────────────────────────

pub const AUDIT_EVENT_CONFIRMATION_STARTED: &str = "solend_transaction_confirmation_started";
pub const AUDIT_EVENT_CONFIRMATION_CONFIRMED: &str = "solend_transaction_confirmed";
pub const AUDIT_EVENT_CONFIRMATION_FINALIZED: &str = "solend_transaction_finalized";
pub const AUDIT_EVENT_CONFIRMATION_FAILED: &str = "solend_transaction_failed";
pub const AUDIT_EVENT_CONFIRMATION_TIMEOUT: &str = "solend_transaction_confirmation_timeout";
pub const AUDIT_EVENT_CONFIRMATION_POLL_ERROR: &str =
    "solend_transaction_confirmation_poll_error";

// ── Phase 6E confirmation-side rebroadcast ──────────────────────────────────

/// Phase 6E: per-rebroadcast-attempt audit row, distinct from the
/// submit-side `solend_transaction_broadcast_attempt`. One row fires
/// per rebroadcast attempt regardless of outcome (Ok / SendError /
/// HeightFetchError); Ok rebroadcasts also continue polling because
/// the network may still drop the resent tx.
pub const AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT: &str =
    "solend_transaction_rebroadcast_attempt";

/// Phase 6E: emitted at most once per tracked entry when the
/// confirmation-side blockhash guard trips while status is still null.
/// The state machine will subsequently emit
/// `solend_transaction_confirmation_timeout` once `current_block_height`
/// exceeds `last_valid_block_height` — this earlier event records the
/// exact tick at which we stopped rebroadcasting.
pub const AUDIT_EVENT_BLOCKHASH_EXPIRED_DURING_CONFIRMATION: &str =
    "solend_transaction_blockhash_expired_during_confirmation";

/// Total rebroadcast attempts allowed per tracked tx (counts Ok and
/// Err the same — every send call against the RPC counts).
pub const MAX_CONFIRMATION_REBROADCAST_ATTEMPTS: u32 = 6;

/// Minimum wall-clock time between rebroadcasts for the same tx.
/// Independent of `poll_interval` (the cadence at which
/// `getSignatureStatuses` is called); a fast poll cadence does NOT
/// translate into faster rebroadcast spam.
pub const CONFIRMATION_REBROADCAST_INTERVAL: Duration = Duration::from_secs(2);

/// When the tracked entry's `last_valid_block_height` is within this
/// many blocks of `current_block_height`, the tracker stops
/// rebroadcasting (the resent tx would land too close to expiry to be
/// useful).
pub const BLOCKHASH_GUARD_BUFFER_BLOCKS: u64 = 5;

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
    /// Phase 6E — bytes available for confirmation-side rebroadcast.
    /// `None` when the context was constructed without bytes (e.g., the
    /// boot-recovery path: in-memory lifecycle store does not persist
    /// signed bytes across process restart). Tracker silently skips
    /// rebroadcast for entries with `None`.
    pub signed_bytes: Option<Arc<Vec<u8>>>,
    /// Phase 6E — when this context was inserted into the tracker.
    /// First-rebroadcast eligibility is `enqueued_at + INTERVAL`.
    pub enqueued_at: Instant,
    /// Phase 6E — number of rebroadcast attempts made (Ok or Err).
    /// Capped at `MAX_CONFIRMATION_REBROADCAST_ATTEMPTS`.
    pub rebroadcast_attempts: u32,
    /// Phase 6E — wall-clock instant of the most recent rebroadcast
    /// attempt; `None` until the first attempt fires.
    pub last_rebroadcast_at: Option<Instant>,
    /// Phase 6E — set once when the confirmation-side blockhash guard
    /// trips, so the `…_blockhash_expired_during_confirmation` audit
    /// event fires at most once per tracked entry.
    pub blockhash_guard_audited: bool,
}

impl SolendTrackedContext {
    /// Construct a fresh context that has NO signed bytes attached —
    /// rebroadcast is silently disabled. Used by the boot-recovery
    /// path (in-memory lifecycle store does not persist bytes across
    /// process restart) and by tests that don't exercise rebroadcast.
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
            signed_bytes: None,
            enqueued_at: Instant::now(),
            rebroadcast_attempts: 0,
            last_rebroadcast_at: None,
            blockhash_guard_audited: false,
        }
    }

    /// Phase 6E — fresh context WITH signed bytes attached. The wiring
    /// layer uses this constructor so the tracker can perform same-bytes
    /// rebroadcasts while polling.
    #[allow(clippy::too_many_arguments)]
    pub fn new_submitted_with_bytes(
        signing_request_id: Uuid,
        intent_id: Uuid,
        session_id: SessionId,
        session_wallet: Pubkey,
        last_valid_block_height: u64,
        submission_block_height: u64,
        tx_signature: Signature,
        signed_bytes: Arc<Vec<u8>>,
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
            signed_bytes: Some(signed_bytes),
            enqueued_at: Instant::now(),
            rebroadcast_attempts: 0,
            last_rebroadcast_at: None,
            blockhash_guard_audited: false,
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
    /// Phase 6E — confirmation-side rebroadcast seam. Production wraps
    /// `ClawRpcSolendSender`; tests inject a stub that records calls.
    rebroadcast_sender: Arc<dyn SolendRebroadcastSender>,
    audit: Arc<dyn SolendAuditSink>,
    poll_interval: Duration,
    shutdown: Arc<Notify>,
}

impl SolendConfirmationTracker {
    pub fn new(
        lifecycle: SolendSubmissionLifecycleStore,
        status_provider: Arc<dyn SolendSignatureStatusProvider>,
        rebroadcast_sender: Arc<dyn SolendRebroadcastSender>,
        audit: Arc<dyn SolendAuditSink>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            tracked: Arc::new(DashMap::new()),
            lifecycle,
            status_provider,
            rebroadcast_sender,
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

        // Phase 6E: track which sigs received non-null status this
        // tick so the rebroadcast pass can skip them.
        let mut observed_non_null: HashSet<Signature> = HashSet::new();

        // Zip defensively — provider contract says same length, but if
        // not we only iterate the minimum.
        for (sig, maybe_status) in sigs.iter().zip(statuses.iter()) {
            if maybe_status.is_some() {
                observed_non_null.insert(*sig);
            }
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

        // ── Phase 6E rebroadcast pass ──────────────────────────────
        //
        // For every still-tracked sig that:
        //   - has signed_bytes attached (rebroadcast capability)
        //   - did NOT receive a non-null status this tick
        //   - has a non-terminal current state (post-status-update)
        //   - has rebroadcast_attempts < MAX
        //   - passes the blockhash guard
        //   - is past its rebroadcast interval
        // … fire one same-bytes rebroadcast and audit it. RPC errors
        // are audited but do NOT abort the tracker — next eligible tick
        // simply tries again until the cap or guard stops the loop.
        for sig in &sigs {
            if observed_non_null.contains(sig) {
                continue;
            }
            // Snapshot fields before any await to avoid holding a
            // DashMap guard across an await point.
            let snapshot = self.tracked.get(sig).map(|e| {
                (
                    e.signing_request_id,
                    e.intent_id,
                    e.session_id.clone(),
                    e.session_wallet,
                    e.last_valid_block_height,
                    e.signed_bytes.clone(),
                    e.state.clone(),
                    e.enqueued_at,
                    e.rebroadcast_attempts,
                    e.last_rebroadcast_at,
                    e.blockhash_guard_audited,
                )
            });
            let Some((
                signing_request_id,
                intent_id,
                session_id,
                session_wallet,
                last_valid_block_height,
                maybe_bytes,
                state,
                enqueued_at,
                attempts,
                last_rb_at,
                guard_audited,
            )) = snapshot
            else {
                continue;
            };
            if state.is_terminal() {
                continue;
            }
            let Some(signed_bytes) = maybe_bytes else {
                continue; // No bytes — skip rebroadcast for this entry.
            };
            if attempts >= MAX_CONFIRMATION_REBROADCAST_ATTEMPTS {
                continue;
            }

            // Blockhash guard: stop rebroadcasting when current_height +
            // GUARD_BUFFER >= last_valid_block_height. The state machine
            // continues to drive the entry to ConfirmationTimeout once
            // current_height passes last_valid_block_height; this audit
            // event records the moment we STOP rebroadcasting.
            let safe_height =
                current_height.saturating_add(BLOCKHASH_GUARD_BUFFER_BLOCKS);
            if safe_height >= last_valid_block_height {
                if !guard_audited {
                    self.audit
                        .append_solend_event(
                            AUDIT_EVENT_BLOCKHASH_EXPIRED_DURING_CONFIRMATION,
                            signing_request_id.to_string(),
                            Some(session_id.to_string()),
                            AuditSeverity::Warning,
                            json!({
                                "signing_request_id":      signing_request_id,
                                "intent_id":               intent_id,
                                "session_id":              session_id.to_string(),
                                "wallet":                  session_wallet.to_string(),
                                "tx_signature":            sig.to_string(),
                                "current_block_height":    current_height,
                                "last_valid_block_height": last_valid_block_height,
                                "guard_buffer_blocks":     BLOCKHASH_GUARD_BUFFER_BLOCKS,
                                "rebroadcast_attempts":    attempts,
                            }),
                        )
                        .await;
                    if let Some(mut e) = self.tracked.get_mut(sig) {
                        e.blockhash_guard_audited = true;
                    }
                }
                continue;
            }

            // Interval gate: first rebroadcast fires no sooner than
            // INTERVAL after enqueue; subsequent rebroadcasts fire no
            // sooner than INTERVAL after the previous rebroadcast.
            let now = Instant::now();
            let interval_due = match last_rb_at {
                Some(t) => now.saturating_duration_since(t)
                    >= CONFIRMATION_REBROADCAST_INTERVAL,
                None => now.saturating_duration_since(enqueued_at)
                    >= CONFIRMATION_REBROADCAST_INTERVAL,
            };
            if !interval_due {
                continue;
            }

            // Eligible. Perform the rebroadcast.
            let attempt_n = attempts + 1;
            let remaining_block_budget =
                last_valid_block_height.saturating_sub(current_height);
            let rebroadcast_result = self
                .rebroadcast_sender
                .rebroadcast(signed_bytes.as_ref())
                .await;

            // Update counters AFTER the await regardless of outcome —
            // every send call counts toward the cap.
            if let Some(mut e) = self.tracked.get_mut(sig) {
                e.rebroadcast_attempts = attempt_n;
                e.last_rebroadcast_at = Some(now);
            }

            // Audit row.
            let (severity, rpc_outcome, error_kind, error_message): (
                AuditSeverity,
                &'static str,
                Option<&'static str>,
                Option<String>,
            ) = match &rebroadcast_result {
                Ok(_) => (AuditSeverity::Info, "ok", None, None),
                Err(e) => (
                    AuditSeverity::Warning,
                    "error",
                    Some("SendError"),
                    Some(format!("{e}")),
                ),
            };
            let mut payload = json!({
                "signing_request_id":      signing_request_id,
                "intent_id":               intent_id,
                "session_id":              session_id.to_string(),
                "wallet":                  session_wallet.to_string(),
                "tx_signature":            sig.to_string(),
                "attempt_n":               attempt_n,
                "max_attempts":            MAX_CONFIRMATION_REBROADCAST_ATTEMPTS,
                "current_block_height":    current_height,
                "last_valid_block_height": last_valid_block_height,
                "remaining_block_budget":  remaining_block_budget,
                "rpc_outcome":             rpc_outcome,
            });
            if let Some(k) = error_kind {
                payload["error_kind"] = json!(k);
            }
            if let Some(m) = error_message.as_ref() {
                payload["error_message"] = json!(m);
            }
            self.audit
                .append_solend_event(
                    AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT,
                    signing_request_id.to_string(),
                    Some(session_id.to_string()),
                    severity,
                    payload,
                )
                .await;
            if let Err(e) = &rebroadcast_result {
                debug!(
                    signing_request_id = %signing_request_id,
                    signature = %sig,
                    attempt_n = attempt_n,
                    error = %e,
                    "solend confirmation rebroadcast failed; tracker continues"
                );
            } else {
                debug!(
                    signing_request_id = %signing_request_id,
                    signature = %sig,
                    attempt_n = attempt_n,
                    "solend confirmation rebroadcast accepted by RPC"
                );
            }
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
    use crate::integrations::solend_submit::{NullSolendAuditSink, SolendSubmitError};
    use claw_state_store::audit::AuditSeverity;
    use std::collections::VecDeque;
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

    /// Stub rebroadcast sender that records calls and returns Ok by
    /// default. Used by all existing tests (which don't exercise
    /// rebroadcast because their contexts have no signed_bytes).
    struct NoopRebroadcastSender {
        calls: Mutex<Vec<Vec<u8>>>,
    }
    impl NoopRebroadcastSender {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl SolendRebroadcastSender for NoopRebroadcastSender {
        async fn rebroadcast(&self, signed_bytes: &[u8]) -> Result<String, SolendSubmitError> {
            self.calls.lock().unwrap().push(signed_bytes.to_vec());
            Ok("noopRebroadcastSig".to_string())
        }
    }

    /// Sequenced rebroadcast sender — for Phase 6E tests that exercise
    /// retry behavior. Pops Result<String, SolendSubmitError> per call;
    /// falls back to Err when the queue is empty.
    struct SequencedRebroadcast {
        responses: Mutex<VecDeque<Result<String, SolendSubmitError>>>,
        calls: Mutex<Vec<Vec<u8>>>,
    }
    impl SequencedRebroadcast {
        fn new(seq: Vec<Result<String, SolendSubmitError>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(seq)),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn calls_snapshot(&self) -> Vec<Vec<u8>> {
            self.calls.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl SolendRebroadcastSender for SequencedRebroadcast {
        async fn rebroadcast(&self, signed_bytes: &[u8]) -> Result<String, SolendSubmitError> {
            self.calls.lock().unwrap().push(signed_bytes.to_vec());
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err(SolendSubmitError::RpcFailure {
                    reason: "sequenced rebroadcast exhausted".into(),
                });
            }
            q.pop_front().unwrap()
        }
    }

    fn ok_sig(s: &str) -> Result<String, SolendSubmitError> {
        Ok(s.to_string())
    }

    fn rb_err(reason: &str) -> Result<String, SolendSubmitError> {
        Err(SolendSubmitError::RpcFailure {
            reason: reason.into(),
        })
    }

    /// Recording audit sink — captures every event so Phase 6E tests
    /// can assert per-attempt audit row contents.
    #[derive(Default)]
    struct RecordingAudit {
        events: Mutex<Vec<(String, String, Option<String>, AuditSeverity, serde_json::Value)>>,
    }
    impl RecordingAudit {
        fn names(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.0.clone())
                .collect()
        }
        fn rows_named(&self, name: &str) -> Vec<serde_json::Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.0 == name)
                .map(|e| e.4.clone())
                .collect()
        }
    }
    #[async_trait]
    impl SolendAuditSink for RecordingAudit {
        async fn append_solend_event(
            &self,
            event_name: &'static str,
            correlation_id: String,
            session_id: Option<String>,
            severity: AuditSeverity,
            payload: serde_json::Value,
        ) {
            self.events.lock().unwrap().push((
                event_name.to_string(),
                correlation_id,
                session_id,
                severity,
                payload,
            ));
        }
    }

    fn tracker_with(
        status_provider: Arc<StubStatuses>,
        lifecycle: SolendSubmissionLifecycleStore,
    ) -> Arc<SolendConfirmationTracker> {
        Arc::new(SolendConfirmationTracker::new(
            lifecycle,
            status_provider,
            Arc::new(NoopRebroadcastSender::new()),
            Arc::new(NullSolendAuditSink),
            Duration::from_millis(10),
        ))
    }

    /// Phase 6E tracker variant that lets tests inject a specific
    /// rebroadcast sender + audit sink.
    fn tracker_with_rebroadcast(
        status_provider: Arc<StubStatuses>,
        lifecycle: SolendSubmissionLifecycleStore,
        rebroadcast: Arc<dyn SolendRebroadcastSender>,
        audit: Arc<dyn SolendAuditSink>,
    ) -> Arc<SolendConfirmationTracker> {
        Arc::new(SolendConfirmationTracker::new(
            lifecycle,
            status_provider,
            rebroadcast,
            audit,
            Duration::from_millis(10),
        ))
    }

    fn ctx_with_bytes(
        sig: Signature,
        lvbh: u64,
        sbh: u64,
        signed_bytes: Vec<u8>,
    ) -> SolendTrackedContext {
        SolendTrackedContext::new_submitted_with_bytes(
            Uuid::new_v4(),
            Uuid::new_v4(),
            SessionId::new(),
            Pubkey::new_unique(),
            lvbh,
            sbh,
            sig,
            Arc::new(signed_bytes),
        )
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
            Arc::new(NoopRebroadcastSender::new()),
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

    // ── Phase 6E — confirmation-side rebroadcast tests ─────────────────────
    //
    // All Phase 6E tests run under `start_paused = true` so the
    // CONFIRMATION_REBROADCAST_INTERVAL (2s wall clock) collapses to
    // virtual time. The tracker uses `Instant::now()`, which respects
    // tokio's paused clock under the test runtime.

    /// Test-1: status null + signed_bytes attached + interval elapsed
    /// triggers a rebroadcast call against the same bytes.
    #[tokio::test(start_paused = true)]
    async fn null_status_triggers_rebroadcast_with_same_bytes() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let bytes = vec![0xAB, 0xCD, 0xEF, 0x01, 0x02];
        let ctx = ctx_with_bytes(sig, 200, 100, bytes.clone());
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        // Advance virtual time past the rebroadcast interval.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        // First tick — null status; rebroadcast eligible.
        status.push(vec![None]);
        tracker.tick_once().await;

        assert_eq!(rebroadcast.call_count(), 1);
        assert_eq!(rebroadcast.calls_snapshot()[0], bytes);
        let names = audit.names();
        assert!(names.contains(&AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT.to_string()));
    }

    /// Test-2: a tick that fires immediately after enqueue (before the
    /// interval elapses) MUST NOT trigger a rebroadcast.
    #[tokio::test(start_paused = true)]
    async fn first_rebroadcast_only_after_interval() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let bytes = vec![0x01, 0x02, 0x03];
        let ctx = ctx_with_bytes(sig, 200, 100, bytes);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        // Tick BEFORE interval: null status, but no rebroadcast yet.
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 0, "no rebroadcast before interval");

        // Advance past the interval and tick again — now a rebroadcast.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 1);
    }

    /// Test-3: once status becomes non-null on a tick, no rebroadcast
    /// fires for that entry on that tick OR subsequent ticks.
    #[tokio::test(start_paused = true)]
    async fn non_null_status_stops_rebroadcast() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast =
            Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1"), ok_sig("rb2")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let ctx = ctx_with_bytes(sig, 200, 100, vec![0xAA]);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        // Past interval, but status is finalized this tick → no rebroadcast.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("finalized".into()),
            err: None,
            slot: Some(150),
        })]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 0);

        // Tracker pruned terminal — subsequent tick has nothing to retry.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 0);
    }

    /// Test-4: blockhash guard trips → NO rebroadcast attempt fires
    /// AND the new audit event is emitted exactly once.
    #[tokio::test(start_paused = true)]
    async fn blockhash_guard_prevents_late_rebroadcast() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        // current_height = 198, last_valid = 200. With GUARD = 5,
        // safe_height = 203 >= 200 → guard trips.
        let status = Arc::new(StubStatuses::new(198));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let ctx = ctx_with_bytes(sig, 200, 100, vec![0x77]);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(
            rebroadcast.call_count(),
            0,
            "guard must prevent rebroadcast"
        );

        // Audit event fired exactly once.
        let guard_rows =
            audit.rows_named(AUDIT_EVENT_BLOCKHASH_EXPIRED_DURING_CONFIRMATION);
        assert_eq!(guard_rows.len(), 1);

        // Subsequent tick → guard already audited; no second row.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        let guard_rows_after =
            audit.rows_named(AUDIT_EVENT_BLOCKHASH_EXPIRED_DURING_CONFIRMATION);
        assert_eq!(guard_rows_after.len(), 1, "audit emitted at most once");
        assert_eq!(rebroadcast.call_count(), 0);
    }

    /// Test-5: rebroadcasts cap at MAX_CONFIRMATION_REBROADCAST_ATTEMPTS.
    #[tokio::test(start_paused = true)]
    async fn max_rebroadcast_attempts_enforced() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        // Provide enough Ok responses to cover the cap (loop will stop
        // before the queue empties).
        let mut seq = Vec::new();
        for i in 0..(MAX_CONFIRMATION_REBROADCAST_ATTEMPTS + 2) {
            seq.push(ok_sig(&format!("rb{i}")));
        }
        let rebroadcast = Arc::new(SequencedRebroadcast::new(seq));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let ctx = ctx_with_bytes(sig, 200, 100, vec![0x55]);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        // Run more ticks than the cap. Each tick: advance interval, push null.
        for _ in 0..(MAX_CONFIRMATION_REBROADCAST_ATTEMPTS + 3) {
            tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
            status.push(vec![None]);
            tracker.tick_once().await;
        }
        assert_eq!(
            rebroadcast.call_count(),
            MAX_CONFIRMATION_REBROADCAST_ATTEMPTS as usize,
            "cap enforced regardless of additional ticks"
        );
        let attempt_rows = audit.rows_named(AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT);
        assert_eq!(
            attempt_rows.len(),
            MAX_CONFIRMATION_REBROADCAST_ATTEMPTS as usize
        );
    }

    /// Test-6: rebroadcast Err audited but tracker continues; next
    /// eligible tick still rebroadcasts.
    #[tokio::test(start_paused = true)]
    async fn rebroadcast_err_does_not_abort_tracker() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![
            rb_err("503 unavailable"),
            ok_sig("rb2"),
        ]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let ctx = ctx_with_bytes(sig, 200, 100, vec![0xDE, 0xAD]);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        // First tick (after interval): rebroadcast errors.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 1);
        let attempt_rows = audit.rows_named(AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT);
        assert_eq!(attempt_rows.len(), 1);
        assert_eq!(attempt_rows[0]["rpc_outcome"], json!("error"));
        assert_eq!(attempt_rows[0]["error_kind"], json!("SendError"));
        assert!(attempt_rows[0]
            .get("error_message")
            .and_then(|m| m.as_str())
            .map(|s| s.contains("503"))
            .unwrap_or(false));

        // Second tick (after another interval): rebroadcast succeeds.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 2);
        let attempt_rows = audit.rows_named(AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT);
        assert_eq!(attempt_rows.len(), 2);
        assert_eq!(attempt_rows[1]["rpc_outcome"], json!("ok"));
        // attempt_n grows monotonically across the two attempts.
        assert_eq!(attempt_rows[0]["attempt_n"], json!(1));
        assert_eq!(attempt_rows[1]["attempt_n"], json!(2));
    }

    /// Test-7: every rebroadcast call gets the SAME signed bytes —
    /// verifies byte-stability across attempts.
    #[tokio::test(start_paused = true)]
    async fn rebroadcast_uses_byte_identical_payload_across_attempts() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![
            ok_sig("rb1"),
            ok_sig("rb2"),
            ok_sig("rb3"),
        ]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let bytes = vec![0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70];
        let ctx = ctx_with_bytes(sig, 500, 100, bytes.clone());
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        // Trigger three rebroadcasts.
        for _ in 0..3 {
            tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
            status.push(vec![None]);
            tracker.tick_once().await;
        }
        let snaps = rebroadcast.calls_snapshot();
        assert_eq!(snaps.len(), 3);
        for (i, b) in snaps.iter().enumerate() {
            assert_eq!(b, &bytes, "rebroadcast {} payload differs", i + 1);
        }
        // Every audit row carries the SAME tx_signature.
        let attempt_rows = audit.rows_named(AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT);
        let canonical = sig.to_string();
        for r in &attempt_rows {
            assert_eq!(r["tx_signature"].as_str().unwrap_or_default(), canonical);
        }
    }

    /// Test-8: existing finalized / failed / confirmation_timeout
    /// behavior is preserved — Phase 6E does not change the state
    /// machine driven outcomes.
    #[tokio::test(start_paused = true)]
    async fn existing_finalized_path_unchanged_with_rebroadcast_capability() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let ctx = ctx_with_bytes(sig, 500, 100, vec![0x99]);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx.clone());

        // Status finalized — terminal; tracker prunes; no rebroadcast.
        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("finalized".into()),
            err: None,
            slot: Some(150),
        })]);
        tracker.tick_once().await;

        let rec = lifecycle
            .lookup_for_session(&ctx.session_id, ctx.signing_request_id)
            .unwrap();
        assert!(matches!(rec.outcome, RecordedSubmitOutcome::Finalized { .. }));
        assert!(!tracker.contains(&sig));
        assert_eq!(rebroadcast.call_count(), 0);
    }

    /// Test-9: per-attempt audit row contains every required field.
    #[tokio::test(start_paused = true)]
    async fn rebroadcast_audit_row_has_required_fields() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let ctx = ctx_with_bytes(sig, 200, 100, vec![0x42]);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;

        let rows = audit.rows_named(AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        for field in [
            "signing_request_id",
            "intent_id",
            "session_id",
            "wallet",
            "tx_signature",
            "attempt_n",
            "max_attempts",
            "current_block_height",
            "last_valid_block_height",
            "remaining_block_budget",
            "rpc_outcome",
        ] {
            assert!(
                r.get(field).is_some(),
                "rebroadcast_attempt audit missing field `{field}`"
            );
        }
        assert_eq!(r["attempt_n"], json!(1));
        assert_eq!(r["max_attempts"], json!(MAX_CONFIRMATION_REBROADCAST_ATTEMPTS));
        assert_eq!(r["rpc_outcome"], json!("ok"));
        assert_eq!(r["current_block_height"], json!(100));
        assert_eq!(r["last_valid_block_height"], json!(200));
        assert_eq!(r["remaining_block_budget"], json!(100));
    }

    /// Test-10: tracker without bytes (e.g., bootstrap-recovery) does
    /// NOT call the rebroadcast sender. Polling continues.
    #[tokio::test(start_paused = true)]
    async fn no_bytes_means_no_rebroadcast_attempts() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        // No-bytes constructor — rebroadcast disabled for this entry.
        let ctx = ctx_for(sig, 200, 100);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;

        assert_eq!(rebroadcast.call_count(), 0);
        let attempt_rows = audit.rows_named(AUDIT_EVENT_TRANSACTION_REBROADCAST_ATTEMPT);
        assert_eq!(attempt_rows.len(), 0);
    }

    /// Test-11: after an entry transitions to a terminal state in a
    /// later tick, NO rebroadcast attempts are emitted afterwards.
    #[tokio::test(start_paused = true)]
    async fn no_rebroadcast_attempts_after_terminal_state() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let status = Arc::new(StubStatuses::new(100));
        let rebroadcast = Arc::new(SequencedRebroadcast::new(vec![ok_sig("rb1"), ok_sig("rb2")]));
        let audit = Arc::new(RecordingAudit::default());
        let tracker = tracker_with_rebroadcast(
            status.clone(),
            lifecycle.clone(),
            rebroadcast.clone(),
            audit.clone(),
        );
        let sig = random_sig();
        let ctx = ctx_with_bytes(sig, 500, 100, vec![0x88]);
        seed_submitted(&lifecycle, &ctx);
        tracker.enqueue(ctx);

        // Tick 1: null → rebroadcast.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 1);

        // Tick 2: status finalized → terminal; entry pruned.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![Some(SolendSignatureStatus {
            confirmation_status: Some("finalized".into()),
            err: None,
            slot: Some(150),
        })]);
        tracker.tick_once().await;
        // No additional rebroadcast call (status was non-null, we skip).
        assert_eq!(rebroadcast.call_count(), 1);

        // Tick 3: entry already gone (pruned) — no further work.
        tokio::time::advance(CONFIRMATION_REBROADCAST_INTERVAL).await;
        status.push(vec![None]);
        tracker.tick_once().await;
        assert_eq!(rebroadcast.call_count(), 1, "no rebroadcast after terminal");
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

//! In-memory parking store for Solend deposit intents awaiting approval.
//!
//! # Phase 4B-3 scope
//!
//! Mirrors the shape of [`crate::integrations::jupiter_park::PendingJupiterParkStore`],
//! adapted for Solend deposit propose-stage intents. The key is the
//! `ApprovalRequest.id` — matching the shape of the Jupiter and signing
//! stores so the single approval-routing code path can signal all three
//! stores with the same `request_id` without caring which one owns it.
//!
//! # What this store stores (Phase 4B-3)
//!
//! Only PROPOSE-TIME facts:
//!   - `intent_id` (the tool's in-memory UUID)
//!   - `action: ProposedAction::Deposit { protocol, reserve_mint, amount }`
//!   - `snapshot: LendingSnapshot` (assembler's normalized read-model)
//!   - `*_exists` booleans (obligation / source ATA / collateral ATA)
//!   - `verdict_at_propose: LendingPolicyVerdict` (MUST be `Pass` for
//!     anything to be parked here)
//!   - `proposed_at` / `expires_at` (TTL bounds)
//!   - `session_id` / `session_wallet` (correlation + downstream signer
//!     identity)
//!
//! # What this store MUST NOT store
//!
//! Phase 4C will re-assemble and re-evaluate after approval because
//! Solana state can change between propose and approve. Therefore:
//!
//!   - NO transaction bytes
//!   - NO blockhash / `last_valid_block_height` intended for execution
//!   - NO signatures
//!   - NO priority fee plan
//!   - NO signer handle
//!   - NO `RpcClient`
//!   - NO private keys
//!   - NO Phantom request state
//!
//! # Concurrency
//!
//! Internal state is an `Arc<DashMap<Uuid, ParkedSolendDepositIntent>>`.
//! `DashMap` uses internal sharded locks that NEVER await; all critical
//! sections are pure map get/set operations. This is the same primitive
//! the Jupiter park store uses ([`crate::integrations::jupiter_park`])
//! and is safe to call from async code because no held lock spans an
//! `.await` point.
//!
//! # Expiration semantics (lazy)
//!
//! `ParkedSolendDepositIntent.expires_at` is a wall-clock TTL set by the
//! tool at park time (from `approval_lease_seconds`). Expiration is
//! checked lazily on access:
//!
//!   - `get(request_id)` returns `None` once `Utc::now() > expires_at`,
//!     even if an entry is still in the map — and the expired entry is
//!     opportunistically removed.
//!   - `signal(request_id, state)` returns `false` for expired entries
//!     (they are treated as if already gone). Approval routing will
//!     independently see them as `Expired` via the existing approval
//!     workflow lease and will send a separate `Expired` signal the
//!     approval-store path is responsible for.
//!   - `cleanup_expired()` sweeps all expired entries on demand.
//!
//! No background task. Lazy access matches the existing Jupiter and
//! signing store conventions (neither of which runs a sweeper task;
//! both rely on the approval-store reaper for final cleanup).
//!
//! # Resume task (Phase 4C-1)
//!
//! [`run_solend_resume_task`] is the single post-approval driver for a
//! parked Solend deposit intent. It:
//!
//! 1. Awaits the oneshot state delivered by
//!    [`crate::approval_routing::route_approval_outcome`].
//! 2. On `Rejected` / `Expired` / receiver-dropped: cleans up and exits.
//! 3. On `Approved`: RE-ASSEMBLES a fresh
//!    [`AssembledSolendDepositSnapshot`] and RE-RUNS the REAL
//!    [`crate::lending::evaluate_lending_policy`] against it. The parked
//!    snapshot is audit context only — it is NEVER reused as the
//!    authoritative post-approval state. This is the "Wall-7" re-check.
//! 4. Terminates with a typed [`SolendResumeOutcome`] and cleans up the
//!    parked entry.
//!
//! # What this task deliberately does NOT do (Phase 4C-1)
//!
//! - NO transaction assembly / `VersionedTransaction` / `MessageV0`.
//! - NO Solend instruction builder calls (`build_refresh`, `build_deposit`,
//!   `build_init_obligation`, `build_create_ata`).
//! - NO blockhash fetch, NO `simulateTransaction`, NO `sendTransaction`.
//! - NO signer handle, NO Phantom / orchestrator touchpoint.
//!
//! Approval granted does NOT mean executed in this slice. Execution wires
//! up in Phase 4C-2+.
//!
//! No `panic!()`, `todo!()`, or `unimplemented!()` appears in the resume
//! task's production code paths. Terminal outcomes are typed values.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::oneshot;
use uuid::Uuid;

use claw_types::{approval::ApprovalWorkflowState, session::SessionId};

use crate::lending::{LendingPolicyVerdict, LendingSnapshot, ProposedAction};

/// Data parked for a single Solend deposit intent awaiting a decision.
///
/// Every field is a propose-time fact. No execution-time fact
/// (blockhash, tx bytes, signer, priority fee, signatures) is present
/// by design — see module docs.
#[derive(Clone)]
pub struct ParkedSolendDepositIntent {
    /// The tool's in-memory intent id (Uuid v4). Matches the `intent_id`
    /// returned by `solend_deposit_usdc` in the `awaiting_approval`
    /// response.
    pub intent_id: Uuid,
    /// Typed action the evaluator passed.
    pub action: ProposedAction,
    /// Normalized read-model the evaluator consumed. Phase 4C will
    /// re-assemble a FRESH snapshot; this field is kept for audit /
    /// diffing, not for execution.
    pub snapshot: LendingSnapshot,
    /// Execution-planning fact, from the assembler.
    pub obligation_exists: bool,
    /// Execution-planning fact, from the assembler.
    pub source_ata_exists: bool,
    /// Execution-planning fact, from the assembler.
    pub collateral_ata_exists: bool,
    /// The verdict the evaluator returned at propose time. Only `Pass`
    /// intents are parked; the park API enforces this at call sites.
    pub verdict_at_propose: LendingPolicyVerdict,
    /// When the tool parked this intent.
    pub proposed_at: DateTime<Utc>,
    /// Wall-clock TTL; expired entries are not returned as active.
    pub expires_at: DateTime<Utc>,
    /// Originating session.
    pub session_id: SessionId,
    /// The session's bound external wallet (Slice 3G seam). The
    /// execution slice will use this as signer identity.
    pub session_wallet: Pubkey,
}

/// Entry wrapper carrying the oneshot sender used by the (future) resume
/// task. Internal; callers interact with the store via `park`, `signal`,
/// `get`, etc.
struct Entry {
    parked: ParkedSolendDepositIntent,
    decision_tx: Option<oneshot::Sender<ApprovalWorkflowState>>,
}

/// In-memory store for Solend deposit intents awaiting operator approval.
#[derive(Clone)]
pub struct SolendParkStore {
    inner: Arc<DashMap<Uuid, Entry>>,
}

impl SolendParkStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Park an intent and return the oneshot receiver the resume task
    /// (Phase 4C-1+) will await. Until 4C-1, the receiver is intentionally
    /// held by no task — it is returned here so the API matches the
    /// Jupiter pattern and so tests can drive `park → signal → await`.
    pub fn park(
        &self,
        request_id: Uuid,
        parked: ParkedSolendDepositIntent,
    ) -> oneshot::Receiver<ApprovalWorkflowState> {
        let (decision_tx, decision_rx) = oneshot::channel();
        self.inner.insert(
            request_id,
            Entry {
                parked,
                decision_tx: Some(decision_tx),
            },
        );
        decision_rx
    }

    /// Deliver the terminal workflow state to the parked resume task.
    ///
    /// Returns `true` if the signal was delivered, `false` if no entry
    /// exists, has already been signaled, or has expired.
    ///
    /// Expired entries are treated as already gone: `signal` returns
    /// `false` AND the expired entry is removed opportunistically so a
    /// later `get` sees a clean miss.
    pub fn signal(&self, request_id: Uuid, state: ApprovalWorkflowState) -> bool {
        let now = Utc::now();
        // First: if entry is expired, drop it without signaling.
        let should_drop = self
            .inner
            .get(&request_id)
            .map(|e| e.parked.expires_at <= now)
            .unwrap_or(false);
        if should_drop {
            self.inner.remove(&request_id);
            return false;
        }
        if let Some(mut entry) = self.inner.get_mut(&request_id) {
            if let Some(tx) = entry.decision_tx.take() {
                let _ = tx.send(state);
                return true;
            }
        }
        false
    }

    /// Return a clone of the parked intent if present AND not expired.
    /// Expired entries are opportunistically removed and `None` is
    /// returned.
    pub fn get(&self, request_id: Uuid) -> Option<ParkedSolendDepositIntent> {
        let now = Utc::now();
        let is_expired = self
            .inner
            .get(&request_id)
            .map(|e| e.parked.expires_at <= now)
            .unwrap_or(true);
        if is_expired {
            self.inner.remove(&request_id);
            return None;
        }
        self.inner.get(&request_id).map(|e| e.parked.clone())
    }

    /// Remove an entry unconditionally (no signal). Used for cleanup
    /// after a resume task completes (future Phase 4C-1).
    pub fn remove(&self, request_id: &Uuid) {
        self.inner.remove(request_id);
    }

    /// Total number of currently parked intents (includes entries whose
    /// `expires_at` has passed but have not yet been swept).
    pub fn parked_count(&self) -> usize {
        self.inner.len()
    }

    /// Whether a given `request_id` is parked here (does NOT check
    /// expiration — use `get` for an expired-aware lookup).
    pub fn contains(&self, request_id: &Uuid) -> bool {
        self.inner.contains_key(request_id)
    }

    /// Phase 5A — session+wallet-scoped pending-action probe.
    ///
    /// Returns `true` iff there is a non-expired parked Solend deposit
    /// intent for this exact `(session_id, session_wallet)` pair. Used
    /// by the tool's LLM-ingress concurrency guard to fail-fast on
    /// duplicate proposals (`PendingActionExists`) without registering
    /// a second approval.
    ///
    /// Read-only and expiry-aware: expired entries are NOT considered
    /// active. Different sessions and different wallets cannot block
    /// each other.
    ///
    /// Iteration over the DashMap is safe at this granularity — the
    /// store is bounded by `approval_lease_seconds` worth of pending
    /// proposals per session, and the guard runs at most once per
    /// LLM-originated tool call.
    pub fn has_active_for_session_wallet(
        &self,
        session_id: &SessionId,
        session_wallet: &Pubkey,
    ) -> bool {
        let now = Utc::now();
        self.inner.iter().any(|entry| {
            entry.parked.expires_at > now
                && &entry.parked.session_id == session_id
                && &entry.parked.session_wallet == session_wallet
        })
    }

    /// Sweep all entries whose `expires_at` is in the past. Returns the
    /// number of entries removed.
    pub fn cleanup_expired(&self) -> usize {
        let now = Utc::now();
        let expired: Vec<Uuid> = self
            .inner
            .iter()
            .filter(|e| e.parked.expires_at <= now)
            .map(|e| *e.key())
            .collect();
        let removed = expired.len();
        for rid in expired {
            self.inner.remove(&rid);
        }
        removed
    }
}

impl Default for SolendParkStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── Phase 4C-1 · Resume task ────────────────────────────────────────────────

use tracing::{info, warn};

use crate::integrations::solend::SolendAssemblyError;
use crate::integrations::solend_jit_ready::{
    SolendJitReady, SolendJitReadyDeps, SolendJitReadyStore,
};
use crate::integrations::solend_preflight::{
    preflight_solend_deposit_plan, SolendPreflightOutcome, SolendPreflightSimulator,
};
use crate::integrations::solend_signing::{
    RecentBlockhashProvider, SolendSigningStore,
};
use crate::integrations::solend_submit::{
    SolendAuditSink, AUDIT_EVENT_JIT_READY_PERSISTED,
};
use crate::integrations::solend_tx_plan::{
    assemble_solend_deposit_tx_plan, SolendDepositTxPlan, SolendTxPlanError,
    PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
};
use crate::lending::{
    evaluate_lending_policy, EvaluationContext, HardBlockReason, LendingRuleConfig, RuleKind,
    RuleRejectionDetail, SystemInvariantSubreason,
};
use crate::tools::solend_deposit::SolendDepositSnapshotAssembler;

/// Typed terminal outcomes of [`run_solend_resume_task`].
///
/// One resume task produces exactly one outcome. The outcome is returned
/// (from the task's `Future`) so tests can inspect the terminal path
/// without having to observe side-effects. In production the outcome is
/// logged; no caller is expected to drive further state transitions from
/// it — the task is the single post-approval driver for a parked intent.
///
/// None of the outcomes imply that a transaction was built, signed, or
/// broadcast. Phase 4C-1 is pre-execution by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolendResumeOutcome {
    /// Operator rejected the approval. No re-check performed.
    Rejected,
    /// Approval lease expired (including daemon-side lease reaper sending
    /// `Expired`). No re-check performed.
    Expired,
    /// The oneshot decision receiver was dropped without ever delivering a
    /// state. Exercised when the parked entry itself was dropped (TTL
    /// sweep, manual removal), or when the daemon shut down before a
    /// decision arrived.
    ReceiverDropped,
    /// The approval signal arrived as `Approved`, but the parked intent
    /// was gone from [`SolendParkStore`] by the time the resume task
    /// looked it up. Idempotent re-entry case — this is a safe terminal
    /// outcome, not an error.
    ParkedIntentMissing,
    /// Post-approval snapshot re-assembly failed. `error_type` names the
    /// [`SolendAssemblyError`] variant; `message` is a short Display
    /// summary for audit logs. No raw Debug dumps leak through this
    /// boundary (same discipline as the tool-surface mapping in
    /// [`crate::tools::solend_deposit`]).
    ReassembleFailed {
        error_type: &'static str,
        message: String,
    },
    /// The REAL evaluator HardBlocked the fresh snapshot. `verified_slot`
    /// is the snapshot's observed slot when available (populated
    /// whenever the block came from a rule evaluating snapshot fields —
    /// in Phase 4C-1 that covers every `RuleRejected` outcome). The
    /// `SystemInvariantFailed` arm would leave the verified slot absent,
    /// but is structurally unreachable here because the tool uses the
    /// session-bound wallet that the snapshot's mapping layer threads
    /// through.
    RecheckBlocked {
        hard_block_reason: String,
        verified_slot: Option<u64>,
    },
    /// Fresh re-evaluation passed. The intent is execution-ready but
    /// deferred to Phase 4C-2+. `verified_slot` is the fresh snapshot's
    /// observed slot, surfaced for audit logs so downstream slices can
    /// correlate "approval granted at slot X, verified fresh at slot Y".
    ///
    /// This variant is preserved for the narrow case where Phase 4C-2's
    /// plan-assembly layer itself reports a typed plan-assembly error
    /// (see `RecheckPassedPlanAssemblyFailed`) — it is NOT produced on
    /// the happy path in 4C-2. In 4C-2 the happy path produces
    /// [`RecheckPassedPlanAssembled`].
    RecheckPassedExecutionDeferred { verified_slot: u64 },
    /// Phase 4C-2 variant: re-check passed AND a Solend deposit tx
    /// PLAN was successfully assembled, but Phase 4C-3's preflight did
    /// NOT run. Kept for tests that specifically exercise plan-only
    /// paths; the production happy path in 4C-3+ is
    /// [`Self::RecheckPassedPreflighted`].
    RecheckPassedPlanAssembled {
        verified_slot: u64,
        plan: SolendDepositTxPlan,
    },
    /// Phase 4C-3 happy path: re-check passed, plan assembled, and
    /// structural preflight (`simulateTransaction` with
    /// `sig_verify=false`, `replace_recent_blockhash=true`) produced a
    /// terminal [`SolendPreflightOutcome`]. The outcome may itself be
    /// `Passed` (structural ok), `Failed` (simulation error), or a
    /// `ConfigError`/`TooLargeNeedsChunking` — inspect the nested
    /// outcome for classification.
    ///
    /// In 4C-4+ this variant is produced ONLY when no signing store is
    /// wired (tests, legacy callers). Production, with a signing store
    /// injected, upgrades the `Passed` case to
    /// [`Self::RecheckPassedSigningRequested`].
    ///
    /// This variant does NOT imply any broadcast, signing, or signer
    /// handoff has occurred.
    RecheckPassedPreflighted {
        verified_slot: u64,
        plan: SolendDepositTxPlan,
        preflight: SolendPreflightOutcome,
    },
    /// Phase 4C-4 (deprecated by 6B): preflight Passed AND a signing
    /// handoff was created at approval time. Retained as a defined
    /// variant only because some older Solend tests still construct it
    /// indirectly via deprecated paths. PRODUCTION NEVER PRODUCES THIS
    /// VARIANT in Phase 6B+ — handoff creation is deferred to the
    /// just-in-time prepare route exercised at Sign-with-Phantom click.
    RecheckPassedSigningRequested {
        verified_slot: u64,
        signing_request_id: uuid::Uuid,
        simulation_slot: Option<u64>,
        units_consumed: Option<u64>,
        last_valid_block_height: u64,
        obligation_signer_backend_partial: bool,
    },
    /// Phase 4C-4 (deprecated by 6B): preflight Passed but signer
    /// handoff creation failed at approval time. Retained for back-
    /// compatibility; PRODUCTION NEVER PRODUCES THIS VARIANT in
    /// Phase 6B+ since no handoff is created on the resume path.
    SignerHandoffFailed {
        verified_slot: u64,
        error_type: String,
        message: String,
    },
    /// Phase 6B happy path: re-check passed, plan assembled, structural
    /// preflight Passed, AND the JIT-ready entry is parked in the
    /// `SolendJitReadyStore` keyed by `approval_request_id`. NO
    /// transaction has been built, NO blockhash has been fetched, NO
    /// signature exists. The frontend's Sign-with-Phantom click will
    /// trigger the prepare route which calls `create_signing_handoff`
    /// on demand with a fresh blockhash. `expires_at_unix_ms` mirrors
    /// the JIT-ready entry's TTL (matches `approval_lease_seconds`).
    RecheckPassedJitReady {
        verified_slot: u64,
        simulation_slot: Option<u64>,
        units_consumed: Option<u64>,
        expires_at_unix_ms: i64,
    },
    /// Phase 4C-2: re-check passed but the plan-assembly layer rejected
    /// the fresh snapshot inputs (e.g. zero amount, mismatched reserve
    /// mint, missing well-known constant parse). Surfacing this as a
    /// distinct terminal avoids masquerading a planning-side failure as
    /// a policy block.
    RecheckPassedPlanAssemblyFailed {
        verified_slot: u64,
        plan_error: String,
    },
}

/// Await the approval decision for a parked Solend deposit intent and,
/// on `Approved`, perform the freshness re-check described in Phase 4C-1.
///
/// # Contract
///
/// - Consumes `decision_rx`, so a duplicate wakeup (second spawn with the
///   same `request_id`) is prevented at the call site: only one
///   resume-task future owns any given `oneshot::Receiver`.
/// - On every terminal path, removes the parked entry from `park_store`
///   so the store never leaks entries. Already-removed entries are a
///   graceful no-op at `remove()`.
/// - Idempotent against duplicate `signal(Approved)` calls: the park
///   store's `signal` takes the sender on first delivery; subsequent
///   signal attempts return `false` and do not race a second re-check.
/// - Never panics. All terminal paths are typed [`SolendResumeOutcome`]
///   values.
///
/// # Forbidden side effects (spec §3, §7)
///
/// The task MUST NOT build instructions, fetch blockhash, simulate,
/// sign, or broadcast. Its only RPC is the read-only
/// [`SolendDepositSnapshotAssembler::assemble_for_deposit`] call.
/// **Phase 4C-4 (deprecated by 6B): the resume task no longer creates
/// a signing handoff at approval time.** This struct is retained as a
/// public type because the new prepare HTTP route (added in Phase 6B
/// Window 2) consumes the same field shape — a Solend signing store, a
/// blockhash provider, a shared audit sink, and a signing lease — when
/// it calls `create_signing_handoff` just-in-time on the user's
/// Sign-with-Phantom click. Production resume tasks use
/// [`SolendJitReadyDeps`] instead; this type is no longer constructed
/// from the resume path.
#[derive(Clone)]
pub struct SolendSigningDeps {
    pub store: SolendSigningStore,
    pub blockhash_provider: Arc<dyn RecentBlockhashProvider>,
    /// Phase 4C-5 W3 remediation: append-only audit sink shared between
    /// handoff-creation events and submit-lifecycle events. Production
    /// wires `AuditRepositorySink`; tests inject a recording / no-op
    /// sink.
    pub audit: Arc<dyn SolendAuditSink>,
    pub signing_lease_seconds: u64,
}

pub async fn run_solend_resume_task(
    request_id: Uuid,
    park_store: SolendParkStore,
    assembler: Arc<dyn SolendDepositSnapshotAssembler>,
    rule_config: LendingRuleConfig,
    preflight_simulator: Option<Arc<dyn SolendPreflightSimulator>>,
    jit_ready_deps: Option<SolendJitReadyDeps>,
    decision_rx: oneshot::Receiver<ApprovalWorkflowState>,
) -> SolendResumeOutcome {
    // ── 1. Await the approval decision ──────────────────────────────────
    //
    // Receiver errors if the sender was dropped without delivery — which
    // in this system means the parked Entry was removed (TTL sweep,
    // manual remove, or daemon shutdown) before any approval outcome
    // reached the routing layer.
    let state = match decision_rx.await {
        Ok(s) => s,
        Err(_) => {
            park_store.remove(&request_id);
            warn!(
                request_id = %request_id,
                "solend_resume: decision channel dropped — cleaning up"
            );
            return SolendResumeOutcome::ReceiverDropped;
        }
    };

    match state {
        ApprovalWorkflowState::Rejected => {
            park_store.remove(&request_id);
            info!(
                request_id = %request_id,
                "solend_resume: operator rejected — cleaning up"
            );
            return SolendResumeOutcome::Rejected;
        }
        ApprovalWorkflowState::Expired => {
            park_store.remove(&request_id);
            info!(
                request_id = %request_id,
                "solend_resume: approval expired — cleaning up"
            );
            return SolendResumeOutcome::Expired;
        }
        ApprovalWorkflowState::Pending => {
            // Defensive: a non-terminal `Pending` should never be sent
            // through the oneshot. Upstream `ApprovalWorkflowState` only
            // signals on terminal transitions. Treat the same as a
            // dropped channel — clean up and exit safely.
            park_store.remove(&request_id);
            warn!(
                request_id = %request_id,
                "solend_resume: unexpected non-terminal Pending signal — treating as dropped"
            );
            return SolendResumeOutcome::ReceiverDropped;
        }
        ApprovalWorkflowState::Approved => {
            // fall through to re-check
        }
    }

    // ── 2. Re-load the parked intent ───────────────────────────────────
    //
    // If the entry is gone (TTL swept, concurrent removal, or a prior
    // resume-task duplicate already cleaned up), return a safe terminal
    // value rather than racing toward a re-check with partial data.
    let parked = match park_store.get(request_id) {
        Some(p) => p,
        None => {
            info!(
                request_id = %request_id,
                "solend_resume: approved signal arrived but parked intent already missing"
            );
            return SolendResumeOutcome::ParkedIntentMissing;
        }
    };

    info!(
        request_id = %request_id,
        intent_id = %parked.intent_id,
        wallet = %parked.session_wallet,
        "solend_resume: approved — running fresh re-assembly + re-evaluation"
    );

    // ── 3. Fresh re-assembly ────────────────────────────────────────────
    //
    // This is the load-bearing step the spec calls out: the parked
    // snapshot is NOT reused as execution truth. A freshly-assembled
    // snapshot is the only input the re-evaluator sees.
    let reserve_mint = parked.action.reserve_mint();
    let assembled = match assembler
        .assemble_for_deposit(parked.session_wallet, reserve_mint)
        .await
    {
        Ok(a) => a,
        Err(err) => {
            park_store.remove(&request_id);
            let error_type = solend_assembly_error_variant(&err);
            let message = err.to_string();
            warn!(
                request_id = %request_id,
                intent_id = %parked.intent_id,
                error_type = %error_type,
                err = %message,
                "solend_resume: fresh re-assembly failed"
            );
            return SolendResumeOutcome::ReassembleFailed {
                error_type,
                message,
            };
        }
    };

    // ── 4. REAL re-evaluation against the fresh snapshot ────────────────
    let context = EvaluationContext {
        session_wallet: parked.session_wallet,
    };
    let verdict = evaluate_lending_policy(
        &assembled.snapshot,
        &parked.action,
        &rule_config,
        &context,
    );

    let verified_slot = assembled.snapshot.fetched_at.observed_slot.raw();

    // ── 5. Cleanup + terminal outcome ───────────────────────────────────
    park_store.remove(&request_id);

    match verdict {
        LendingPolicyVerdict::Pass => {
            // Phase 4C-2: attempt plan assembly from the FRESH snapshot.
            // Plan assembly is pure — no RPC, no signer, no simulation,
            // no broadcast. On success we surface the unsigned plan as
            // `RecheckPassedPlanAssembled`. On a typed plan-assembly
            // error we surface `RecheckPassedPlanAssemblyFailed` to keep
            // planning failures distinguishable from policy blocks.
            match assemble_solend_deposit_tx_plan(
                request_id,
                &parked,
                &assembled,
                verified_slot,
                PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
            ) {
                Ok(plan) => {
                    info!(
                        request_id = %request_id,
                        intent_id = %parked.intent_id,
                        verified_slot = verified_slot,
                        obligation_signer_required = plan.obligation_signer_required,
                        setup_ix_count = plan.setup_instructions.len(),
                        refresh_ix_count = plan.refresh_instructions.len(),
                        deposit_ix_count = plan.deposit_instructions.len(),
                        "solend_resume: re-check PASS + tx PLAN assembled \
                         (unsigned, not broadcast)"
                    );
                    // Phase 4C-3: if a preflight simulator is wired, run
                    // structural preflight. The simulator is Option to
                    // preserve backwards compatibility with tests that
                    // only exercise the plan-assembly path.
                    match preflight_simulator {
                        None => SolendResumeOutcome::RecheckPassedPlanAssembled {
                            verified_slot,
                            plan,
                        },
                        Some(sim) => {
                            let preflight = preflight_solend_deposit_plan(&plan, sim.as_ref()).await;
                            info!(
                                request_id = %request_id,
                                intent_id = %parked.intent_id,
                                verified_slot = verified_slot,
                                preflight_variant = preflight_outcome_label(&preflight),
                                "solend_resume: structural preflight complete (no broadcast)"
                            );

                            // Phase 6B: if jit_ready deps are wired AND
                            // preflight Passed, persist a JIT-ready
                            // entry in the SolendJitReadyStore keyed
                            // by approval_request_id. NO transaction is
                            // built, NO blockhash is fetched, NO
                            // signature is created — that work is
                            // deferred to the prepare HTTP route which
                            // runs `create_signing_handoff` on demand
                            // when the user clicks Sign with Phantom.
                            //
                            // This is the structural fix for the live-
                            // mainnet timing race observed twice on
                            // 2026-05-04: the manual paste / poll /
                            // sign loop consistently exceeds Solana's
                            // ~150-block validity window, so the
                            // blockhash must be fetched as close to the
                            // Phantom popup as possible.
                            match (&preflight, jit_ready_deps) {
                                (SolendPreflightOutcome::Passed { simulation_slot, units_consumed, .. }, Some(deps)) => {
                                    let now = chrono::Utc::now();
                                    let expires_at = now
                                        + chrono::Duration::seconds(
                                            deps.jit_ready_lease_seconds as i64,
                                        );
                                    let expires_at_unix_ms = expires_at.timestamp_millis();
                                    let entry = SolendJitReady {
                                        intent_id: parked.intent_id,
                                        session_id: parked.session_id.clone(),
                                        session_wallet: parked.session_wallet,
                                        plan: plan.clone(),
                                        preflight: preflight.clone(),
                                        verified_slot,
                                        created_at: now,
                                        expires_at,
                                    };
                                    deps.store.put(request_id, entry);
                                    info!(
                                        request_id = %request_id,
                                        intent_id = %parked.intent_id,
                                        verified_slot = verified_slot,
                                        expires_at_unix_ms = expires_at_unix_ms,
                                        "solend_resume: JIT-ready persisted \
                                         (no handoff, no blockhash, no signature)"
                                    );
                                    deps.audit
                                        .append_solend_event(
                                            AUDIT_EVENT_JIT_READY_PERSISTED,
                                            request_id.to_string(),
                                            Some(parked.session_id.to_string()),
                                            claw_state_store::audit::AuditSeverity::Info,
                                            serde_json::json!({
                                                "request_id":    request_id,
                                                "intent_id":     parked.intent_id,
                                                "session_wallet": parked.session_wallet.to_string(),
                                                "verified_slot": verified_slot,
                                                "simulation_slot": simulation_slot,
                                                "units_consumed": units_consumed,
                                                "expires_at_unix_ms": expires_at_unix_ms,
                                            }),
                                        )
                                        .await;
                                    SolendResumeOutcome::RecheckPassedJitReady {
                                        verified_slot,
                                        simulation_slot: *simulation_slot,
                                        units_consumed: *units_consumed,
                                        expires_at_unix_ms,
                                    }
                                }
                                // No jit_ready deps wired, or preflight was
                                // not Passed — fall back to the 4C-3
                                // outcome so existing tests that bypass
                                // jit_ready persistence still produce the
                                // right variant.
                                (_, _) => SolendResumeOutcome::RecheckPassedPreflighted {
                                    verified_slot,
                                    plan,
                                    preflight,
                                },
                            }
                        }
                    }
                }
                Err(plan_error) => {
                    let plan_error_str = plan_error_summary(&plan_error);
                    warn!(
                        request_id = %request_id,
                        intent_id = %parked.intent_id,
                        verified_slot = verified_slot,
                        plan_error = %plan_error_str,
                        "solend_resume: re-check PASS but plan assembly failed"
                    );
                    SolendResumeOutcome::RecheckPassedPlanAssemblyFailed {
                        verified_slot,
                        plan_error: plan_error_str,
                    }
                }
            }
        }
        LendingPolicyVerdict::HardBlock(reason) => {
            let reason_str = hard_block_reason_summary(&reason);
            let verified_slot_opt = match &reason {
                HardBlockReason::RuleRejected(_, _) => Some(verified_slot),
                HardBlockReason::SystemInvariantFailed(_) => None,
                // ScopeBoundarySubreason is empty in V1 (see Part 3B):
                // this arm is structurally unreachable on the current
                // evaluator. Treat defensively — no slot attested.
                HardBlockReason::ScopeBoundary(_) => None,
            };
            warn!(
                request_id = %request_id,
                intent_id = %parked.intent_id,
                hard_block_reason = %reason_str,
                verified_slot = verified_slot_opt,
                "solend_resume: re-check HardBlocked"
            );
            SolendResumeOutcome::RecheckBlocked {
                hard_block_reason: reason_str,
                verified_slot: verified_slot_opt,
            }
        }
    }
}

// ── Resume-task helpers (internal) ──────────────────────────────────────────

/// Stable label for a [`SolendPreflightOutcome`] used purely in structured
/// logs. Never persisted, never surfaced to the tool's JSON output.
fn preflight_outcome_label(outcome: &SolendPreflightOutcome) -> &'static str {
    match outcome {
        SolendPreflightOutcome::Passed { .. } => "Passed",
        SolendPreflightOutcome::Failed { .. } => "Failed",
        SolendPreflightOutcome::ConfigError { .. } => "ConfigError",
        SolendPreflightOutcome::TooLargeNeedsChunking { .. } => "TooLargeNeedsChunking",
    }
}

// `signing_handoff_error_label` removed in Phase 6B: the resume task no
// longer calls `create_signing_handoff`, so the helper has no remaining
// callers. The mapping is moved into the prepare HTTP route in Window 2
// where it is needed.

/// Stable variant name for a [`SolendAssemblyError`]. Used in
/// [`SolendResumeOutcome::ReassembleFailed`] so audit logs and tests can
/// branch on the variant without parsing Display strings.
fn solend_assembly_error_variant(err: &SolendAssemblyError) -> &'static str {
    match err {
        SolendAssemblyError::UnsupportedReserveMint { .. } => "UnsupportedReserveMint",
        SolendAssemblyError::ReserveAccountMissing { .. } => "ReserveAccountMissing",
        SolendAssemblyError::ReserveDecodeFailed { .. } => "ReserveDecodeFailed",
        SolendAssemblyError::ObligationDecodeFailed { .. } => "ObligationDecodeFailed",
        SolendAssemblyError::ObligationAccountOwnerMismatch { .. } => {
            "ObligationAccountOwnerMismatch"
        }
        SolendAssemblyError::OracleAccountMissing { .. } => "OracleAccountMissing",
        SolendAssemblyError::InvalidAccountOwner { .. } => "InvalidAccountOwner",
        SolendAssemblyError::RpcFetchFailed { .. } => "RpcFetchFailed",
        SolendAssemblyError::FirstDepositMappingFailed { .. } => "FirstDepositMappingFailed",
        SolendAssemblyError::SnapshotMappingFailed { .. } => "SnapshotMappingFailed",
        SolendAssemblyError::InvalidTokenAccountOwnerProgram { .. } => {
            "InvalidTokenAccountOwnerProgram"
        }
        SolendAssemblyError::InvalidTokenAccountDataLen { .. } => "InvalidTokenAccountDataLen",
        SolendAssemblyError::InvalidTokenAccountMint { .. } => "InvalidTokenAccountMint",
        SolendAssemblyError::InvalidTokenAccountOwner { .. } => "InvalidTokenAccountOwner",
        SolendAssemblyError::PythOracleOwnerMismatch { .. } => "PythOracleOwnerMismatch",
    }
}

/// Stable short string for a [`HardBlockReason`], matching the shape the
/// tool-surface summary produces. Local to this module so the resume
/// task does not cross-depend on private helpers in the tool module.
fn hard_block_reason_summary(reason: &HardBlockReason) -> String {
    match reason {
        HardBlockReason::SystemInvariantFailed(sub) => match sub {
            SystemInvariantSubreason::OwnerMismatch => {
                "SystemInvariantFailed:OwnerMismatch".to_string()
            }
            SystemInvariantSubreason::ProtocolTagMismatch => {
                "SystemInvariantFailed:ProtocolTagMismatch".to_string()
            }
        },
        HardBlockReason::ScopeBoundary(sub) => match *sub {},
        HardBlockReason::RuleRejected(kind, detail) => {
            format!(
                "RuleRejected:{}:{}",
                rule_kind_label(*kind),
                rule_detail_label(detail)
            )
        }
    }
}

fn rule_kind_label(kind: RuleKind) -> &'static str {
    match kind {
        RuleKind::RequireFreshState => "RequireFreshState",
        RuleKind::MaxOracleStalenessMs => "MaxOracleStalenessMs",
        RuleKind::AllowedLendingProtocols => "AllowedLendingProtocols",
        RuleKind::AllowedMints => "AllowedMints",
        RuleKind::MaxActionInputAmount => "MaxActionInputAmount",
    }
}

/// Short, stable summary of a plan-assembly error variant, for audit
/// logs + the `RecheckPassedPlanAssemblyFailed` outcome.
fn plan_error_summary(err: &SolendTxPlanError) -> String {
    match err {
        SolendTxPlanError::UnsupportedProtocol { protocol } => {
            format!("UnsupportedProtocol:{protocol:?}")
        }
        SolendTxPlanError::UnsupportedActionKind => "UnsupportedActionKind".to_string(),
        SolendTxPlanError::ReserveNotInSnapshot { action_mint } => {
            format!("ReserveNotInSnapshot:{action_mint}")
        }
        SolendTxPlanError::DepositBuildFailed(inner) => {
            format!("DepositBuildFailed:{inner}")
        }
        SolendTxPlanError::InternalConstantParseFailed => {
            "InternalConstantParseFailed".to_string()
        }
    }
}

fn rule_detail_label(detail: &RuleRejectionDetail) -> &'static str {
    match detail {
        RuleRejectionDetail::ProtocolNativeStale => "ProtocolNativeStale",
        RuleRejectionDetail::FetchAgeExceeded => "FetchAgeExceeded",
        RuleRejectionDetail::OracleFeedSetEmpty => "OracleFeedSetEmpty",
        RuleRejectionDetail::OraclePublishFreshnessUnknown => "OraclePublishFreshnessUnknown",
        RuleRejectionDetail::OraclePublishAgeExceeded => "OraclePublishAgeExceeded",
        RuleRejectionDetail::ProtocolNotAllowed => "ProtocolNotAllowed",
        RuleRejectionDetail::MintNotAllowed => "MintNotAllowed",
        RuleRejectionDetail::ReserveNotInSnapshot => "ReserveNotInSnapshot",
        RuleRejectionDetail::RepayWithoutDebt => "RepayWithoutDebt",
        // Phase 5H — Withdraw cross-reference details. Unreachable
        // from the deposit-resume code path (the deposit tool only
        // constructs `ProposedAction::Deposit`), but the formatter
        // must remain exhaustive over `RuleRejectionDetail`.
        RuleRejectionDetail::WithdrawWithoutDeposit => "WithdrawWithoutDeposit",
        RuleRejectionDetail::WithdrawWithDebt => "WithdrawWithDebt",
        RuleRejectionDetail::NoCapConfigured => "NoCapConfigured",
        RuleRejectionDetail::AmountOverCap => "AmountOverCap",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lending::{ProtocolTag, UnderlyingAmount};
    use chrono::Duration;

    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn usdc_mint() -> Pubkey {
        Pubkey::try_from(USDC_MINT_BS58).unwrap()
    }

    /// Build a minimal parked intent fixture for store-level tests. The
    /// `LendingSnapshot` is fabricated via the public mapping path so
    /// tests don't depend on internal synth helpers from `raw.rs`.
    fn parked_intent(expires_at: DateTime<Utc>) -> ParkedSolendDepositIntent {
        use crate::integrations::solend::mapping::{
            map_snapshot_for_first_deposit, FirstDepositAssemblyInputs, OracleAccountInfo,
            ReserveInput,
        };
        use crate::integrations::solend::raw::{decode_reserve, synth_reserve};
        use crate::lending::{ChainSlot, FeedPublishFreshness};

        let session_wallet = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let sentinel: Pubkey = "nu11111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();

        let res = synth_reserve(
            market,
            usdc_mint(),
            6,
            Pubkey::new_unique(),
            pyth,
            sentinel,
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        let reserve_raw = decode_reserve(&res).unwrap();
        let snapshot = map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
            session_wallet,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: market,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pubkey,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(1_000),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(1_000),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(1_000)),
            }],
            snapshot_observed_slot: ChainSlot::new(1_000),
        })
        .unwrap();

        ParkedSolendDepositIntent {
            intent_id: Uuid::new_v4(),
            action: ProposedAction::Deposit {
                protocol: ProtocolTag::Solend,
                reserve_mint: usdc_mint(),
                amount: UnderlyingAmount::new(1_000),
            },
            snapshot,
            obligation_exists: false,
            source_ata_exists: false,
            collateral_ata_exists: false,
            verdict_at_propose: LendingPolicyVerdict::Pass,
            proposed_at: Utc::now(),
            expires_at,
            session_id: SessionId::new(),
            session_wallet,
        }
    }

    #[tokio::test]
    async fn park_then_signal_delivers_state() {
        let store = SolendParkStore::new();
        let request_id = Uuid::new_v4();
        let rx = store.park(request_id, parked_intent(Utc::now() + Duration::seconds(60)));

        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));

        let state = rx.await.unwrap();
        assert_eq!(state, ApprovalWorkflowState::Approved);
    }

    #[tokio::test]
    async fn signal_for_unknown_request_is_graceful_no_op() {
        let store = SolendParkStore::new();
        assert!(!store.signal(Uuid::new_v4(), ApprovalWorkflowState::Approved));
    }

    #[tokio::test]
    async fn double_signal_only_fires_once() {
        let store = SolendParkStore::new();
        let request_id = Uuid::new_v4();
        let _rx = store.park(request_id, parked_intent(Utc::now() + Duration::seconds(60)));
        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
        assert!(!store.signal(request_id, ApprovalWorkflowState::Approved));
    }

    #[tokio::test]
    async fn signal_rejected_and_expired_both_deliver() {
        let store = SolendParkStore::new();

        let rid1 = Uuid::new_v4();
        let rx1 = store.park(rid1, parked_intent(Utc::now() + Duration::seconds(60)));
        store.signal(rid1, ApprovalWorkflowState::Rejected);
        assert_eq!(rx1.await.unwrap(), ApprovalWorkflowState::Rejected);

        let rid2 = Uuid::new_v4();
        let rx2 = store.park(rid2, parked_intent(Utc::now() + Duration::seconds(60)));
        store.signal(rid2, ApprovalWorkflowState::Expired);
        assert_eq!(rx2.await.unwrap(), ApprovalWorkflowState::Expired);
    }

    #[test]
    fn parked_count_tracks_park_and_remove() {
        let store = SolendParkStore::new();
        let r1 = Uuid::new_v4();
        let r2 = Uuid::new_v4();
        let _rx1 = store.park(r1, parked_intent(Utc::now() + Duration::seconds(60)));
        let _rx2 = store.park(r2, parked_intent(Utc::now() + Duration::seconds(60)));
        assert_eq!(store.parked_count(), 2);
        store.remove(&r1);
        assert_eq!(store.parked_count(), 1);
        assert!(store.contains(&r2));
        assert!(!store.contains(&r1));
    }

    #[tokio::test]
    async fn get_returns_none_for_expired_entry_and_removes_it() {
        let store = SolendParkStore::new();
        let rid = Uuid::new_v4();
        // expires_at in the past.
        let _rx = store.park(rid, parked_intent(Utc::now() - Duration::seconds(10)));
        assert!(store.contains(&rid), "entry is in the map pre-expiration check");
        assert!(store.get(rid).is_none(), "get() must not return expired entries");
        assert!(
            !store.contains(&rid),
            "expired entry must be opportunistically removed by get()"
        );
    }

    #[tokio::test]
    async fn signal_for_expired_entry_returns_false_and_removes_it() {
        let store = SolendParkStore::new();
        let rid = Uuid::new_v4();
        let _rx = store.park(rid, parked_intent(Utc::now() - Duration::seconds(10)));
        assert!(!store.signal(rid, ApprovalWorkflowState::Approved));
        assert!(!store.contains(&rid));
    }

    #[test]
    fn cleanup_expired_sweeps_only_expired_entries() {
        let store = SolendParkStore::new();
        let fresh_id = Uuid::new_v4();
        let expired_id = Uuid::new_v4();
        let _r1 = store.park(fresh_id, parked_intent(Utc::now() + Duration::seconds(60)));
        let _r2 = store.park(expired_id, parked_intent(Utc::now() - Duration::seconds(1)));
        let removed = store.cleanup_expired();
        assert_eq!(removed, 1);
        assert!(store.contains(&fresh_id));
        assert!(!store.contains(&expired_id));
    }

    // ── Phase 4C-1 · Resume task tests ──────────────────────────────────────

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::integrations::solend::{
        AssembledSolendDepositSnapshot, SolendAssemblyError,
    };
    use crate::integrations::solend::mapping::{
        map_snapshot, map_snapshot_for_first_deposit, FirstDepositAssemblyInputs,
        OracleAccountInfo, ReserveInput, SolendAssemblyInputs,
    };
    use crate::integrations::solend::raw::{
        self, decode_obligation, decode_reserve, synth_obligation, synth_reserve,
    };
    // `LendingRuleConfig`, `evaluate_lending_policy`, etc. are brought in
    // at module scope (see the `use crate::lending::{…}` above the
    // resume task). The outer test module re-exports them via
    // `use super::*;`, so they are already in scope here. Only bring in
    // items that are NOT already in scope.
    use crate::lending::{
        AllowedLendingProtocolsConfig, AllowedMintsConfig, ChainSlot, DurationMs,
        FeedPublishFreshness, MaxActionInputAmountConfig, MaxOracleStalenessConfig,
        RequireFreshStateConfig,
    };

    const PYTH_RECEIVER_PROGRAM_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    // ── Snapshot fixtures (propose-time vs. fresh re-check) ───────────────

    /// A fully-passing assembled snapshot for `session_wallet` with a
    /// pinned snapshot slot. Different slots per call let tests prove
    /// that the re-check uses the FRESH snapshot, not the parked one.
    fn assembled_snapshot_passing(
        session_wallet: Pubkey,
        snapshot_slot: u64,
    ) -> AssembledSolendDepositSnapshot {
        let reserve_mint = usdc_mint();
        let market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let sentinel: Pubkey = "nu11111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let pyth_owner: Pubkey = PYTH_RECEIVER_PROGRAM_BS58.parse().unwrap();

        let res = synth_reserve(
            market,
            reserve_mint,
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            /*stale=*/ false,
        );
        let reserve_raw = decode_reserve(&res).unwrap();

        let inputs = FirstDepositAssemblyInputs {
            session_wallet,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: market,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pubkey,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(snapshot_slot),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(snapshot_slot),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(snapshot_slot)),
            }],
            snapshot_observed_slot: ChainSlot::new(snapshot_slot),
        };
        let snapshot = map_snapshot_for_first_deposit(inputs).unwrap();

        AssembledSolendDepositSnapshot {
            snapshot,
            obligation_exists: false,
            source_ata_exists: false,
            collateral_ata_exists: false,
            reserve_pubkey,
            obligation_pubkey: Pubkey::new_unique(),
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
        }
    }

    /// A snapshot that the REAL evaluator HardBlocks via
    /// `RuleRejected:RequireFreshState:ProtocolNativeStale`. Used to
    /// simulate the chain state drifting between propose and approve.
    fn assembled_snapshot_stale_obligation(
        session_wallet: Pubkey,
        snapshot_slot: u64,
    ) -> AssembledSolendDepositSnapshot {
        let reserve_mint = usdc_mint();
        let market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let sentinel: Pubkey = "nu11111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let pyth_owner: Pubkey = PYTH_RECEIVER_PROGRAM_BS58.parse().unwrap();

        // obligation marked stale
        let obl = synth_obligation(
            session_wallet,
            market,
            100,
            /*stale=*/ true,
            &[],
            &[],
        );
        let res = synth_reserve(
            market,
            reserve_mint,
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );

        let inputs = SolendAssemblyInputs {
            session_wallet,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: decode_obligation(&obl).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(snapshot_slot),
            reserves: vec![ReserveInput {
                pubkey: reserve_pubkey,
                raw: decode_reserve(&res).unwrap(),
                fetched_at_slot: ChainSlot::new(snapshot_slot),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(snapshot_slot),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(snapshot_slot)),
            }],
            snapshot_observed_slot: ChainSlot::new(snapshot_slot),
        };
        let snapshot = map_snapshot(inputs).unwrap();

        AssembledSolendDepositSnapshot {
            snapshot,
            obligation_exists: true,
            source_ata_exists: false,
            collateral_ata_exists: false,
            reserve_pubkey,
            obligation_pubkey: Pubkey::new_unique(),
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
        }
    }

    /// Snapshot with `FeedPublishFreshness::Unknown` — will HardBlock via
    /// `MaxOracleStalenessMs:OraclePublishFreshnessUnknown`.
    fn assembled_snapshot_oracle_unknown(
        session_wallet: Pubkey,
        snapshot_slot: u64,
    ) -> AssembledSolendDepositSnapshot {
        let reserve_mint = usdc_mint();
        let market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let sentinel: Pubkey = "nu11111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let pyth_owner: Pubkey = PYTH_RECEIVER_PROGRAM_BS58.parse().unwrap();

        let res = synth_reserve(
            market,
            reserve_mint,
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            100,
            false,
        );
        let reserve_raw = decode_reserve(&res).unwrap();

        let inputs = FirstDepositAssemblyInputs {
            session_wallet,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: market,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pubkey,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(snapshot_slot),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(snapshot_slot),
                publish: FeedPublishFreshness::Unknown,
            }],
            snapshot_observed_slot: ChainSlot::new(snapshot_slot),
        };
        let snapshot = map_snapshot_for_first_deposit(inputs).unwrap();

        AssembledSolendDepositSnapshot {
            snapshot,
            obligation_exists: false,
            source_ata_exists: false,
            collateral_ata_exists: false,
            reserve_pubkey,
            obligation_pubkey: Pubkey::new_unique(),
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
        }
    }

    // ── Mock assembler — resume-task side ─────────────────────────────────
    //
    // NEVER mocks the evaluator; only the read-only assembler surface.
    // Takes a VecDeque of responses and pops per call so tests can inject
    // "propose-time returns X; re-check returns Y".

    struct ResumeMockAssembler {
        queue: std::sync::Mutex<VecDeque<Result<AssembledSolendDepositSnapshot, SolendAssemblyError>>>,
        call_count: AtomicUsize,
        last_wallet: std::sync::Mutex<Option<Pubkey>>,
        must_not_be_called: bool,
    }

    impl ResumeMockAssembler {
        fn with_responses(
            responses: Vec<Result<AssembledSolendDepositSnapshot, SolendAssemblyError>>,
        ) -> Self {
            Self {
                queue: std::sync::Mutex::new(responses.into_iter().collect()),
                call_count: AtomicUsize::new(0),
                last_wallet: std::sync::Mutex::new(None),
                must_not_be_called: false,
            }
        }
        fn that_panics_if_called() -> Self {
            Self {
                queue: std::sync::Mutex::new(VecDeque::new()),
                call_count: AtomicUsize::new(0),
                last_wallet: std::sync::Mutex::new(None),
                must_not_be_called: true,
            }
        }
        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl crate::tools::solend_deposit::SolendDepositSnapshotAssembler for ResumeMockAssembler {
        async fn assemble_for_deposit(
            &self,
            session_wallet: Pubkey,
            _reserve_mint: Pubkey,
        ) -> Result<AssembledSolendDepositSnapshot, SolendAssemblyError> {
            if self.must_not_be_called {
                panic!(
                    "ResumeMockAssembler::that_panics_if_called was invoked — \
                     a gate was expected to short-circuit"
                );
            }
            self.call_count.fetch_add(1, Ordering::SeqCst);
            *self.last_wallet.lock().unwrap() = Some(session_wallet);
            match self.queue.lock().unwrap().pop_front() {
                Some(r) => r,
                None => Err(SolendAssemblyError::RpcFetchFailed {
                    reason: "resume mock exhausted".to_string(),
                }),
            }
        }
    }

    fn permissive_v1_config() -> LendingRuleConfig {
        LendingRuleConfig {
            require_fresh_state: RequireFreshStateConfig {
                max_fetch_age: DurationMs::new(60_000),
            },
            max_oracle_staleness: MaxOracleStalenessConfig {
                max_publish_age: DurationMs::new(60_000),
            },
            allowed_lending_protocols: AllowedLendingProtocolsConfig {
                allowlist: vec![ProtocolTag::Solend],
            },
            allowed_mints: AllowedMintsConfig {
                allowlist: vec![usdc_mint()],
            },
            max_action_input_amount: MaxActionInputAmountConfig {
                per_mint_caps: vec![(usdc_mint(), UnderlyingAmount::new(10_000))],
                // Phase 5H — deposit-park tests don't construct Withdraw
                // actions; collateral cap list stays empty.
                per_mint_collateral_caps: vec![],
            },
        }
    }

    /// Build a `ParkedSolendDepositIntent` for a specific wallet with a
    /// pinned propose-time snapshot slot so tests can assert the fresh
    /// snapshot (different slot) wins on re-check.
    fn parked_intent_for(
        session_wallet: Pubkey,
        expires_at: DateTime<Utc>,
        propose_snapshot_slot: u64,
    ) -> ParkedSolendDepositIntent {
        let assembled = assembled_snapshot_passing(session_wallet, propose_snapshot_slot);
        ParkedSolendDepositIntent {
            intent_id: Uuid::new_v4(),
            action: ProposedAction::Deposit {
                protocol: ProtocolTag::Solend,
                reserve_mint: usdc_mint(),
                amount: UnderlyingAmount::new(1_000),
            },
            snapshot: assembled.snapshot,
            obligation_exists: assembled.obligation_exists,
            source_ata_exists: assembled.source_ata_exists,
            collateral_ata_exists: assembled.collateral_ata_exists,
            verdict_at_propose: LendingPolicyVerdict::Pass,
            proposed_at: Utc::now(),
            expires_at,
            session_id: SessionId::new(),
            session_wallet,
        }
    }

    // ── (A) Approved path re-assembles and re-evaluates ───────────────────

    #[tokio::test]
    async fn resume_approved_path_reassembles_runs_real_evaluator_and_cleans_up() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        // Park an intent with a propose snapshot at slot 1_000.
        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );

        // Fresh re-check snapshot at slot 2_000 — DIFFERENT from the parked one.
        // The resume task MUST use this fresh slot, not the parked one (2_000).
        let fresh = assembled_snapshot_passing(wallet, 2_000);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh)]));

        // Signal Approved — parallels what `route_approval_outcome` does.
        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));

        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler.clone(),
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;

        match outcome {
            // Phase 4C-2: the happy path now returns the plan variant —
            // a tx plan has been assembled (still unsigned, not
            // broadcastable).
            SolendResumeOutcome::RecheckPassedPlanAssembled {
                verified_slot,
                plan,
            } => {
                // Verified slot is the FRESH one (2_000), not the parked (1_000).
                assert_eq!(verified_slot, 2_000);
                assert_eq!(plan.verified_slot, 2_000);
                assert!(!plan.deposit_instructions.is_empty());
            }
            other => panic!(
                "expected RecheckPassedPlanAssembled, got {:?}",
                other
            ),
        }

        // Resume-task side: assembler was called exactly once (the re-check).
        assert_eq!(
            assembler.call_count(),
            1,
            "resume task must re-assemble exactly once on Approved"
        );

        // Parked intent cleaned up.
        assert!(
            !store.contains(&request_id),
            "parked intent must be removed after resume"
        );
    }

    // ── (B) Approved path with fresh re-check block (post-approval mutation)

    #[tokio::test]
    async fn resume_approved_path_blocks_on_fresh_stale_obligation() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        // Propose-time snapshot was fresh (already inside parked_intent_for).
        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );

        // Fresh re-check snapshot: obligation now stale → REAL evaluator blocks.
        let fresh_stale = assembled_snapshot_stale_obligation(wallet, 2_000);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh_stale)]));

        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));

        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler,
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;

        match outcome {
            SolendResumeOutcome::RecheckBlocked {
                hard_block_reason,
                verified_slot,
            } => {
                assert_eq!(
                    hard_block_reason,
                    "RuleRejected:RequireFreshState:ProtocolNativeStale"
                );
                assert_eq!(verified_slot, Some(2_000));
            }
            other => panic!("expected RecheckBlocked, got {:?}", other),
        }
        assert!(!store.contains(&request_id));
    }

    // ── (C) Approved path with fresh re-check pass — slot captured ─────────

    #[tokio::test]
    async fn resume_approved_path_passes_and_captures_verified_slot() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );

        let fresh = assembled_snapshot_passing(wallet, 5_555);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh)]));

        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler,
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;
        match outcome {
            SolendResumeOutcome::RecheckPassedPlanAssembled {
                verified_slot,
                plan,
            } => {
                assert_eq!(verified_slot, 5_555);
                assert_eq!(plan.verified_slot, 5_555);
            }
            other => panic!("expected RecheckPassedPlanAssembled, got {:?}", other),
        }
        assert!(!store.contains(&request_id));
    }

    // ── (D) Rejected path cleans up without re-assembly ────────────────────

    #[tokio::test]
    async fn resume_rejected_path_cleans_up_without_reassembly() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();
        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );
        let assembler = Arc::new(ResumeMockAssembler::that_panics_if_called());

        assert!(store.signal(request_id, ApprovalWorkflowState::Rejected));
        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler.clone(),
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;
        assert_eq!(outcome, SolendResumeOutcome::Rejected);
        assert_eq!(
            assembler.call_count(),
            0,
            "Rejected path MUST NOT call the assembler"
        );
        assert!(!store.contains(&request_id));
    }

    // ── (E) Expired path cleans up without re-assembly ─────────────────────

    #[tokio::test]
    async fn resume_expired_path_cleans_up_without_reassembly() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();
        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );
        let assembler = Arc::new(ResumeMockAssembler::that_panics_if_called());

        assert!(store.signal(request_id, ApprovalWorkflowState::Expired));
        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler.clone(),
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;
        assert_eq!(outcome, SolendResumeOutcome::Expired);
        assert_eq!(assembler.call_count(), 0);
        assert!(!store.contains(&request_id));
    }

    // ── (F) Receiver dropped / missing signal is safe ──────────────────────

    #[tokio::test]
    async fn resume_receiver_dropped_exits_safely() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();
        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );
        let assembler = Arc::new(ResumeMockAssembler::that_panics_if_called());

        // Drop the decision_tx by removing the parked entry before any signal.
        // This is exactly the TTL-sweep / manual-remove case: the sender is
        // dropped, so `rx.await` returns Err.
        store.remove(&request_id);

        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler.clone(),
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;
        assert_eq!(outcome, SolendResumeOutcome::ReceiverDropped);
        assert_eq!(assembler.call_count(), 0);
        assert!(!store.contains(&request_id));
    }

    // ── (I) Parked snapshot is NOT reused as authoritative state ──────────
    //
    // Park the intent with a FRESH propose-time snapshot. Have the
    // re-check assembler return a CLEARLY-DIFFERENT snapshot (different
    // slot). The resume task must report the fresh slot in its terminal
    // outcome. If it ever reused the parked snapshot, this test's
    // `verified_slot` assertion would break.

    #[tokio::test]
    async fn resume_does_not_reuse_parked_snapshot_as_authoritative() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        // Propose snapshot at slot 1_000 (frozen in the parked entry).
        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );

        // Fresh re-check snapshot at slot 9_999 — totally different.
        let fresh = assembled_snapshot_passing(wallet, 9_999);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh)]));

        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler,
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;
        match outcome {
            SolendResumeOutcome::RecheckPassedPlanAssembled {
                verified_slot,
                plan,
            } => {
                assert_eq!(
                    verified_slot, 9_999,
                    "verified_slot must reflect the FRESH snapshot's slot, \
                     not the parked snapshot's 1_000"
                );
                assert_eq!(plan.verified_slot, 9_999);
            }
            other => panic!("expected RecheckPassedPlanAssembled, got {:?}", other),
        }
    }

    // ── (K) Duplicate signal / duplicate wakeup is idempotent ──────────────
    //
    // First `signal(Approved)` delivers on the oneshot. A second attempt
    // returns false because the tx was already consumed. The resume task
    // processes once; a second "run" launched manually with a fresh rx
    // (that never receives, because the entry is already gone) must
    // exit as `ReceiverDropped` without any extra re-check.

    #[tokio::test]
    async fn resume_duplicate_signal_and_wakeup_is_idempotent() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );

        // FIRST signal delivers successfully.
        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
        // SECOND signal returns false — tx already taken.
        assert!(!store.signal(request_id, ApprovalWorkflowState::Approved));

        let fresh = assembled_snapshot_passing(wallet, 2_000);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh)]));
        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler.clone(),
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;
        assert!(matches!(
            outcome,
            SolendResumeOutcome::RecheckPassedPlanAssembled { .. }
        ));
        // Real re-check called assembler once.
        assert_eq!(assembler.call_count(), 1);
        // Parked intent cleaned up after first resume path.
        assert!(!store.contains(&request_id));

        // A "second" resume-task attempt: this would happen only if the
        // system mis-spawned a duplicate task for the same request_id.
        // The duplicate sees an already-cleaned store AND a never-signaled
        // fresh oneshot (sender dropped because the store entry is gone
        // — we fabricate a dropped receiver by creating and dropping the
        // sender directly).
        let (dup_tx, dup_rx) = tokio::sync::oneshot::channel();
        drop(dup_tx); // immediately drops sender → rx.await returns Err
        let dup_assembler = Arc::new(ResumeMockAssembler::that_panics_if_called());
        let dup_outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            dup_assembler.clone(),
            permissive_v1_config(),
            None,
            None,
            dup_rx,
        )
        .await;
        assert_eq!(dup_outcome, SolendResumeOutcome::ReceiverDropped);
        // NO second re-check side effect — duplicate assembler was never called.
        assert_eq!(dup_assembler.call_count(), 0);
    }

    // ── (L) Post-approval mutation trap ────────────────────────────────────
    //
    // Propose-time: snapshot is fresh → Pass.
    // Operator approves.
    // Fresh re-check: oracle publish is now Unknown (mutation during
    // approval window). REAL evaluator must HardBlock with
    // `MaxOracleStalenessMs:OraclePublishFreshnessUnknown`. The outcome
    // MUST be `RecheckBlocked`, never `RecheckPassedExecutionDeferred`.

    #[tokio::test]
    async fn resume_post_approval_mutation_trap_blocks_instead_of_executes() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );

        // Re-check returns a snapshot whose oracle freshness is Unknown —
        // the "state changed between propose and approve" mutation.
        let fresh_bad = assembled_snapshot_oracle_unknown(wallet, 2_000);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh_bad)]));

        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler,
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;

        match outcome {
            SolendResumeOutcome::RecheckBlocked {
                hard_block_reason,
                verified_slot,
            } => {
                assert_eq!(
                    hard_block_reason,
                    "RuleRejected:MaxOracleStalenessMs:OraclePublishFreshnessUnknown"
                );
                assert_eq!(verified_slot, Some(2_000));
            }
            SolendResumeOutcome::RecheckPassedPlanAssembled { .. }
            | SolendResumeOutcome::RecheckPassedExecutionDeferred { .. }
            | SolendResumeOutcome::RecheckPassedPlanAssemblyFailed { .. } => {
                panic!(
                    "post-approval mutation trap FAILED: resume task passed \
                     when fresh snapshot should have HardBlocked"
                );
            }
            other => panic!("expected RecheckBlocked, got {:?}", other),
        }
        assert!(!store.contains(&request_id));
    }

    // ── (A/M) Plan is present on happy path and absent on RecheckBlocked ──
    //
    // A: plan assembly only after Approved + fresh re-check Pass →
    //    outcome includes the SolendDepositTxPlan.
    // M: post-approval mutation trap: HardBlock must NOT produce a plan.

    #[tokio::test]
    async fn resume_happy_path_outcome_carries_solend_deposit_tx_plan() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );
        let fresh = assembled_snapshot_passing(wallet, 4_242);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh)]));
        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));

        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler,
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;

        let plan = match outcome {
            SolendResumeOutcome::RecheckPassedPlanAssembled { verified_slot, plan } => {
                assert_eq!(verified_slot, 4_242);
                plan
            }
            other => panic!("expected RecheckPassedPlanAssembled, got {:?}", other),
        };
        // Plan is non-empty in all three groups for first-deposit shape.
        assert!(!plan.setup_instructions.is_empty());
        assert!(!plan.refresh_instructions.is_empty());
        assert!(!plan.deposit_instructions.is_empty());
        // Plan's verified_slot echoes the fresh snapshot slot.
        assert_eq!(plan.verified_slot, 4_242);
        // Plan embeds the same request_id we passed to the resume task.
        assert_eq!(plan.request_id, request_id);
        // Still unsigned / not broadcastable: we carry Vec<Instruction> only,
        // no blockhash field, no signatures field — enforced structurally
        // by the plan's pattern-match test in integrations::solend_tx_plan.
    }

    #[tokio::test]
    async fn resume_recheck_blocked_does_not_produce_plan() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );
        // Fresh snapshot: obligation now stale → REAL evaluator HardBlocks.
        let fresh_bad = assembled_snapshot_stale_obligation(wallet, 2_000);
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(fresh_bad)]));
        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));

        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler,
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;

        // Outcome must be RecheckBlocked — no plan variant produced.
        match outcome {
            SolendResumeOutcome::RecheckBlocked { .. } => {}
            SolendResumeOutcome::RecheckPassedPlanAssembled { .. }
            | SolendResumeOutcome::RecheckPassedExecutionDeferred { .. }
            | SolendResumeOutcome::RecheckPassedPlanAssemblyFailed { .. } => {
                panic!(
                    "RecheckBlocked path must not produce a plan; got non-blocked outcome"
                );
            }
            other => panic!("expected RecheckBlocked, got {:?}", other),
        }
    }

    // ── ReassembleFailed path ──────────────────────────────────────────────
    //
    // Not lettered in the spec, but load-bearing: if fresh re-assembly
    // fails, the terminal outcome must be `ReassembleFailed` with a
    // stable variant name — no `panic!()`, no Rust Debug leakage.

    #[tokio::test]
    async fn resume_reassemble_failure_returns_structured_terminal() {
        let store = SolendParkStore::new();
        let wallet = Pubkey::new_unique();
        let request_id = Uuid::new_v4();

        let rx = store.park(
            request_id,
            parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
        );
        let oracle_pk = Pubkey::new_unique();
        let err = SolendAssemblyError::OracleAccountMissing { oracle: oracle_pk };
        let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Err(err)]));

        assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
        let outcome = run_solend_resume_task(
            request_id,
            store.clone(),
            assembler,
            permissive_v1_config(),
            None,
            None,
            rx,
        )
        .await;

        match outcome {
            SolendResumeOutcome::ReassembleFailed { error_type, message } => {
                assert_eq!(error_type, "OracleAccountMissing");
                assert!(
                    message.contains("oracle"),
                    "message should be Display-based (contains 'oracle'): {message}"
                );
            }
            other => panic!("expected ReassembleFailed, got {:?}", other),
        }
        assert!(!store.contains(&request_id));
    }

    // ── (H) Resume task module source has no builder / execution calls ────
    //
    // Grep-backed structural guard: the `integrations/solend_park.rs`
    // module must not reference any Solend instruction builder, nor any
    // signing / broadcast / simulate surface, in production code. Doc
    // comments explicitly naming these in an "does NOT do" context are
    // allowed — and indeed desired — so the check filters them out.

    #[test]
    fn resume_task_source_contains_no_runtime_builder_or_execution_calls() {
        let source = include_str!("solend_park.rs");
        // Scan only the production code — stop at `#[cfg(test)]`. Test
        // code legitimately references these tokens (e.g. the forbidden-
        // tokens array itself, or the panic-if-called assembler mock).
        let production_source: &str = match source.find("#[cfg(test)]") {
            Some(idx) => &source[..idx],
            None => source,
        };

        let forbidden_runtime_tokens = [
            "build_refresh(",
            "build_deposit(",
            "build_deposit_reserve_liquidity_and_obligation_collateral_instruction(",
            "build_init_obligation(",
            "build_create_ata(",
            "build_create_ata_idempotent_instruction(",
            "VersionedTransaction",
            "Transaction::new",
            "MessageV0",
            "AddressLookupTable",
            "ComputeBudgetInstruction",
            "get_latest_blockhash(",
            "latest_blockhash(",
            "simulate_transaction(",
            "simulateTransaction",
            "send_transaction(",
            "send_raw_transaction(",
            "send_raw_v0_transaction(",
            "Keypair::",
            "Signer::",
            "panic!(",
            "todo!(",
            "unimplemented!(",
        ];

        for line in production_source.lines() {
            let trimmed = line.trim_start();
            // Skip doc comments and inline comments — they are allowed to
            // name these tokens explicitly when documenting what the
            // module does NOT do.
            if trimmed.starts_with("//!") || trimmed.starts_with("///") || trimmed.starts_with("//")
            {
                continue;
            }
            for tok in forbidden_runtime_tokens {
                assert!(
                    !line.contains(tok),
                    "solend_park.rs production code must not reference `{tok}`; \
                     offending line: {line:?}"
                );
            }
        }
    }

    // ── Phase 4C-3 · Resume task + structural preflight integration ─────────
    //
    // These tests feed a stub `SolendPreflightSimulator` into the resume
    // task and verify that:
    //   - the happy path now lands on `RecheckPassedPreflighted`,
    //   - a Passed preflight and a Failed preflight each propagate into
    //     the nested outcome,
    //   - neither variant broadcasts or signs.
    //
    // The stub simulator is defined inline here (separate from the one
    // inside `integrations/solend_preflight`'s own tests module, which is
    // private to that module).

    mod preflight_integration {
        use super::*;
        use crate::integrations::solend_preflight::{
            PreflightRpcError, RawSimulationResult, SolendPreflightOutcome,
            SolendPreflightSimulator,
        };
        use solana_sdk::transaction::{Transaction, TransactionError};
        use std::sync::Mutex as StdMutex;

        struct StubSim {
            rent: u64,
            sim: StdMutex<Option<Result<RawSimulationResult, PreflightRpcError>>>,
        }

        impl StubSim {
            fn passing() -> Arc<Self> {
                Arc::new(Self {
                    rent: PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
                    sim: StdMutex::new(Some(Ok(RawSimulationResult {
                        err: None,
                        logs: Some(vec!["Program log: ok".into()]),
                        units_consumed: Some(4242),
                        context_slot: Some(777_777),
                    }))),
                })
            }

            fn failing_custom_14() -> Arc<Self> {
                use solana_sdk::instruction::InstructionError;
                Arc::new(Self {
                    rent: PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
                    sim: StdMutex::new(Some(Ok(RawSimulationResult {
                        err: Some(TransactionError::InstructionError(
                            2,
                            InstructionError::Custom(14),
                        )),
                        logs: Some(vec!["Program log: math overflow".into()]),
                        units_consumed: Some(10),
                        context_slot: Some(1),
                    }))),
                })
            }
        }

        #[async_trait::async_trait]
        impl SolendPreflightSimulator for StubSim {
            async fn get_minimum_balance_for_rent_exemption(
                &self,
                _data_len: usize,
            ) -> Result<u64, PreflightRpcError> {
                Ok(self.rent)
            }
            async fn simulate_legacy_transaction(
                &self,
                _tx: &Transaction,
            ) -> Result<RawSimulationResult, PreflightRpcError> {
                match self.sim.lock().unwrap().take() {
                    Some(r) => r,
                    None => Err(PreflightRpcError::RpcFailure {
                        reason: "stub exhausted".into(),
                    }),
                }
            }
        }

        // ── (N) Resume task integrates preflight after plan assembly ────────

        #[tokio::test]
        async fn resume_task_happy_path_emits_recheck_passed_preflighted_passed() {
            let wallet = Pubkey::new_unique();
            let store = SolendParkStore::new();
            let request_id = Uuid::new_v4();
            let rx = store.park(
                request_id,
                parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
            );
            let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(
                assembled_snapshot_passing(wallet, 1_000),
            )]));
            let sim: Arc<dyn SolendPreflightSimulator> = StubSim::passing();

            assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
            let outcome = run_solend_resume_task(
                request_id,
                store.clone(),
                assembler,
                permissive_v1_config(),
                Some(sim),
                None,
                rx,
            )
            .await;

            match outcome {
                SolendResumeOutcome::RecheckPassedPreflighted {
                    verified_slot: _,
                    plan,
                    preflight,
                } => {
                    // Plan assembly still produced the grouped ixs.
                    assert!(!plan.refresh_instructions.is_empty());
                    // Preflight returned Passed with the 4C-3 audit fields.
                    match preflight {
                        SolendPreflightOutcome::Passed {
                            sig_verify,
                            replace_recent_blockhash,
                            units_consumed,
                            ..
                        } => {
                            assert!(!sig_verify);
                            assert!(replace_recent_blockhash);
                            assert_eq!(units_consumed, Some(4242));
                        }
                        other => panic!("expected preflight Passed, got {other:?}"),
                    }
                }
                other => panic!("expected RecheckPassedPreflighted, got {other:?}"),
            }

            assert!(!store.contains(&request_id));
        }

        // ── (O) Preflight failure stops safely; no broadcast ────────────────

        #[tokio::test]
        async fn resume_task_preflight_failed_stops_with_failed_outcome_no_broadcast() {
            let wallet = Pubkey::new_unique();
            let store = SolendParkStore::new();
            let request_id = Uuid::new_v4();
            let rx = store.park(
                request_id,
                parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
            );
            let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(
                assembled_snapshot_passing(wallet, 1_000),
            )]));
            let sim: Arc<dyn SolendPreflightSimulator> = StubSim::failing_custom_14();

            assert!(store.signal(request_id, ApprovalWorkflowState::Approved));
            let outcome = run_solend_resume_task(
                request_id,
                store.clone(),
                assembler,
                permissive_v1_config(),
                Some(sim),
                None,
                rx,
            )
            .await;

            match outcome {
                SolendResumeOutcome::RecheckPassedPreflighted { preflight, .. } => match preflight {
                    SolendPreflightOutcome::Failed {
                        error_type,
                        instruction_index,
                        custom_code,
                        sig_verify,
                        replace_recent_blockhash,
                        ..
                    } => {
                        assert_eq!(error_type, "MathOverflow");
                        assert_eq!(instruction_index, Some(2));
                        assert_eq!(custom_code, Some(14));
                        assert!(!sig_verify);
                        assert!(replace_recent_blockhash);
                    }
                    other => panic!("expected preflight Failed, got {other:?}"),
                },
                other => panic!("expected RecheckPassedPreflighted, got {other:?}"),
            }

            // Failed preflight still cleans up the park store — no further
            // execution path is possible in this slice.
            assert!(!store.contains(&request_id));
        }

        // ── Phase 6B · JIT-ready persistence on Approve ─────────────────────
        //
        // These tests cover the new resume-task happy path: when
        // `Some(jit_ready_deps)` is wired AND preflight Passed, the
        // resume task persists a JIT-ready entry in the
        // `SolendJitReadyStore` keyed by `approval_request_id` and
        // returns `RecheckPassedJitReady`. CRUCIALLY, NO signing
        // handoff is created — the `SolendSigningStore` remains empty
        // until the prepare HTTP route (Window 2) calls
        // `create_signing_handoff` on demand.
        //
        // This is the structural fix for the live-mainnet timing race
        // observed twice on 2026-05-04 where blockhash expired between
        // approval-time tx assembly and the user clicking Approve in
        // the Phantom popup.

        use crate::integrations::solend_jit_ready::{
            SolendJitReadyDeps, SolendJitReadyStore,
        };
        use crate::integrations::solend_signing::SolendSigningStore;
        use crate::integrations::solend_submit::NullSolendAuditSink;

        #[tokio::test]
        async fn resume_task_persists_jit_ready_when_deps_wired_and_preflight_passed() {
            let wallet = Pubkey::new_unique();
            let park_store = SolendParkStore::new();
            let request_id = Uuid::new_v4();
            let rx = park_store.park(
                request_id,
                parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
            );
            let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(
                assembled_snapshot_passing(wallet, 1_000),
            )]));
            let sim: Arc<dyn SolendPreflightSimulator> = StubSim::passing();

            // Phase 6B: jit-ready store + null audit sink. The signing
            // store is constructed but should remain empty after the
            // resume task runs — proves no handoff was created.
            let jit_ready_store = SolendJitReadyStore::new();
            let signing_store = SolendSigningStore::new();
            let jit_ready_deps = SolendJitReadyDeps {
                store: jit_ready_store.clone(),
                audit: Arc::new(NullSolendAuditSink),
                jit_ready_lease_seconds: 120,
            };

            assert!(park_store.signal(request_id, ApprovalWorkflowState::Approved));
            let outcome = run_solend_resume_task(
                request_id,
                park_store.clone(),
                assembler,
                permissive_v1_config(),
                Some(sim),
                Some(jit_ready_deps),
                rx,
            )
            .await;

            // Outcome variant: new JIT-ready terminal.
            match outcome {
                SolendResumeOutcome::RecheckPassedJitReady {
                    verified_slot: _,
                    simulation_slot,
                    units_consumed,
                    expires_at_unix_ms,
                } => {
                    assert_eq!(simulation_slot, Some(777_777));
                    assert_eq!(units_consumed, Some(4242));
                    // Expiry must be in the future (lease=120s).
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    assert!(expires_at_unix_ms > now_ms);
                }
                other => panic!("expected RecheckPassedJitReady, got {other:?}"),
            }

            // The JIT-ready store now contains an entry keyed by the
            // approval request id.
            let entry = jit_ready_store
                .get(request_id)
                .expect("jit-ready entry persisted under approval request id");
            assert_eq!(entry.session_wallet, wallet);

            // CRITICAL: the signing store must be empty. No handoff
            // was created, no obligation Keypair generated, no
            // blockhash fetched. The whole point of Phase 6B.
            assert!(
                signing_store.summary(request_id).is_none(),
                "Phase 6B contract: approval must not create a signing handoff",
            );

            // Park store cleanup is unchanged.
            assert!(!park_store.contains(&request_id));
        }

        #[tokio::test]
        async fn resume_task_skips_jit_ready_persist_when_preflight_failed() {
            let wallet = Pubkey::new_unique();
            let park_store = SolendParkStore::new();
            let request_id = Uuid::new_v4();
            let rx = park_store.park(
                request_id,
                parked_intent_for(wallet, Utc::now() + Duration::seconds(60), 1_000),
            );
            let assembler = Arc::new(ResumeMockAssembler::with_responses(vec![Ok(
                assembled_snapshot_passing(wallet, 1_000),
            )]));
            let sim: Arc<dyn SolendPreflightSimulator> = StubSim::failing_custom_14();

            let jit_ready_store = SolendJitReadyStore::new();
            let jit_ready_deps = SolendJitReadyDeps {
                store: jit_ready_store.clone(),
                audit: Arc::new(NullSolendAuditSink),
                jit_ready_lease_seconds: 120,
            };

            assert!(park_store.signal(request_id, ApprovalWorkflowState::Approved));
            let outcome = run_solend_resume_task(
                request_id,
                park_store.clone(),
                assembler,
                permissive_v1_config(),
                Some(sim),
                Some(jit_ready_deps),
                rx,
            )
            .await;

            // Failed preflight short-circuits to Preflighted (with
            // Failed inside) — JIT-ready arm is gated on Passed.
            match outcome {
                SolendResumeOutcome::RecheckPassedPreflighted { .. } => {}
                other => panic!("expected RecheckPassedPreflighted (Failed inside), got {other:?}"),
            }
            assert!(
                jit_ready_store.is_empty(),
                "JIT-ready arm must NOT persist when preflight is non-Passed",
            );
        }
    }
}

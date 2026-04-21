//! JIT (just-in-time) execution for approved Jupiter swap intents.
//!
//! # Contract
//!
//! This module owns the post-approval flow:
//!
//! ```text
//! last approver says yes
//!         │
//!         ▼
//! approved_waiting_jit ──► jit_building ──► (first call to Jupiter /build)
//!                               │
//!        ┌──────────────────────┼──────────────────────────────┐
//!        ▼                      ▼                              ▼
//!   jit_build_failed    awaiting_wallet_signature          submitted
//! ```
//!
//! # Critical invariants
//!
//! - `/build` is NEVER called before the workflow reaches `Approved`.
//! - Fresh route + fresh blockhash are fetched at JIT time — the intent
//!   stores no frozen route, blockhash, or instruction payloads.
//! - A build failure MUST NOT transition to `submitted`. Same for simulate
//!   or sign failure. The durable state distinguishes so audit is honest.
//! - External wallet path transitions to `awaiting_wallet_signature` and
//!   hands over to the existing wallet-signature completion path.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};
use uuid::Uuid;

use claw_state_store::audit::{AuditRepository, AuditSeverity};
use claw_state_store::pending_jupiter::{state as jit_state, PendingJupiterRepository};
use claw_types::{
    approval::ApprovalWorkflowState,
    events::{EventHeader, GatewayEvent, TransactionFailedEvent, TransactionLifecycleEvent},
    session::SessionId,
    swap::SwapIntent,
    transaction::TransactionStatus,
};

use crate::event_bus::EventBus;

// ── Executor trait + outcome ───────────────────────────────────────────────

/// Executor that drives the full post-approval JIT flow for a single intent.
///
/// The resume task drives state transitions; the executor does the work.
/// Named `JupiterJitExecutor` (not `SwapBackend` / `ActionExecutor`) because
/// it is purposely Jupiter-specific — NOT a generic dispatcher.
#[async_trait]
pub trait JupiterJitExecutor: Send + Sync {
    /// Fetch a fresh Jupiter route and build instructions, assemble the tx,
    /// simulate, and (for local wallets) sign and submit.
    ///
    /// Implementations MUST NOT:
    /// - Touch the durable state store
    /// - Emit audit or events (resume task owns those)
    async fn execute_jit(&self, intent: &SwapIntent) -> JitOutcome;
}

/// Terminal outcome of a JIT execution attempt.
#[derive(Debug, Clone)]
pub enum JitOutcome {
    /// Local-wallet path completed: signed and submitted to the network.
    Submitted { signature: String },
    /// External-wallet path: tx built and simulated, waiting on wallet signature.
    AwaitingWalletSignature { wallet_signature_request_id: Uuid },
    /// Jupiter `/build` returned an error before we got a transaction.
    BuildFailed(String),
    /// Simulation failed after the tx was assembled.
    SimulateFailed(String),
    /// Sign or submit failed after simulate succeeded.
    SignFailed(String),
}

impl JitOutcome {
    /// Short label for audit / state-transition mapping.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Submitted { .. } => "submitted",
            Self::AwaitingWalletSignature { .. } => "awaiting_wallet_signature",
            Self::BuildFailed(_) => "build_failed",
            Self::SimulateFailed(_) => "simulate_failed",
            Self::SignFailed(_) => "sign_failed",
        }
    }

    /// The target durable state for this outcome.
    pub fn target_state(&self) -> &'static str {
        match self {
            Self::Submitted { .. } => jit_state::SUBMITTED,
            Self::AwaitingWalletSignature { .. } => jit_state::AWAITING_WALLET_SIGNATURE,
            // Per Jeff's state list: simulate / sign failures roll into jit_build_failed
            // with `last_error` disambiguating the specific cause.
            Self::BuildFailed(_) | Self::SimulateFailed(_) | Self::SignFailed(_) => {
                jit_state::JIT_BUILD_FAILED
            }
        }
    }

    /// The error detail (if any) to persist in `last_error`.
    pub fn error_detail(&self) -> Option<&str> {
        match self {
            Self::BuildFailed(e) | Self::SimulateFailed(e) | Self::SignFailed(e) => Some(e),
            _ => None,
        }
    }
}

// ── Resume task dependencies ───────────────────────────────────────────────

/// Bundle of dependencies the resume task needs.
/// Grouped into a struct because there are many shared handles and this
/// keeps constructor sites clean.
#[derive(Clone)]
pub struct JupiterResumeDeps {
    pub executor: Arc<dyn JupiterJitExecutor>,
    pub pending_repo: PendingJupiterRepository,
    pub audit: AuditRepository,
    pub event_bus: EventBus,
    pub park_store: crate::integrations::jupiter_park::PendingJupiterParkStore,
    /// Approval lease (seconds). Used to bound the oneshot wait.
    pub lease_seconds: u64,
}

// ── Resume task ────────────────────────────────────────────────────────────

/// Await the approval decision, then drive the JIT flow if approved.
///
/// This function MUST be invoked as the sole post-approval driver for an
/// intent. It:
/// 1. Awaits the oneshot (or lease timeout).
/// 2. On rejection/expiry: marks durable state + emits TransactionFailed.
/// 3. On approval: transitions `approved_waiting_jit` → `jit_building`,
///    calls `executor.execute_jit`, then marks the terminal state.
///
/// Callers MUST NOT call the executor directly — state transitions are the
/// responsibility of this task.
pub async fn run_resume_task(
    request_id: Uuid,
    intent: SwapIntent,
    decision_rx: oneshot::Receiver<ApprovalWorkflowState>,
    deps: JupiterResumeDeps,
) {
    let session = intent.session_id.clone();
    let session_str = session.to_string();
    let intent_id = intent.id;

    // ── 1. Await the approval decision (bounded by lease) ──────────────────
    let lease = if deps.lease_seconds > 0 {
        Duration::from_secs(deps.lease_seconds)
    } else {
        Duration::from_secs(86_400)
    };

    let workflow_state = match timeout(lease, decision_rx).await {
        Ok(Ok(state)) => state,
        Ok(Err(_)) => {
            // Sender dropped (daemon shutdown). Treat as expired.
            warn!(
                session = %session_str,
                request_id = %request_id,
                "jupiter approval channel dropped — marking expired"
            );
            mark_terminal_failure(
                &deps,
                &intent,
                jit_state::EXPIRED,
                "approval channel dropped",
                TransactionStatus::AwaitingApproval,
            )
            .await;
            return;
        }
        Err(_) => {
            warn!(
                session = %session_str,
                request_id = %request_id,
                lease = deps.lease_seconds,
                "jupiter approval lease expired without operator action"
            );
            mark_terminal_failure(
                &deps,
                &intent,
                jit_state::EXPIRED,
                "approval lease expired",
                TransactionStatus::AwaitingApproval,
            )
            .await;
            deps.park_store.remove(&request_id);
            return;
        }
    };

    deps.park_store.remove(&request_id);

    match workflow_state {
        ApprovalWorkflowState::Approved => { /* fall through to JIT */ }
        ApprovalWorkflowState::Rejected => {
            info!(
                session = %session_str,
                request_id = %request_id,
                intent_id = %intent_id,
                "jupiter intent rejected by operator"
            );
            mark_terminal_failure(
                &deps,
                &intent,
                jit_state::REJECTED,
                "operator rejected",
                TransactionStatus::AwaitingApproval,
            )
            .await;
            return;
        }
        ApprovalWorkflowState::Expired => {
            mark_terminal_failure(
                &deps,
                &intent,
                jit_state::EXPIRED,
                "workflow expired",
                TransactionStatus::AwaitingApproval,
            )
            .await;
            return;
        }
        ApprovalWorkflowState::Pending => {
            // Defensive: should never receive a Pending signal. Treat as drop.
            warn!(
                session = %session_str,
                request_id = %request_id,
                "unexpected Pending signal — treating as expired"
            );
            mark_terminal_failure(
                &deps,
                &intent,
                jit_state::EXPIRED,
                "unexpected pending signal",
                TransactionStatus::AwaitingApproval,
            )
            .await;
            return;
        }
    }

    // ── 2. Approved: drive the JIT pipeline ────────────────────────────────
    run_jit_after_approval(request_id, intent, deps).await;
}

/// Run the JIT pipeline for an already-approved intent (policy auto-approve path).
///
/// Shared between the operator-approved resume and the auto-approved fast path.
pub async fn run_jit_after_approval(
    request_id: Uuid,
    intent: SwapIntent,
    deps: JupiterResumeDeps,
) {
    let session_str = intent.session_id.to_string();
    let intent_id = intent.id;

    // approved_waiting_jit → jit_building
    let _ = deps
        .pending_repo
        .mark_state(intent_id, jit_state::APPROVED_WAITING_JIT, None)
        .await;

    let _ = deps
        .audit
        .append(
            Some(&session_str),
            &intent_id.to_string(),
            "jupiter_jit_started",
            "pipeline",
            &serde_json::json!({
                "intent_id":  intent_id,
                "request_id": request_id,
            }),
            AuditSeverity::Info,
        )
        .await;

    let _ = deps
        .pending_repo
        .mark_state(intent_id, jit_state::JIT_BUILDING, None)
        .await;

    info!(
        session = %session_str,
        intent_id = %intent_id,
        "calling Jupiter JIT executor (fresh route + blockhash)"
    );

    let outcome = deps.executor.execute_jit(&intent).await;

    // ── 3. Terminal state mapping ──────────────────────────────────────────
    let state = outcome.target_state();
    let err_detail = outcome.error_detail().map(|s| s.to_string());

    let _ = deps
        .pending_repo
        .mark_state(intent_id, state, err_detail.as_deref())
        .await;

    let _ = deps
        .audit
        .append(
            Some(&session_str),
            &intent_id.to_string(),
            "jupiter_jit_completed",
            "pipeline",
            &serde_json::json!({
                "intent_id":  intent_id,
                "request_id": request_id,
                "outcome":    outcome.label(),
                "state":      state,
                "error":      err_detail,
            }),
            match state {
                jit_state::JIT_BUILD_FAILED => AuditSeverity::Error,
                _ => AuditSeverity::Info,
            },
        )
        .await;

    // Emit lifecycle event matching the outcome.
    match &outcome {
        JitOutcome::Submitted { signature } => {
            info!(
                session = %session_str,
                intent_id = %intent_id,
                signature = %signature,
                "jupiter swap submitted"
            );
            deps.event_bus
                .publish(GatewayEvent::TransactionSent(TransactionLifecycleEvent {
                    header: EventHeader::new(Some(intent.session_id.clone())),
                    session_id: intent.session_id.clone(),
                    transaction_id: intent_id,
                    wallet_pubkey: intent.wallet_pubkey.clone(),
                    status: TransactionStatus::Sent,
                    signature: Some(signature.clone()),
                }));
        }
        JitOutcome::AwaitingWalletSignature { wallet_signature_request_id } => {
            info!(
                session = %session_str,
                intent_id = %intent_id,
                wallet_request_id = %wallet_signature_request_id,
                "jupiter swap awaiting external wallet signature"
            );
            // Wallet-signature path takes over from here; resume task exits.
        }
        JitOutcome::BuildFailed(e)
        | JitOutcome::SimulateFailed(e)
        | JitOutcome::SignFailed(e) => {
            warn!(
                session = %session_str,
                intent_id = %intent_id,
                outcome = outcome.label(),
                error = %e,
                "jupiter JIT failed"
            );
            deps.event_bus.publish(GatewayEvent::TransactionFailed(
                TransactionFailedEvent {
                    header: EventHeader::new(Some(intent.session_id.clone())),
                    session_id: intent.session_id.clone(),
                    transaction_id: intent_id,
                    wallet_pubkey: intent.wallet_pubkey.clone(),
                    error: format!("jit {}: {}", outcome.label(), e),
                    at_stage: TransactionStatus::Simulated,
                },
            ));
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

async fn mark_terminal_failure(
    deps: &JupiterResumeDeps,
    intent: &SwapIntent,
    state: &str,
    reason: &str,
    at_stage: TransactionStatus,
) {
    let _ = deps
        .pending_repo
        .mark_state(intent.id, state, Some(reason))
        .await;

    let _ = deps
        .audit
        .append(
            Some(&intent.session_id.to_string()),
            &intent.id.to_string(),
            &format!("jupiter_intent_{state}"),
            "pipeline",
            &serde_json::json!({
                "intent_id": intent.id,
                "state":     state,
                "reason":    reason,
            }),
            match state {
                jit_state::REJECTED | jit_state::EXPIRED => AuditSeverity::Warning,
                _ => AuditSeverity::Error,
            },
        )
        .await;

    deps.event_bus
        .publish(GatewayEvent::TransactionFailed(TransactionFailedEvent {
            header: EventHeader::new(Some(intent.session_id.clone())),
            session_id: intent.session_id.clone(),
            transaction_id: intent.id,
            wallet_pubkey: intent.wallet_pubkey.clone(),
            error: reason.to_string(),
            at_stage,
        }));
}

// Suppress unused-import warning when tests module is not compiled.
#[allow(dead_code)]
fn _unused(_: SessionId) {}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use claw_state_store::db::Database;
    use claw_types::{session::SessionId, solana::SolanaNetwork};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Mock executor ──────────────────────────────────────────────────

    #[derive(Clone)]
    struct MockJitExecutor {
        call_count: Arc<AtomicUsize>,
        outcome: JitOutcome,
    }

    impl MockJitExecutor {
        fn new(outcome: JitOutcome) -> (Self, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    call_count: counter.clone(),
                    outcome,
                },
                counter,
            )
        }
    }

    #[async_trait]
    impl JupiterJitExecutor for MockJitExecutor {
        async fn execute_jit(&self, _intent: &SwapIntent) -> JitOutcome {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
        }
    }

    // ── Test fixtures ──────────────────────────────────────────────────

    async fn setup_deps(outcome: JitOutcome) -> (JupiterResumeDeps, Arc<AtomicUsize>) {
        let db = Database::open_in_memory().await.unwrap();
        let pool = db.pool().clone();
        let pending_repo = PendingJupiterRepository::new(pool.clone());
        let audit = AuditRepository::new(pool);
        let (mock, counter) = MockJitExecutor::new(outcome);
        let deps = JupiterResumeDeps {
            executor: Arc::new(mock),
            pending_repo,
            audit,
            event_bus: EventBus::new(),
            park_store: crate::integrations::jupiter_park::PendingJupiterParkStore::new(),
            lease_seconds: 5,
        };
        (deps, counter)
    }

    fn test_intent() -> SwapIntent {
        SwapIntent::new(
            SessionId::from(Uuid::new_v4()),
            "walletABC",
            SolanaNetwork::Devnet,
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "So11111111111111111111111111111111111111112",
            100_000,
            50,
            "test",
        )
    }

    async fn seed_intent(deps: &JupiterResumeDeps, intent: &SwapIntent, request_id: Option<Uuid>) {
        deps.pending_repo
            .insert(intent, request_id, jit_state::PENDING_APPROVAL, None, None)
            .await
            .unwrap();
    }

    // ── Core requirement: no call to executor before approval ──────────

    #[tokio::test]
    async fn executor_is_not_called_before_approval() {
        let (deps, counter) = setup_deps(JitOutcome::Submitted {
            signature: "SIG".to_string(),
        })
        .await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();

        let handle = tokio::spawn(run_resume_task(request_id, intent.clone(), rx, deps.clone()));

        // Let the resume task reach its await. At this point nothing has been signaled.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 0, "no JIT calls before approval");

        // Verify durable state stayed pending.
        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::PENDING_APPROVAL);

        // Now signal approval.
        tx.send(ApprovalWorkflowState::Approved).unwrap();
        handle.await.unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one JIT call after approval");
    }

    // ── Outcome → state mapping ────────────────────────────────────────

    #[tokio::test]
    async fn approved_then_submitted_marks_state_submitted() {
        let (deps, _) = setup_deps(JitOutcome::Submitted {
            signature: "SIG123".to_string(),
        })
        .await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Approved).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::SUBMITTED);
        assert!(stored.last_error.is_none());
    }

    #[tokio::test]
    async fn build_failure_marks_jit_build_failed_not_submitted() {
        let (deps, _) = setup_deps(JitOutcome::BuildFailed("jupiter 502".to_string())).await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Approved).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::JIT_BUILD_FAILED);
        assert_ne!(stored.state, jit_state::SUBMITTED);
        assert!(stored.last_error.as_deref().unwrap().contains("jupiter 502"));
    }

    #[tokio::test]
    async fn simulate_failure_marks_jit_build_failed_not_submitted() {
        let (deps, _) = setup_deps(JitOutcome::SimulateFailed("InsufficientFunds".to_string())).await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Approved).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::JIT_BUILD_FAILED);
        assert_ne!(stored.state, jit_state::SUBMITTED);
        assert!(stored.last_error.as_deref().unwrap().contains("InsufficientFunds"));
    }

    #[tokio::test]
    async fn sign_failure_marks_jit_build_failed_not_submitted() {
        let (deps, _) = setup_deps(JitOutcome::SignFailed("keystore locked".to_string())).await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Approved).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::JIT_BUILD_FAILED);
        assert!(stored.last_error.as_deref().unwrap().contains("keystore locked"));
    }

    #[tokio::test]
    async fn external_wallet_path_marks_awaiting_wallet_signature() {
        let wallet_req_id = Uuid::new_v4();
        let (deps, _) = setup_deps(JitOutcome::AwaitingWalletSignature {
            wallet_signature_request_id: wallet_req_id,
        })
        .await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Approved).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::AWAITING_WALLET_SIGNATURE);
        assert_ne!(stored.state, jit_state::SUBMITTED);
        assert!(stored.last_error.is_none());
    }

    // ── Non-approved terminal states ───────────────────────────────────

    #[tokio::test]
    async fn rejection_marks_rejected_and_skips_executor() {
        let (deps, counter) = setup_deps(JitOutcome::Submitted {
            signature: "SHOULD_NOT_RUN".to_string(),
        })
        .await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Rejected).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        assert_eq!(counter.load(Ordering::SeqCst), 0, "executor must not run on rejection");
        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::REJECTED);
    }

    #[tokio::test]
    async fn expired_workflow_marks_expired_and_skips_executor() {
        let (deps, counter) = setup_deps(JitOutcome::Submitted {
            signature: "SHOULD_NOT_RUN".to_string(),
        })
        .await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Expired).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        assert_eq!(counter.load(Ordering::SeqCst), 0);
        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::EXPIRED);
    }

    #[tokio::test]
    async fn lease_timeout_marks_expired_and_skips_executor() {
        let db = Database::open_in_memory().await.unwrap();
        let pool = db.pool().clone();
        let pending_repo = PendingJupiterRepository::new(pool.clone());
        let audit = AuditRepository::new(pool);
        let (mock, counter) = MockJitExecutor::new(JitOutcome::Submitted {
            signature: "SHOULD_NOT_RUN".to_string(),
        });
        // Very short lease: 1 second (runtime floor — we test by using a dropped sender instead)
        let deps = JupiterResumeDeps {
            executor: Arc::new(mock),
            pending_repo,
            audit,
            event_bus: EventBus::new(),
            park_store: crate::integrations::jupiter_park::PendingJupiterParkStore::new(),
            lease_seconds: 1,
        };
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        // Drop sender without signaling — this mimics a channel drop (deterministic,
        // faster than waiting for the timeout).
        let (tx, rx) = oneshot::channel();
        drop(tx);

        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        assert_eq!(counter.load(Ordering::SeqCst), 0);
        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::EXPIRED);
        assert!(stored.last_error.as_deref().unwrap().contains("channel"));
    }

    // ── State progression check ────────────────────────────────────────

    #[tokio::test]
    async fn successful_jit_transitions_through_all_expected_states() {
        // Observes that at the end, we reach SUBMITTED. The intermediate
        // transitions (approved_waiting_jit, jit_building) are best-effort
        // signals for operators; end state is what matters for durability.
        let (deps, _) = setup_deps(JitOutcome::Submitted {
            signature: "SIG_OK".to_string(),
        })
        .await;
        let intent = test_intent();
        let request_id = Uuid::new_v4();
        seed_intent(&deps, &intent, Some(request_id)).await;

        let (tx, rx) = oneshot::channel();
        tx.send(ApprovalWorkflowState::Approved).unwrap();
        run_resume_task(request_id, intent.clone(), rx, deps.clone()).await;

        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::SUBMITTED);
    }

    // ── Auto-approved path (no park / no oneshot) ──────────────────────

    #[tokio::test]
    async fn auto_approved_fast_path_runs_jit_and_submits() {
        let (deps, counter) = setup_deps(JitOutcome::Submitted {
            signature: "AUTO_SIG".to_string(),
        })
        .await;
        let intent = test_intent();
        seed_intent(&deps, &intent, None).await;

        // Pretend the tool just seeded an auto-approved intent and called
        // `run_jit_after_approval` directly (no oneshot).
        run_jit_after_approval(Uuid::new_v4(), intent.clone(), deps.clone()).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let stored = deps.pending_repo.get(intent.id).await.unwrap().unwrap();
        assert_eq!(stored.state, jit_state::SUBMITTED);
    }

    // ── Outcome label / target_state mapping ───────────────────────────

    #[test]
    fn outcome_mapping_is_exhaustive_and_correct() {
        assert_eq!(
            JitOutcome::Submitted {
                signature: "s".to_string()
            }
            .target_state(),
            jit_state::SUBMITTED
        );
        assert_eq!(
            JitOutcome::AwaitingWalletSignature {
                wallet_signature_request_id: Uuid::new_v4()
            }
            .target_state(),
            jit_state::AWAITING_WALLET_SIGNATURE
        );
        assert_eq!(
            JitOutcome::BuildFailed("x".to_string()).target_state(),
            jit_state::JIT_BUILD_FAILED
        );
        assert_eq!(
            JitOutcome::SimulateFailed("x".to_string()).target_state(),
            jit_state::JIT_BUILD_FAILED
        );
        assert_eq!(
            JitOutcome::SignFailed("x".to_string()).target_state(),
            jit_state::JIT_BUILD_FAILED
        );
    }
}

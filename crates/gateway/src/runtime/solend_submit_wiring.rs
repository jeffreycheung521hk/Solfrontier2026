//! Phase 4C-6 — Solend submit handler wiring.
//!
//! Bridges the `claw-api` [`SolendSignatureHandler`] trait to the
//! gateway-owned Solend signing store, submit pipeline, and submission
//! lifecycle cache. `daemon.rs` constructs one [`GatewaySolendSignatureHandler`]
//! and installs it into `AppState::solend_signatures` as a
//! [`SolendSignatureHandlerRef`].
//!
//! # Responsibilities
//!
//! - **Retrieve:** session-ownership-scoped lookup in
//!   [`SolendSigningStore::tx_bytes_for_session`]. Found entries
//!   return the base64-encoded `tx_bytes` plus non-secret metadata.
//!   The obligation Keypair is NEVER exposed.
//! - **Submit:** session ownership is re-checked via
//!   [`SolendSigningStore::summary_for_session`] (peek, non-consuming)
//!   before calling [`submit_signed_solend_transaction`]. On a
//!   `Submitted` / `Expired` / `Rejected` / `BroadcastFailed` outcome
//!   the terminal state is recorded in
//!   [`SolendSubmissionLifecycleStore`] (first-write-wins). On
//!   `NotFound` — including the peek-missed case — the lifecycle cache
//!   is consulted for idempotent UX recovery.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT broadcast directly. All broadcast goes through
//!   [`submit_signed_solend_transaction`] which owns the verification
//!   pipeline.
//! - Does NOT mutate the parked Transaction. It reads bytes and
//!   forwards them.
//! - Does NOT poll signature status directly. Confirmation tracking
//!   lives in [`SolendConfirmationTracker`] whose `run()` loop is
//!   spawned by [`wire_solend_confirmation_tracker`] below (Phase 4C-7).
//! - The HTTP-driven `retrieve`/`submit` methods spawn no tasks of
//!   their own; each call is driven by the inbound request's tokio
//!   task.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use tracing::{debug, info, warn};

use claw_api::state::{
    SolendRetrievalResult, SolendSignatureHandler, SolendSubmitResult,
};
use claw_types::session::SessionId;

use crate::integrations::solend_confirmation::{
    SolendConfirmationTracker, SolendTrackedContext, TrackResult,
};
use crate::integrations::solend_lifecycle::{
    RecordedSubmitOutcome, RecoveredSubmission, SolendSubmissionLifecycleStore,
};
use crate::integrations::solend_signing::SolendSigningStore;
use crate::integrations::solend_submit::{
    submit_signed_solend_transaction, SolendAuditSink, SolendBlockHeightProvider,
    SolendSubmitOutcome, SolendTransactionSender,
};

/// Gateway-side implementation of [`SolendSignatureHandler`].
///
/// All fields are cloneable / `Arc`-wrapped so the containing
/// `AppState` clone on every request is cheap.
#[derive(Clone)]
pub struct GatewaySolendSignatureHandler {
    signing_store: SolendSigningStore,
    lifecycle_store: SolendSubmissionLifecycleStore,
    tx_sender: Arc<dyn SolendTransactionSender>,
    audit: Arc<dyn SolendAuditSink>,
    block_height_provider: Arc<dyn SolendBlockHeightProvider>,
    /// Phase 4C-7 confirmation tracker. `None` in minimal test harnesses
    /// that exercise submit verification without a confirmation loop.
    /// Production ALWAYS passes `Some(..)` — the daemon wires it
    /// alongside the run task.
    confirmation_tracker: Option<Arc<SolendConfirmationTracker>>,
}

impl GatewaySolendSignatureHandler {
    pub fn new(
        signing_store: SolendSigningStore,
        lifecycle_store: SolendSubmissionLifecycleStore,
        tx_sender: Arc<dyn SolendTransactionSender>,
        audit: Arc<dyn SolendAuditSink>,
        block_height_provider: Arc<dyn SolendBlockHeightProvider>,
    ) -> Self {
        Self {
            signing_store,
            lifecycle_store,
            tx_sender,
            audit,
            block_height_provider,
            confirmation_tracker: None,
        }
    }

    /// Attach a confirmation tracker. Idempotent: successive calls
    /// replace the handle. The tracker's `run()` task must be spawned
    /// by the caller (typically the daemon wiring block).
    pub fn with_confirmation_tracker(
        mut self,
        tracker: Arc<SolendConfirmationTracker>,
    ) -> Self {
        self.confirmation_tracker = Some(tracker);
        self
    }
}

fn recovered_to_retrieval_result(
    recovered: RecoveredSubmission,
) -> SolendRetrievalResult {
    let signing_request_id = recovered.signing_request_id;
    match recovered.outcome {
        RecordedSubmitOutcome::Submitted {
            tx_signature,
            intent_id,
            last_valid_block_height,
            ..
        } => SolendRetrievalResult::Submitted {
            signing_request_id,
            intent_id,
            tx_signature,
            last_valid_block_height,
        },
        RecordedSubmitOutcome::Confirming {
            tx_signature,
            slot,
            intent_id,
            last_valid_block_height,
            ..
        } => SolendRetrievalResult::Confirming {
            signing_request_id,
            intent_id,
            tx_signature,
            slot,
            last_valid_block_height,
        },
        RecordedSubmitOutcome::Finalized {
            tx_signature,
            slot,
            intent_id,
            ..
        } => SolendRetrievalResult::Finalized {
            signing_request_id,
            intent_id,
            tx_signature,
            slot,
        },
        RecordedSubmitOutcome::ExecutionFailed {
            tx_signature,
            intent_id,
            err,
            ..
        } => SolendRetrievalResult::Failed {
            signing_request_id,
            intent_id,
            tx_signature,
            err,
        },
        RecordedSubmitOutcome::ConfirmationTimeout {
            tx_signature,
            last_valid_block_height,
            current_block_height,
            reason,
        } => SolendRetrievalResult::ConfirmationTimeout {
            signing_request_id,
            tx_signature,
            last_valid_block_height,
            current_block_height,
            requires_reproposal: true,
            reason,
        },
        RecordedSubmitOutcome::Rejected { error_type, message } => {
            SolendRetrievalResult::Rejected {
                signing_request_id,
                error_type,
                message,
            }
        }
        RecordedSubmitOutcome::BroadcastFailed { error_type, message } => {
            SolendRetrievalResult::BroadcastFailed {
                signing_request_id,
                error_type,
                message,
            }
        }
        RecordedSubmitOutcome::Expired { reason } => {
            SolendRetrievalResult::PreSubmitExpired { reason }
        }
    }
}

fn recovered_to_submit_result(
    recovered: RecoveredSubmission,
) -> SolendSubmitResult {
    let recorded_at_unix_ms = recovered.recorded_at.timestamp_millis();
    let signing_request_id = recovered.signing_request_id;
    match recovered.outcome {
        // Any variant carrying a `tx_signature` collapses to the
        // idempotent `Recovered` wire shape for the POST endpoint.
        // The client uses the GET endpoint to read the latest
        // confirmation state (Submitted / Confirming / Finalized /
        // ExecutionFailed / ConfirmationTimeout). Keeping POST's
        // outcome set minimal avoids duplicating lifecycle-read logic
        // into the POST contract.
        RecordedSubmitOutcome::Submitted { tx_signature, .. }
        | RecordedSubmitOutcome::Confirming { tx_signature, .. }
        | RecordedSubmitOutcome::Finalized { tx_signature, .. }
        | RecordedSubmitOutcome::ExecutionFailed { tx_signature, .. }
        | RecordedSubmitOutcome::ConfirmationTimeout { tx_signature, .. } => {
            SolendSubmitResult::Recovered {
                signing_request_id,
                tx_signature,
                recorded_at_unix_ms,
            }
        }
        RecordedSubmitOutcome::Expired { reason } => SolendSubmitResult::Expired { reason },
        RecordedSubmitOutcome::Rejected { error_type, message } => {
            SolendSubmitResult::Rejected { error_type, message }
        }
        RecordedSubmitOutcome::BroadcastFailed { error_type, message } => {
            SolendSubmitResult::BroadcastFailed {
                signing_request_id,
                error_type,
                message,
            }
        }
    }
}

impl SolendSignatureHandler for GatewaySolendSignatureHandler {
    fn retrieve(
        &self,
        session_id: &SessionId,
        signing_request_id: uuid::Uuid,
    ) -> Pin<Box<dyn Future<Output = SolendRetrievalResult> + Send + '_>> {
        let session_id = session_id.clone();
        Box::pin(async move {
            // 1. Still awaiting user signature? Return the parked bytes.
            if let Some((summary, tx_bytes)) = self
                .signing_store
                .tx_bytes_for_session(&session_id, signing_request_id)
            {
                let unsigned_tx_b64 =
                    base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
                return SolendRetrievalResult::Found {
                    signing_request_id: summary.signing_request_id,
                    intent_id: summary.intent_id,
                    session_wallet: summary.session_wallet.to_string(),
                    unsigned_tx_b64,
                    obligation_signer_backend_partial: summary
                        .obligation_signer_backend_partial,
                    last_valid_block_height: summary.last_valid_block_height,
                    expires_at_unix_ms: summary.expires_at.timestamp_millis(),
                    verified_slot: summary.verified_slot,
                    simulation_slot: summary.simulation_slot,
                    units_consumed: summary.units_consumed,
                };
            }

            // 2. Phase 4C-7 — consume happened; consult the
            //    lifecycle store for the latest known state. This
            //    returns the same snapshot the confirmation tracker
            //    has been maintaining in-memory.
            if let Some(recovered) = self
                .lifecycle_store
                .lookup_for_session(&session_id, signing_request_id)
            {
                return recovered_to_retrieval_result(recovered);
            }

            // 3. No parked entry, no lifecycle record, OR session
            //    mismatch — collapse to `NotFound` (never leaks
            //    existence to cross-session probes).
            SolendRetrievalResult::NotFound
        })
    }

    fn submit(
        &self,
        session_id: &SessionId,
        signing_request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> Pin<Box<dyn Future<Output = SolendSubmitResult> + Send + '_>> {
        let session_id = session_id.clone();
        Box::pin(async move {
            // Peek (non-consuming) for session ownership. If the entry
            // is present but owned by a different session, this returns
            // None just like "missing". Both collapse to NotFound after
            // consulting the lifecycle cache for UX recovery.
            let peek = self
                .signing_store
                .summary_for_session(&session_id, signing_request_id);

            if peek.is_none() {
                return match self
                    .lifecycle_store
                    .lookup_for_session(&session_id, signing_request_id)
                {
                    Some(r) => recovered_to_submit_result(r),
                    None => SolendSubmitResult::NotFound,
                };
            }

            // Ownership verified. Delegate to the 4C-5 submit pipeline.
            let outcome = submit_signed_solend_transaction(
                signing_request_id,
                &signed_tx_b64,
                &self.signing_store,
                &*self.tx_sender,
                &*self.audit,
                &*self.block_height_provider,
            )
            .await;

            match outcome {
                SolendSubmitOutcome::Submitted {
                    tx_signature,
                    signing_request_id,
                    intent_id,
                    session_wallet,
                    verified_slot,
                    last_valid_block_height,
                } => {
                    let _ = self.lifecycle_store.record(
                        signing_request_id,
                        session_id.clone(),
                        RecordedSubmitOutcome::Submitted {
                            tx_signature: tx_signature.clone(),
                            intent_id,
                            session_wallet,
                            verified_slot,
                            last_valid_block_height,
                        },
                    );
                    info!(
                        signing_request_id = %signing_request_id,
                        intent_id = %intent_id,
                        "solend submit lifecycle recorded Submitted"
                    );

                    // Phase 4C-7 — hand the signature to the
                    // confirmation tracker. Fail-open on enqueue
                    // failure: the chain-side broadcast has already
                    // succeeded; withholding the user's success
                    // response because our in-memory poller can't
                    // parse the signature would be worse for UX than
                    // dropping confirmation tracking for this one tx.
                    // We emit a warning-severity audit event so the
                    // drop is visible.
                    if let Some(ref tracker) = self.confirmation_tracker {
                        // Fetch submission block height once; if the
                        // RPC fails here we use last_valid_block_height
                        // as a conservative upper bound so the state
                        // machine's expiry comparison still works.
                        let submission_height = self
                            .block_height_provider
                            .current_block_height()
                            .await
                            .unwrap_or(last_valid_block_height);
                        match tx_signature.parse::<solana_sdk::signature::Signature>() {
                            Ok(parsed_sig) => {
                                let ctx = SolendTrackedContext::new_submitted(
                                    signing_request_id,
                                    intent_id,
                                    session_id.clone(),
                                    session_wallet,
                                    last_valid_block_height,
                                    submission_height,
                                    parsed_sig,
                                );
                                match tracker.enqueue(ctx) {
                                    TrackResult::Started => {
                                        info!(
                                            signing_request_id = %signing_request_id,
                                            signature = %tx_signature,
                                            "solend confirmation tracker enqueued"
                                        );
                                    }
                                    TrackResult::AlreadyTracked => {
                                        debug!(
                                            signing_request_id = %signing_request_id,
                                            signature = %tx_signature,
                                            "solend confirmation tracker: signature already tracked"
                                        );
                                    }
                                    TrackResult::AlreadyTerminal => {
                                        debug!(
                                            signing_request_id = %signing_request_id,
                                            signature = %tx_signature,
                                            "solend confirmation tracker: signing_request terminal"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    signing_request_id = %signing_request_id,
                                    signature = %tx_signature,
                                    error = %e,
                                    "solend confirmation tracker: cannot parse signature; tx broadcast but confirmation not tracked"
                                );
                                self.audit
                                    .append_solend_event(
                                        crate::integrations::solend_confirmation::AUDIT_EVENT_CONFIRMATION_POLL_ERROR,
                                        signing_request_id.to_string(),
                                        Some(session_id.to_string()),
                                        claw_state_store::audit::AuditSeverity::Warning,
                                        serde_json::json!({
                                            "signing_request_id": signing_request_id,
                                            "tx_signature":       tx_signature,
                                            "error":              format!("{e}"),
                                            "phase":              "enqueue_parse_signature",
                                        }),
                                    )
                                    .await;
                            }
                        }
                    } else {
                        debug!(
                            signing_request_id = %signing_request_id,
                            "solend confirmation tracker not wired (test harness)"
                        );
                    }

                    SolendSubmitResult::Submitted {
                        signing_request_id,
                        intent_id,
                        session_wallet: session_wallet.to_string(),
                        tx_signature,
                        verified_slot,
                        last_valid_block_height,
                    }
                }
                // The parked entry was consumed between peek and call
                // (concurrent duplicate from the same session). The
                // winner's lifecycle record will populate; re-consult.
                SolendSubmitOutcome::NotFound => match self
                    .lifecycle_store
                    .lookup_for_session(&session_id, signing_request_id)
                {
                    Some(r) => recovered_to_submit_result(r),
                    None => SolendSubmitResult::NotFound,
                },
                SolendSubmitOutcome::Expired { reason } => {
                    let _ = self.lifecycle_store.record(
                        signing_request_id,
                        session_id.clone(),
                        RecordedSubmitOutcome::Expired {
                            reason: reason.clone(),
                        },
                    );
                    SolendSubmitResult::Expired { reason }
                }
                SolendSubmitOutcome::Rejected { error_type, message } => {
                    let _ = self.lifecycle_store.record(
                        signing_request_id,
                        session_id.clone(),
                        RecordedSubmitOutcome::Rejected {
                            error_type: error_type.clone(),
                            message: message.clone(),
                        },
                    );
                    SolendSubmitResult::Rejected { error_type, message }
                }
                SolendSubmitOutcome::BroadcastFailed {
                    error_type,
                    message,
                    signing_request_id,
                } => {
                    let _ = self.lifecycle_store.record(
                        signing_request_id,
                        session_id.clone(),
                        RecordedSubmitOutcome::BroadcastFailed {
                            error_type: error_type.clone(),
                            message: message.clone(),
                        },
                    );
                    SolendSubmitResult::BroadcastFailed {
                        signing_request_id,
                        error_type,
                        message,
                    }
                }
            }
        })
    }
}

// ── Phase 4C-7 confirmation tracker wiring ──────────────────────────────────
//
// Keeps the daemon call-site to a single line (per DEBT.md D-2). This
// function:
//
// 1. Constructs the production [`ClawRpcStatusProvider`] adapter
//    wrapping the shared `RpcPool`.
// 2. Constructs a [`SolendConfirmationTracker`] driven by that
//    provider, with Solend-specific audit + lifecycle sinks.
// 3. Performs boot-up recovery: scans the lifecycle store for any
//    non-terminal (Submitted / Confirming) entries and re-enqueues
//    their signatures into the tracker. **No re-submit, no re-sign,
//    no re-handoff.** The state machine picks up where it left off
//    from on-chain observation alone.
// 4. Spawns the tracker's `run()` loop on a background tokio task.
// 5. Returns the `Arc<SolendConfirmationTracker>` so the caller can
//    wire it into [`GatewaySolendSignatureHandler`] via
//    `with_confirmation_tracker(..)`.
//
// **Persistence limitation (documented).** The lifecycle store is
// in-memory; process-restart recovery for user-facing submit state
// requires persistent lifecycle storage, which is outside the scope of
// this slice. Within a single daemon lifetime (handler replacement in
// tests, hot-reload scenarios) the recovery step above restores
// tracker coverage faithfully.

use crate::integrations::solend_confirmation::ClawRpcStatusProvider;
use crate::integrations::solend_confirmation::SolendSignatureStatusProvider;

/// Construct the Solend confirmation tracker, run boot-up recovery,
/// and spawn the polling task. See module docs for design + D-2 scope.
pub fn wire_solend_confirmation_tracker(
    lifecycle_store: SolendSubmissionLifecycleStore,
    rpc_pool: Arc<claw_solana_core::rpc::RpcPool>,
    audit: Arc<dyn crate::integrations::solend_submit::SolendAuditSink>,
    poll_interval: std::time::Duration,
) -> Arc<SolendConfirmationTracker> {
    let status_provider: Arc<dyn SolendSignatureStatusProvider> =
        Arc::new(ClawRpcStatusProvider::new(rpc_pool));
    let tracker = Arc::new(SolendConfirmationTracker::new(
        lifecycle_store.clone(),
        status_provider,
        audit,
        poll_interval,
    ));

    // Boot-up recovery: any non-terminal entries left over from a
    // handler replacement get re-enqueued. A fresh process sees an
    // empty store — the true-restart limitation is documented above.
    let mut recovered = 0usize;
    let mut unparseable = 0usize;
    for (signing_request_id, session_id, outcome) in lifecycle_store.non_terminal_snapshot() {
        let (tx_signature, intent_id, session_wallet, last_valid_block_height) = match outcome {
            RecordedSubmitOutcome::Submitted {
                tx_signature,
                intent_id,
                session_wallet,
                last_valid_block_height,
                ..
            }
            | RecordedSubmitOutcome::Confirming {
                tx_signature,
                intent_id,
                session_wallet,
                last_valid_block_height,
                ..
            } => (tx_signature, intent_id, session_wallet, last_valid_block_height),
            // Terminal / pre-submit variants excluded by `non_terminal_snapshot`.
            _ => continue,
        };
        let sig = match tx_signature.parse::<solana_sdk::signature::Signature>() {
            Ok(s) => s,
            Err(_) => {
                unparseable += 1;
                continue;
            }
        };
        let ctx = SolendTrackedContext::new_submitted(
            signing_request_id,
            intent_id,
            session_id,
            session_wallet,
            last_valid_block_height,
            // `submission_block_height = 0` is a conservative default
            // for recovery. The state machine's expiry logic uses
            // `current_height > last_valid_block_height` which doesn't
            // depend on `submission_height`; the grace-period logic
            // only lowers the bar for declaring `Dropped` (a
            // provisional state), which is still polled through to a
            // definitive terminal.
            0,
            sig,
        );
        if matches!(tracker.enqueue(ctx), TrackResult::Started) {
            recovered += 1;
        }
    }
    if recovered > 0 || unparseable > 0 {
        info!(
            recovered,
            unparseable,
            "solend confirmation recovery: re-enqueued non-terminal lifecycle entries"
        );
    }

    // Spawn the polling loop. Runs until `tracker.shutdown()` or the
    // runtime drops.
    let task_tracker = tracker.clone();
    tokio::spawn(async move { task_tracker.run().await });
    info!("solend confirmation tracker task spawned");

    tracker
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use solana_sdk::pubkey::Pubkey;
    use std::sync::Mutex;

    use crate::integrations::solend_submit::{NullSolendAuditSink, SolendSubmitError};

    // ── Stubs for the trait seams ──────────────────────────────────────────

    struct StubSender {
        calls: Mutex<usize>,
        signature: String,
    }

    impl StubSender {
        fn ok(sig: &str) -> Self {
            Self {
                calls: Mutex::new(0),
                signature: sig.to_string(),
            }
        }
        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl SolendTransactionSender for StubSender {
        async fn send_raw_transaction(
            &self,
            _tx_bytes: &[u8],
        ) -> Result<String, SolendSubmitError> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.signature.clone())
        }
    }

    struct StubHeight {
        height: u64,
    }

    #[async_trait]
    impl SolendBlockHeightProvider for StubHeight {
        async fn current_block_height(&self) -> Result<u64, SolendSubmitError> {
            Ok(self.height)
        }
    }

    // ── Retrieval: not-found short-circuits ────────────────────────────────

    #[tokio::test]
    async fn retrieve_returns_not_found_when_store_is_empty() {
        let handler = GatewaySolendSignatureHandler::new(
            SolendSigningStore::new(),
            SolendSubmissionLifecycleStore::new(120),
            Arc::new(StubSender::ok("any")),
            Arc::new(NullSolendAuditSink),
            Arc::new(StubHeight { height: 0 }),
        );

        let result = handler
            .retrieve(&SessionId::new(), uuid::Uuid::new_v4())
            .await;
        assert_eq!(result, SolendRetrievalResult::NotFound);
    }

    // ── Submit: NotFound + empty lifecycle → NotFound ──────────────────────

    #[tokio::test]
    async fn submit_returns_not_found_when_nothing_parked_and_no_lifecycle() {
        let handler = GatewaySolendSignatureHandler::new(
            SolendSigningStore::new(),
            SolendSubmissionLifecycleStore::new(120),
            Arc::new(StubSender::ok("any")),
            Arc::new(NullSolendAuditSink),
            Arc::new(StubHeight { height: 0 }),
        );

        let result = handler
            .submit(
                &SessionId::new(),
                uuid::Uuid::new_v4(),
                "AAAA".to_string(),
            )
            .await;
        assert_eq!(result, SolendSubmitResult::NotFound);
    }

    // ── Submit: NotFound + lifecycle Submitted → Recovered ─────────────────

    #[tokio::test]
    async fn submit_returns_recovered_when_lifecycle_has_prior_submission() {
        let signing_store = SolendSigningStore::new();
        let lifecycle_store = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle_store.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::Submitted {
                tx_signature: "CACHED-SIG".to_string(),
                intent_id: uuid::Uuid::new_v4(),
                session_wallet: Pubkey::new_unique(),
                verified_slot: 1,
                last_valid_block_height: 2,
            },
        );

        let sender = Arc::new(StubSender::ok("never-called"));
        let handler = GatewaySolendSignatureHandler::new(
            signing_store,
            lifecycle_store,
            sender.clone(),
            Arc::new(NullSolendAuditSink),
            Arc::new(StubHeight { height: 0 }),
        );

        let result = handler.submit(&owner, rid, "AAAA".to_string()).await;
        match result {
            SolendSubmitResult::Recovered {
                signing_request_id,
                tx_signature,
                ..
            } => {
                assert_eq!(signing_request_id, rid);
                assert_eq!(tx_signature, "CACHED-SIG");
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
        assert_eq!(
            sender.call_count(),
            0,
            "recovered path MUST NOT re-broadcast"
        );
    }

    // ── Submit: NotFound + lifecycle Rejected → Rejected ───────────────────

    #[tokio::test]
    async fn submit_returns_rejected_when_lifecycle_has_prior_rejection() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::Rejected {
                error_type: "MESSAGE_HASH_MISMATCH".to_string(),
                message: "user tampered with instructions".to_string(),
            },
        );

        let handler = GatewaySolendSignatureHandler::new(
            SolendSigningStore::new(),
            lifecycle,
            Arc::new(StubSender::ok("never-called")),
            Arc::new(NullSolendAuditSink),
            Arc::new(StubHeight { height: 0 }),
        );

        let result = handler.submit(&owner, rid, "AAAA".to_string()).await;
        match result {
            SolendSubmitResult::Rejected { error_type, .. } => {
                assert_eq!(error_type, "MESSAGE_HASH_MISMATCH");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    // ── Submit: wrong session → NotFound (no lifecycle leak) ───────────────

    #[tokio::test]
    async fn submit_returns_not_found_for_wrong_session_even_when_lifecycle_has_it() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let attacker = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::Submitted {
                tx_signature: "DO-NOT-LEAK".to_string(),
                intent_id: uuid::Uuid::new_v4(),
                session_wallet: Pubkey::new_unique(),
                verified_slot: 1,
                last_valid_block_height: 2,
            },
        );

        let handler = GatewaySolendSignatureHandler::new(
            SolendSigningStore::new(),
            lifecycle,
            Arc::new(StubSender::ok("never-called")),
            Arc::new(NullSolendAuditSink),
            Arc::new(StubHeight { height: 0 }),
        );

        let result = handler.submit(&attacker, rid, "AAAA".to_string()).await;
        assert_eq!(result, SolendSubmitResult::NotFound);
    }

    // ── 4C-7 retrieve-after-submit lifecycle states ───────────────────────

    fn handler_with_lifecycle(
        lifecycle: SolendSubmissionLifecycleStore,
    ) -> GatewaySolendSignatureHandler {
        GatewaySolendSignatureHandler::new(
            SolendSigningStore::new(),
            lifecycle,
            Arc::new(StubSender::ok("never-called")),
            Arc::new(NullSolendAuditSink),
            Arc::new(StubHeight { height: 0 }),
        )
    }

    #[tokio::test]
    async fn retrieve_returns_submitted_when_lifecycle_has_submitted() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::Submitted {
                tx_signature: "SUBMIT-SIG".to_string(),
                intent_id: uuid::Uuid::new_v4(),
                session_wallet: Pubkey::new_unique(),
                verified_slot: 5,
                last_valid_block_height: 1_000,
            },
        );
        let handler = handler_with_lifecycle(lifecycle);
        let result = handler.retrieve(&owner, rid).await;
        match result {
            SolendRetrievalResult::Submitted {
                signing_request_id,
                tx_signature,
                ..
            } => {
                assert_eq!(signing_request_id, rid);
                assert_eq!(tx_signature, "SUBMIT-SIG");
            }
            other => panic!("expected Submitted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_returns_confirming_when_lifecycle_has_confirming() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::Confirming {
                tx_signature: "CONF-SIG".to_string(),
                slot: 12_345,
                intent_id: uuid::Uuid::new_v4(),
                session_wallet: Pubkey::new_unique(),
                last_valid_block_height: 1_000,
            },
        );
        let handler = handler_with_lifecycle(lifecycle);
        let result = handler.retrieve(&owner, rid).await;
        match result {
            SolendRetrievalResult::Confirming { slot, .. } => assert_eq!(slot, 12_345),
            other => panic!("expected Confirming, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_returns_finalized_when_lifecycle_terminal_success() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::Finalized {
                tx_signature: "FINAL-SIG".to_string(),
                slot: 77_777,
                intent_id: uuid::Uuid::new_v4(),
                session_wallet: Pubkey::new_unique(),
                last_valid_block_height: 1_000,
            },
        );
        let handler = handler_with_lifecycle(lifecycle);
        let result = handler.retrieve(&owner, rid).await;
        match result {
            SolendRetrievalResult::Finalized {
                slot, tx_signature, ..
            } => {
                assert_eq!(slot, 77_777);
                assert_eq!(tx_signature, "FINAL-SIG");
            }
            other => panic!("expected Finalized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_returns_confirmation_timeout_with_requires_reproposal() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::ConfirmationTimeout {
                tx_signature: "TIMEOUT-SIG".to_string(),
                last_valid_block_height: 1_000,
                current_block_height: 1_100,
                reason: "height past".into(),
            },
        );
        let handler = handler_with_lifecycle(lifecycle);
        let result = handler.retrieve(&owner, rid).await;
        match result {
            SolendRetrievalResult::ConfirmationTimeout {
                requires_reproposal,
                current_block_height,
                ..
            } => {
                assert!(requires_reproposal);
                assert_eq!(current_block_height, 1_100);
            }
            other => panic!("expected ConfirmationTimeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_returns_failed_on_execution_error() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::ExecutionFailed {
                tx_signature: "FAIL-SIG".to_string(),
                intent_id: uuid::Uuid::new_v4(),
                session_wallet: Pubkey::new_unique(),
                err: "Custom(42)".into(),
            },
        );
        let handler = handler_with_lifecycle(lifecycle);
        let result = handler.retrieve(&owner, rid).await;
        match result {
            SolendRetrievalResult::Failed { err, .. } => assert_eq!(err, "Custom(42)"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_hides_lifecycle_state_from_wrong_session() {
        let lifecycle = SolendSubmissionLifecycleStore::new(120);
        let owner = SessionId::new();
        let attacker = SessionId::new();
        let rid = uuid::Uuid::new_v4();
        lifecycle.record(
            rid,
            owner.clone(),
            RecordedSubmitOutcome::Finalized {
                tx_signature: "DO-NOT-LEAK".to_string(),
                slot: 5,
                intent_id: uuid::Uuid::new_v4(),
                session_wallet: Pubkey::new_unique(),
                last_valid_block_height: 1_000,
            },
        );
        let handler = handler_with_lifecycle(lifecycle);
        let result = handler.retrieve(&attacker, rid).await;
        assert_eq!(result, SolendRetrievalResult::NotFound);
    }

    // ── Forbidden-scan ─────────────────────────────────────────────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("solend_submit_wiring.rs");
        // Needles use the `.<method>(` call-shape so we catch actual
        // invocations without false-matching the `impl SolendTransactionSender`
        // method declaration in this module's own `#[cfg(test)]` stub.
        //
        // Note: `tokio::spawn(` is NOT scanned — Phase 4C-7 legitimately
        // spawns the tracker's `run()` task via
        // `wire_solend_confirmation_tracker`. The scanner in
        // `solend_confirmation.rs` covers that module individually.
        let needles = [
            format!("{}{}{}", ".", "send_raw_", "transaction("),
            format!("{}{}{}", ".", "send_raw_v0_", "transaction("),
            format!("{}{}{}", ".", "confirm_", "transaction("),
            format!("{}{}", "Versioned", "Transaction"),
            format!("{}{}", "Message", "V0"),
            format!("{}{}", "AddressLookup", "Table"),
            format!("{}{}", "Transaction::", "new_signed_with_payer("),
            format!("{}{}", "simulate_", "transaction("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n),
                "solend_submit_wiring.rs must not contain `{n}`"
            );
        }
    }
}

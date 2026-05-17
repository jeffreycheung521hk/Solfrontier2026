//! Lifecycle persister — durable persistence + event emission for transaction tracking.
//!
//! Receives [`StateChange`] messages from the [`TransactionTracker`] via an
//! unbounded mpsc channel and:
//!
//! 1. Persists state transitions to SQLite (`transaction_tracking` table)
//! 2. Emits execution-plane events to the [`EventBus`]
//! 3. Appends audit records to [`AuditRepository`]
//!
//! All operations are idempotent: event emission flags in the DB prevent
//! duplicate events across restarts.
//!
//! # Architecture
//!
//! The persister runs as a dedicated background task, decoupled from the
//! tracker's RPC polling loop. This prevents DB writes and event emission
//! from blocking RPC calls.
//!
//! # Defensive correction
//!
//! The `Expired -> Confirmed` override is handled gracefully: the DB row
//! is updated to `confirmed` and `TransactionConfirmed` is emitted.

use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use claw_solana_core::tracker::StateChange;
use claw_solana_core::tx_lifecycle::TxState;
use claw_state_store::audit::{AuditRepository, AuditSeverity};
use claw_state_store::tracking::TransactionTrackingRepository;
use claw_types::events::{
    EventHeader, GatewayEvent, TransactionFailedEvent, TransactionLifecycleEvent,
};
use claw_types::session::SessionId;
use claw_types::transaction::TransactionStatus;

use claw_observability::metrics::MetricsRegistry;
use crate::event_bus::EventBus;

/// Listens for transaction lifecycle state changes and persists them
/// durably, emitting events and audit records exactly once.
pub struct LifecyclePersister {
    tracking_repo: TransactionTrackingRepository,
    audit:         AuditRepository,
    event_bus:     EventBus,
    rx:            mpsc::UnboundedReceiver<StateChange>,
    metrics:       std::sync::Arc<MetricsRegistry>,
}

impl LifecyclePersister {
    /// Creates a new persister.
    pub fn new(
        tracking_repo: TransactionTrackingRepository,
        audit:         AuditRepository,
        event_bus:     EventBus,
        rx:            mpsc::UnboundedReceiver<StateChange>,
        metrics:       std::sync::Arc<MetricsRegistry>,
    ) -> Self {
        Self { tracking_repo, audit, event_bus, rx, metrics }
    }

    /// Runs the persister loop. Spawned as a background task.
    pub async fn run(mut self) {
        info!("lifecycle persister started");

        while let Some(change) = self.rx.recv().await {
            self.handle_change(&change).await;
        }

        info!("lifecycle persister stopped (channel closed)");
    }

    async fn handle_change(&self, change: &StateChange) {
        let state_str = tx_state_to_str(&change.to);
        let slot = change.to.slot();
        let err = match &change.to {
            TxState::Failed { err } => Some(err.as_str()),
            _ => None,
        };

        // Step 1: Persist state transition to DB.
        if let Err(e) = self.tracking_repo.update_state(
            &change.signature,
            state_str,
            slot,
            err,
        ).await {
            warn!(
                signature = %change.signature,
                state = state_str,
                error = %e,
                "failed to persist lifecycle state — will retry on next observation"
            );
            return; // Don't emit events if DB write failed.
        }

        // Step 2: Emit event + audit based on new state.
        // Check DB flags to prevent duplicate emission.
        let session_id = SessionId::from(
            change.session_id.parse::<uuid::Uuid>().unwrap_or(uuid::Uuid::nil())
        );

        match &change.to {
            TxState::Confirmed { slot } => {
                self.metrics.transactions_confirmed.increment();
                self.emit_confirmed(change, &session_id, *slot).await;
            }
            TxState::Finalized { slot } => {
                // If confirmed wasn't emitted yet (direct Submitted→Finalized jump),
                // emit it first.
                self.emit_confirmed(change, &session_id, *slot).await;
                self.emit_finalized(change, &session_id, *slot).await;
            }
            TxState::Failed { ref err } => {
                self.metrics.transactions_failed.increment();
                self.emit_execution_failed(change, &session_id, err).await;
            }
            TxState::Dropped => {
                self.metrics.transactions_failed.increment();
                self.emit_dropped(change, &session_id).await;
            }
            TxState::Expired => {
                self.metrics.transactions_failed.increment();
                self.emit_expired(change, &session_id).await;
            }
            TxState::Submitted => {
                // No event for Submitted — already emitted at submission time.
            }
        }
    }

    async fn emit_confirmed(&self, change: &StateChange, session_id: &SessionId, slot: u64) {
        // Idempotency: check DB flag.
        if let Err(e) = self.tracking_repo.mark_event_emitted(
            &change.signature, "confirmed_emitted",
        ).await {
            warn!(error = %e, "failed to mark confirmed_emitted");
            return;
        }

        let _ = self.audit.append(
            Some(&change.session_id),
            &change.transaction_id.to_string(),
            "transaction_confirmed",
            "tracker",
            &json!({
                "signature":      change.signature,
                "transaction_id": change.transaction_id,
                "wallet_pubkey":  change.wallet_pubkey,
                "slot":           slot,
            }),
            AuditSeverity::Info,
        ).await;

        self.event_bus.publish(GatewayEvent::TransactionConfirmed(
            TransactionLifecycleEvent {
                header:         EventHeader::new(Some(session_id.clone())),
                session_id:     session_id.clone(),
                transaction_id: change.transaction_id,
                wallet_pubkey:  change.wallet_pubkey.clone(),
                status:         TransactionStatus::Confirmed,
                signature:      Some(change.signature.clone()),
            },
        ));

        info!(
            signature = %change.signature,
            slot,
            "transaction confirmed"
        );
    }

    async fn emit_finalized(&self, change: &StateChange, session_id: &SessionId, slot: u64) {
        if let Err(e) = self.tracking_repo.mark_event_emitted(
            &change.signature, "finalized_emitted",
        ).await {
            warn!(error = %e, "failed to mark finalized_emitted");
            return;
        }

        let _ = self.audit.append(
            Some(&change.session_id),
            &change.transaction_id.to_string(),
            "transaction_finalized",
            "tracker",
            &json!({
                "signature":      change.signature,
                "transaction_id": change.transaction_id,
                "wallet_pubkey":  change.wallet_pubkey,
                "slot":           slot,
            }),
            AuditSeverity::Info,
        ).await;

        self.event_bus.publish(GatewayEvent::TransactionFinalized(
            TransactionLifecycleEvent {
                header:         EventHeader::new(Some(session_id.clone())),
                session_id:     session_id.clone(),
                transaction_id: change.transaction_id,
                wallet_pubkey:  change.wallet_pubkey.clone(),
                status:         TransactionStatus::Finalized,
                signature:      Some(change.signature.clone()),
            },
        ));

        info!(
            signature = %change.signature,
            slot,
            "transaction finalized"
        );
    }

    async fn emit_execution_failed(&self, change: &StateChange, session_id: &SessionId, err: &str) {
        if let Err(e) = self.tracking_repo.mark_event_emitted(
            &change.signature, "failed_emitted",
        ).await {
            warn!(error = %e, "failed to mark failed_emitted");
            return;
        }

        let _ = self.audit.append(
            Some(&change.session_id),
            &change.transaction_id.to_string(),
            "transaction_execution_failed",
            "tracker",
            &json!({
                "signature":      change.signature,
                "transaction_id": change.transaction_id,
                "wallet_pubkey":  change.wallet_pubkey,
                "error":          err,
            }),
            AuditSeverity::Warning,
        ).await;

        self.event_bus.publish(GatewayEvent::TransactionExecutionFailed(
            TransactionFailedEvent {
                header:         EventHeader::new(Some(session_id.clone())),
                session_id:     session_id.clone(),
                transaction_id: change.transaction_id,
                wallet_pubkey:  change.wallet_pubkey.clone(),
                error:          err.to_string(),
                at_stage:       TransactionStatus::Sent,
            },
        ));

        warn!(
            signature = %change.signature,
            error = err,
            "transaction execution failed on-chain"
        );
    }

    async fn emit_dropped(&self, change: &StateChange, session_id: &SessionId) {
        if let Err(e) = self.tracking_repo.mark_event_emitted(
            &change.signature, "dropped_emitted",
        ).await {
            warn!(error = %e, "failed to mark dropped_emitted");
            return;
        }

        let _ = self.audit.append(
            Some(&change.session_id),
            &change.transaction_id.to_string(),
            "transaction_dropped",
            "tracker",
            &json!({
                "signature":      change.signature,
                "transaction_id": change.transaction_id,
                "wallet_pubkey":  change.wallet_pubkey,
                "block_height":   change.block_height,
            }),
            AuditSeverity::Warning,
        ).await;

        self.event_bus.publish(GatewayEvent::TransactionDropped(
            TransactionLifecycleEvent {
                header:         EventHeader::new(Some(session_id.clone())),
                session_id:     session_id.clone(),
                transaction_id: change.transaction_id,
                wallet_pubkey:  change.wallet_pubkey.clone(),
                status:         TransactionStatus::Failed,
                signature:      Some(change.signature.clone()),
            },
        ));

        warn!(
            signature = %change.signature,
            "transaction dropped (not observed on-chain after grace period)"
        );
    }

    async fn emit_expired(&self, change: &StateChange, session_id: &SessionId) {
        if let Err(e) = self.tracking_repo.mark_event_emitted(
            &change.signature, "expired_emitted",
        ).await {
            warn!(error = %e, "failed to mark expired_emitted");
            return;
        }

        let _ = self.audit.append(
            Some(&change.session_id),
            &change.transaction_id.to_string(),
            "transaction_expired",
            "tracker",
            &json!({
                "signature":      change.signature,
                "transaction_id": change.transaction_id,
                "wallet_pubkey":  change.wallet_pubkey,
                "block_height":   change.block_height,
            }),
            AuditSeverity::Warning,
        ).await;

        self.event_bus.publish(GatewayEvent::TransactionExpired(
            TransactionLifecycleEvent {
                header:         EventHeader::new(Some(session_id.clone())),
                session_id:     session_id.clone(),
                transaction_id: change.transaction_id,
                wallet_pubkey:  change.wallet_pubkey.clone(),
                status:         TransactionStatus::Expired,
                signature:      Some(change.signature.clone()),
            },
        ));

        warn!(
            signature = %change.signature,
            "transaction expired (blockhash invalid)"
        );
    }
}

/// Converts a `TxState` to the string representation for the DB.
fn tx_state_to_str(state: &TxState) -> &'static str {
    match state {
        TxState::Submitted => "submitted",
        TxState::Dropped => "dropped",
        TxState::Confirmed { .. } => "confirmed",
        TxState::Finalized { .. } => "finalized",
        TxState::Failed { .. } => "failed",
        TxState::Expired => "expired",
    }
}

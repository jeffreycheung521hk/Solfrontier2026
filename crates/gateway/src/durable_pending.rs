//! Write-through durability for pending workflow state (P1-1).
//!
//! This module provides `DurablePendingState`, a thin wrapper around
//! `PendingStateRepository` that the daemon wires into caller sites.
//! It handles serialization/deserialization of domain types to/from
//! the SQLite persistence layer.
//!
//! # Mutation ordering
//!
//! **CREATE path**: The durable DB write must succeed BEFORE the in-memory
//! store mutation. If the DB write fails, the caller must not acknowledge
//! the request as live. This prevents pending requests that exist in memory
//! but would be lost on crash.
//!
//! **CONSUME path**: The DB row must be marked terminal BEFORE in-memory
//! removal. If the process crashes after marking terminal but before
//! in-memory removal, restart will see the terminal row and skip it.
//! If the process crashes after in-memory removal but before marking
//! terminal, the row is still terminal (already marked).
//!
//! # Fail-closed
//!
//! `persist_*` methods return `Result`. If they fail, the caller must
//! treat the operation as failed and not proceed to create live state.
//! `mark_*_terminal` methods are best-effort: if they fail, the row
//! remains in 'pending' state and will be recovered on restart — which
//! is safe because the in-memory state was already consumed. The worst
//! case is a duplicate recovery, which is handled by exactly-once guards
//! in the in-memory stores.

use std::time::Duration;

use tracing::{info, warn};
use uuid::Uuid;

use claw_state_store::pending_state::PendingStateRepository;
use claw_types::{
    approval::ApprovalRequest,
    policy::PolicyVerdict,
    transaction::{SimulationResult, TransactionProposal},
    session::SessionId,
};

use crate::{
    approval_store::ApprovalStore,
    completion_metadata::{CompletionMeta, CompletionMetadataStore},
    orchestrator::SignatureOrchestrator,
    orchestrator::types::{SignatureRequest, SignatureRequestId},
    pending_signing::PendingSigningStore,
};

/// Write-through durability layer for pending workflow state.
///
/// Clone-friendly (Arc-wrapped repository inside PendingStateRepository).
#[derive(Clone)]
pub struct DurablePendingState {
    repo: PendingStateRepository,
}

impl DurablePendingState {
    /// Create a new durable state wrapper.
    pub fn new(repo: PendingStateRepository) -> Self {
        Self { repo }
    }

    /// Access the underlying repository (for tests).
    pub fn repo(&self) -> &PendingStateRepository {
        &self.repo
    }

    // ── Pending approval persistence ─────────────────────────────────────

    /// Persist a pending approval + signing store entry.
    ///
    /// **Must be called BEFORE in-memory store mutations.** If this returns
    /// `Err`, the caller must not create the in-memory live state.
    pub async fn persist_approval(
        &self,
        request_id:     Uuid,
        approval:       &ApprovalRequest,
        proposal:       &TransactionProposal,
        tx_bytes:       &[u8],
        simulation:     &SimulationResult,
        policy_verdict: &PolicyVerdict,
    ) -> Result<(), String> {
        let approval_json = serde_json::to_string(approval)
            .map_err(|e| format!("serialize approval: {e}"))?;
        let simulation_json = serde_json::to_string(simulation)
            .map_err(|e| format!("serialize simulation: {e}"))?;
        let verdict_json = serde_json::to_string(policy_verdict)
            .map_err(|e| format!("serialize verdict: {e}"))?;
        let tx_bytes_hex = hex::encode(tx_bytes);

        self.repo.insert_pending_approval(
            &request_id.to_string(),
            &proposal.session_id.to_string(),
            &proposal.id.to_string(),
            &proposal.description,
            &proposal.wallet_pubkey,
            &approval_json,
            &tx_bytes_hex,
            &simulation_json,
            &verdict_json,
        ).await.map_err(|e| format!("DB write: {e}"))
    }

    /// Mark a pending approval as terminal (consumed/rejected/expired).
    ///
    /// **Must be called BEFORE in-memory removal.** Best-effort: if this
    /// fails, the row stays 'pending' and may be recovered on restart,
    /// which is safe (the approval will be re-presented to the operator).
    pub async fn mark_approval_terminal(&self, request_id: Uuid, state: &str, reason: Option<&str>) {
        if let Err(e) = self.repo.mark_approval_terminal(
            &request_id.to_string(), state, reason,
        ).await {
            warn!(request_id = %request_id, error = %e, "failed to mark approval terminal");
        }
    }

    // ── Pending signature persistence ────────────────────────────────────

    /// Persist a pending orchestrator signature entry.
    ///
    /// **Must be called BEFORE the request is acknowledged as live.**
    pub async fn persist_signature(
        &self,
        request_id:         &SignatureRequestId,
        session_id:         &SessionId,
        transaction_id:     Uuid,
        expected_signer:    &str,
        description:        &str,
        finalized_tx_bytes: &[u8],
        adapter_type:       &str,
    ) -> Result<(), String> {
        self.repo.insert_pending_signature(
            &request_id.0.to_string(),
            &session_id.to_string(),
            &transaction_id.to_string(),
            expected_signer,
            description,
            finalized_tx_bytes,
            adapter_type,
        ).await.map_err(|e| format!("DB write: {e}"))
    }

    /// Mark a pending signature as terminal (consumed/expired/rejected).
    ///
    /// **Must be called BEFORE in-memory removal.** Best-effort.
    pub async fn mark_signature_terminal(&self, request_id: &SignatureRequestId, state: &str, reason: Option<&str>) {
        if let Err(e) = self.repo.mark_signature_terminal(
            &request_id.0.to_string(), state, reason,
        ).await {
            warn!(request_id = %request_id, error = %e, "failed to mark signature terminal");
        }
    }

    // ── Atomic signature + completion meta persistence ─────────────────

    /// Atomically persist both a pending signature and its completion
    /// metadata in a single SQLite transaction.
    ///
    /// **Must succeed BEFORE the request is acknowledged as live (before
    /// returning `request_id` to the caller or emitting events).**
    ///
    /// If this fails, no durable row is created — the caller must roll
    /// back the in-memory state via `orchestrator.abort_pending()`.
    pub async fn persist_signature_and_meta(
        &self,
        request_id:         &SignatureRequestId,
        session_id:         &SessionId,
        transaction_id:     Uuid,
        expected_signer:    &str,
        description:        &str,
        finalized_tx_bytes: &[u8],
        adapter_type:       &str,
        fee_lamports:       Option<u64>,
    ) -> Result<(), String> {
        self.repo.insert_signature_and_meta(
            &request_id.0.to_string(),
            &session_id.to_string(),
            &transaction_id.to_string(),
            expected_signer,
            description,
            finalized_tx_bytes,
            adapter_type,
            fee_lamports,
        ).await.map_err(|e| format!("DB transaction: {e}"))
    }

    // ── Completion metadata persistence ──────────────────────────────────

    /// Persist completion metadata.
    pub async fn persist_completion_meta(&self, request_id: Uuid, fee_lamports: Option<u64>) -> Result<(), String> {
        self.repo.insert_completion_meta(
            &request_id.to_string(),
            fee_lamports,
        ).await.map_err(|e| format!("DB write: {e}"))
    }

    /// Mark completion metadata as terminal. Best-effort.
    pub async fn mark_completion_meta_terminal(&self, request_id: Uuid, state: &str) {
        if let Err(e) = self.repo.mark_completion_meta_terminal(
            &request_id.to_string(), state,
        ).await {
            warn!(request_id = %request_id, error = %e, "failed to mark completion meta terminal");
        }
    }

    // ── Startup recovery ─────────────────────────────────────────────────

    /// Recover pending state from SQLite into in-memory stores.
    ///
    /// Called once at daemon startup, before accepting requests.
    ///
    /// 1. Mark stale 'pending' rows as 'expired' (older than `max_age`).
    /// 2. Load remaining 'pending' rows and reconstruct in-memory stores.
    /// 3. Corrupt rows are marked 'rejected' and skipped.
    ///
    /// Only rows with `state = 'pending'` are loaded. Terminal rows
    /// (consumed, expired, rejected) are never revived.
    pub async fn recover(
        &self,
        max_age: Duration,
        approval_store:    &ApprovalStore,
        pending_signing:   &PendingSigningStore,
        orchestrator:      &SignatureOrchestrator,
        completion_meta:   &CompletionMetadataStore,
    ) -> RecoveryReport {
        let mut report = RecoveryReport::default();

        // Step 1: Mark stale pending records as expired.
        let max_age_ms = max_age.as_millis() as i64;
        match self.repo.expire_older_than(max_age_ms).await {
            Ok(counts) => {
                report.expired_approvals    = counts.approvals;
                report.expired_signatures   = counts.signatures;
                report.expired_meta         = counts.meta;
                if counts.approvals > 0 || counts.signatures > 0 || counts.meta > 0 {
                    info!(
                        expired_approvals = counts.approvals,
                        expired_signatures = counts.signatures,
                        expired_meta = counts.meta,
                        "marked stale pending records as expired on startup"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to expire stale records — proceeding with recovery");
            }
        }

        // Step 2: Recover pending approvals.
        match self.repo.load_pending_approvals().await {
            Ok(rows) => {
                for row in rows {
                    match self.recover_approval_row(&row, approval_store, pending_signing) {
                        Ok(()) => report.recovered_approvals += 1,
                        Err(reason) => {
                            warn!(
                                request_id = %row.request_id,
                                reason = %reason,
                                "failed to recover pending approval — marking as rejected"
                            );
                            let _ = self.repo.mark_approval_terminal(
                                &row.request_id, "rejected", Some(&reason),
                            ).await;
                            report.corrupt_approvals += 1;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to load pending approvals for recovery");
            }
        }

        // Step 3: Recover pending signatures.
        match self.repo.load_pending_signatures().await {
            Ok(rows) => {
                for row in rows {
                    match self.recover_signature_row(&row, orchestrator) {
                        Ok(()) => report.recovered_signatures += 1,
                        Err(reason) => {
                            warn!(
                                request_id = %row.request_id,
                                reason = %reason,
                                "failed to recover pending signature — marking as rejected"
                            );
                            let _ = self.repo.mark_signature_terminal(
                                &row.request_id, "rejected", Some(&reason),
                            ).await;
                            report.corrupt_signatures += 1;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to load pending signatures for recovery");
            }
        }

        // Step 4: Recover completion metadata.
        match self.repo.load_completion_meta().await {
            Ok(rows) => {
                for row in rows {
                    match Uuid::parse_str(&row.request_id) {
                        Ok(id) => {
                            completion_meta.insert(id, CompletionMeta {
                                fee_lamports: row.fee_lamports.map(|f| f as u64),
                            });
                            report.recovered_meta += 1;
                        }
                        Err(e) => {
                            warn!(
                                request_id = %row.request_id,
                                error = %e,
                                "corrupt completion meta UUID — marking as rejected"
                            );
                            let _ = self.repo.mark_completion_meta_terminal(
                                &row.request_id, "rejected",
                            ).await;
                            report.corrupt_meta += 1;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to load completion metadata for recovery");
            }
        }

        report
    }

    fn recover_approval_row(
        &self,
        row: &claw_state_store::pending_state::PendingApprovalRow,
        approval_store: &ApprovalStore,
        pending_signing: &PendingSigningStore,
    ) -> Result<(), String> {
        let approval: ApprovalRequest = serde_json::from_str(&row.approval_json)
            .map_err(|e| format!("approval_json: {e}"))?;
        let simulation: SimulationResult = serde_json::from_str(&row.simulation_json)
            .map_err(|e| format!("simulation_json: {e}"))?;
        let verdict: PolicyVerdict = serde_json::from_str(&row.verdict_json)
            .map_err(|e| format!("verdict_json: {e}"))?;
        let tx_bytes = hex::decode(&row.tx_bytes_hex)
            .map_err(|e| format!("tx_bytes_hex: {e}"))?;

        let proposal = TransactionProposal {
            id:                   approval.transaction_id,
            session_id:           approval.session_id.clone(),
            wallet_pubkey:        row.wallet_pubkey.clone(),
            network:              claw_types::solana::SolanaNetwork::Devnet,
            description:          approval.description.clone(),
            transaction_b64:      String::new(),
            instructions_summary: vec![],
            created_at:           approval.requested_at,
        };

        approval_store.register(approval);

        let tx: solana_sdk::transaction::Transaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| format!("tx deserialize: {e}"))?;

        let request_id = Uuid::parse_str(&row.request_id)
            .map_err(|e| format!("request_id parse: {e}"))?;

        let _decision_rx = pending_signing.park(
            request_id, proposal, &tx, simulation, verdict,
        ).map_err(|e| format!("re-park: {e}"))?;

        Ok(())
    }

    fn recover_signature_row(
        &self,
        row: &claw_state_store::pending_state::PendingSignatureRow,
        orchestrator: &SignatureOrchestrator,
    ) -> Result<(), String> {
        let request_id = Uuid::parse_str(&row.request_id)
            .map_err(|e| format!("request_id: {e}"))?;
        let session_id_uuid = Uuid::parse_str(&row.session_id)
            .map_err(|e| format!("session_id: {e}"))?;
        let transaction_id = Uuid::parse_str(&row.transaction_id)
            .map_err(|e| format!("transaction_id: {e}"))?;
        let expected_signer = row.expected_signer.parse::<solana_sdk::pubkey::Pubkey>()
            .map_err(|e| format!("expected_signer: {e}"))?;

        let request = SignatureRequest {
            id: SignatureRequestId::from(request_id),
            session_id: SessionId::from(session_id_uuid),
            transaction_id,
            finalized_tx_bytes: row.finalized_tx_bytes.clone(),
            expected_signer,
            description: row.description.clone(),
        };

        orchestrator.recover_pending(request, row.adapter_type.clone())
            .map_err(|e| format!("recover_pending: {e}"))
    }
}

/// Summary of what happened during startup recovery.
#[derive(Debug, Default, Clone)]
pub struct RecoveryReport {
    /// Number of stale approval rows marked expired.
    pub expired_approvals:    u64,
    /// Number of stale signature rows marked expired.
    pub expired_signatures:   u64,
    /// Number of stale completion meta rows marked expired.
    pub expired_meta:         u64,
    /// Number of approval rows successfully recovered.
    pub recovered_approvals:  u64,
    /// Number of signature rows successfully recovered.
    pub recovered_signatures: u64,
    /// Number of completion meta rows successfully recovered.
    pub recovered_meta:       u64,
    /// Number of corrupt approval rows marked rejected.
    pub corrupt_approvals:    u64,
    /// Number of corrupt signature rows marked rejected.
    pub corrupt_signatures:   u64,
    /// Number of corrupt completion meta rows marked rejected.
    pub corrupt_meta:         u64,
}

impl RecoveryReport {
    /// Whether any records were recovered.
    pub fn any_recovered(&self) -> bool {
        self.recovered_approvals > 0 || self.recovered_signatures > 0 || self.recovered_meta > 0
    }

    /// Whether any corrupt records were found.
    pub fn any_corrupt(&self) -> bool {
        self.corrupt_approvals > 0 || self.corrupt_signatures > 0 || self.corrupt_meta > 0
    }
}

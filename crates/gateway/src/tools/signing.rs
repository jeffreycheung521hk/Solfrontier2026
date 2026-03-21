//! Gateway-private `submit_for_signing` tool.
//!
//! This tool is NOT in `claw-tool-system` because it requires access to
//! gateway-level state: `TransactionReviewPipeline`, `PendingSigningStore`,
//! `ApprovalStore`, and the `EventBus`. These would create circular crate
//! dependencies if placed in the tool-system crate.
//!
//! Instead this tool is constructed in `daemon.rs` and injected into the
//! `ToolRegistry` via `ToolRegistry::with_tool()` at startup.
//!
//! # What this tool does
//!
//! 1. Decodes a base64-encoded unsigned `Transaction` from the agent.
//! 2. Runs simulate → policy (Control Plane stages).
//! 3. If execution-ready (auto-approved), builds a `SignatureRequest` and
//!    submits it through `SignatureOrchestrator` (Execution Plane boundary).
//! 4. On `AwaitingApproval`:
//!    - Parks the transaction in `PendingSigningStore` (Control Plane)
//!    - Registers an `ApprovalRequest` in `ApprovalStore`
//!    - Emits `ApprovalRequested` event to the event bus
//!    - Returns `{ status: "awaiting_approval", approval_request_id }` to the agent
//! 5. On `PolicyBlocked`: returns the rejection reason.
//! 6. On `SimulationFailed`: returns the simulation error.
//!
//! # Sign-path resume after operator approval
//!
//! When the operator POSTs to `POST /sessions/:id/approve`:
//! - `GatewayApprovalHandler::decide_inner()` calls `pending_signing.signal(request_id, approved)`
//! - This unblocks the background task that is `await`ing the oneshot receiver
//!   returned by `PendingSigningStore::park()`
//! - That task reads the finalized artifact from the control-plane continuation store,
//!   builds a `SignatureRequest`, and submits through `SignatureOrchestrator`
//! - The signature is persisted to the audit log and emitted as `TransactionSigned` event
//!
//! # Durability
//!
//! The `PendingSigningStore` is in-memory. Parked approvals are lost on daemon restart.
//! On restart the operator must re-submit the original agent command.
//! This is a documented V1 limitation.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use solana_sdk::transaction::Transaction;
use tracing::{info, warn};
use uuid::Uuid;

use claw_risk_engine::{PolicyEvaluationContext, PolicySet};
use claw_state_store::audit::{AuditRepository, AuditSeverity};
use claw_state_store::spend::SpendRepository;
use claw_tool_system::{errors::ToolError, tool::Tool};
use claw_types::{
    approval::ApprovalRequest,
    events::{ApprovalLifecycleEvent, EventHeader, GatewayEvent, TransactionFailedEvent, TransactionLifecycleEvent, WalletSignatureEvent},
    policy::PolicyVerdict,
    tool::{ToolInput, ToolOutput, ToolSpec},
    transaction::{SimulationResult, TransactionProposal, TransactionStatus},
};
use claw_wallet_engine::pipeline::TransactionReviewPipeline;

use crate::{
    approval_store::ApprovalStore,
    completion_metadata::{CompletionMeta, CompletionMetadataStore},
    durable_pending::DurablePendingState,
    event_bus::EventBus,
    external_wallet::ExternalWalletStore,
    orchestrator::SignatureOrchestrator,
    orchestrator::types::{SignatureOutcome, SignatureRequest, SignatureRequestId},
    pending_signing::PendingSigningStore,
};

/// Gateway-private tool that runs the full transaction review pipeline.
///
/// Injected into the tool registry by `GatewayDaemon::run()`.
pub struct SubmitForSigningTool {
    pipeline:         TransactionReviewPipeline,
    orchestrator:     SignatureOrchestrator,
    pending_signing:  PendingSigningStore,
    approval_store:   ApprovalStore,
    event_bus:        EventBus,
    audit:            AuditRepository,
    spend:            SpendRepository,
    policy_set:       Arc<PolicySet>,
    network:          claw_types::solana::SolanaNetwork,
    external_wallet:  ExternalWalletStore,
    completion_meta:  CompletionMetadataStore,
    durable:          Option<DurablePendingState>,
}

impl SubmitForSigningTool {
    pub fn new(
        pipeline:         TransactionReviewPipeline,
        orchestrator:     SignatureOrchestrator,
        pending_signing:  PendingSigningStore,
        approval_store:   ApprovalStore,
        event_bus:        EventBus,
        audit:            AuditRepository,
        spend:            SpendRepository,
        policy_set:       Arc<PolicySet>,
        network:          claw_types::solana::SolanaNetwork,
        external_wallet:  ExternalWalletStore,
        completion_meta:  CompletionMetadataStore,
    ) -> Self {
        Self {
            pipeline,
            orchestrator,
            pending_signing,
            approval_store,
            event_bus,
            audit,
            spend,
            policy_set,
            network,
            external_wallet,
            completion_meta,
            durable: None,
        }
    }

    /// Enable durable persistence for pending state (P1-1).
    pub fn with_durable(mut self, durable: DurablePendingState) -> Self {
        self.durable = Some(durable);
        self
    }
}

#[async_trait]
impl Tool for SubmitForSigningTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "submit_for_signing".to_string(),
            description: "Submits a built transaction through the full review pipeline: \
                simulate → policy check → sign (if approved). \
                If the policy requires human approval, returns an approval_request_id. \
                The operator must then approve via POST /sessions/:id/approve. \
                Returns the on-chain signature if auto-approved, or awaiting_approval status."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["transaction_b64", "description", "wallet_pubkey"],
                "properties": {
                    "transaction_b64": {
                        "type": "string",
                        "description": "Base64-encoded unsigned Solana transaction (from build_transfer or similar)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Human-readable description of what this transaction does (shown in approval request)"
                    },
                    "wallet_pubkey": {
                        "type": "string",
                        "description": "The public key of the wallet that will sign this transaction"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["signed", "awaiting_approval", "awaiting_wallet_signature", "policy_blocked", "simulation_failed"]
                    },
                    "signature":                    { "type": ["string", "null"] },
                    "approval_request_id":          { "type": ["string", "null"] },
                    "wallet_signature_request_id":  { "type": ["string", "null"] },
                    "unsigned_tx_b64":              { "type": ["string", "null"] },
                    "policy_verdict":               { "type": ["string", "null"] },
                    "error":                        { "type": ["string", "null"] }
                }
            }),
            required_capabilities: vec!["propose_signing".to_string()],
            supports_streaming: false,
            timeout_ms: 60_000, // 60s — may await human approval
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let tx_b64 = input.parameters["transaction_b64"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "transaction_b64 required".to_string(),
            })?;
        let description = input.parameters["description"]
            .as_str()
            .unwrap_or("agent-submitted transaction")
            .to_string();
        let wallet_pubkey = input.parameters["wallet_pubkey"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(tx_b64)
            .map_err(|e| ToolError::InvalidInput {
                reason: format!("invalid base64: {e}"),
            })?;

        let tx: Transaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| ToolError::InvalidInput {
                reason: format!("invalid transaction bytes: {e}"),
            })?;

        let proposal = TransactionProposal {
            id:                   Uuid::new_v4(),
            session_id:           input.session_id.clone(),
            wallet_pubkey:        wallet_pubkey.clone(),
            network:              self.network,
            description:          description.clone(),
            transaction_b64:      tx_b64.to_string(),
            instructions_summary: vec![],
            created_at:           chrono::Utc::now(),
        };

        // Query accumulated spend for policy evaluation.
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let wallet_daily_spend = self.spend
            .wallet_daily_spend(&wallet_pubkey, &today)
            .await
            .unwrap_or(0);
        let session_spend = self.spend
            .session_spend(&input.session_id.to_string())
            .await
            .unwrap_or(0);

        let session_str = input.session_id.to_string();

        // ── Control Plane: simulate + policy ────────────────────────────────────
        let policy_set = self.policy_set.clone();
        let network = self.network;
        let proposal_for_policy = proposal.clone();
        let session_id_for_policy = input.session_id.clone();

        // Stage 1: Simulate
        let simulated = self.pipeline.simulate(&proposal, tx).await
            .map_err(|e| match e {
                claw_wallet_engine::errors::WalletError::SimulationFailed { error } => {
                    warn!(session = %session_str, error = %error, "simulation failed");
                    ToolError::ExecutionFailed(format!("simulation failed: {error}"))
                }
                other => ToolError::ExecutionFailed(format!("simulation error: {other}")),
            })?;

        // Stage 2: Policy evaluation
        let approved = match self.pipeline.evaluate_policy(
            &proposal,
            simulated,
            |sim| {
                let ctx = PolicyEvaluationContext {
                    proposal:                    &proposal_for_policy,
                    simulation_result:           Some(sim),
                    network,
                    session_id:                  &session_id_for_policy,
                    session_spend_lamports:       session_spend,
                    wallet_daily_spend_lamports:  wallet_daily_spend,
                };
                policy_set.evaluate(&ctx)
            },
        ) {
            Ok(a) => a,
            Err(claw_wallet_engine::errors::WalletError::PolicyBlocked { verdict }) => {
                warn!(
                    session = %session_str,
                    verdict = %verdict.label(),
                    "transaction blocked by policy"
                );

                let _ = self.audit.append(
                    Some(&session_str),
                    &proposal.id.to_string(),
                    "transaction_policy_blocked",
                    "pipeline",
                    &json!({
                        "proposal_id":    proposal.id,
                        "policy_verdict": verdict.label(),
                    }),
                    AuditSeverity::Warning,
                ).await;

                return Ok(ToolOutput {
                    tool_name:   "submit_for_signing".to_string(),
                    success:     false,
                    data:        Some(json!({
                        "status":         "policy_blocked",
                        "policy_verdict": verdict.label(),
                    })),
                    error:       Some(format!("policy blocked: {}", verdict.label())),
                    duration_ms: 0,
                });
            }
            Err(other) => {
                return Err(ToolError::ExecutionFailed(format!("policy error: {other}")));
            }
        };

        let simulation = approved.inner.simulation.clone();
        let policy_verdict = approved.policy_verdict.clone();
        let finalized_tx = approved.inner.inner;

        // ── Routing decision: awaiting approval vs execution-ready ──────────────

        if policy_verdict.requires_human() {
            let is_external = self.external_wallet.is_external_wallet(&input.session_id, &wallet_pubkey);

            // Park in PendingSigningStore (Control Plane) for human approval.
            let request_id = Uuid::new_v4();

            let approval_request = ApprovalRequest {
                id:              request_id,
                session_id:      input.session_id.clone(),
                transaction_id:  proposal.id,
                description:     description.clone(),
                policy_verdict:  policy_verdict.clone(),
                simulation:      simulation.clone(),
                requested_at:    chrono::Utc::now(),
                decided:         false,
            };

            // P1-1 hardening: DB write BEFORE in-memory mutation.
            // If the DB write fails, do not create live pending state.
            if let Some(ref durable) = self.durable {
                let tx_bytes = bincode::serialize(&finalized_tx)
                    .map_err(|e| ToolError::ExecutionFailed(format!("serialize tx: {e}")))?;
                durable.persist_approval(
                    request_id, &approval_request, &proposal,
                    &tx_bytes, &simulation, &policy_verdict,
                ).await.map_err(|e| ToolError::ExecutionFailed(
                    format!("failed to persist pending approval: {e}")
                ))?;
            }

            let decision_rx = self.pending_signing.park(
                request_id,
                proposal.clone(),
                &finalized_tx,
                simulation.clone(),
                policy_verdict.clone(),
            ).map_err(|e| ToolError::ExecutionFailed(e))?;

            self.approval_store.register(approval_request.clone());

            let audit_action = if is_external { "approval_requested_external_wallet" } else { "approval_requested" };
            let _ = self.audit.append(
                Some(&session_str),
                &request_id.to_string(),
                audit_action,
                "pipeline",
                &json!({
                    "request_id":     request_id,
                    "proposal_id":    proposal.id,
                    "description":    description,
                    "policy_verdict": policy_verdict.label(),
                    "wallet_pubkey":  wallet_pubkey,
                    "signer_type":    if is_external { "external" } else { "local" },
                }),
                AuditSeverity::Warning,
            ).await;

            self.event_bus.publish(GatewayEvent::ApprovalRequested(
                ApprovalLifecycleEvent {
                    header:         EventHeader::new(Some(input.session_id.clone())),
                    session_id:     input.session_id.clone(),
                    request_id,
                    transaction_id: proposal.id,
                    approved:       None,
                },
            ));

            info!(
                session = %session_str,
                request_id = %request_id,
                "transaction parked awaiting operator approval"
            );

            // Spawn the appropriate resume background task.
            {
                let orchestrator    = self.orchestrator.clone();
                let pending_signing = self.pending_signing.clone();
                let audit           = self.audit.clone();
                let spend           = self.spend.clone();
                let event_bus       = self.event_bus.clone();
                let session_id      = input.session_id.clone();
                let proposal2       = proposal.clone();
                let wallet_pubkey2  = wallet_pubkey.clone();
                let completion_meta = self.completion_meta.clone();
                let durable         = self.durable.clone();

                tokio::spawn(async move {
                    resume_after_approval(
                        decision_rx,
                        request_id,
                        proposal2,
                        orchestrator,
                        pending_signing,
                        audit,
                        spend,
                        event_bus,
                        session_id,
                        wallet_pubkey2,
                        completion_meta,
                        durable,
                    ).await;
                });
            }

            let mut data = json!({
                "status":              "awaiting_approval",
                "approval_request_id": request_id.to_string(),
                "message":             "Transaction is awaiting operator approval. \
                                        Use POST /sessions/:id/approve with this request_id."
            });
            if is_external {
                data["signer_type"] = json!("external");
            }

            return Ok(ToolOutput {
                tool_name:   "submit_for_signing".to_string(),
                success:     true,
                data:        Some(data),
                error:       None,
                duration_ms: 0,
            });
        }

        // ── Execution-ready: route through SignatureOrchestrator ─────────────────

        let finalized_tx_bytes = bincode::serialize(&finalized_tx)
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to serialize tx: {e}")))?;

        let expected_signer = wallet_pubkey.parse::<solana_sdk::pubkey::Pubkey>()
            .map_err(|e| ToolError::ExecutionFailed(format!("invalid wallet pubkey: {e}")))?;

        let sig_request_id = SignatureRequestId::new();
        let sig_request = SignatureRequest {
            id: sig_request_id,
            session_id: input.session_id.clone(),
            transaction_id: proposal.id,
            finalized_tx_bytes: finalized_tx_bytes.clone(),
            expected_signer,
            description: description.clone(),
        };

        let outcome = self.orchestrator.submit(sig_request).await
            .map_err(|e| ToolError::ExecutionFailed(format!("orchestrator: {e}")))?;

        match outcome {
            SignatureOutcome::Signed { signature, request_id: _, signed_tx_bytes: _, adapter_type: _, transaction_id: _, expected_signer: _ } => {
                info!(
                    session = %session_str,
                    proposal_id = %proposal.id,
                    signature = %signature,
                    "transaction signed via orchestrator"
                );

                // Record spend at Signed stage.
                if let Some(fee) = simulation.fee_lamports {
                    let _ = self.spend.record_spend(
                        &wallet_pubkey,
                        &session_str,
                        &proposal.id.to_string(),
                        fee,
                    ).await;
                }

                let _ = self.audit.append(
                    Some(&session_str),
                    &proposal.id.to_string(),
                    "transaction_signed",
                    "pipeline",
                    &json!({
                        "proposal_id":    proposal.id,
                        "wallet_pubkey":  wallet_pubkey,
                        "signature":      signature,
                        "description":    description,
                        "policy_verdict": policy_verdict.label(),
                        "compute_units":  simulation.compute_units_used,
                    }),
                    AuditSeverity::Info,
                ).await;

                self.event_bus.publish(GatewayEvent::TransactionSigned(
                    TransactionLifecycleEvent {
                        header:         EventHeader::new(Some(input.session_id.clone())),
                        session_id:     input.session_id.clone(),
                        transaction_id: proposal.id,
                        wallet_pubkey:  wallet_pubkey.clone(),
                        status:         TransactionStatus::Signed,
                        signature:      Some(signature.clone()),
                    },
                ));

                Ok(ToolOutput {
                    tool_name:   "submit_for_signing".to_string(),
                    success:     true,
                    data:        Some(json!({
                        "status":    "signed",
                        "signature": signature,
                    })),
                    error:       None,
                    duration_ms: 0,
                })
            }

            SignatureOutcome::Pending { request_id, adapter_type, finalized_tx_bytes } => {
                // External wallet path — orchestrator returned Pending.
                // In-memory PendingEntry exists in orchestrator. Before exposing
                // any request_id externally, we MUST have durable backing.

                // Durable-first: atomically persist signature + completion meta.
                // If this fails, roll back the in-memory state and fail closed.
                if let Some(ref durable) = self.durable {
                    if let Err(e) = durable.persist_signature_and_meta(
                        &request_id, &input.session_id, proposal.id,
                        &wallet_pubkey, &description, &finalized_tx_bytes, &adapter_type,
                        simulation.fee_lamports,
                    ).await {
                        warn!(
                            session = %session_str,
                            request_id = %request_id,
                            error = %e,
                            "durable persist failed — rolling back pending signature"
                        );

                        // Rollback: remove in-memory entry. Not a natural expiry.
                        let _ = self.orchestrator.abort_pending(&request_id);

                        let _ = self.audit.append(
                            Some(&session_str),
                            &request_id.0.to_string(),
                            "persistence_failed_rollback",
                            "pipeline",
                            &json!({
                                "request_id":    request_id.0,
                                "proposal_id":   proposal.id,
                                "wallet_pubkey": wallet_pubkey,
                                "error":         e,
                            }),
                            AuditSeverity::Error,
                        ).await;

                        return Err(ToolError::ExecutionFailed(
                            format!("failed to establish durable pending state: {e}")
                        ));
                    }
                }

                // Durable backing confirmed — safe to create in-memory metadata
                // and expose the request_id externally.
                self.completion_meta.insert(request_id.0, CompletionMeta {
                    fee_lamports: simulation.fee_lamports,
                });

                let unsigned_tx_b64 = base64::engine::general_purpose::STANDARD.encode(&finalized_tx_bytes);

                let _ = self.audit.append(
                    Some(&session_str),
                    &request_id.0.to_string(),
                    "wallet_signature_requested",
                    "pipeline",
                    &json!({
                        "request_id":     request_id.0,
                        "proposal_id":    proposal.id,
                        "description":    description,
                        "wallet_pubkey":  wallet_pubkey,
                        "policy_verdict": policy_verdict.label(),
                        "signer_type":    "external",
                    }),
                    AuditSeverity::Info,
                ).await;

                self.event_bus.publish(GatewayEvent::WalletSignatureRequested(
                    WalletSignatureEvent {
                        header:         EventHeader::new(Some(input.session_id.clone())),
                        session_id:     input.session_id.clone(),
                        request_id:     request_id.0,
                        transaction_id: proposal.id,
                        wallet_pubkey:  wallet_pubkey.clone(),
                        error:          None,
                    },
                ));

                info!(
                    session = %session_str,
                    request_id = %request_id,
                    wallet = %wallet_pubkey,
                    "transaction pending external wallet signature via orchestrator"
                );

                Ok(ToolOutput {
                    tool_name:   "submit_for_signing".to_string(),
                    success:     true,
                    data:        Some(json!({
                        "status":                       "awaiting_wallet_signature",
                        "wallet_signature_request_id":  request_id.0.to_string(),
                        "unsigned_tx_b64":              unsigned_tx_b64,
                        "wallet_pubkey":                wallet_pubkey,
                        "message":                      "Transaction is awaiting signature from the external wallet. \
                                                         Sign the unsigned_tx_b64 and submit via \
                                                         POST /sessions/:id/wallet-signatures."
                    })),
                    error:       None,
                    duration_ms: 0,
                })
            }
        }
    }
}

// ── Unified background resume task ────────────────────────────────────────────
//
// Spawned when a transaction is parked awaiting operator approval (both local
// and external wallet paths). Awaits the operator's oneshot decision, then:
// - On approval:  reads finalized artifact from PendingSigningStore,
//                 builds a SignatureRequest, submits through SignatureOrchestrator
// - On rejection: logs + emits rejection event; removes parked entry
// - On timeout/disconnect: logs and removes parked entry

pub async fn resume_after_approval(
    decision_rx:     tokio::sync::oneshot::Receiver<bool>,
    request_id:      Uuid,
    proposal:        TransactionProposal,
    orchestrator:    SignatureOrchestrator,
    pending_signing: PendingSigningStore,
    audit:           AuditRepository,
    spend:           SpendRepository,
    event_bus:       EventBus,
    session_id:      claw_types::session::SessionId,
    wallet_pubkey:   String,
    completion_meta: CompletionMetadataStore,
    durable:         Option<DurablePendingState>,
) {
    let session_str = session_id.to_string();

    // Wait for the operator's decision.
    let approved = match decision_rx.await {
        Ok(decision) => decision,
        Err(_) => {
            // Sender dropped (e.g., daemon shutting down).
            warn!(
                session = %session_str,
                request_id = %request_id,
                "approval oneshot dropped — cleaning up parked transaction"
            );
            // P1-1 hardening: mark terminal BEFORE in-memory removal.
            if let Some(ref durable) = durable {
                durable.mark_approval_terminal(request_id, "consumed", Some("sender dropped")).await;
            }
            pending_signing.remove(&request_id);
            return;
        }
    };

    if !approved {
        info!(
            session = %session_str,
            request_id = %request_id,
            "operator rejected transaction — terminal"
        );
        // P1-1 hardening: mark terminal BEFORE in-memory removal.
        if let Some(ref durable) = durable {
            durable.mark_approval_terminal(request_id, "rejected", Some("operator rejected")).await;
        }
        pending_signing.remove(&request_id);

        event_bus.publish(GatewayEvent::TransactionFailed(
            TransactionFailedEvent {
                header:         EventHeader::new(Some(session_id.clone())),
                session_id:     session_id.clone(),
                transaction_id: proposal.id,
                wallet_pubkey:  wallet_pubkey.clone(),
                error:          "operator rejected".to_string(),
                at_stage:       TransactionStatus::AwaitingApproval,
            },
        ));
        return;
    }

    // P1-1 hardening: mark DB row terminal BEFORE in-memory consumption.
    // This prevents stale resurrection if the process crashes between
    // the DB mark and the in-memory take.
    if let Some(ref durable) = durable {
        durable.mark_approval_terminal(request_id, "consumed", Some("operator approved")).await;
    }

    // Operator approved — take the parked entry.
    let parked = match pending_signing.take(&request_id) {
        Some(p) => p,
        None => {
            warn!(
                session = %session_str,
                request_id = %request_id,
                "parked approval entry gone — may have been cleaned up already"
            );
            return;
        }
    };

    let simulation = parked.simulation.clone();

    // Parse the expected signer pubkey.
    let expected_signer = match wallet_pubkey.parse::<solana_sdk::pubkey::Pubkey>() {
        Ok(pk) => pk,
        Err(e) => {
            warn!(
                session = %session_str,
                request_id = %request_id,
                error = %e,
                "invalid wallet pubkey in parked entry"
            );
            return;
        }
    };

    // Build a SignatureRequest from the finalized artifact and submit
    // through the orchestrator (Execution Plane boundary).
    let sig_request_id = SignatureRequestId::new();
    let sig_request = SignatureRequest {
        id: sig_request_id,
        session_id: session_id.clone(),
        transaction_id: parked.proposal.id,
        finalized_tx_bytes: parked.tx_bytes.clone(),
        expected_signer,
        description: parked.proposal.description.clone(),
    };

    let outcome = orchestrator.submit(sig_request).await;

    match outcome {
        Ok(SignatureOutcome::Signed { signature, request_id: _, signed_tx_bytes: _, adapter_type: _, transaction_id: _, expected_signer: _ }) => {
            info!(
                session = %session_str,
                request_id = %request_id,
                signature = %signature,
                "transaction signed after operator approval via orchestrator"
            );

            // Record spend at Signed stage (resume path).
            if let Some(fee) = simulation.fee_lamports {
                let _ = spend.record_spend(
                    &parked.proposal.wallet_pubkey,
                    &session_str,
                    &parked.proposal.id.to_string(),
                    fee,
                ).await;
            }

            let _ = audit.append(
                Some(&session_str),
                &request_id.to_string(),
                "transaction_signed_after_approval",
                "pipeline",
                &json!({
                    "request_id":     request_id,
                    "proposal_id":    parked.proposal.id,
                    "signature":      signature,
                    "policy_verdict": parked.policy_verdict.label(),
                    "compute_units":  simulation.compute_units_used,
                }),
                AuditSeverity::Info,
            ).await;

            event_bus.publish(GatewayEvent::TransactionSigned(
                TransactionLifecycleEvent {
                    header:         EventHeader::new(Some(session_id.clone())),
                    session_id:     session_id.clone(),
                    transaction_id: parked.proposal.id,
                    wallet_pubkey:  parked.proposal.wallet_pubkey.clone(),
                    status:         TransactionStatus::Signed,
                    signature:      Some(signature),
                },
            ));
        }

        Ok(SignatureOutcome::Pending { request_id: pending_req_id, adapter_type, finalized_tx_bytes }) => {
            // External wallet path after approval — orchestrator returned Pending.
            // Durable-first: must persist before emitting any operator-visible events.
            let wallet_sig_request_id = pending_req_id.0;

            if let Some(ref durable) = durable {
                if let Err(e) = durable.persist_signature_and_meta(
                    &pending_req_id, &session_id, proposal.id,
                    &wallet_pubkey, &proposal.description, &finalized_tx_bytes, &adapter_type,
                    simulation.fee_lamports,
                ).await {
                    // Fail closed: rollback in-memory pending, emit explicit failure.
                    warn!(
                        session = %session_str,
                        request_id = %wallet_sig_request_id,
                        error = %e,
                        "durable persist failed after approval — rolling back pending signature"
                    );

                    let _ = orchestrator.abort_pending(&pending_req_id);

                    let _ = audit.append(
                        Some(&session_str),
                        &wallet_sig_request_id.to_string(),
                        "wallet_signature_persistence_failed",
                        "pipeline",
                        &json!({
                            "request_id":          wallet_sig_request_id,
                            "approval_request_id": request_id,
                            "proposal_id":         proposal.id,
                            "wallet_pubkey":       wallet_pubkey,
                            "error":               e,
                        }),
                        AuditSeverity::Error,
                    ).await;

                    event_bus.publish(GatewayEvent::TransactionFailed(
                        TransactionFailedEvent {
                            header:         EventHeader::new(Some(session_id.clone())),
                            session_id:     session_id.clone(),
                            transaction_id: proposal.id,
                            wallet_pubkey:  wallet_pubkey.clone(),
                            error:          format!("durable persist failed after approval: {e}"),
                            at_stage:       TransactionStatus::Signed,
                        },
                    ));
                    return;
                }
            }

            // Durable backing confirmed — safe to populate in-memory metadata
            // and emit operator-visible events.
            completion_meta.insert(wallet_sig_request_id, CompletionMeta {
                fee_lamports: simulation.fee_lamports,
            });

            let _ = audit.append(
                Some(&session_str),
                &wallet_sig_request_id.to_string(),
                "wallet_signature_requested_after_approval",
                "pipeline",
                &json!({
                    "request_id":          wallet_sig_request_id,
                    "approval_request_id": request_id,
                    "proposal_id":         proposal.id,
                    "wallet_pubkey":       wallet_pubkey,
                }),
                AuditSeverity::Info,
            ).await;

            event_bus.publish(GatewayEvent::WalletSignatureRequested(
                WalletSignatureEvent {
                    header:         EventHeader::new(Some(session_id.clone())),
                    session_id:     session_id.clone(),
                    request_id:     wallet_sig_request_id,
                    transaction_id: proposal.id,
                    wallet_pubkey:  wallet_pubkey.clone(),
                    error:          None,
                },
            ));

            info!(
                session = %session_str,
                request_id = %wallet_sig_request_id,
                wallet = %wallet_pubkey,
                "external wallet: transaction approved, now awaiting wallet signature via orchestrator"
            );
        }

        Err(e) => {
            warn!(
                session = %session_str,
                request_id = %request_id,
                error = %e,
                "signing failed after operator approval"
            );

            let _ = audit.append(
                Some(&session_str),
                &request_id.to_string(),
                "transaction_sign_failed",
                "pipeline",
                &json!({ "error": e.to_string(), "request_id": request_id }),
                AuditSeverity::Error,
            ).await;

            event_bus.publish(GatewayEvent::TransactionFailed(
                TransactionFailedEvent {
                    header:         EventHeader::new(Some(session_id.clone())),
                    session_id:     session_id.clone(),
                    transaction_id: parked.proposal.id,
                    wallet_pubkey:  parked.proposal.wallet_pubkey.clone(),
                    error:          e.to_string(),
                    at_stage:       TransactionStatus::Signed,
                },
            ));
        }
    }
}

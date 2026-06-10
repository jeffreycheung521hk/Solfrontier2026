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
use solana_sdk::{
    compute_budget,
    pubkey::Pubkey,
    system_instruction::SystemInstruction,
    system_program,
    transaction::Transaction,
};
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
    transaction::{AccountRole, InstructionSummary, TransactionProposal, TransactionStatus},
};
use claw_wallet_engine::pipeline::TransactionReviewPipeline;

use claw_observability::metrics::MetricsRegistry;
use crate::{
    approval_store::ApprovalStore,
    completion_metadata::{CompletionMeta, CompletionMetadataStore},
    durable_pending::DurablePendingState,
    event_bus::EventBus,
    external_wallet::ExternalWalletStore,
    orchestrator::SignatureOrchestrator,
    orchestrator::types::{SignatureOutcome, SignatureRequest, SignatureRequestId},
    pending_signing::PendingSigningStore,
    session_policy::SessionPolicyStore,
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
    metrics:          Arc<MetricsRegistry>,
    /// Per-session policy overrides (shared with GatewaySessionAdapter).
    session_policy:   SessionPolicyStore,
    /// Per-wallet policy rules (from config, read-only).
    wallet_policy:    crate::wallet_policy::WalletPolicyStore,
    /// Approval lease duration in seconds.
    approval_lease_seconds: u64,
    /// Policy alert dispatcher.
    alerts:           crate::policy_alerting::AlertDispatcher,
    /// RPC client for submitting signed transactions to the network.
    rpc_client:       Option<claw_solana_core::rpc::ClawRpcClient>,
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
        metrics:          Arc<MetricsRegistry>,
        session_policy:   SessionPolicyStore,
        wallet_policy:    crate::wallet_policy::WalletPolicyStore,
        approval_lease_seconds: u64,
        alerts:           crate::policy_alerting::AlertDispatcher,
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
            metrics,
            session_policy,
            wallet_policy,
            approval_lease_seconds,
            alerts,
            rpc_client: None,
        }
    }

    /// Enable durable persistence for pending state (P1-1).
    pub fn with_durable(mut self, durable: DurablePendingState) -> Self {
        self.durable = Some(durable);
        self
    }

    /// Enable auto-submission of signed transactions to the network.
    pub fn with_rpc_client(mut self, rpc: claw_solana_core::rpc::ClawRpcClient) -> Self {
        self.rpc_client = Some(rpc);
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
                    "policy_rule_name":             { "type": ["string", "null"] },
                    "policy_reason":                { "type": ["string", "null"] },
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
            instructions_summary: summarize_transaction_instructions(&tx),
            created_at:           chrono::Utc::now(),
        };

        self.metrics.transactions_proposed.increment();

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

        // Build the layered effective policy:
        //   wallet rules → session rules → global rules → defaults
        let mut effective_policy = match self.session_policy.get(&input.session_id) {
            Some(session_rules) => {
                info!(
                    session = %session_str,
                    session_rules = session_rules.len(),
                    "using session-scoped policy overrides"
                );
                policy_set.with_session_rules(&session_rules)
            }
            None => (*policy_set).clone(),
        };

        // Prepend wallet-specific rules (highest priority after wallet itself).
        if let Some(wallet_rules) = self.wallet_policy.get(&wallet_pubkey) {
            effective_policy = effective_policy.with_session_rules(wallet_rules);
        }

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
        // Capture evaluation metadata from the closure for audit/observability.
        let eval_meta: std::cell::Cell<Option<(usize, usize, Option<usize>)>> =
            std::cell::Cell::new(None);

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
                let result = effective_policy.evaluate(&ctx);
                eval_meta.set(Some((
                    result.rules_total,
                    result.rules_evaluated,
                    result.matched_rule_index,
                )));
                result.verdict
            },
        ) {
            Ok(a) => a,
            Err(claw_wallet_engine::errors::WalletError::PolicyBlocked { verdict }) => {
                self.metrics.policy_rejections.increment();
                if let Some(rule) = verdict.rule_name() {
                    self.metrics.policy_rule_hits.increment(rule);
                }
                warn!(
                    session = %session_str,
                    verdict = %verdict.label(),
                    rule = ?verdict.rule_name(),
                    "transaction blocked by policy"
                );

                let _ = self.audit.append(
                    Some(&session_str),
                    &proposal.id.to_string(),
                    "policy_evaluation",
                    "pipeline",
                    &policy_evaluation_audit(&proposal, &verdict, eval_meta.get()),
                    AuditSeverity::Warning,
                ).await;

                self.alerts.send(
                    "policy_rejected",
                    claw_types::alert::AlertSeverity::Warning,
                    &policy_blocked_error(&verdict),
                    policy_evaluation_audit(&proposal, &verdict, eval_meta.get()),
                );

                return Ok(ToolOutput {
                    tool_name:   "submit_for_signing".to_string(),
                    success:     false,
                    data:        Some(with_policy_fields(json!({
                        "status":         "policy_blocked",
                        "policy_verdict": verdict.label(),
                    }), &verdict)),
                    error:       Some(policy_blocked_error(&verdict)),
                    duration_ms: 0,
                });
            }
            Err(other) => {
                return Err(ToolError::ExecutionFailed(format!("policy error: {other}")));
            }
        };

        let simulation = approved.inner().simulation().clone();
        let policy_verdict = approved.policy_verdict().clone();
        let last_valid_block_height = approved.inner().last_valid_block_height();
        let finalized_tx = approved.inner().inner().clone();

        // ── Policy observability: audit + per-rule counter ─────────────────────
        if let Some(rule) = policy_verdict.rule_name() {
            self.metrics.policy_rule_hits.increment(rule);
        }
        let audit_severity = if policy_verdict.requires_human() {
            AuditSeverity::Warning
        } else {
            AuditSeverity::Info
        };
        let _ = self.audit.append(
            Some(&session_str),
            &proposal.id.to_string(),
            "policy_evaluation",
            "pipeline",
            &policy_evaluation_audit(&proposal, &policy_verdict, eval_meta.get()),
            audit_severity,
        ).await;

        // ── Routing decision: awaiting approval vs execution-ready ──────────────

        if policy_verdict.requires_human() {
            let is_external = self.external_wallet.is_external_wallet(&input.session_id, &wallet_pubkey);

            // Park in PendingSigningStore (Control Plane) for human approval.
            let request_id = Uuid::new_v4();

            // Extract approval chain or single role from the verdict.
            let (required_approver_role, approval_chain) = match &policy_verdict {
                PolicyVerdict::RequiresHumanApproval {
                    required_approver_role,
                    approval_chain,
                    ..
                } => (required_approver_role.clone(), approval_chain.clone()),
                _ => (None, None),
            };

            let approval_request = ApprovalRequest {
                id:              request_id,
                session_id:      input.session_id.clone(),
                transaction_id:  proposal.id,
                description:     description.clone(),
                policy_verdict:  policy_verdict.clone(),
                simulation:      simulation.clone(),
                requested_at:    chrono::Utc::now(),
                decided:         false,
                required_approver_role: required_approver_role.clone(),
            };

            // Build the approval workflow (single-stage or multi-stage).
            let workflow = if let Some(ref chain) = approval_chain {
                use claw_types::approval::ApprovalStage;
                let stages = chain
                    .iter()
                    .enumerate()
                    .map(|(i, cs)| ApprovalStage {
                        index: i,
                        allowed_roles: vec![cs.role.clone()],
                        min_approvals: cs.min_approvals,
                        decisions: vec![],
                    })
                    .collect();
                let now = chrono::Utc::now();
                claw_types::approval::ApprovalWorkflow {
                    request_id,
                    session_id: input.session_id.clone(),
                    state: claw_types::approval::ApprovalWorkflowState::Pending,
                    stages,
                    created_at: now,
                    updated_at: now,
                    expires_at: None,
                }.with_lease_seconds(self.approval_lease_seconds)
            } else {
                claw_types::approval::ApprovalWorkflow::single_stage(
                    request_id,
                    input.session_id.clone(),
                    required_approver_role,
                ).with_lease_seconds(self.approval_lease_seconds)
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
                last_valid_block_height,
            ).map_err(|e| ToolError::ExecutionFailed(e))?;

            self.approval_store.register(approval_request.clone(), workflow);

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

                let lease_seconds = self.approval_lease_seconds;
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
                        lease_seconds,
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
            data = with_policy_fields(data, &policy_verdict);

            return Ok(ToolOutput {
                tool_name:   "submit_for_signing".to_string(),
                success:     true,
                data:        Some(data),
                error:       None,
                duration_ms: 0,
            });
        }

        // ── Execution-ready: route through SignatureOrchestrator ─────────────────

        // Internal storage: bincode format (Rust-native).
        // Conversion to wire format happens at the API boundary (pending_for_session).
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
            SignatureOutcome::Signed { signature, request_id: _, signed_tx_bytes, adapter_type: _, transaction_id: _, expected_signer: _ } => {
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

                // Auto-submit to RPC if rpc_client is configured.
                let mut tx_signature = None;
                let mut submitted = false;
                if let Some(ref rpc) = self.rpc_client {
                    match rpc.send_raw_transaction(&signed_tx_bytes).await {
                        Ok(sig) => {
                            info!(
                                session = %session_str,
                                proposal_id = %proposal.id,
                                tx_signature = %sig,
                                "transaction submitted to network"
                            );
                            self.metrics.transactions_sent.increment();
                            self.metrics.rpc_calls_total.increment();
                            tx_signature = Some(sig);
                            submitted = true;

                            self.event_bus.publish(GatewayEvent::TransactionSent(
                                TransactionLifecycleEvent {
                                    header:         EventHeader::new(Some(input.session_id.clone())),
                                    session_id:     input.session_id.clone(),
                                    transaction_id: proposal.id,
                                    wallet_pubkey:  wallet_pubkey.clone(),
                                    status:         TransactionStatus::Sent,
                                    signature:      Some(signature.clone()),
                                },
                            ));
                        }
                        Err(e) => {
                            warn!(
                                session = %session_str,
                                proposal_id = %proposal.id,
                                error = %e,
                                "failed to submit signed transaction to network"
                            );
                            self.metrics.rpc_calls_failed.increment();
                        }
                    }
                }

                Ok(ToolOutput {
                    tool_name:   "submit_for_signing".to_string(),
                    success:     true,
                    data:        Some(with_policy_fields(json!({
                        "status":       "signed",
                        "signature":    signature,
                        "submitted":    submitted,
                        "tx_signature": tx_signature,
                    }), &policy_verdict)),
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
                    last_valid_block_height: Some(last_valid_block_height),
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
                    data:        Some(with_policy_fields(json!({
                        "status":                       "awaiting_wallet_signature",
                        "wallet_signature_request_id":  request_id.0.to_string(),
                        "unsigned_tx_b64":              unsigned_tx_b64,
                        "wallet_pubkey":                wallet_pubkey,
                        "message":                      "Transaction is awaiting signature from the external wallet. \
                                                         Sign the unsigned_tx_b64 and submit via \
                                                         POST /sessions/:id/wallet-signatures."
                    }), &policy_verdict)),
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
    decision_rx:     tokio::sync::oneshot::Receiver<claw_types::approval::ApprovalWorkflowState>,
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
    lease_seconds:   u64,
) {
    use claw_types::approval::ApprovalWorkflowState;

    let session_str = session_id.to_string();

    // Wait for the workflow to reach a terminal state, bounded by the lease.
    // This ensures the parked signing task is actively reaped when no operator
    // action arrives within the lease window — otherwise the oneshot would
    // hang forever and approval_lease_seconds would be dead config.
    let lease_duration = if lease_seconds > 0 {
        std::time::Duration::from_secs(lease_seconds)
    } else {
        // 0 = no explicit lease; use a generous 24-hour fallback so the task
        // still eventually wakes if the operator never responds.
        std::time::Duration::from_secs(86_400)
    };

    let workflow_state = match tokio::time::timeout(lease_duration, decision_rx).await {
        Ok(Ok(state)) => state,
        Ok(Err(_)) => {
            // Sender dropped (daemon shutting down).
            warn!(
                session = %session_str,
                request_id = %request_id,
                "approval channel dropped — treating as expired"
            );
            if let Some(ref durable) = durable {
                durable.mark_approval_terminal(request_id, "expired", Some("channel dropped")).await;
            }
            pending_signing.remove(&request_id);
            let _ = audit.append(
                Some(&session_str),
                &request_id.to_string(),
                "approval_channel_dropped",
                "system",
                &serde_json::json!({ "request_id": request_id }),
                AuditSeverity::Warning,
            ).await;
            return;
        }
        Err(_) => {
            // Lease timeout: no operator action within the lease window.
            // This is the active expiry enforcement path — without it,
            // approval_lease_seconds would only be checked on operator-initiated
            // decide() and would never fire if the operator simply walked away.
            warn!(
                session = %session_str,
                request_id = %request_id,
                lease_seconds = lease_seconds,
                "approval lease expired without operator action — auto-expiring"
            );
            if let Some(ref durable) = durable {
                durable.mark_approval_terminal(request_id, "expired", Some("lease timeout")).await;
            }
            pending_signing.remove(&request_id);
            let _ = audit.append(
                Some(&session_str),
                &request_id.to_string(),
                "approval_lease_expired_no_action",
                "system",
                &serde_json::json!({
                    "request_id":    request_id,
                    "lease_seconds": lease_seconds,
                    "reason":        "no operator decision within lease window",
                }),
                AuditSeverity::Warning,
            ).await;
            event_bus.publish(GatewayEvent::TransactionFailed(
                TransactionFailedEvent {
                    header:         EventHeader::new(Some(session_id.clone())),
                    session_id:     session_id.clone(),
                    transaction_id: proposal.id,
                    wallet_pubkey:  wallet_pubkey.clone(),
                    error:          "approval lease expired".to_string(),
                    at_stage:       TransactionStatus::AwaitingApproval,
                },
            ));
            return;
        }
    };

    if !workflow_state.is_approved() {
        info!(
            session = %session_str,
            request_id = %request_id,
            state = workflow_state.label(),
            "workflow reached non-approved terminal state"
        );
        if let Some(ref durable) = durable {
            durable.mark_approval_terminal(request_id, workflow_state.label(), None).await;
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
                last_valid_block_height: if parked.last_valid_block_height > 0 {
                    Some(parked.last_valid_block_height)
                } else {
                    None // Recovery path: captured at submission via get_block_height()
                },
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

// ── Helper: decode instructions from an unsigned transaction ─────────────────

/// Best-effort decode of the instructions in an unsigned `Transaction`.
/// Returns a summary for each instruction, including transfer amount
/// when the instruction is a System Program transfer.
fn summarize_transaction_instructions(tx: &Transaction) -> Vec<InstructionSummary> {
    tx.message
        .instructions
        .iter()
        .filter_map(|ix| {
            let program_id_index = ix.program_id_index as usize;
            let program_id = tx
                .message
                .account_keys
                .get(program_id_index)?
                .to_string();

            let decoded = decode_instruction(&program_id, &ix.data, &tx.message.account_keys, &ix.accounts);

            let accounts = ix
                .accounts
                .iter()
                .filter_map(|&idx| {
                    let pubkey = tx.message.account_keys.get(idx as usize)?.to_string();
                    let is_signer = (idx as usize) < tx.message.header.num_required_signatures as usize;
                    let is_writable = tx.message.is_maybe_writable(idx as usize, None);
                    Some(AccountRole {
                        pubkey,
                        label: None,
                        is_signer,
                        is_writable,
                    })
                })
                .collect();

            Some(InstructionSummary {
                program_id,
                program_name:    decoded.program_name,
                description:     decoded.description,
                transfer_lamports: decoded.transfer_lamports,
                token_transfer:  decoded.token_transfer,
                is_legacy_token_transfer: decoded.is_legacy_token_transfer,
                accounts,
            })
        })
        .collect()
}

/// Result of decoding an instruction.
struct DecodedInstruction {
    program_name: Option<String>,
    description: String,
    transfer_lamports: Option<u64>,
    token_transfer: Option<claw_types::transaction::TokenTransfer>,
    is_legacy_token_transfer: bool,
}

/// SPL Token program ID.
const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// SPL Token-2022 program ID.
const SPL_TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Decode a single instruction.
fn decode_instruction(
    program_id: &str,
    data: &[u8],
    account_keys: &[Pubkey],
    account_indices: &[u8],
) -> DecodedInstruction {
    let system_id = system_program::id().to_string();
    let compute_budget_id = compute_budget::id().to_string();

    if program_id == system_id {
        if let Ok(sys_ix) = bincode::deserialize::<SystemInstruction>(data) {
            match sys_ix {
                SystemInstruction::Transfer { lamports } => {
                    let to = account_indices
                        .get(1)
                        .and_then(|&i| account_keys.get(i as usize))
                        .map(|pk| pk.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    return DecodedInstruction {
                        program_name: Some("System Program".to_string()),
                        description: format!("Transfer {} lamports to {}", lamports, to),
                        transfer_lamports: Some(lamports),
                        token_transfer: None,
                        is_legacy_token_transfer: false,
                    };
                }
                SystemInstruction::CreateAccount { lamports, space, owner } => {
                    return DecodedInstruction {
                        program_name: Some("System Program".to_string()),
                        description: format!(
                            "CreateAccount: {} lamports, {} bytes, owner {}",
                            lamports, space, owner
                        ),
                        transfer_lamports: Some(lamports),
                        token_transfer: None,
                        is_legacy_token_transfer: false,
                    };
                }
                _ => {
                    return DecodedInstruction {
                        program_name: Some("System Program".to_string()),
                        description: format!("{:?}", sys_ix),
                        transfer_lamports: None,
                        token_transfer: None,
                        is_legacy_token_transfer: false,
                    };
                }
            }
        }
    }

    if program_id == compute_budget_id {
        return DecodedInstruction {
            program_name: Some("Compute Budget".to_string()),
            description: "ComputeBudget instruction".to_string(),
            transfer_lamports: None,
            token_transfer: None,
            is_legacy_token_transfer: false,
        };
    }

    // ── SPL Token / Token-2022 decoding ─────────────────────────────────────
    if program_id == SPL_TOKEN_PROGRAM_ID || program_id == SPL_TOKEN_2022_PROGRAM_ID {
        if let Some(decoded) = decode_spl_token_instruction(data, account_keys, account_indices) {
            return decoded;
        }
        // Fallback for unrecognized SPL Token instructions.
        return DecodedInstruction {
            program_name: Some("SPL Token".to_string()),
            description: "SPL Token instruction".to_string(),
            transfer_lamports: None,
            token_transfer: None,
            is_legacy_token_transfer: false,
        };
    }

    // Unknown program — return a generic summary.
    DecodedInstruction {
        program_name: None,
        description: format!("Instruction for program {}", &program_id[..8.min(program_id.len())]),
        transfer_lamports: None,
        token_transfer: None,
        is_legacy_token_transfer: false,
    }
}

/// Decode an SPL Token instruction.
///
/// V1 supports `TransferChecked` (instruction tag 12), which carries the mint
/// directly. Legacy `Transfer` (tag 3) does NOT include the mint in its
/// accounts and would require an RPC lookup to resolve — not done in V1.
///
/// TransferChecked layout:
/// - data[0] = 12 (instruction tag)
/// - data[1..9] = amount (u64 little-endian)
/// - data[9] = decimals (u8)
///
/// Accounts:
/// - [0] source token account
/// - [1] mint
/// - [2] destination token account
/// - [3] authority (signer)
fn decode_spl_token_instruction(
    data: &[u8],
    account_keys: &[Pubkey],
    account_indices: &[u8],
) -> Option<DecodedInstruction> {
    if data.is_empty() {
        return None;
    }

    let tag = data[0];
    match tag {
        // TransferChecked
        12 => {
            if data.len() < 10 || account_indices.len() < 4 {
                return None;
            }
            let amount = u64::from_le_bytes(data[1..9].try_into().ok()?);
            let decimals = data[9];

            let source = account_keys.get(account_indices[0] as usize)?.to_string();
            let mint = account_keys.get(account_indices[1] as usize)?.to_string();
            let destination = account_keys.get(account_indices[2] as usize)?.to_string();

            Some(DecodedInstruction {
                program_name: Some("SPL Token".to_string()),
                description: format!(
                    "TransferChecked: {} (raw units, {} decimals) of mint {} → {}",
                    amount, decimals, mint, destination
                ),
                transfer_lamports: None,
                token_transfer: Some(claw_types::transaction::TokenTransfer {
                    mint,
                    amount,
                    decimals: Some(decimals),
                    source,
                    destination,
                }),
                is_legacy_token_transfer: false,
            })
        }
        // Transfer (legacy, no mint in accounts)
        3 => {
            if data.len() < 9 || account_indices.len() < 3 {
                return None;
            }
            let amount = u64::from_le_bytes(data[1..9].try_into().ok()?);
            let source = account_keys.get(account_indices[0] as usize)?.to_string();
            let destination = account_keys.get(account_indices[1] as usize)?.to_string();

            // Legacy Transfer: mint is not in accounts, would need RPC lookup.
            // Document this and return without token_transfer (caller can still
            // see this is a token transfer from program_name + description).
            Some(DecodedInstruction {
                program_name: Some("SPL Token".to_string()),
                description: format!(
                    "Transfer (legacy): {} raw units {} → {} (mint not resolved)",
                    amount, source, destination
                ),
                transfer_lamports: None,
                token_transfer: None,
                is_legacy_token_transfer: true,
            })
        }
        _ => None,
    }
}

// ── Helper: attach policy metadata to response JSON ──────────────────────────

/// Merges `policy_rule_name` and `policy_reason` into a response `serde_json::Value`.
fn with_policy_fields(mut data: serde_json::Value, verdict: &PolicyVerdict) -> serde_json::Value {
    if let Some(rule_name) = verdict.rule_name() {
        data["policy_rule_name"] = serde_json::json!(rule_name);
    }
    if let Some(reason) = verdict.reason() {
        data["policy_reason"] = serde_json::json!(reason);
    }
    data
}

/// Builds a human-readable error string for policy-blocked responses.
fn policy_blocked_error(verdict: &PolicyVerdict) -> String {
    match (verdict.rule_name(), verdict.reason()) {
        (Some(rule), Some(reason)) => {
            format!("policy blocked by rule '{}': {}", rule, reason)
        }
        (Some(rule), None) => {
            format!("policy blocked by rule '{}'", rule)
        }
        (None, Some(reason)) => {
            format!("policy blocked: {}", reason)
        }
        (None, None) => {
            format!("policy blocked: {}", verdict.label())
        }
    }
}

// ── Helper: structured audit payload for policy evaluation ───────────────────

/// Builds a JSON audit payload capturing the full policy evaluation context.
///
/// `eval_meta` is `(rules_total, rules_evaluated, matched_rule_index)` captured
/// from `PolicyEvaluationResult` inside the evaluate closure.
fn policy_evaluation_audit(
    proposal: &TransactionProposal,
    verdict: &PolicyVerdict,
    eval_meta: Option<(usize, usize, Option<usize>)>,
) -> serde_json::Value {
    let programs: Vec<&str> = proposal
        .instructions_summary
        .iter()
        .map(|ix| ix.program_id.as_str())
        .collect();

    let destinations: Vec<&str> = proposal
        .instructions_summary
        .iter()
        .flat_map(|ix| ix.accounts.iter())
        .filter(|acc| !acc.is_signer && acc.is_writable)
        .map(|acc| acc.pubkey.as_str())
        .collect();

    let total_transfer_lamports: u64 = proposal
        .instructions_summary
        .iter()
        .filter_map(|ix| ix.transfer_lamports)
        .sum();

    let (rules_total, rules_evaluated, matched_rule_index) = eval_meta.unwrap_or((0, 0, None));

    json!({
        "proposal_id":             proposal.id,
        "verdict":                 verdict.label(),
        "matched_rule":            verdict.rule_name(),
        "reason":                  verdict.reason(),
        "rules_total":             rules_total,
        "rules_evaluated":         rules_evaluated,
        "matched_rule_index":      matched_rule_index,
        "instructions_count":      proposal.instructions_summary.len(),
        "programs":                programs,
        "destinations":            destinations,
        "total_transfer_lamports": total_transfer_lamports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        message::Message,
        system_instruction,
        transaction::Transaction,
    };

    #[test]
    fn summarize_system_transfer() {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let lamports = 42_000_000u64;

        let ix = system_instruction::transfer(&from, &to, lamports);
        let msg = Message::new(&[ix], Some(&from));
        let tx = Transaction::new_unsigned(msg);

        let summaries = summarize_transaction_instructions(&tx);
        assert_eq!(summaries.len(), 1);

        let s = &summaries[0];
        assert_eq!(s.program_id, system_program::id().to_string());
        assert_eq!(s.program_name.as_deref(), Some("System Program"));
        assert_eq!(s.transfer_lamports, Some(lamports));
        assert!(
            s.description.contains(&lamports.to_string()),
            "description should mention lamports: {}",
            s.description
        );
        assert!(
            s.description.contains(&to.to_string()),
            "description should mention destination: {}",
            s.description
        );
    }

    #[test]
    fn summarize_unknown_program_fallback() {
        let from = Pubkey::new_unique();
        let unknown_program = Pubkey::new_unique();

        let ix = solana_sdk::instruction::Instruction::new_with_bytes(
            unknown_program,
            &[0xDE, 0xAD],
            vec![solana_sdk::instruction::AccountMeta::new(from, true)],
        );
        let msg = Message::new(&[ix], Some(&from));
        let tx = Transaction::new_unsigned(msg);

        let summaries = summarize_transaction_instructions(&tx);
        assert_eq!(summaries.len(), 1);

        let s = &summaries[0];
        assert_eq!(s.program_id, unknown_program.to_string());
        assert!(s.program_name.is_none(), "unknown program should have no name");
        assert_eq!(s.transfer_lamports, None);
        assert!(
            s.description.starts_with("Instruction for program"),
            "should have fallback description: {}",
            s.description
        );
    }

    #[test]
    fn summarize_create_account() {
        let from = Pubkey::new_unique();
        let new_account = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let lamports = 1_000_000u64;
        let space = 165u64;

        let ix = system_instruction::create_account(&from, &new_account, lamports, space, &owner);
        let msg = Message::new(&[ix], Some(&from));
        let tx = Transaction::new_unsigned(msg);

        let summaries = summarize_transaction_instructions(&tx);
        assert_eq!(summaries.len(), 1);

        let s = &summaries[0];
        assert_eq!(s.program_name.as_deref(), Some("System Program"));
        assert_eq!(s.transfer_lamports, Some(lamports));
        assert!(
            s.description.contains("CreateAccount"),
            "description should mention CreateAccount: {}",
            s.description
        );
    }

    #[test]
    fn summarize_multiple_instructions() {
        let from = Pubkey::new_unique();
        let to1 = Pubkey::new_unique();
        let to2 = Pubkey::new_unique();

        let ix1 = system_instruction::transfer(&from, &to1, 1_000);
        let ix2 = system_instruction::transfer(&from, &to2, 2_000);
        let msg = Message::new(&[ix1, ix2], Some(&from));
        let tx = Transaction::new_unsigned(msg);

        let summaries = summarize_transaction_instructions(&tx);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].transfer_lamports, Some(1_000));
        assert_eq!(summaries[1].transfer_lamports, Some(2_000));
    }

    #[test]
    fn summarize_compute_budget_instruction() {
        let payer = Pubkey::new_unique();
        let to = Pubkey::new_unique();

        let budget_ix = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(200_000);
        let transfer_ix = system_instruction::transfer(&payer, &to, 5_000);
        let msg = Message::new(&[budget_ix, transfer_ix], Some(&payer));
        let tx = Transaction::new_unsigned(msg);

        let summaries = summarize_transaction_instructions(&tx);
        assert_eq!(summaries.len(), 2);

        assert_eq!(summaries[0].program_name.as_deref(), Some("Compute Budget"));
        assert_eq!(summaries[0].transfer_lamports, None);

        assert_eq!(summaries[1].program_name.as_deref(), Some("System Program"));
        assert_eq!(summaries[1].transfer_lamports, Some(5_000));
    }

    #[test]
    fn summarize_spl_token_transfer_checked() {
        // Build a manual SPL Token TransferChecked instruction:
        // tag=12, amount=42_000_000 (42 USDC raw), decimals=6
        // accounts: [source, mint, destination, authority]
        let source = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let spl_token_program: Pubkey = SPL_TOKEN_PROGRAM_ID.parse().unwrap();

        let mut data = vec![12u8]; // TransferChecked tag
        data.extend_from_slice(&42_000_000u64.to_le_bytes()); // amount
        data.push(6u8); // decimals

        let ix = solana_sdk::instruction::Instruction::new_with_bytes(
            spl_token_program,
            &data,
            vec![
                solana_sdk::instruction::AccountMeta::new(source, false),
                solana_sdk::instruction::AccountMeta::new_readonly(mint, false),
                solana_sdk::instruction::AccountMeta::new(destination, false),
                solana_sdk::instruction::AccountMeta::new_readonly(authority, true),
            ],
        );
        let msg = Message::new(&[ix], Some(&authority));
        let tx = Transaction::new_unsigned(msg);

        let summaries = summarize_transaction_instructions(&tx);
        assert_eq!(summaries.len(), 1);

        let s = &summaries[0];
        assert_eq!(s.program_name.as_deref(), Some("SPL Token"));
        assert!(s.transfer_lamports.is_none());

        let tt = s.token_transfer.as_ref().expect("should decode token transfer");
        assert_eq!(tt.mint, mint.to_string());
        assert_eq!(tt.amount, 42_000_000);
        assert_eq!(tt.decimals, Some(6));
        assert_eq!(tt.source, source.to_string());
        assert_eq!(tt.destination, destination.to_string());
    }

    #[test]
    fn summarize_spl_token_legacy_transfer_returns_no_token_transfer() {
        // Legacy Transfer (tag 3) — no mint in accounts, V1 doesn't resolve
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let spl_token_program: Pubkey = SPL_TOKEN_PROGRAM_ID.parse().unwrap();

        let mut data = vec![3u8]; // Transfer tag
        data.extend_from_slice(&100u64.to_le_bytes());

        let ix = solana_sdk::instruction::Instruction::new_with_bytes(
            spl_token_program,
            &data,
            vec![
                solana_sdk::instruction::AccountMeta::new(source, false),
                solana_sdk::instruction::AccountMeta::new(destination, false),
                solana_sdk::instruction::AccountMeta::new_readonly(authority, true),
            ],
        );
        let msg = Message::new(&[ix], Some(&authority));
        let tx = Transaction::new_unsigned(msg);

        let summaries = summarize_transaction_instructions(&tx);
        assert_eq!(summaries.len(), 1);

        let s = &summaries[0];
        assert_eq!(s.program_name.as_deref(), Some("SPL Token"));
        assert!(s.token_transfer.is_none(), "legacy Transfer doesn't decode mint in V1");
        assert!(s.is_legacy_token_transfer, "legacy Transfer flag must be set for policy to detect bypass attempts");
        assert!(s.description.contains("legacy"));
    }

    #[test]
    fn summarize_transfer_checked_is_not_marked_legacy() {
        // TransferChecked must NOT be marked as legacy, otherwise the
        // LegacyTokenTransferPresent condition would false-positive and
        // block all token transfers.
        let source = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let spl_token_program: Pubkey = SPL_TOKEN_PROGRAM_ID.parse().unwrap();

        let mut data = vec![12u8]; // TransferChecked tag
        data.extend_from_slice(&1_000u64.to_le_bytes());
        data.push(6u8); // decimals

        let ix = solana_sdk::instruction::Instruction::new_with_bytes(
            spl_token_program,
            &data,
            vec![
                solana_sdk::instruction::AccountMeta::new(source, false),
                solana_sdk::instruction::AccountMeta::new_readonly(mint, false),
                solana_sdk::instruction::AccountMeta::new(destination, false),
                solana_sdk::instruction::AccountMeta::new_readonly(authority, true),
            ],
        );
        let msg = Message::new(&[ix], Some(&authority));
        let tx = Transaction::new_unsigned(msg);
        let summaries = summarize_transaction_instructions(&tx);

        assert!(!summaries[0].is_legacy_token_transfer,
            "TransferChecked must NOT be flagged as legacy");
        assert!(summaries[0].token_transfer.is_some(),
            "TransferChecked must produce a token_transfer");
    }

    #[test]
    fn policy_evaluation_audit_captures_full_context() {
        use claw_types::{
            policy::PolicyVerdict,
            session::SessionId,
            solana::SolanaNetwork,
            transaction::{AccountRole, InstructionSummary, TransactionProposal},
        };
        use uuid::Uuid;

        let proposal = TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey: "wallet".to_string(),
            network: SolanaNetwork::Devnet,
            description: "audit test".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![InstructionSummary {
                program_id: "11111111111111111111111111111111".to_string(),
                program_name: Some("System Program".to_string()),
                description: "Transfer 42000000 lamports".to_string(),
                transfer_lamports: Some(42_000_000),
                token_transfer: None,
                is_legacy_token_transfer: false,
                accounts: vec![
                    AccountRole {
                        pubkey: "wallet".to_string(),
                        label: Some("from".to_string()),
                        is_signer: true,
                        is_writable: true,
                    },
                    AccountRole {
                        pubkey: "dest".to_string(),
                        label: Some("to".to_string()),
                        is_signer: false,
                        is_writable: true,
                    },
                ],
            }],
            created_at: chrono::Utc::now(),
        };

        let verdict = PolicyVerdict::Rejected {
            reason: "amount exceeds threshold".to_string(),
            rule_name: "large-transfer-block".to_string(),
        };

        let audit = policy_evaluation_audit(&proposal, &verdict, Some((3, 1, Some(0))));

        assert_eq!(audit["verdict"], "rejected");
        assert_eq!(audit["matched_rule"], "large-transfer-block");
        assert_eq!(audit["reason"], "amount exceeds threshold");
        assert_eq!(audit["total_transfer_lamports"], 42_000_000);
        assert_eq!(audit["instructions_count"], 1);

        let programs = audit["programs"].as_array().unwrap();
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0], "11111111111111111111111111111111");

        let destinations = audit["destinations"].as_array().unwrap();
        assert_eq!(destinations.len(), 1);
        assert_eq!(destinations[0], "dest");

        // A3: evaluation metadata
        assert_eq!(audit["rules_total"], 3);
        assert_eq!(audit["rules_evaluated"], 1);
        assert_eq!(audit["matched_rule_index"], 0);
    }

    #[test]
    fn policy_evaluation_audit_approved_has_no_reason() {
        use claw_types::{
            policy::PolicyVerdict,
            session::SessionId,
            solana::SolanaNetwork,
            transaction::{TransactionProposal},
        };
        use uuid::Uuid;

        let proposal = TransactionProposal {
            id: Uuid::new_v4(),
            session_id: SessionId::from(Uuid::new_v4()),
            wallet_pubkey: "wallet".to_string(),
            network: SolanaNetwork::Devnet,
            description: "audit test".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![],
            created_at: chrono::Utc::now(),
        };

        let verdict = PolicyVerdict::Approved {
            rule_name: "allow-all".to_string(),
        };

        let audit = policy_evaluation_audit(&proposal, &verdict, Some((3, 1, Some(0))));

        assert_eq!(audit["verdict"], "approved");
        assert_eq!(audit["matched_rule"], "allow-all");
        assert!(audit["reason"].is_null());
        assert_eq!(audit["total_transfer_lamports"], 0);

        // A3: evaluation metadata
        assert_eq!(audit["rules_total"], 3);
        assert_eq!(audit["rules_evaluated"], 1);
        assert_eq!(audit["matched_rule_index"], 0);
    }
}

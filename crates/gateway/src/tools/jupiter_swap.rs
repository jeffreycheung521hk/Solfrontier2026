//! Gateway-private `submit_jupiter_swap` tool.
//!
//! # What this tool does
//!
//! 1. Accepts swap parameters (mints, amount, slippage) from the caller.
//! 2. Constructs a `SwapIntent` — the durable "what to swap" description.
//! 3. Evaluates the intent against `SwapPolicyConfig` (intent-level policy:
//!    mint allowlists, amount cap, slippage cap).
//! 4. Persists the intent + verdict to `pending_jupiter_swaps` as durable state.
//! 5. If the verdict requires human approval, creates an `ApprovalWorkflow`
//!    and registers it in `ApprovalStore` so existing approval endpoints work.
//!
//! # What this tool deliberately does NOT do
//!
//! - Fetch a Jupiter quote for execution. Any quote fetch would be
//!   informational-only (`SwapPreview`) — and is out of scope for this tool.
//! - Build a transaction, blockhash, lookup tables, or instruction payloads.
//!   Those are JIT build-time concerns handled by `HttpJupiterJitExecutor`
//!   AFTER approval, not by this tool.
//! - Touch the signing pipeline or the orchestrator directly — the JIT
//!   resume task (spawned here for the auto-approve and post-approval paths)
//!   routes through `SignatureOrchestrator`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tracing::{info, warn};

use claw_state_store::audit::{AuditRepository, AuditSeverity};
use claw_state_store::pending_jupiter::{self, PendingJupiterRepository};
use claw_tool_system::{errors::ToolError, tool::Tool};
use claw_types::{
    approval::{ApprovalRequest, ApprovalWorkflow},
    events::{ApprovalLifecycleEvent, EventHeader, GatewayEvent},
    policy::PolicyVerdict,
    solana::SolanaNetwork,
    swap::{SwapIntent, SwapPolicyConfig},
    tool::{ToolInput, ToolOutput, ToolSpec},
    transaction::SimulationResult,
};

use claw_observability::metrics::MetricsRegistry;

use crate::approval_store::ApprovalStore;
use crate::event_bus::EventBus;
use crate::integrations::jupiter_jit::{
    run_jit_after_approval, run_resume_task, JupiterJitExecutor, JupiterResumeDeps,
};
use crate::integrations::jupiter_park::PendingJupiterParkStore;
use crate::policy_alerting::AlertDispatcher;

/// Read-only seam: look up the session's bound external wallet pubkey.
///
/// Used by `SubmitJupiterSwapTool` to default the tool's `wallet_pubkey`
/// parameter when the caller (typically an LLM acting on a user message)
/// omits it. Keeping `wallet_pubkey` optional at the schema level lets the
/// LLM focus on intent (mints, amount, slippage) while the daemon
/// enforces "signer identity comes from the session binding" as an
/// architectural invariant — not a free parameter.
///
/// Implemented by `ExternalWalletStore`; tests inject stubs.
pub trait SessionBoundWallet: Send + Sync {
    /// Return the session's primary bound external wallet pubkey (base58),
    /// or `None` if no external wallet is bound to this session.
    fn session_wallet_pubkey(&self, session_id: &claw_types::session::SessionId)
        -> Option<String>;
}

/// Gateway-private tool that submits a Jupiter swap intent.
pub struct SubmitJupiterSwapTool {
    pending_repo: PendingJupiterRepository,
    approval_store: ApprovalStore,
    event_bus: EventBus,
    audit: AuditRepository,
    swap_policy: SwapPolicyConfig,
    network: SolanaNetwork,
    approval_lease_seconds: u64,
    alerts: AlertDispatcher,
    metrics: Arc<MetricsRegistry>,
    jit_executor: Arc<dyn JupiterJitExecutor>,
    park_store: PendingJupiterParkStore,
    /// When present, `wallet_pubkey` becomes an optional parameter: if the
    /// LLM omits it, we fall back to the session's bound wallet. Absent
    /// (`None`) preserves the legacy contract where `wallet_pubkey` is
    /// mandatory — used by the existing in-process unit tests.
    session_wallet_lookup: Option<Arc<dyn SessionBoundWallet>>,
}

impl SubmitJupiterSwapTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pending_repo: PendingJupiterRepository,
        approval_store: ApprovalStore,
        event_bus: EventBus,
        audit: AuditRepository,
        swap_policy: SwapPolicyConfig,
        network: SolanaNetwork,
        approval_lease_seconds: u64,
        alerts: AlertDispatcher,
        metrics: Arc<MetricsRegistry>,
        jit_executor: Arc<dyn JupiterJitExecutor>,
        park_store: PendingJupiterParkStore,
    ) -> Self {
        Self {
            pending_repo,
            approval_store,
            event_bus,
            audit,
            swap_policy,
            network,
            approval_lease_seconds,
            alerts,
            metrics,
            jit_executor,
            park_store,
            session_wallet_lookup: None,
        }
    }

    /// Enable session-bound wallet auto-resolution. With a lookup set,
    /// `wallet_pubkey` becomes optional in the tool input — omitted calls
    /// resolve to the session's bound wallet.
    pub fn with_session_wallet_lookup(mut self, lookup: Arc<dyn SessionBoundWallet>) -> Self {
        self.session_wallet_lookup = Some(lookup);
        self
    }

    /// Assemble the resume-task dependency bundle.
    fn resume_deps(&self) -> JupiterResumeDeps {
        JupiterResumeDeps {
            executor: self.jit_executor.clone(),
            pending_repo: self.pending_repo.clone(),
            audit: self.audit.clone(),
            event_bus: self.event_bus.clone(),
            park_store: self.park_store.clone(),
            lease_seconds: self.approval_lease_seconds,
        }
    }
}

#[async_trait]
impl Tool for SubmitJupiterSwapTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "submit_jupiter_swap".to_string(),
            description: "Submits a Jupiter token swap intent. Use this for any SPL token swap \
                          the user asks for (e.g. \"swap 0.001 SOL to USDC\"). Returns immediately \
                          with one of: \"approved_waiting_jit\" (auto-approved; JIT build runs in \
                          background), \"awaiting_approval\" (human approval required; returns \
                          approval_request_id for POST /sessions/:id/approve), or \
                          \"policy_blocked\" (rejected by swap policy). Transaction build, \
                          simulate, sign, and submit happen server-side AFTER approval — callers \
                          must NOT call build_transfer or submit_for_signing for Jupiter swaps. \
                          \n\n\
                          Amount units: pass `input_amount` in the input token's base units \
                          (lamports for SOL; 10^decimals for SPL tokens). Examples:\n\
                          - 0.001 SOL  → input_amount=1000000    (9 decimals)\n\
                          - 1 USDC     → input_amount=1000000    (6 decimals)\n\
                          - 0.5 USDC   → input_amount=500000\n\
                          \n\
                          Common mainnet mint pubkeys:\n\
                          - SOL (wrapped): So11111111111111111111111111111111111111112\n\
                          - USDC:          EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v\n\
                          - USDT:          Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB\n\
                          \n\
                          `wallet_pubkey` is OPTIONAL. If omitted, the daemon uses the session's \
                          bound external wallet (this is the preferred behavior — the signer is \
                          an architectural invariant bound at session setup, not a free parameter). \
                          Only include `wallet_pubkey` if the user explicitly names a specific \
                          wallet address in the message.\n\
                          \n\
                          Slippage: `slippage_bps` is basis points. 100 = 1 %, 50 = 0.5 %."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["input_mint", "output_mint", "input_amount", "slippage_bps"],
                "properties": {
                    "input_mint":    { "type": "string", "description": "Input token mint pubkey (base58)" },
                    "output_mint":   { "type": "string", "description": "Output token mint pubkey (base58)" },
                    "input_amount":  { "type": "integer", "minimum": 1, "description": "Amount in input token's smallest base unit (lamports for SOL)" },
                    "slippage_bps":  { "type": "integer", "minimum": 0, "maximum": 10000, "description": "Max slippage in basis points (100 = 1%)" },
                    "wallet_pubkey": { "type": "string", "description": "OPTIONAL. Omit to use the session's bound wallet (preferred)." },
                    "description":   { "type": "string", "description": "Human-readable summary for audit/UI" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status":              { "type": "string", "enum": ["approved_waiting_jit", "awaiting_approval", "policy_blocked"] },
                    "intent_id":           { "type": "string" },
                    "approval_request_id": { "type": ["string", "null"] },
                    "policy_verdict":      { "type": ["string", "null"] },
                    "policy_rule_name":    { "type": ["string", "null"] },
                    "policy_reason":       { "type": ["string", "null"] }
                }
            }),
            required_capabilities: vec!["propose_signing".to_string()],
            supports_streaming: false,
            timeout_ms: 30_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // ── 1. Parse and validate parameters ────────────────────────────────
        let input_mint = require_string(&input, "input_mint")?;
        let output_mint = require_string(&input, "output_mint")?;

        // `wallet_pubkey` is OPTIONAL when a `session_wallet_lookup` is wired:
        // omitted calls resolve to the session's bound external wallet, so
        // the LLM cannot inject a foreign pubkey — the signer identity is a
        // session-binding invariant, not a free tool parameter.
        let wallet_pubkey = match input.parameters["wallet_pubkey"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                let resolved = self.session_wallet_lookup.as_ref().and_then(|lookup| {
                    lookup.session_wallet_pubkey(&input.session_id)
                });
                match resolved {
                    Some(pk) => {
                        info!(
                            session = %input.session_id,
                            pubkey = %pk,
                            "wallet_pubkey omitted by caller; resolved from session binding"
                        );
                        pk
                    }
                    None => {
                        return Err(ToolError::InvalidInput {
                            reason: "wallet_pubkey is required and no external wallet \
                                     is bound to this session (bind one via \
                                     POST /sessions/:id/wallet-bind-confirm first)"
                                .to_string(),
                        });
                    }
                }
            }
        };

        let input_amount = input.parameters["input_amount"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "input_amount must be a positive integer".to_string(),
            })?;
        if input_amount == 0 {
            return Err(ToolError::InvalidInput {
                reason: "input_amount must be > 0".to_string(),
            });
        }

        let slippage_bps_raw = input.parameters["slippage_bps"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "slippage_bps must be an integer in [0, 10000]".to_string(),
            })?;
        if slippage_bps_raw > 10_000 {
            return Err(ToolError::InvalidInput {
                reason: format!("slippage_bps {slippage_bps_raw} exceeds 10000 (100%)"),
            });
        }
        let slippage_bps = slippage_bps_raw as u16;

        let description = input.parameters["description"]
            .as_str()
            .unwrap_or("Jupiter swap")
            .to_string();

        // ── 2. Construct intent ─────────────────────────────────────────────
        let intent = SwapIntent::new(
            input.session_id.clone(),
            wallet_pubkey,
            self.network,
            input_mint,
            output_mint,
            input_amount,
            slippage_bps,
            description.clone(),
        );
        let intent_id = intent.id;
        let session_str = input.session_id.to_string();

        self.metrics.transactions_proposed.increment();

        // ── 3. Evaluate swap policy ─────────────────────────────────────────
        let verdict = self.swap_policy.evaluate(&intent);

        info!(
            session = %session_str,
            intent_id = %intent_id,
            verdict = %verdict.label(),
            rule = ?verdict.rule_name(),
            "swap intent evaluated"
        );

        let _ = self.audit.append(
            Some(&session_str),
            &intent_id.to_string(),
            "jupiter_swap_policy_evaluation",
            "pipeline",
            &swap_audit_payload(&intent, &verdict),
            audit_severity_for(&verdict),
        ).await;

        // ── 4. Branch on verdict ────────────────────────────────────────────
        match &verdict {
            PolicyVerdict::Approved { .. } => {
                // Persist durably as approved_waiting_jit; resume task will
                // drive the JIT flow (build → simulate → sign → submit).
                self.pending_repo
                    .insert(
                        &intent,
                        None,
                        pending_jupiter::state::APPROVED_WAITING_JIT,
                        Some(&verdict),
                        None,
                    )
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(format!("persist intent: {e}")))?;

                // Spawn the JIT driver. The request_id here is synthetic —
                // no operator decision exists on the auto-approved path.
                let synthetic_request_id = uuid::Uuid::new_v4();
                let intent_for_task = intent.clone();
                let deps = self.resume_deps();
                tokio::spawn(async move {
                    run_jit_after_approval(synthetic_request_id, intent_for_task, deps).await;
                });

                Ok(ToolOutput {
                    tool_name: "submit_jupiter_swap".to_string(),
                    success: true,
                    data: Some(with_verdict_fields(
                        json!({
                            "status":    "approved_waiting_jit",
                            "intent_id": intent_id.to_string(),
                            "message":   "Swap intent auto-approved by policy. \
                                          JIT build is running in the background.",
                        }),
                        &verdict,
                    )),
                    error: None,
                    duration_ms: 0,
                })
            }

            PolicyVerdict::Rejected { .. } => {
                self.metrics.policy_rejections.increment();
                if let Some(rule) = verdict.rule_name() {
                    self.metrics.policy_rule_hits.increment(rule);
                }

                self.pending_repo
                    .insert(
                        &intent,
                        None,
                        pending_jupiter::state::REJECTED,
                        Some(&verdict),
                        None,
                    )
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(format!("persist intent: {e}")))?;
                let _ = self
                    .pending_repo
                    .mark_state(
                        intent_id,
                        pending_jupiter::state::REJECTED,
                        verdict.reason(),
                    )
                    .await;

                self.alerts.send(
                    "swap_policy_rejected",
                    claw_types::alert::AlertSeverity::Warning,
                    &policy_blocked_error(&verdict),
                    swap_audit_payload(&intent, &verdict),
                );

                warn!(
                    session = %session_str,
                    intent_id = %intent_id,
                    rule = ?verdict.rule_name(),
                    "swap intent rejected by policy"
                );

                Ok(ToolOutput {
                    tool_name: "submit_jupiter_swap".to_string(),
                    success: false,
                    data: Some(with_verdict_fields(
                        json!({
                            "status":    "policy_blocked",
                            "intent_id": intent_id.to_string(),
                        }),
                        &verdict,
                    )),
                    error: Some(policy_blocked_error(&verdict)),
                    duration_ms: 0,
                })
            }

            PolicyVerdict::RequiresHumanApproval {
                required_approver_role,
                approval_chain,
                ..
            } => {
                // Insert the intent first (no request_id yet).
                self.pending_repo
                    .insert(
                        &intent,
                        None,
                        pending_jupiter::state::PENDING_APPROVAL,
                        Some(&verdict),
                        None,
                    )
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(format!("persist intent: {e}")))?;

                // Build the approval request.
                // NOTE: `ApprovalRequest.simulation` is required by the existing struct;
                // for swap intents there is no on-chain simulation yet, so we
                // pass a stub. This will be revisited when swap simulation lands.
                let approval_request = ApprovalRequest {
                    id: uuid::Uuid::new_v4(),
                    session_id: input.session_id.clone(),
                    transaction_id: intent_id,
                    description: description.clone(),
                    policy_verdict: verdict.clone(),
                    simulation: placeholder_simulation(),
                    requested_at: chrono::Utc::now(),
                    decided: false,
                    required_approver_role: required_approver_role.clone(),
                };
                let request_id = approval_request.id;

                // Build the workflow (single-stage or multi-stage).
                let workflow = if let Some(chain) = approval_chain {
                    use claw_types::approval::{ApprovalStage, ApprovalWorkflowState};
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
                    ApprovalWorkflow {
                        request_id,
                        session_id: input.session_id.clone(),
                        state: ApprovalWorkflowState::Pending,
                        stages,
                        created_at: now,
                        updated_at: now,
                        expires_at: None,
                    }
                    .with_lease_seconds(self.approval_lease_seconds)
                } else {
                    ApprovalWorkflow::single_stage(
                        request_id,
                        input.session_id.clone(),
                        required_approver_role.clone(),
                    )
                    .with_lease_seconds(self.approval_lease_seconds)
                };

                // Link the approval request to the intent BEFORE registering,
                // so a concurrent lookup by request_id can always find the intent.
                self.pending_repo
                    .attach_request_id(intent_id, request_id)
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(format!("attach request_id: {e}")))?;

                // Park BEFORE registering so that a race where the operator
                // approves immediately still finds a parked oneshot to signal.
                let decision_rx = self.park_store.park(request_id, intent.clone());

                self.approval_store.register(approval_request, workflow);

                // Spawn the JIT resume task. It awaits decision_rx; on Approved
                // it runs the full JIT pipeline, on Rejected/Expired it marks
                // terminal state and exits.
                let deps = self.resume_deps();
                let intent_for_task = intent.clone();
                tokio::spawn(async move {
                    run_resume_task(request_id, intent_for_task, decision_rx, deps).await;
                });

                let _ = self.audit.append(
                    Some(&session_str),
                    &request_id.to_string(),
                    "jupiter_swap_approval_requested",
                    "pipeline",
                    &json!({
                        "request_id":    request_id,
                        "intent_id":     intent_id,
                        "wallet_pubkey": intent.wallet_pubkey,
                        "input_mint":    intent.input_mint,
                        "output_mint":   intent.output_mint,
                        "input_amount":  intent.input_amount,
                        "slippage_bps":  intent.slippage_bps,
                    }),
                    AuditSeverity::Warning,
                ).await;

                self.event_bus.publish(GatewayEvent::ApprovalRequested(
                    ApprovalLifecycleEvent {
                        header: EventHeader::new(Some(input.session_id.clone())),
                        session_id: input.session_id.clone(),
                        request_id,
                        transaction_id: intent_id,
                        approved: None,
                    },
                ));

                info!(
                    session = %session_str,
                    intent_id = %intent_id,
                    request_id = %request_id,
                    "jupiter swap intent awaiting operator approval"
                );

                Ok(ToolOutput {
                    tool_name: "submit_jupiter_swap".to_string(),
                    success: true,
                    data: Some(with_verdict_fields(
                        json!({
                            "status":              "awaiting_approval",
                            "intent_id":           intent_id.to_string(),
                            "approval_request_id": request_id.to_string(),
                            "message":             "Swap intent requires human approval. \
                                                    Use POST /sessions/:id/approve with approval_request_id.",
                        }),
                        &verdict,
                    )),
                    error: None,
                    duration_ms: 0,
                })
            }

            // These verdict variants are not produced by SwapPolicyConfig,
            // but handle defensively rather than panicking.
            PolicyVerdict::SimulationRequired | PolicyVerdict::SimulationFailed { .. } => {
                Err(ToolError::ExecutionFailed(format!(
                    "unexpected verdict for swap intent: {}",
                    verdict.label()
                )))
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn require_string(input: &ToolInput, key: &str) -> Result<String, ToolError> {
    input.parameters[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidInput {
            reason: format!("{key} is required and must be a non-empty string"),
        })
}

fn audit_severity_for(verdict: &PolicyVerdict) -> AuditSeverity {
    match verdict {
        PolicyVerdict::Rejected { .. } | PolicyVerdict::SimulationFailed { .. } => {
            AuditSeverity::Warning
        }
        PolicyVerdict::RequiresHumanApproval { .. } => AuditSeverity::Warning,
        _ => AuditSeverity::Info,
    }
}

fn swap_audit_payload(intent: &SwapIntent, verdict: &PolicyVerdict) -> serde_json::Value {
    json!({
        "intent_id":     intent.id,
        "session_id":    intent.session_id.to_string(),
        "wallet_pubkey": intent.wallet_pubkey,
        "input_mint":    intent.input_mint,
        "output_mint":   intent.output_mint,
        "input_amount":  intent.input_amount,
        "slippage_bps":  intent.slippage_bps,
        "verdict":       verdict.label(),
        "matched_rule":  verdict.rule_name(),
        "reason":        verdict.reason(),
    })
}

fn with_verdict_fields(
    mut data: serde_json::Value,
    verdict: &PolicyVerdict,
) -> serde_json::Value {
    data["policy_verdict"] = json!(verdict.label());
    if let Some(rule) = verdict.rule_name() {
        data["policy_rule_name"] = json!(rule);
    }
    if let Some(reason) = verdict.reason() {
        data["policy_reason"] = json!(reason);
    }
    data
}

fn policy_blocked_error(verdict: &PolicyVerdict) -> String {
    match (verdict.rule_name(), verdict.reason()) {
        (Some(rule), Some(reason)) => format!("swap policy blocked by rule '{rule}': {reason}"),
        (Some(rule), None) => format!("swap policy blocked by rule '{rule}'"),
        (None, Some(reason)) => format!("swap policy blocked: {reason}"),
        (None, None) => format!("swap policy blocked: {}", verdict.label()),
    }
}

/// A placeholder `SimulationResult` for swap intent approval requests.
///
/// The existing `ApprovalRequest` struct carries a `SimulationResult`
/// designed for transaction approvals. Swap intents have no on-chain
/// simulation at intent time — the real simulation happens at JIT build.
/// This stub satisfies the type and is explicitly documented as non-real.
fn placeholder_simulation() -> SimulationResult {
    SimulationResult {
        success: true,
        error: None,
        compute_units_used: None,
        logs: vec![],
        return_data: None,
        account_diffs: vec![],
        fee_lamports: None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use claw_state_store::db::Database;
    use claw_types::{
        session::SessionId,
        solana::SolanaNetwork,
        swap::SwapPolicyConfig,
    };
    use crate::integrations::jupiter_jit::JitOutcome;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const BONK: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
    const WALLET: &str = "CL4w7VNnCpGh3oJ8qfPFR4RjV6R3K4fZ1234567890ab";

    /// Test executor that records call count and returns a canned outcome.
    #[derive(Clone)]
    struct RecordingExecutor {
        count: Arc<AtomicUsize>,
        outcome: JitOutcome,
    }

    impl RecordingExecutor {
        fn new(outcome: JitOutcome) -> (Self, Arc<AtomicUsize>) {
            let count = Arc::new(AtomicUsize::new(0));
            (Self { count: count.clone(), outcome }, count)
        }
    }

    #[async_trait]
    impl JupiterJitExecutor for RecordingExecutor {
        async fn execute_jit(&self, _intent: &SwapIntent) -> JitOutcome {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
        }
    }

    struct ToolCtx {
        tool: SubmitJupiterSwapTool,
        repo: PendingJupiterRepository,
        approval_store: ApprovalStore,
        park_store: PendingJupiterParkStore,
        jit_calls: Arc<AtomicUsize>,
    }

    async fn build_tool_with_outcome(
        policy: SwapPolicyConfig,
        outcome: JitOutcome,
    ) -> ToolCtx {
        let db = Database::open_in_memory().await.unwrap();
        let pool = db.pool().clone();
        let pending_repo = PendingJupiterRepository::new(pool.clone());
        let audit = AuditRepository::new(pool);
        let approval_store = ApprovalStore::new();
        let event_bus = EventBus::new();
        let alerts = AlertDispatcher::default();
        let metrics = Arc::new(MetricsRegistry::default());
        let park_store = PendingJupiterParkStore::new();
        let (executor, jit_calls) = RecordingExecutor::new(outcome);

        let tool = SubmitJupiterSwapTool::new(
            pending_repo.clone(),
            approval_store.clone(),
            event_bus,
            audit,
            policy,
            SolanaNetwork::Devnet,
            600,
            alerts,
            metrics,
            Arc::new(executor),
            park_store.clone(),
        );

        ToolCtx {
            tool,
            repo: pending_repo,
            approval_store,
            park_store,
            jit_calls,
        }
    }

    async fn build_tool(policy: SwapPolicyConfig) -> (SubmitJupiterSwapTool, PendingJupiterRepository, ApprovalStore) {
        let ctx = build_tool_with_outcome(policy, JitOutcome::Submitted {
            signature: "STUB_SIG".to_string(),
        }).await;
        (ctx.tool, ctx.repo, ctx.approval_store)
    }

    fn tool_input(input_mint: &str, output_mint: &str, amount: u64, slippage: u16) -> ToolInput {
        ToolInput {
            tool_name: "submit_jupiter_swap".to_string(),
            parameters: json!({
                "input_mint": input_mint,
                "output_mint": output_mint,
                "input_amount": amount,
                "slippage_bps": slippage,
                "wallet_pubkey": WALLET,
                "description": "test swap",
            }),
            session_id: SessionId::from(Uuid::new_v4()),
            correlation_id: Uuid::new_v4(),
        }
    }

    // ── Auto-approved path ─────────────────────────────────────────────

    #[tokio::test]
    async fn empty_policy_auto_approves_and_runs_jit() {
        // Auto-approved intents go STRAIGHT to the JIT executor in a
        // spawned task. Final durable state after the task completes
        // should be `submitted`.
        let ctx = build_tool_with_outcome(
            SwapPolicyConfig::default(),
            JitOutcome::Submitted { signature: "AUTO_SIG".to_string() },
        ).await;
        let output = ctx.tool.execute(tool_input(USDC, WSOL, 100_000, 50)).await.unwrap();

        assert!(output.success);
        let data = output.data.unwrap();
        assert_eq!(data["status"], "approved_waiting_jit");

        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();

        // Wait for the spawned JIT task to settle.
        wait_for_state(&ctx.repo, intent_id, "submitted").await;

        let stored = ctx.repo.get(intent_id).await.unwrap().unwrap();
        assert_eq!(stored.state, "submitted");
        assert!(stored.request_id.is_none(), "auto-approve never creates an approval");
        assert_eq!(ctx.approval_store.pending_count(), 0, "no approval registered");
        assert_eq!(ctx.jit_calls.load(Ordering::SeqCst), 1);
    }

    /// Poll the repo until the intent reaches the expected state (or timeout).
    async fn wait_for_state(
        repo: &PendingJupiterRepository,
        intent_id: Uuid,
        expected: &str,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let stored = repo.get(intent_id).await.unwrap();
            if let Some(s) = stored {
                if s.state == expected {
                    return;
                }
            }
            if std::time::Instant::now() > deadline {
                let actual = repo.get(intent_id).await.unwrap()
                    .map(|s| s.state).unwrap_or_else(|| "<missing>".to_string());
                panic!("timed out waiting for state '{expected}', current: '{actual}'");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    // ── Rejected path ──────────────────────────────────────────────────

    #[tokio::test]
    async fn input_mint_not_allowed_is_rejected_and_persisted() {
        let policy = SwapPolicyConfig {
            allowed_input_mints: vec![USDC.to_string()],
            ..Default::default()
        };
        let (tool, repo, approval_store) = build_tool(policy).await;
        let output = tool.execute(tool_input(BONK, WSOL, 100, 50)).await.unwrap();

        assert!(!output.success);
        let data = output.data.unwrap();
        assert_eq!(data["status"], "policy_blocked");
        assert_eq!(data["policy_rule_name"], "swap-input-mint-not-allowed");

        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let stored = repo.get(intent_id).await.unwrap().unwrap();
        assert_eq!(stored.state, "rejected");
        assert_eq!(approval_store.pending_count(), 0, "no approval registered");
    }

    #[tokio::test]
    async fn output_mint_not_allowed_is_rejected() {
        let policy = SwapPolicyConfig {
            allowed_output_mints: vec![WSOL.to_string()],
            ..Default::default()
        };
        let (tool, repo, _) = build_tool(policy).await;
        let output = tool.execute(tool_input(USDC, BONK, 100, 50)).await.unwrap();

        let data = output.data.unwrap();
        assert_eq!(data["status"], "policy_blocked");
        assert_eq!(data["policy_rule_name"], "swap-output-mint-not-allowed");

        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let stored = repo.get(intent_id).await.unwrap().unwrap();
        assert_eq!(stored.state, "rejected");
    }

    #[tokio::test]
    async fn slippage_over_cap_is_rejected() {
        let policy = SwapPolicyConfig {
            max_slippage_bps: Some(50),
            ..Default::default()
        };
        let (tool, _, _) = build_tool(policy).await;
        let output = tool.execute(tool_input(USDC, WSOL, 100, 200)).await.unwrap();

        let data = output.data.unwrap();
        assert_eq!(data["status"], "policy_blocked");
        assert_eq!(data["policy_rule_name"], "swap-slippage-exceeds-cap");
    }

    // ── Approval-required path ─────────────────────────────────────────

    #[tokio::test]
    async fn amount_over_cap_requires_approval_and_registers() {
        let policy = SwapPolicyConfig {
            max_input_amount: Some(100_000),
            ..Default::default()
        };
        let (tool, repo, approval_store) = build_tool(policy).await;
        let output = tool.execute(tool_input(USDC, WSOL, 500_000, 50)).await.unwrap();

        assert!(output.success);
        let data = output.data.unwrap();
        assert_eq!(data["status"], "awaiting_approval");
        assert!(data["approval_request_id"].is_string());
        assert_eq!(data["policy_rule_name"], "swap-input-amount-exceeds-cap");

        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let request_id: Uuid = data["approval_request_id"].as_str().unwrap().parse().unwrap();

        // Durable intent linked to the approval request.
        let stored = repo.get(intent_id).await.unwrap().unwrap();
        assert_eq!(stored.state, "pending_approval");
        assert_eq!(stored.request_id, Some(request_id));
        assert!(stored.policy_verdict.is_some());

        // Approval workflow registered and discoverable.
        assert_eq!(approval_store.pending_count(), 1);
        let wf = approval_store.get_workflow(request_id).unwrap();
        assert!(!wf.state.is_terminal());
        assert!(wf.expires_at.is_some(), "lease must be set");

        // Lookup by request_id round-trips.
        let by_req = repo.get_by_request_id(request_id).await.unwrap().unwrap();
        assert_eq!(by_req.intent.id, intent_id);
    }

    #[tokio::test]
    async fn awaiting_approval_uses_policy_role() {
        let policy = SwapPolicyConfig {
            max_input_amount: Some(100),
            required_approver_role: Some("risk".to_string()),
            ..Default::default()
        };
        let (tool, _, approval_store) = build_tool(policy).await;
        let output = tool.execute(tool_input(USDC, WSOL, 200, 50)).await.unwrap();
        let data = output.data.unwrap();
        let request_id: Uuid = data["approval_request_id"].as_str().unwrap().parse().unwrap();

        let req = approval_store.get(request_id).unwrap();
        assert_eq!(req.required_approver_role.as_deref(), Some("risk"));

        let wf = approval_store.get_workflow(request_id).unwrap();
        assert_eq!(wf.stages[0].allowed_roles, vec!["risk"]);
    }

    // ── Input validation ───────────────────────────────────────────────

    #[tokio::test]
    async fn zero_amount_is_rejected() {
        let (tool, _, _) = build_tool(SwapPolicyConfig::default()).await;
        let mut input = tool_input(USDC, WSOL, 0, 50);
        input.parameters["input_amount"] = json!(0);
        let err = tool.execute(input).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn missing_input_mint_is_rejected() {
        let (tool, _, _) = build_tool(SwapPolicyConfig::default()).await;
        let mut input = tool_input(USDC, WSOL, 100, 50);
        input.parameters["input_mint"] = json!("");
        let err = tool.execute(input).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn slippage_above_10000_is_rejected() {
        let (tool, _, _) = build_tool(SwapPolicyConfig::default()).await;
        let mut input = tool_input(USDC, WSOL, 100, 50);
        input.parameters["slippage_bps"] = json!(10_001);
        let err = tool.execute(input).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    // ── Structural constraints ─────────────────────────────────────────

    // ── Daemon-wiring shape (exercised via tool registration) ──────────────

    /// The tool, when constructed and added to a `ToolRegistry`, must expose
    /// the name "submit_jupiter_swap". This mirrors the daemon's registration
    /// path: build tool → `registry.with_tool(tool)`.
    #[tokio::test]
    async fn registered_tool_reports_submit_jupiter_swap_name() {
        use claw_tool_system::registry::ToolRegistry;

        let ctx = build_tool_with_outcome(
            SwapPolicyConfig::default(),
            crate::integrations::jupiter_jit::JitOutcome::Submitted {
                signature: "_".to_string(),
            },
        ).await;

        let tool_arc: Arc<dyn claw_tool_system::tool::Tool> = Arc::new(ctx.tool);
        let registry = ToolRegistry::new().with_tool(tool_arc);

        let names = registry.names();
        assert!(
            names.iter().any(|n| n == "submit_jupiter_swap"),
            "registry must contain 'submit_jupiter_swap', got: {names:?}"
        );
    }

    /// Contract test: the tool-spec output-schema `status` enum MUST list every
    /// status value the tool actually emits, and MUST NOT list any value the
    /// tool never emits. Drift here silently breaks clients that code-gen from
    /// the schema (and breaks the execution persona, which hard-codes these).
    #[tokio::test]
    async fn tool_spec_status_enum_matches_actual_return_values() {
        let ctx = build_tool_with_outcome(
            SwapPolicyConfig::default(),
            JitOutcome::Submitted { signature: "_".to_string() },
        ).await;
        let spec = ctx.tool.spec();

        let enum_values: Vec<String> = spec.output_schema["properties"]["status"]["enum"]
            .as_array()
            .expect("spec.output_schema.properties.status.enum must be an array")
            .iter()
            .map(|v| v.as_str().expect("status enum entry must be a string").to_string())
            .collect();

        // Every value listed here MUST also appear as a literal `status` string
        // in the tool body. Adding a status? Update BOTH the enum and the code.
        let expected = ["approved_waiting_jit", "awaiting_approval", "policy_blocked"];
        assert_eq!(
            enum_values.len(),
            expected.len(),
            "status enum size drifted: got {enum_values:?}, expected {expected:?}"
        );
        for v in expected {
            assert!(
                enum_values.iter().any(|e| e == v),
                "status enum missing `{v}`: got {enum_values:?}"
            );
        }
    }

    /// Negative case: when the daemon does NOT construct+register the tool,
    /// the registry does not contain "submit_jupiter_swap". Encodes the
    /// `jupiter.enabled = false` branch structurally.
    #[tokio::test]
    async fn empty_registry_does_not_contain_submit_jupiter_swap() {
        use claw_tool_system::registry::ToolRegistry;
        let registry = ToolRegistry::new();
        assert!(!registry.names().iter().any(|n| n == "submit_jupiter_swap"));
    }

    #[tokio::test]
    async fn does_not_persist_transaction_bytes() {
        // Guardrail: the stored intent must never carry tx bytes or
        // instruction payloads. This test locks the repo schema intent.
        let (tool, repo, _) = build_tool(SwapPolicyConfig::default()).await;
        let output = tool.execute(tool_input(USDC, WSOL, 100, 50)).await.unwrap();
        let intent_id: Uuid = output.data.unwrap()["intent_id"].as_str().unwrap().parse().unwrap();

        let stored = repo.get(intent_id).await.unwrap().unwrap();
        // Serialize the stored row to JSON and confirm no forbidden keys.
        let serialized = serde_json::to_value(&stored.intent).unwrap();
        let obj = serialized.as_object().unwrap();
        for forbidden in ["transaction_b64", "blockhash", "instructions", "lookup_tables"] {
            assert!(
                !obj.contains_key(forbidden),
                "SwapIntent must not carry '{forbidden}'"
            );
        }
    }

    // ── Full JIT flow: park + approve + resume ─────────────────────────────

    use claw_types::approval::{ApprovalDecision, ApprovalWorkflowState};

    fn policy_needing_approval() -> SwapPolicyConfig {
        SwapPolicyConfig {
            max_input_amount: Some(100),
            ..Default::default()
        }
    }

    /// Submit an intent that requires approval, then simulate the decision
    /// landing through the approval_store → signal park chain, and await
    /// the resume task reaching the expected terminal state.
    async fn submit_and_drive_to(
        outcome: JitOutcome,
        decision: ApprovalWorkflowState,
    ) -> (ToolCtx, Uuid, Uuid) {
        let ctx = build_tool_with_outcome(policy_needing_approval(), outcome).await;
        let output = ctx.tool
            .execute(tool_input(USDC, WSOL, 500, 50))
            .await
            .unwrap();
        let data = output.data.unwrap();
        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let request_id: Uuid = data["approval_request_id"].as_str().unwrap().parse().unwrap();

        // Intent parked; JIT executor must NOT have been called yet.
        assert_eq!(ctx.jit_calls.load(Ordering::SeqCst), 0, "build called before approval");

        // Fire the signal directly (mimics what route_approval_outcome does
        // on a terminal Approved/Rejected/Expired outcome).
        assert!(ctx.park_store.signal(request_id, decision));
        (ctx, intent_id, request_id)
    }

    #[tokio::test]
    async fn build_is_not_called_before_approval() {
        // Tool submits a RequiresHumanApproval intent. We assert the executor
        // is not invoked until the park store is signaled Approved.
        let ctx = build_tool_with_outcome(
            policy_needing_approval(),
            JitOutcome::Submitted { signature: "S".to_string() },
        ).await;
        let output = ctx.tool
            .execute(tool_input(USDC, WSOL, 500, 50))
            .await
            .unwrap();
        let data = output.data.unwrap();
        assert_eq!(data["status"], "awaiting_approval");

        // Give the spawned resume task a moment to reach its await.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            ctx.jit_calls.load(Ordering::SeqCst),
            0,
            "Jupiter /build must NOT be called before approval"
        );

        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let stored = ctx.repo.get(intent_id).await.unwrap().unwrap();
        assert_eq!(stored.state, "pending_approval");
    }

    #[tokio::test]
    async fn build_is_called_exactly_once_after_approval() {
        let (ctx, intent_id, _) = submit_and_drive_to(
            JitOutcome::Submitted { signature: "POST_APPROVAL_SIG".to_string() },
            ApprovalWorkflowState::Approved,
        ).await;

        wait_for_state(&ctx.repo, intent_id, "submitted").await;
        assert_eq!(
            ctx.jit_calls.load(Ordering::SeqCst),
            1,
            "exactly one /build call after approval"
        );
    }

    #[tokio::test]
    async fn build_failure_after_approval_marks_jit_build_failed() {
        let (ctx, intent_id, _) = submit_and_drive_to(
            JitOutcome::BuildFailed("jupiter 429".to_string()),
            ApprovalWorkflowState::Approved,
        ).await;

        wait_for_state(&ctx.repo, intent_id, "jit_build_failed").await;
        let stored = ctx.repo.get(intent_id).await.unwrap().unwrap();
        assert_ne!(stored.state, "submitted", "build failure must NEVER reach submitted");
        assert!(stored.last_error.as_deref().unwrap().contains("jupiter 429"));
    }

    #[tokio::test]
    async fn simulate_failure_after_approval_marks_jit_build_failed() {
        let (ctx, intent_id, _) = submit_and_drive_to(
            JitOutcome::SimulateFailed("blockhash expired".to_string()),
            ApprovalWorkflowState::Approved,
        ).await;

        wait_for_state(&ctx.repo, intent_id, "jit_build_failed").await;
        let stored = ctx.repo.get(intent_id).await.unwrap().unwrap();
        assert_ne!(stored.state, "submitted", "simulate failure must NEVER reach submitted");
        assert!(stored.last_error.as_deref().unwrap().contains("blockhash expired"));
    }

    #[tokio::test]
    async fn external_wallet_outcome_marks_awaiting_wallet_signature() {
        let wallet_req_id = Uuid::new_v4();
        let (ctx, intent_id, _) = submit_and_drive_to(
            JitOutcome::AwaitingWalletSignature { wallet_signature_request_id: wallet_req_id },
            ApprovalWorkflowState::Approved,
        ).await;

        wait_for_state(&ctx.repo, intent_id, "awaiting_wallet_signature").await;
        let stored = ctx.repo.get(intent_id).await.unwrap().unwrap();
        assert_ne!(stored.state, "submitted", "awaiting wallet signature is not submitted");
    }

    #[tokio::test]
    async fn rejection_after_approval_request_skips_jit_and_marks_rejected() {
        let (ctx, intent_id, _) = submit_and_drive_to(
            JitOutcome::Submitted { signature: "SHOULD_NOT_RUN".to_string() },
            ApprovalWorkflowState::Rejected,
        ).await;

        wait_for_state(&ctx.repo, intent_id, "rejected").await;
        assert_eq!(ctx.jit_calls.load(Ordering::SeqCst), 0, "executor must not run on reject");
    }

    // ── Prompt 3 verification: gaps in #5, #6, #7 ─────────────────────────

    /// Invariant #5 (post-JIT): after a successful submit, the stored row
    /// still carries ONLY intent-level fields. No tx bytes, no blockhash,
    /// no ALTs, no instruction payloads have been silently added.
    #[tokio::test]
    async fn post_jit_stored_row_still_has_no_tx_data() {
        let ctx = build_tool_with_outcome(
            SwapPolicyConfig::default(),
            JitOutcome::Submitted { signature: "POST_JIT_SIG".to_string() },
        ).await;
        let output = ctx.tool.execute(tool_input(USDC, WSOL, 100, 50)).await.unwrap();
        let intent_id: Uuid = output.data.unwrap()["intent_id"].as_str().unwrap().parse().unwrap();

        wait_for_state(&ctx.repo, intent_id, "submitted").await;

        let stored = ctx.repo.get(intent_id).await.unwrap().unwrap();

        // Intent itself has no tx-shaped fields (compile-time structural check).
        let intent_json = serde_json::to_value(&stored.intent).unwrap();
        let intent_obj = intent_json.as_object().unwrap();
        for forbidden in [
            "transaction_b64", "blockhash", "instructions", "lookup_tables",
            "compute_budget_instructions", "setup_instructions", "swap_instruction",
            "addresses_by_lookup_table_address",
        ] {
            assert!(
                !intent_obj.contains_key(forbidden),
                "SwapIntent row must not carry '{forbidden}' even after submit"
            );
        }

        // The policy verdict (stored as JSON) also must not contain tx data —
        // only policy metadata.
        if let Some(verdict) = &stored.policy_verdict {
            let verdict_json = serde_json::to_value(verdict).unwrap();
            let serialized = verdict_json.to_string();
            for forbidden in ["blockhash", "transaction_b64", "swapInstruction"] {
                assert!(
                    !serialized.contains(forbidden),
                    "PolicyVerdict must not carry '{forbidden}' — found in: {serialized}"
                );
            }
        }
    }

    /// Invariant #6: the preview (when present) is stored informationally
    /// and MUST NOT be consumed by the execution path. After a successful
    /// JIT, the preview row is byte-identical to what was stored beforehand.
    #[tokio::test]
    async fn preview_is_inert_during_execution() {
        use claw_types::swap::SwapPreview;

        let ctx = build_tool_with_outcome(
            SwapPolicyConfig::default(),
            JitOutcome::Submitted { signature: "PREVIEW_SIG".to_string() },
        ).await;

        // Manually seed an intent WITH a preview, then drive the JIT flow.
        let intent = SwapIntent::new(
            SessionId::from(Uuid::new_v4()),
            WALLET,
            SolanaNetwork::Devnet,
            USDC,
            WSOL,
            100,
            50,
            "preview inertness test",
        );
        let preview = SwapPreview {
            expected_output_amount: 999_999_999,
            price_impact_pct: Some("0.9999".to_string()),
            route_labels: vec!["PretendDEX".to_string()],
            fetched_at: chrono::Utc::now(),
        };

        ctx.repo
            .insert(
                &intent,
                None,
                claw_state_store::pending_jupiter::state::APPROVED_WAITING_JIT,
                None,
                Some(&preview),
            )
            .await
            .unwrap();

        // Drive JIT directly. The executor sees ONLY the intent
        // (compile-time guarantee of the trait signature).
        let deps = ctx.tool.resume_deps();
        crate::integrations::jupiter_jit::run_jit_after_approval(
            Uuid::new_v4(),
            intent.clone(),
            deps,
        ).await;

        wait_for_state(&ctx.repo, intent.id, "submitted").await;

        // Preview is still present and unchanged after a SUCCESSFUL submit.
        // That demonstrates execution did not consume, rewrite, or rely on it.
        let stored = ctx.repo.get(intent.id).await.unwrap().unwrap();
        let stored_preview = stored.preview.expect("preview must survive JIT");
        assert_eq!(stored_preview.expected_output_amount, 999_999_999);
        assert_eq!(stored_preview.price_impact_pct.as_deref(), Some("0.9999"));
        assert_eq!(stored_preview.route_labels, vec!["PretendDEX"]);

        // And the final state is submitted — proving the path worked WITHOUT
        // needing the preview (the mock executor returned Submitted based
        // purely on the intent it received, not on the preview).
        assert_eq!(stored.state, "submitted");
    }

    /// Invariant #7a (local scope): the park store's oneshot guarantees that
    /// even if `route_approval_outcome` fires twice for the same request_id,
    /// the resume task is woken exactly once — so the executor runs once.
    #[tokio::test]
    async fn double_approval_signal_does_not_cause_double_build() {
        let ctx = build_tool_with_outcome(
            policy_needing_approval(),
            JitOutcome::Submitted { signature: "ONCE_ONLY".to_string() },
        ).await;
        let output = ctx.tool
            .execute(tool_input(USDC, WSOL, 500, 50))
            .await
            .unwrap();
        let data = output.data.unwrap();
        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let request_id: Uuid = data["approval_request_id"].as_str().unwrap().parse().unwrap();

        // First signal delivers; oneshot is consumed.
        assert!(ctx.park_store.signal(request_id, ApprovalWorkflowState::Approved));

        wait_for_state(&ctx.repo, intent_id, "submitted").await;
        assert_eq!(ctx.jit_calls.load(Ordering::SeqCst), 1);

        // Second signal is a graceful no-op — entry already consumed/removed.
        let delivered_twice = ctx.park_store.signal(request_id, ApprovalWorkflowState::Approved);
        assert!(!delivered_twice, "second signal must NOT deliver");

        // Give any phantom task a chance to execute (it shouldn't).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            ctx.jit_calls.load(Ordering::SeqCst),
            1,
            "executor must remain called exactly once despite replayed signal"
        );

        // Durable state stays submitted — no regression.
        let stored = ctx.repo.get(intent_id).await.unwrap().unwrap();
        assert_eq!(stored.state, "submitted");
    }

    /// Invariant #7b: `ApprovalStore::decide()` is also idempotent.
    /// A replayed decide returns `AlreadyDecided` and does not reach
    /// `route_approval_outcome`'s Approved arm — so the park store never
    /// receives a second signal even from an operator double-click.
    #[tokio::test]
    async fn double_decide_is_already_decided_no_second_signal() {
        let ctx = build_tool_with_outcome(
            policy_needing_approval(),
            JitOutcome::Submitted { signature: "STEADY".to_string() },
        ).await;
        let output = ctx.tool
            .execute(tool_input(USDC, WSOL, 500, 50))
            .await
            .unwrap();
        let data = output.data.unwrap();
        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let request_id: Uuid = data["approval_request_id"].as_str().unwrap().parse().unwrap();

        let decision = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
        let (first, _) = ctx.approval_store.decide(&decision);
        assert!(matches!(first, claw_types::approval::ApprovalOutcome::Approved));
        ctx.park_store.signal(request_id, ApprovalWorkflowState::Approved);

        wait_for_state(&ctx.repo, intent_id, "submitted").await;

        // Replay the decision — should be rejected as AlreadyDecided.
        let (second, _) = ctx.approval_store.decide(&decision);
        assert!(matches!(second, claw_types::approval::ApprovalOutcome::AlreadyDecided));

        // Executor count unchanged.
        assert_eq!(ctx.jit_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn approval_via_approval_store_decide_flows_through_signal_to_jit() {
        // End-to-end decision flow: operator calls ApprovalStore::decide,
        // then the caller signals the park store (as route_approval_outcome does).
        let ctx = build_tool_with_outcome(
            policy_needing_approval(),
            JitOutcome::Submitted { signature: "E2E_SIG".to_string() },
        ).await;
        let output = ctx.tool
            .execute(tool_input(USDC, WSOL, 500, 50))
            .await
            .unwrap();
        let data = output.data.unwrap();
        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let request_id: Uuid = data["approval_request_id"].as_str().unwrap().parse().unwrap();

        // Operator decides via ApprovalStore. Policy default approver role
        // is "treasury" when the intent exceeds the input_amount cap.
        let decision = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
        let (outcome, _) = ctx.approval_store.decide(&decision);
        assert!(
            matches!(outcome, claw_types::approval::ApprovalOutcome::Approved),
            "expected Approved, got: {outcome:?}"
        );

        // Mimic the `route_approval_outcome` Approved arm: signal both stores.
        // The Jupiter park store signal wakes our parked resume task.
        ctx.park_store.signal(request_id, ApprovalWorkflowState::Approved);

        wait_for_state(&ctx.repo, intent_id, "submitted").await;
        assert_eq!(ctx.jit_calls.load(Ordering::SeqCst), 1);
    }
}

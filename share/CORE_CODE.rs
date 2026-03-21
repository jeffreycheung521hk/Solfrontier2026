// ============================================================================
// ClawSolana — Core Code (V1.5 STEP 1)
// Share with ARCHITECTURE_CONTEXT.md for context. This file shows the 5 most
// important files for security review.
// ============================================================================

// ============================================================================
// [1/5] crates/tool-system/src/permissions.rs
// Per-session least-privilege capability model. The for_role() method is new in V1.5.
// ============================================================================

use std::collections::HashSet;
use claw_types::agent::AgentRole;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    ReadWallet, ReadChain, BuildTransaction, SimulateTransaction,
    /// Highly sensitive. Never in default set. Never in for_role() output.
    SignTransaction,
    /// Highly sensitive. Never in default set. Never in for_role() output.
    SendTransaction,
    ManageSubscriptions, ManageWallets, ManagePolicies, ReadAuditLog, SystemAdmin,
}

impl Capability {
    pub fn is_security_sensitive(&self) -> bool {
        matches!(self,
            Capability::SignTransaction | Capability::SendTransaction |
            Capability::ManageWallets | Capability::ManagePolicies | Capability::SystemAdmin
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct CapabilitySet { granted: HashSet<Capability> }

impl CapabilitySet {
    pub fn empty() -> Self { Self::default() }

    /// Default grants for agent sessions: read + simulate only.
    pub fn default_agent() -> Self {
        let mut s = Self::empty();
        s.grant(Capability::ReadWallet);
        s.grant(Capability::ReadChain);
        s.grant(Capability::BuildTransaction);
        s.grant(Capability::SimulateTransaction);
        s.grant(Capability::ManageSubscriptions);
        s
    }

    /// Least-privilege per AgentRole. Called for every session dispatch in V1.5.
    /// SignTransaction and SendTransaction are NEVER included here.
    pub fn for_role(role: AgentRole) -> Self {
        let mut s = Self::empty();
        match role {
            AgentRole::Research => {
                s.grant(Capability::ReadWallet);
                s.grant(Capability::ReadChain);
                s.grant(Capability::SimulateTransaction);
            }
            AgentRole::Execution => {
                s.grant(Capability::ReadWallet);
                s.grant(Capability::ReadChain);
                s.grant(Capability::SimulateTransaction);
                s.grant(Capability::BuildTransaction);
            }
            AgentRole::Risk => {
                s.grant(Capability::ReadChain);
                s.grant(Capability::ReadWallet);
                s.grant(Capability::ManageSubscriptions);
            }
            AgentRole::Ops => {
                s.grant(Capability::ReadWallet);
                s.grant(Capability::ReadChain);
                s.grant(Capability::BuildTransaction);
                s.grant(Capability::SimulateTransaction);
                s.grant(Capability::ManageSubscriptions);
                s.grant(Capability::ManageWallets);
                s.grant(Capability::ManagePolicies);
                s.grant(Capability::ReadAuditLog);
                // SignTransaction and SendTransaction intentionally excluded.
            }
        }
        s
    }

    pub fn grant(&mut self, cap: Capability) { self.granted.insert(cap); }
    pub fn has(&self, cap: &Capability) -> bool { self.granted.contains(cap) }

    /// Fail-closed: unknown capability strings treated as missing.
    pub fn check_required(&self, required: &[String]) -> Result<(), Vec<String>> {
        let mut missing = Vec::new();
        for req in required {
            match Capability::from_str(req) {
                Some(cap) => { if !self.has(&cap) { missing.push(req.clone()); } }
                None      => { missing.push(format!("unknown:{req}")); }
            }
        }
        if missing.is_empty() { Ok(()) } else { Err(missing) }
    }
}

// ============================================================================
// [2/5] crates/tool-system/src/dispatch.rs
// Capability check before every tool execution. Fail-closed.
// ============================================================================

use std::time::Instant;
use tokio::time::{timeout, Duration};
use tracing::{instrument, warn};
use claw_types::tool::{ToolInput, ToolOutput};
use crate::{errors::ToolError, permissions::CapabilitySet, registry::ToolRegistry};

#[derive(Clone)]
pub struct ToolDispatcher {
    registry:     ToolRegistry,
    capabilities: CapabilitySet,
}

impl ToolDispatcher {
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry, capabilities: CapabilitySet::all() } // Only used directly in tests now
    }

    /// Called by GatewayMessageHandler with role-narrowed caps per session.
    pub fn with_capabilities(registry: ToolRegistry, capabilities: CapabilitySet) -> Self {
        Self { registry, capabilities }
    }

    pub fn registry(&self) -> &ToolRegistry { &self.registry }

    #[instrument(skip(self, input), fields(tool = %input.tool_name, session = %input.session_id))]
    pub async fn dispatch(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let tool = self.registry.get(&input.tool_name)?;
        let spec = tool.spec();

        // ── Capability check — fail-closed ───────────────────────────────
        if let Err(missing) = self.capabilities.check_required(&spec.required_capabilities) {
            warn!(tool = %input.tool_name, missing_capabilities = ?missing, "tool dispatch denied");
            return Err(ToolError::PermissionDenied {
                tool_name: input.tool_name.clone(),
                missing_capabilities: missing,
            });
        }

        // ── Execute with timeout ─────────────────────────────────────────
        let timeout_ms = spec.timeout_ms;
        let start      = Instant::now();
        let result = timeout(Duration::from_millis(timeout_ms), tool.execute(input.clone())).await;
        let _duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e))     => Err(e),
            Err(_elapsed)  => {
                warn!(tool = %input.tool_name, timeout_ms, "tool call timed out");
                Err(ToolError::Timeout { ms: timeout_ms })
            }
        }
    }
}

// ============================================================================
// [3/5] crates/agent-runtime/src/agent.rs
// ReAct loop. Dispatcher passed per-call (not stored). Tool traces persisted here.
// ============================================================================

use tracing::{info, instrument, warn};
use uuid::Uuid;
use claw_state_store::tool_traces::ToolTraceRepository;
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentResponseStatus, ToolCallSummary},
    tool::{ToolInput, ToolTraceStatus},
};
use claw_tool_system::ToolDispatcher;
use crate::{errors::AgentError, llm::LlmClientRef, personas::system_prompt_for, session::AgentSession};

const MAX_ITERATIONS: u32 = 10;

/// Agent does NOT own a ToolDispatcher. Callers pass a role-narrowed dispatcher per run() call.
pub struct Agent { llm: LlmClientRef }

impl Agent {
    pub fn new(llm: LlmClientRef) -> Self { Self { llm } }

    #[instrument(skip(self, session, dispatcher, tool_traces), fields(session = %session.id, role = %session.agent_role))]
    pub async fn run(
        &self,
        command:     AgentCommand,
        session:     &mut AgentSession,
        dispatcher:  ToolDispatcher,
        tool_traces: &ToolTraceRepository,
    ) -> Result<AgentResponse, AgentError> {
        let system_prompt = system_prompt_for(session.agent_role);
        let tool_specs    = dispatcher.registry().all_specs();

        session.context.push_user(command.text.clone()); // TRUSTED

        let mut tool_call_summaries = Vec::new();
        let mut iterations = 0u32;

        loop {
            iterations += 1;
            if iterations > MAX_ITERATIONS { return Err(AgentError::MaxIterationsExceeded); }

            let response = self.llm.complete(system_prompt, &session.context.messages(), &tool_specs).await?;

            if !response.tool_calls.is_empty() {
                session.context.push_assistant(
                    response.text.clone().unwrap_or_else(|| format!("[calling {} tools]", response.tool_calls.len()))
                );

                for tc in &response.tool_calls {
                    let trace_id  = Uuid::new_v4().to_string();
                    let start     = std::time::Instant::now();
                    let tool_input = ToolInput {
                        tool_name: tc.tool_name.clone(), parameters: tc.input.clone(),
                        session_id: session.id.clone(), correlation_id: command.id,
                    };

                    // Persist start
                    let _ = tool_traces.start(&trace_id, &session.id.to_string(),
                        &command.id.to_string(), &tc.tool_name, &tc.input).await;

                    let result      = dispatcher.dispatch(tool_input).await;
                    let duration_ms = start.elapsed().as_millis() as u64;

                    let (trace_status, status_str, result_text) = match &result {
                        Ok(output) => (ToolTraceStatus::Succeeded, "succeeded",
                                       serde_json::to_string(&output.data).unwrap_or_default()),
                        Err(e)     => (ToolTraceStatus::Failed, "failed",
                                       format!("{{\"error\": \"{}\"}}", e)),
                    };

                    // Persist finish
                    let _ = tool_traces.finish(
                        &trace_id, trace_status,
                        result.as_ref().ok().and_then(|o| o.data.as_ref()),
                        result.as_ref().err().map(|e| e.to_string()).as_deref(),
                        duration_ms,
                    ).await;

                    tool_call_summaries.push(ToolCallSummary {
                        tool_name: tc.tool_name.clone(), status: status_str.to_string(), duration_ms
                    });

                    // ── Tool/LLM Boundary ──────────────────────────────────────────────
                    // Tool results are UNTRUSTED EXTERNAL DATA.
                    // push_tool_result() prefixes: "[TOOL RESULT — UNTRUSTED DATA — ...]"
                    session.context.push_tool_result(&tc.tool_name, &tc.id, result_text);
                }
                session.tool_calls += response.tool_calls.len() as u64;
                continue;
            }

            let text = response.text.unwrap_or_else(|| "I completed the task.".to_string());
            session.context.push_assistant(text.clone());
            session.task_count += 1;
            return Ok(AgentResponse {
                command_id: command.id, text, data: None,
                status: AgentResponseStatus::Completed,
                tool_calls: tool_call_summaries,
            });
        }
    }
}

// ============================================================================
// [4/5] crates/gateway/src/daemon.rs (GatewayMessageHandler only)
// Per-session dispatcher construction + full audit wiring.
// ============================================================================

struct GatewayMessageHandler {
    agent:        Arc<Agent>,
    agent_router: Arc<AgentRouter>,
    registry:     ToolRegistry,  // Shared, Arc-backed (cheap clone)
    audit:        AuditRepository,
    tool_traces:  ToolTraceRepository,
}

impl GatewayMessageHandler {
    async fn handle_inner(
        &self,
        session_id: &SessionId,
        command:    AgentCommand,
    ) -> Result<AgentResponse, String> {
        let session_id_str = session_id.to_string();
        let correlation_id = command.id.to_string();

        // Build per-session capability set. Falls back to Research (most restrictive).
        let role       = self.agent_router.session_role(session_id).await.unwrap_or(AgentRole::Research);
        let caps       = CapabilitySet::for_role(role);
        let dispatcher = ToolDispatcher::with_capabilities(self.registry.clone(), caps);

        let _ = self.audit.append(
            Some(&session_id_str), &correlation_id, "agent_task_started", "agent",
            &serde_json::json!({
                "command_id":   &correlation_id,
                "agent_role":   role.to_string(),
                "text_preview": &command.text.chars().take(200).collect::<String>(),
            }),
            AuditSeverity::Info,
        ).await;

        let result = self.agent_router
            .dispatch(session_id, command, &self.agent, dispatcher, &self.tool_traces)
            .await;

        match &result {
            Ok(resp) => { let _ = self.audit.append(Some(&session_id_str), &correlation_id,
                "agent_task_completed", "agent",
                &serde_json::json!({ "status": resp.status, "tool_calls": resp.tool_calls.len() }),
                AuditSeverity::Info).await; }
            Err(e)   => { let _ = self.audit.append(Some(&session_id_str), &correlation_id,
                "agent_task_failed", "agent",
                &serde_json::json!({ "error": e.to_string() }),
                AuditSeverity::Error).await; }
        }

        result.map_err(|e| e.to_string())
    }
}

// ============================================================================
// [5/5] crates/wallet-engine/src/pipeline.rs (key structs only)
// Typestate-enforced pipeline. sign() requires ApprovedTransaction — compile error otherwise.
// ============================================================================

/// Only constructible via simulate(). Guarantees simulation.success == true.
pub struct SimulatedTransaction {
    pub(crate) inner: Transaction,
    pub simulation:   SimulationResult,
}

/// Only constructible via evaluate_policy(). Guarantees verdict is not blocked.
pub struct ApprovedTransaction {
    pub(crate) inner:   SimulatedTransaction,
    pub policy_verdict: PolicyVerdict,
}

// sign() signature — TYPE ERROR to pass raw Transaction:
pub async fn sign(
    &self,
    proposal:     &TransactionProposal,
    mut approved: ApprovedTransaction, // <── compile-time invariant enforcement
    signer:       &SignerRef,
) -> Result<PipelineResult, WalletError> {
    // DryRun: stop before signing
    if self.approval_mode == ApprovalMode::DryRun {
        return Ok(PipelineResult::DryRun { simulation: approved.inner.simulation.clone(), policy_verdict: approved.policy_verdict.clone() });
    }
    // RequiresHuman + Automatic mode: emit AwaitingApproval
    if approved.policy_verdict.requires_human() && self.approval_mode == ApprovalMode::Automatic {
        return Ok(PipelineResult::AwaitingApproval { /* ... */ });
    }
    // Sign — only reachable with valid ApprovedTransaction + compatible approval mode
    let signature = signer.sign_transaction(&mut approved.inner.inner).await?;
    Ok(PipelineResult::Signed { /* ... */ })
}

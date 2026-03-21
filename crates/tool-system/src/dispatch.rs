//! Tool dispatcher — runs tools with capability check, timeout, and trace recording.
//!
//! # Capability enforcement
//!
//! Before executing any tool, the dispatcher checks whether the caller's
//! `CapabilitySet` contains all capabilities declared in the tool's spec.
//! This is enforced at the dispatch site — tools are never invoked without
//! the required capabilities being present.
//!
//! # V1 limitation
//!
//! In V1, the capability set is `CapabilitySet::all()` (all capabilities granted).
//! Per-session narrowing based on `AgentRole` is a V2 feature.
//! The check infrastructure is live; the narrowing is the next step.
//! Tracked in TASK.md under "Per-session capability narrowing".

use std::time::Instant;

use tokio::time::{timeout, Duration};
use tracing::{instrument, warn};

use claw_types::tool::{ToolInput, ToolOutput, ToolTraceStatus};

use crate::{
    errors::ToolError,
    permissions::CapabilitySet,
    registry::ToolRegistry,
};

/// Dispatches tool calls with capability enforcement, timeout, and trace recording.
#[derive(Clone)]
pub struct ToolDispatcher {
    registry: ToolRegistry,
    /// The capability set used to authorize tool invocations.
    ///
    /// V1: always `CapabilitySet::all()`.
    /// V2: per-session capability set, narrowed by AgentRole and policy.
    capabilities: CapabilitySet,
}

impl ToolDispatcher {
    /// Creates a dispatcher with `CapabilitySet::all()` (V1 default).
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            capabilities: CapabilitySet::all(), // V1: all caps granted; V2: narrow by role
        }
    }

    /// Creates a dispatcher with an explicit capability set (for testing or future use).
    pub fn with_capabilities(registry: ToolRegistry, capabilities: CapabilitySet) -> Self {
        Self { registry, capabilities }
    }

    /// Returns a reference to the underlying tool registry.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Dispatches a tool call.
    ///
    /// # Steps
    ///
    /// 1. Look up the tool by name (ToolError::NotFound if missing)
    /// 2. Check that the dispatcher's capability set satisfies the tool's requirements
    ///    (ToolError::PermissionDenied if any capability is missing)
    /// 3. Execute the tool with timeout enforcement
    /// 4. Return ToolOutput on success or ToolError on failure/timeout
    #[instrument(skip(self, input), fields(tool = %input.tool_name, session = %input.session_id))]
    pub async fn dispatch(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let tool = self.registry.get(&input.tool_name)?;
        let spec = tool.spec();

        // ── Capability check ──────────────────────────────────────────────
        // Fail-closed: if any required capability is missing, deny execution.
        if let Err(missing) = self.capabilities.check_required(&spec.required_capabilities) {
            warn!(
                tool = %input.tool_name,
                missing_capabilities = ?missing,
                "tool dispatch denied: missing capabilities"
            );
            return Err(ToolError::PermissionDenied {
                tool_name:           input.tool_name.clone(),
                missing_capabilities: missing,
            });
        }

        // ── Execute with timeout ──────────────────────────────────────────
        let timeout_ms = spec.timeout_ms;
        let start      = Instant::now();

        let result = timeout(
            Duration::from_millis(timeout_ms),
            tool.execute(input.clone()),
        )
        .await;

        let _duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(output))  => Ok(output),
            Ok(Err(e))      => Err(e),
            Err(_elapsed)   => {
                warn!(
                    tool = %input.tool_name,
                    timeout_ms,
                    "tool call timed out"
                );
                Err(ToolError::Timeout { ms: timeout_ms })
            }
        }
    }
}

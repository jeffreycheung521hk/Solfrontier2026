//! The core agent execution loop.
//!
//! Implements a ReAct-style loop:
//!   1. Receive task (as `AgentCommand`)
//!   2. Send to LLM with system prompt + tool specs
//!   3. If LLM requests tool calls → dispatch them → send results back
//!   4. Repeat until LLM responds with `end_turn` or max iterations hit
//!   5. Return `AgentResponse`
//!
//! Max iterations: 10 (prevents infinite loops from buggy LLM responses)
//!
//! # Tool/LLM boundary
//!
//! Tool results are pushed via `context.push_tool_result()` (NOT `push_user()`).
//! This ensures they are tagged as untrusted external data in the context.
//! See `llm/context.rs` for the full explanation.

use tracing::{info, instrument, warn};
use uuid::Uuid;

use claw_state_store::tool_traces::ToolTraceRepository;
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentResponseStatus, ToolCallSummary},
    tool::{ToolInput, ToolTraceStatus},
};
use claw_tool_system::ToolDispatcher;

use crate::{
    errors::AgentError,
    llm::LlmClientRef,
    personas::system_prompt_for,
    session::AgentSession,
};

const MAX_ITERATIONS: u32 = 100;

/// The agent executor.
///
/// `Agent` does not own a `ToolDispatcher` — callers pass a per-session
/// dispatcher (narrowed to the session's `AgentRole`) at each `run()` call.
/// This enables least-privilege capability enforcement per session.
pub struct Agent {
    llm: LlmClientRef,
}

impl Agent {
    pub fn new(llm: LlmClientRef) -> Self {
        Self { llm }
    }

    /// Executes a command within the given session, using the provided dispatcher.
    ///
    /// `dispatcher` must already be narrowed to the session's capability set.
    /// `tool_traces` is used to persist each tool invocation's start/finish.
    #[instrument(skip(self, session, dispatcher, tool_traces), fields(session = %session.id, role = %session.agent_role))]
    pub async fn run(
        &self,
        command: AgentCommand,
        session: &mut AgentSession,
        dispatcher: ToolDispatcher,
        tool_traces: &ToolTraceRepository,
    ) -> Result<AgentResponse, AgentError> {
        let system_prompt = system_prompt_for(session.agent_role);
        let tool_specs = dispatcher.registry().all_specs();

        // Add the operator's command to history as a trusted UserInstruction.
        session.context.push_user(command.text.clone());

        let mut tool_call_summaries = Vec::new();
        let mut iterations = 0u32;

        loop {
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                warn!(session = %session.id, "agent hit max iterations");
                return Err(AgentError::MaxIterationsExceeded);
            }

            let context_messages = session.context.messages();
            let response = self
                .llm
                .complete(system_prompt, &context_messages, &tool_specs)
                .await?;

            // If there are tool calls, dispatch them and continue
            if !response.tool_calls.is_empty() {
                // Record the assistant's tool-calling intent with native tool_use blocks.
                // This ensures the Anthropic API sees the correct request/response pairing.
                session.context.push_assistant_tool_use(
                    response.text.clone(),
                    response.tool_calls.clone(),
                );

                for tc in &response.tool_calls {
                    info!(tool = %tc.tool_name, call_id = %tc.id, "agent invoking tool");

                    let trace_id  = Uuid::new_v4().to_string();
                    let start     = std::time::Instant::now();
                    let tool_input = ToolInput {
                        tool_name:      tc.tool_name.clone(),
                        parameters:     tc.input.clone(),
                        session_id:     session.id.clone(),
                        correlation_id: command.id,
                    };

                    // Persist: tool invocation started
                    let _ = tool_traces.start(
                        &trace_id,
                        &session.id.to_string(),
                        &command.id.to_string(),
                        &tc.tool_name,
                        &tc.input,
                    ).await;

                    let result      = dispatcher.dispatch(tool_input).await;
                    let duration_ms = start.elapsed().as_millis() as u64;

                    let (trace_status, status_str, result_text) = match &result {
                        Ok(output) => (
                            ToolTraceStatus::Succeeded,
                            "succeeded",
                            serde_json::to_string(&output.data).unwrap_or_default(),
                        ),
                        Err(e) => (
                            ToolTraceStatus::Failed,
                            "failed",
                            format!("{{\"error\": \"{}\"}}", e),
                        ),
                    };

                    // Persist: tool invocation finished
                    let _ = tool_traces.finish(
                        &trace_id,
                        trace_status,
                        result.as_ref().ok().and_then(|o| o.data.as_ref()),
                        result.as_ref().err().map(|e| e.to_string()).as_deref(),
                        duration_ms,
                    ).await;

                    tool_call_summaries.push(ToolCallSummary {
                        tool_name:   tc.tool_name.clone(),
                        status:      status_str.to_string(),
                        duration_ms,
                    });

                    // ─── Tool/LLM Boundary ────────────────────────────────
                    // Tool results are UNTRUSTED EXTERNAL DATA.
                    // Use push_tool_result (NOT push_user) so the context
                    // manager tags them with the untrusted-data prefix.
                    // This is the key defense against prompt injection via
                    // on-chain data or RPC responses.
                    session.context.push_tool_result(
                        &tc.tool_name,
                        &tc.id,
                        result_text,
                    );
                }

                session.tool_calls += response.tool_calls.len() as u64;
                continue;
            }

            // No tool calls → final response
            let text = response
                .text
                .unwrap_or_else(|| "I completed the task.".to_string());

            // Record the final assistant response.
            session.context.push_assistant(text.clone());

            session.task_count += 1;

            return Ok(AgentResponse {
                command_id: command.id,
                text,
                data:   None,
                status: AgentResponseStatus::Completed,
                tool_calls: tool_call_summaries,
            });
        }
    }
}

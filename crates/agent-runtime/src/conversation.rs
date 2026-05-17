//! Phase 5C — strict one-turn conversational handler.
//!
//! # Purpose
//!
//! Sits between a user message and the existing tool dispatcher. Calls
//! the LLM provider exactly **once**, normalizes its tool-call output,
//! enforces the multi-tool-rejection policy, and dispatches at most
//! one tool through [`ToolDispatcher`]. Returns immediately after the
//! tool executes — there is no provider re-call, no autonomous loop,
//! no re-feed of tool output back to the LLM.
//!
//! # Why a separate handler
//!
//! [`crate::agent::Agent`] runs a ReAct loop (LLM → tool → LLM → …)
//! capped by `MAX_ITERATIONS`. That shape is appropriate for Research
//! / Risk personas that benefit from chained reasoning. It is the
//! WRONG shape for a propose-then-stop tool surface where the LLM's
//! "next thought" after an `awaiting_approval` outcome could:
//!
//!  - autonomously re-propose with different parameters,
//!  - autonomously try a `submit` / `approve` (rejected by capability
//!    gate but still a wasted RPC turn), or
//!  - autonomously feed sensitive control-plane state into its own
//!    next prompt and then leak it back through the assistant text.
//!
//! Phase 5C's handler refuses every one of those by **never calling
//! the provider a second time within a single user turn**.
//!
//! # Role separation
//!
//! The handler always sends:
//!  - **System / Developer** message: the immutable safety contract
//!    string the caller passes in. The user's message text NEVER goes
//!    here.
//!  - **User** message: the user's verbatim input as a single
//!    [`LlmMessage::text("user", ...)`].
//!
//! User content with text like "ignore previous instructions" cannot
//! reach the System slot because the handler does not even read user
//! messages into a System-role buffer. The `system: &str` parameter on
//! [`LlmClient::complete`] is the only system surface and it is owned
//! by the caller, not by the conversation history.
//!
//! # What this handler does NOT do
//!
//! - Does NOT call the provider in a loop.
//! - Does NOT feed tool output back to the provider for "summarization".
//! - Does NOT approve, retrieve signing payload, submit, broadcast, or
//!   confirm.
//! - Does NOT include `tx_bytes` / `transaction_base64` / signatures /
//!   keypair material / RPC payloads / preflight logs in any message
//!   it constructs.
//! - Does NOT use `Debug` formatting (`{:?}`, `{:#?}`) when shaping
//!   conversation history. Tool output is rendered through
//!   [`serde_json::to_value`] on the `ToolOutput` JSON DTO that
//!   Phase 5A already minimized.
//! - Does NOT panic on malformed provider output. Every parse fault
//!   maps to a typed [`ConversationOutcome`] variant with zero
//!   side effects.
//! - Does NOT mutate state outside what the dispatched tool itself
//!   writes through its existing seam.

use async_trait::async_trait;
use uuid::Uuid;

use claw_tool_system::{
    errors::ToolError,
    registry::ToolRegistry,
    ToolDispatcher,
};
use claw_types::{
    session::SessionId,
    tool::{ToolInput, ToolOutput, ToolSpec},
};

use crate::errors::AgentError;
use crate::llm::{ContentBlock, LlmClient, LlmClientRef, LlmMessage, LlmResponse, LlmToolCall};

/// Test-only and provider-agnostic LLM client that returns a
/// pre-scripted sequence of [`LlmResponse`] values. Each call to
/// [`LlmClient::complete`] consumes one response; once the sequence
/// is exhausted, further calls fail with [`AgentError::Llm`].
///
/// Used by Phase 5C's deterministic safety harness. Production
/// providers (`claw_agent_runtime::llm::openai`,
/// `claw_agent_runtime::llm::anthropic`) are NOT replaced by this; it
/// is purely a test seam that lets the conversational handler be
/// exercised without any network call or API key.
///
/// **`call_count()`** is the load-bearing signal for the one-turn
/// guarantee tests: after the handler finishes, `call_count` must be
/// exactly 1 for any single-user-turn invocation.
pub struct ScriptedLlmProvider {
    queue: std::sync::Mutex<std::collections::VecDeque<LlmResponse>>,
    /// Records every `(system, messages, tools)` triple the handler
    /// passed in, so tests can assert role separation.
    calls: std::sync::Mutex<Vec<ScriptedCall>>,
}

#[derive(Debug, Clone)]
pub struct ScriptedCall {
    pub system: String,
    pub messages: Vec<LlmMessage>,
    pub tool_names: Vec<String>,
}

impl ScriptedLlmProvider {
    pub fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            queue: std::sync::Mutex::new(responses.into_iter().collect()),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Convenience: a provider that returns a single response with no
    /// tool calls and the given assistant text. Used by Class E tests.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::new(vec![LlmResponse {
            text: Some(text.into()),
            tool_calls: Vec::new(),
            stop_reason: "end_turn".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        }])
    }

    /// Convenience: a provider that returns a single response with the
    /// given tool calls and no text.
    pub fn tool_calls(calls: Vec<LlmToolCall>) -> Self {
        Self::new(vec![LlmResponse {
            text: None,
            tool_calls: calls,
            stop_reason: "tool_use".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        }])
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    pub fn nth_call(&self, n: usize) -> Option<ScriptedCall> {
        self.calls.lock().unwrap().get(n).cloned()
    }
}

#[async_trait]
impl LlmClient for ScriptedLlmProvider {
    async fn complete(
        &self,
        system: &str,
        messages: &[LlmMessage],
        tools: &[ToolSpec],
    ) -> Result<LlmResponse, AgentError> {
        self.calls.lock().unwrap().push(ScriptedCall {
            system: system.to_string(),
            messages: messages.to_vec(),
            tool_names: tools.iter().map(|t| t.name.clone()).collect(),
        });
        match self.queue.lock().unwrap().pop_front() {
            Some(r) => Ok(r),
            None => Err(AgentError::Llm(
                "ScriptedLlmProvider exhausted: handler asked for a second turn".to_string(),
            )),
        }
    }
}

/// Possible outcomes of [`ConversationHandler::handle_one_turn`].
///
/// Every variant is a one-shot terminal — the handler never resumes
/// past returning one of these.
#[derive(Debug)]
pub enum ConversationOutcome {
    /// The provider returned no tool calls. The plain assistant text
    /// (if any) is forwarded verbatim. No state was mutated.
    AssistantText(Option<String>),
    /// The provider returned exactly one tool call and the tool ran.
    /// `output` is the minimized [`ToolOutput`] (Phase 5A shape).
    ToolDispatched {
        tool_name: String,
        output: ToolOutput,
    },
    /// The provider returned 2+ tool calls. The whole assistant turn
    /// is rejected per Phase 5C's multi-tool policy. **No tool was
    /// dispatched.**
    MultipleToolCallsRejected { count: usize },
    /// The provider returned a tool call referencing a tool that is
    /// either not in the dispatcher's registry or whose required
    /// capabilities the session does not hold.
    UnknownOrDeniedTool { tool_name: String, reason: String },
    /// The provider returned a tool call whose `input` field is
    /// structurally invalid. Dispatch was not attempted.
    MalformedToolArguments { tool_name: String, reason: String },
    /// The provider's overall response shape is outside the contract
    /// (e.g. invalid JSON in an OpenAI argument string at the parser
    /// layer). Dispatch was not attempted.
    MalformedProviderOutput { reason: String },
    /// The dispatcher rejected the tool call (validation failure,
    /// permission denied, timeout, etc). Dispatch happened but the
    /// tool refused to act; control-plane invariants enforced.
    ToolError {
        tool_name: String,
        error: ToolError,
    },
}

/// Strict one-turn conversational handler. See module docs.
pub struct ConversationHandler {
    llm: LlmClientRef,
    registry: ToolRegistry,
    /// Pre-narrowed dispatcher carrying the session's CapabilitySet.
    dispatcher: ToolDispatcher,
    /// The immutable safety/capability contract injected as the
    /// System / Developer message on every provider call. The user
    /// text NEVER mutates this string; it is built once by the
    /// caller (typically the daemon wiring or a test fixture) and
    /// passed by reference to the provider.
    system_prompt: String,
}

impl ConversationHandler {
    pub fn new(
        llm: LlmClientRef,
        registry: ToolRegistry,
        dispatcher: ToolDispatcher,
        system_prompt: String,
    ) -> Self {
        Self {
            llm,
            registry,
            dispatcher,
            system_prompt,
        }
    }

    /// Drive exactly one provider turn for `user_text` from `session_id`.
    ///
    /// Strict one-turn:
    ///   - calls `LlmClient::complete` exactly once,
    ///   - dispatches at most one tool,
    ///   - never re-calls the provider with the tool result.
    pub async fn handle_one_turn(
        &self,
        session_id: SessionId,
        user_text: String,
    ) -> ConversationOutcome {
        // ── Provider call (turn 1, the only turn) ──────────────────────────
        //
        // System prompt (capability contract) is the immutable string the
        // caller built. User text is wrapped strictly in a User-role
        // message. Tool specs come from the narrowed registry; the
        // capability gate fires inside the dispatcher.
        let user_msg = LlmMessage::text("user", user_text);
        let messages = vec![user_msg];
        let tool_specs = self.registry.all_specs();

        let response = match self
            .llm
            .complete(&self.system_prompt, &messages, &tool_specs)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ConversationOutcome::MalformedProviderOutput {
                    reason: format!("provider error: {e}"),
                };
            }
        };

        // ── Multi-tool policy ──────────────────────────────────────────────
        match response.tool_calls.len() {
            0 => return ConversationOutcome::AssistantText(response.text.clone()),
            1 => {} // proceed
            n => {
                return ConversationOutcome::MultipleToolCallsRejected { count: n };
            }
        }

        let call = &response.tool_calls[0];

        // ── Anti-panic JSON shape check ────────────────────────────────────
        //
        // The provider parsers (`openai.rs`, `anthropic.rs`) already
        // turn the wire shape into a `Value`. We additionally require
        // the input to be a JSON object — string / array / null / bool
        // would never deserialize into the tool's struct anyway, but
        // we route them to a clean `MalformedToolArguments` so the
        // outcome is typed at the conversation layer rather than
        // surfaced as a less-specific `ToolError::InvalidInput`.
        if !call.input.is_object() {
            return ConversationOutcome::MalformedToolArguments {
                tool_name: call.tool_name.clone(),
                reason: format!(
                    "tool input must be a JSON object; got {}",
                    type_label(&call.input)
                ),
            };
        }

        // ── Registry exact-match + capability gate via dispatcher ──────────
        //
        // `registry.get` returns `ToolError::NotFound` for any name not
        // exactly registered (no fuzzy matching, no normalization).
        // `dispatcher.dispatch` then runs the capability check before
        // executing. Any error is mapped to a typed outcome below.
        let tool_input = ToolInput {
            tool_name: call.tool_name.clone(),
            parameters: call.input.clone(),
            session_id,
            correlation_id: Uuid::new_v4(),
        };
        match self.dispatcher.dispatch(tool_input).await {
            Ok(output) => ConversationOutcome::ToolDispatched {
                tool_name: call.tool_name.clone(),
                output,
            },
            Err(ToolError::NotFound { name }) => ConversationOutcome::UnknownOrDeniedTool {
                tool_name: name.clone(),
                reason: "tool not found in narrowed registry".to_string(),
            },
            Err(ToolError::PermissionDenied { tool_name, .. }) => {
                ConversationOutcome::UnknownOrDeniedTool {
                    tool_name,
                    reason: "session capability set lacks required capability".to_string(),
                }
            }
            Err(other) => ConversationOutcome::ToolError {
                tool_name: call.tool_name.clone(),
                error: other,
            },
        }
        // STRICT ONE-TURN: control flow returns here. There is no
        // `loop {}`, no `while`, no auto-resubmission. Whatever the
        // dispatched tool returned, the conversation halts.
    }

    /// Phase 5C history-shaping helper. Renders an outcome's
    /// LLM-history projection — what a future turn (in a future slice)
    /// could safely include as an assistant tool_result block.
    /// **Never uses `Debug` formatting.** Always serializes the
    /// minimized `ToolOutput` DTO via `serde_json` — Phase 5A guarantees
    /// that DTO contains no key material / raw bytes / RPC payloads.
    pub fn render_history_block(outcome: &ConversationOutcome) -> Option<ContentBlock> {
        match outcome {
            ConversationOutcome::AssistantText(_) => None,
            ConversationOutcome::ToolDispatched { tool_name, output } => {
                // Serialize the ToolOutput to JSON and stringify the
                // `data` field for tool_result content. We deliberately
                // do NOT use Rust's Debug formatter for the output —
                // see module-level docs.
                let json = serde_json::to_value(output).ok()?;
                let safe = serde_json::to_string(&json).ok()?;
                Some(ContentBlock::ToolResult {
                    tool_use_id: tool_name.clone(),
                    content: safe,
                })
            }
            ConversationOutcome::MultipleToolCallsRejected { count } => {
                Some(ContentBlock::ToolResult {
                    tool_use_id: "policy".to_string(),
                    content: format!(
                        "{{\"status\":\"multiple_tool_calls_rejected\",\"count\":{count}}}"
                    ),
                    })
            }
            ConversationOutcome::UnknownOrDeniedTool { tool_name, .. } => {
                Some(ContentBlock::ToolResult {
                    tool_use_id: tool_name.clone(),
                    content: "{\"status\":\"unknown_or_denied_tool\"}".to_string(),
                    })
            }
            ConversationOutcome::MalformedToolArguments { tool_name, .. } => {
                Some(ContentBlock::ToolResult {
                    tool_use_id: tool_name.clone(),
                    content: "{\"status\":\"malformed_tool_arguments\"}".to_string(),
                    })
            }
            ConversationOutcome::MalformedProviderOutput { .. } => Some(ContentBlock::ToolResult {
                tool_use_id: "provider".to_string(),
                content: "{\"status\":\"malformed_provider_output\"}".to_string(),
            }),
            ConversationOutcome::ToolError { tool_name, error } => Some(ContentBlock::ToolResult {
                tool_use_id: tool_name.clone(),
                // `ToolError`'s Display impl is curated; we use Display
                // not Debug here to avoid leaking internal struct names.
                content: format!(
                    "{{\"status\":\"tool_error\",\"message\":{}}}",
                    serde_json::to_string(&error.to_string()).unwrap_or_else(|_| "\"\"".to_string())
                ),
            }),
        }
    }
}

fn type_label(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// Re-export the LlmClientRef alias and core types so downstream test
// crates can construct handlers without depending on the llm module
// path directly.
pub use crate::llm::{ContentBlock as LlmContentBlock, LlmClientRef as ConversationLlmClientRef};

// ── Forbidden runtime references (structural source guard) ───────────────────

#[cfg(test)]
mod source_guard_tests {
    /// Static guard: no Debug formatting in this module's
    /// history-rendering path. Phase 5C explicitly forbids leaking
    /// internal enum names via `{:?}` / `{:#?}` into LLM history.
    #[test]
    fn no_debug_formatting_in_history_path() {
        const SOURCE: &str = include_str!("conversation.rs");
        // Needles assembled at runtime so this test does not match
        // its own source.
        let needles = [
            format!("{}{}", "format!(\"{", ":?}"),
            format!("{}{}", "format!(\"{", ":#?}"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "conversation.rs must not use `{n}` for history shaping"
            );
        }
    }

    /// Static guard: no provider URL literals, no env-var reads, no
    /// SDK call sites, no client-construction patterns.
    #[test]
    fn no_provider_network_or_api_key_references() {
        const SOURCE: &str = include_str!("conversation.rs");
        let needles = [
            format!("{}{}{}", "var(\"", "OPENAI_API_K", "EY\")"),
            format!("{}{}{}", "var(\"", "ANTHROPIC_API_K", "EY\")"),
            format!("{}{}", "https://api.openai.", "com"),
            format!("{}{}", "https://api.anthropic.", "com"),
            format!("{}{}", "reqwest::Client::", "new("),
            format!("{}{}", "anthropic_sdk::", "Client"),
            format!("{}{}", "openai_sdk::", "Client"),
            format!("{}{}", "client.chat.", "completions("),
            format!("{}{}", "client.messages.", "create("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "conversation.rs must not contain provider call shape `{n}`"
            );
        }
    }

    /// Static guard: no signing / submit / broadcast / confirmation /
    /// keypair / tx-byte-exposure call sites.
    #[test]
    fn no_execution_path_references() {
        const SOURCE: &str = include_str!("conversation.rs");
        let needles = [
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "confirm_", "transaction("),
            format!("{}{}", ".get_signature_", "statuses("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "create_signing_", "handoff("),
            format!("{}{}", "Transaction::", "new_signed_with_payer("),
            format!("{}{}", "Keypair::", "new("),
            format!("{}{}", "Keypair::", "from_bytes"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "conversation.rs must not contain runtime call `{n}`"
            );
        }
    }
}

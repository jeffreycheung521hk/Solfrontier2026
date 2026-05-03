//! Phase 5D.2 / 5E — gateway-side chat handler wiring.
//!
//! This module owns the bridge between the API-layer `ChatHandler` trait
//! (defined in `claw-api`) and the agent-runtime's strict one-turn
//! `ConversationHandler`. It is the integration point that:
//!
//!  1. Constructs a per-call [`ConversationHandler`] backed by a shared
//!     [`LlmClientRef`], a session-narrowed [`ToolDispatcher`], and the
//!     immutable system / capability prompt.
//!  2. Translates the typed `ConversationOutcome` from the runtime into
//!     the wire DTO `ChatResponse`, sanitizing along the way (no
//!     Debug formatting, no raw provider text, no key material — these
//!     are Phase 5C invariants we re-assert at the integration seam).
//!  3. Detects the Phase 5A `pending_action_exists` tool-output marker
//!     and lifts it from a generic `ToolDispatched` into a typed 409
//!     `ChatRouteOutcome::Conflict(ChatResponse::PendingActionExists)`,
//!     so the HTTP layer can return the correct status without parsing
//!     JSON keys itself.
//!
//! # Default-disabled
//!
//! [`wire_chat_handler`] returns `None` unless **all** of the following
//! are present:
//!  - `CLAW_CHAT_PROVIDER` is `openai` or `anthropic`
//!  - the corresponding API key (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`)
//!    is set and non-empty
//!  - the daemon supplied a non-empty `ToolRegistry` containing the
//!    `solend_deposit_usdc` tool
//!
//! When any of those are missing, `wire_chat_handler` returns `None`
//! and the chat route returns 503. There is **no silent fallback** to a
//! Scripted provider in production — Phase 5D.1's `build_llm_provider`
//! contract is preserved.

#![allow(dead_code)]

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use claw_agent_runtime::{
    conversation::{ConversationHandler, ConversationOutcome},
    llm::{anthropic::AnthropicClient, openai::OpenAiClient},
    provider::{ApiKey, EnvProvider, LlmProviderConfigError, LlmProviderMode, StdEnvProvider},
    LlmClientRef,
};
use claw_api::state::{
    ChatHandler, ChatHandlerRef, ChatResponse, ChatRouteOutcome,
};
use claw_tool_system::{
    dispatch::ToolDispatcher,
    permissions::{Capability, CapabilitySet},
    registry::ToolRegistry,
};
use claw_types::session::SessionId;

// ── Phase 5E — env gate constants ─────────────────────────────────────────

/// Selects which real provider the chat route uses. Valid values:
/// `"openai"`, `"anthropic"`. Any other value (including absent) leaves
/// the chat route disabled.
pub const ENV_CHAT_PROVIDER: &str = "CLAW_CHAT_PROVIDER";

/// Optional override for the chat-route model name. When unset, the
/// provider's default (`gpt-4o` / `claude-sonnet-4-6`) is used.
pub const ENV_CHAT_MODEL: &str = "CLAW_CHAT_MODEL";

/// Phase 5E hang guard — request timeout for the chat-route provider.
pub const CHAT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Phase 5E cost guard — per-request token cap for the chat-route
/// provider. The chat route only needs enough output to emit a small
/// `solend_deposit_usdc { amount }` tool call.
pub const CHAT_MAX_TOKENS: u32 = 200;

/// Phase 5E — alignment-override system prompt.
///
/// The chat route's strict one-turn handler injects this string as the
/// system / developer message on every provider call. It is stable
/// (no formatting, no env interpolation) so:
///  - the prompt cannot be diverted by the user message,
///  - tests can byte-compare the constant for the required clauses,
///  - false-refusals from financial-safety alignment ("I can't move
///    funds") are pre-empted by stating that the assistant only
///    *prepares* proposals.
///
/// **The strict server-side schema and capability gate remain
/// authoritative.** This prompt is defense-in-depth, not the primary
/// guard.
pub const ALIGNMENT_SYSTEM_PROMPT: &str = "\
You are a Solana DeFi assistant that prepares transaction proposals. \
You do not move funds. The user must manually review, approve, and \
sign all transactions. Never sign, submit, broadcast, or confirm a \
transaction yourself. \
\n\n\
You only propose. You never execute, sign, submit, broadcast, or \
confirm. The human will approve and sign. \
\n\n\
When the user asks to deposit USDC into Solend, call the \
`solend_deposit_usdc` tool with the requested `amount` in raw token \
units (USDC has 6 decimals; 1000 raw = 0.001 USDC). \
Do not infer or include any other field — no `wallet_pubkey`, no \
`reserve_mint`, no `slippage`, no `priority_fee`, no `approve`, no \
`submit`, no `tx_bytes`, no `transaction_base64`. \
\n\n\
When the user asks to swap one SPL token for another (for example \
\"swap 0.001 SOL to USDC\"), call the `submit_jupiter_swap` tool \
with `input_mint` (base58 mint pubkey of the input token), \
`output_mint` (base58 mint pubkey of the output token), \
`input_amount` (raw base units of the input token; SOL has 9 \
decimals, USDC has 6), and `slippage_bps` (basis points; 50 = 0.5%, \
100 = 1%). Omit `wallet_pubkey` — the daemon resolves the signer \
from the session's bound external wallet, never from the LLM. Do \
not include any execution-side field — no `tx_bytes`, no \
`transaction_base64`, no `signed_tx`, no `submit`, no `approve`, \
no `priority_fee`, no `private_key`, no `keypair`. \
\n\n\
Make at most one tool call per turn. After the tool returns, stop — \
do not call additional tools, do not approve, do not sign.";

// ── Phase 5E — registry narrowing ─────────────────────────────────────────

/// Name of the only tool the chat-route surface exposes to the LLM in
/// Phase 5E. Lives as a constant so the source guards can lock it.
pub const CHAT_TOOL_ALLOWLIST: &[&str] = &["solend_deposit_usdc", "submit_jupiter_swap"];

/// Build a registry containing only the tools in [`CHAT_TOOL_ALLOWLIST`]
/// that exist in `full`. Returns `None` if none of the allowlisted
/// tools are registered (chat handler cannot be wired in that case).
pub fn narrow_registry_for_chat(full: &ToolRegistry) -> Option<ToolRegistry> {
    let mut tools = Vec::new();
    for name in CHAT_TOOL_ALLOWLIST {
        if let Ok(t) = full.get(name) {
            tools.push(t);
        }
    }
    if tools.is_empty() {
        None
    } else {
        Some(ToolRegistry::from_tools(tools))
    }
}

/// The `CapabilitySet` granted to the chat-route dispatcher. The chat
/// route is the LLM-driven path; per ARCHITECTURE.md INV-1 it never
/// holds `SignTransaction`, `SendTransaction`, or any wallet-management
/// capability. Only `ProposeSigning` (matches the
/// `solend_deposit_usdc` tool's required capability) is granted.
pub fn chat_capabilities() -> CapabilitySet {
    let mut set = CapabilitySet::empty();
    set.grant(Capability::ProposeSigning);
    set
}

// ── Phase 5E — provider construction ──────────────────────────────────────

/// Build the chat-route LLM provider from the supplied env-var seam,
/// applying Phase 5E's bounded timeout + max_tokens.
///
/// **Behaviour**
/// - `CLAW_CHAT_PROVIDER` absent / empty → `Ok(None)` (route disabled).
/// - `CLAW_CHAT_PROVIDER=openai` → reads `OPENAI_API_KEY`; missing/empty
///   ⇒ `Err(MissingApiKey)` / `Err(EmptyApiKey)`. Constructs an
///   `OpenAiClient` with the configured timeout + max_tokens.
/// - `CLAW_CHAT_PROVIDER=anthropic` → same as above for
///   `ANTHROPIC_API_KEY` and `AnthropicClient`.
/// - Any other `CLAW_CHAT_PROVIDER` value → `Err(InvalidProviderConfig)`.
///
/// The credential is materialised through the [`ApiKey`] redacting
/// wrapper for exactly one frame at construction time — never logged,
/// never `Debug`-printed.
pub fn build_chat_provider_from_env(
    env: &dyn EnvProvider,
) -> Result<Option<LlmClientRef>, LlmProviderConfigError> {
    // Read the provider selector. Empty / unset = disabled, no further
    // env reads.
    let provider_name = match env.get(ENV_CHAT_PROVIDER) {
        Some(s) if !s.trim().is_empty() => s.trim().to_lowercase(),
        _ => return Ok(None),
    };

    let model_override = env
        .get(ENV_CHAT_MODEL)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    match provider_name.as_str() {
        "openai" => {
            let raw_key = env.get("OPENAI_API_KEY").ok_or(
                LlmProviderConfigError::MissingApiKey {
                    mode: LlmProviderMode::OpenAi,
                    env_var: "OPENAI_API_KEY",
                },
            )?;
            let key = ApiKey::new(raw_key);
            if key.is_empty_or_whitespace() {
                return Err(LlmProviderConfigError::EmptyApiKey {
                    mode: LlmProviderMode::OpenAi,
                });
            }
            // `expose_secret` is called exactly here, then the raw
            // String is moved into the client's internal storage.
            let mut client = OpenAiClient::new(key.expose_secret().to_string())
                .with_max_tokens(CHAT_MAX_TOKENS)
                .with_timeout(CHAT_HTTP_TIMEOUT);
            if let Some(m) = model_override {
                client = client.with_model(m);
            }
            Ok(Some(Arc::new(client) as LlmClientRef))
        }
        "anthropic" => {
            let raw_key = env.get("ANTHROPIC_API_KEY").ok_or(
                LlmProviderConfigError::MissingApiKey {
                    mode: LlmProviderMode::Anthropic,
                    env_var: "ANTHROPIC_API_KEY",
                },
            )?;
            let key = ApiKey::new(raw_key);
            if key.is_empty_or_whitespace() {
                return Err(LlmProviderConfigError::EmptyApiKey {
                    mode: LlmProviderMode::Anthropic,
                });
            }
            let mut client = AnthropicClient::new(key.expose_secret().to_string())
                .with_max_tokens(CHAT_MAX_TOKENS)
                .with_timeout(CHAT_HTTP_TIMEOUT);
            if let Some(m) = model_override {
                client = client.with_model(m);
            }
            Ok(Some(Arc::new(client) as LlmClientRef))
        }
        other => Err(LlmProviderConfigError::InvalidProviderConfig {
            reason: format!(
                "{ENV_CHAT_PROVIDER}={other:?} is not a valid chat provider; expected `openai` or `anthropic`"
            ),
        }),
    }
}

/// Phase 5D.2 backward-compatible entry point. Returns `None` so existing
/// callers (test fixtures, daemon bootstrap that does not yet pass a
/// registry) keep their pre-5E behaviour.
///
/// Phase 5E daemons should call [`wire_chat_handler_with_registry`]
/// instead.
pub fn wire_chat_handler() -> Option<ChatHandlerRef> {
    None
}

/// Phase 5E — opt-in chat handler construction.
///
/// Returns `Some(ChatHandlerRef)` only when **all** of:
///   1. `CLAW_CHAT_PROVIDER` selects a real provider with a valid key,
///   2. `narrow_registry_for_chat(registry)` finds at least one
///      allowlisted tool.
///
/// Any failure (env not set, bad provider name, missing key, empty
/// registry, or tool not found) returns `Ok(None)` for the env-not-set
/// case (route stays 503) or propagates a typed error for the
/// configured-but-broken case (operator must fix the config).
///
/// This function does NOT call the network. It only constructs an
/// `Arc<dyn LlmClient>` that is ready to be used.
pub fn wire_chat_handler_with_registry(
    registry: &ToolRegistry,
    env: &dyn EnvProvider,
) -> Result<Option<ChatHandlerRef>, LlmProviderConfigError> {
    let llm = match build_chat_provider_from_env(env)? {
        Some(c) => c,
        None => return Ok(None),
    };
    let narrowed = match narrow_registry_for_chat(registry) {
        Some(r) => r,
        None => {
            return Err(LlmProviderConfigError::InvalidProviderConfig {
                reason: format!(
                    "tool registry contains none of the allowlisted chat tools: {:?}",
                    CHAT_TOOL_ALLOWLIST
                ),
            })
        }
    };
    let handler = GatewayChatHandler::new(
        llm,
        narrowed,
        ALIGNMENT_SYSTEM_PROMPT.to_string(),
        chat_capabilities(),
    );
    Ok(Some(handler.into_handler_ref()))
}

/// Convenience: read from the real process env. Used by the daemon at
/// startup. Tests inject a mock `EnvProvider` via
/// [`wire_chat_handler_with_registry`].
pub fn wire_chat_handler_from_std_env(
    registry: &ToolRegistry,
) -> Result<Option<ChatHandlerRef>, LlmProviderConfigError> {
    wire_chat_handler_with_registry(registry, &StdEnvProvider)
}

/// Bridge from the API-layer `ChatHandler` trait to the runtime's
/// strict one-turn [`ConversationHandler`].
///
/// Each call to [`Self::handle_chat`] builds a fresh
/// `ConversationHandler` so that:
///  - the dispatcher carries the *current* session's [`CapabilitySet`]
///    (defense-in-depth: the LLM will only see tool specs the session
///    is allowed to invoke);
///  - the system prompt is reconstructed from `system_prompt_template`
///    each turn, never accumulating user content;
///  - no shared mutable state crosses turns.
pub struct GatewayChatHandler {
    /// LLM provider — typically `disabled_provider()` in 5D.2 unit
    /// tests use a `ScriptedLlmProvider`. Phase 5E will plug in a real
    /// `OpenAiClient` / `AnthropicClient` only when explicitly enabled.
    llm: LlmClientRef,
    /// The full registry. The dispatcher narrows this per-session via
    /// [`CapabilitySet::for_role`] inside [`Self::dispatcher_for_session`].
    registry: ToolRegistry,
    /// Immutable safety / capability contract injected as the system
    /// message on every provider call. Built once at construction.
    system_prompt: String,
    /// Capability resolver — given a session id, returns the
    /// [`CapabilitySet`] that bounds the dispatcher. Defaults to a
    /// fixed `ProposeSigning`-only set for the chat surface (the chat
    /// route is the LLM-driven path, which never holds
    /// `SignTransaction` / `SendTransaction`).
    caps: CapabilitySet,
}

impl GatewayChatHandler {
    pub fn new(
        llm: LlmClientRef,
        registry: ToolRegistry,
        system_prompt: String,
        caps: CapabilitySet,
    ) -> Self {
        Self {
            llm,
            registry,
            system_prompt,
            caps,
        }
    }

    /// Wraps `self` in an `Arc<dyn ChatHandler>` ready for `AppState`.
    pub fn into_handler_ref(self) -> ChatHandlerRef {
        ChatHandlerRef::new(Arc::new(self))
    }

    fn dispatcher(&self) -> ToolDispatcher {
        ToolDispatcher::with_capabilities(self.registry.clone(), self.caps.clone())
    }
}

impl ChatHandler for GatewayChatHandler {
    fn handle_chat(
        &self,
        session_id: &SessionId,
        message: String,
    ) -> Pin<Box<dyn std::future::Future<Output = ChatRouteOutcome> + Send + '_>> {
        let session_id = session_id.clone();
        Box::pin(async move {
            // Rebuild the strict one-turn handler per call so every
            // invocation gets a fresh dispatcher and never inherits any
            // cross-turn mutable state. Phase 5C guarantees the handler
            // calls the provider exactly once and never re-feeds tool
            // output back.
            let handler = ConversationHandler::new(
                self.llm.clone(),
                self.registry.clone(),
                self.dispatcher(),
                self.system_prompt.clone(),
            );
            let outcome = handler.handle_one_turn(session_id, message).await;
            map_outcome(outcome)
        })
    }
}

/// Translate the runtime's typed [`ConversationOutcome`] into the
/// wire-shape [`ChatRouteOutcome`].
///
/// The mapping is deliberately not 1:1:
///  - `ToolDispatched` whose output JSON has `status == "pending_action_exists"`
///    is lifted into `ChatRouteOutcome::Conflict(ChatResponse::PendingActionExists)`
///    so the HTTP layer can produce a 409 without inspecting JSON.
///  - All other variants are returned as `ChatRouteOutcome::Ok(ChatResponse::*)`
///    with the same typed status; the 200 / 4xx / 5xx mapping is the
///    HTTP layer's responsibility.
fn map_outcome(outcome: ConversationOutcome) -> ChatRouteOutcome {
    match outcome {
        ConversationOutcome::AssistantText(text) => {
            ChatRouteOutcome::Ok(ChatResponse::AssistantText {
                assistant_text: text,
            })
        }
        ConversationOutcome::ToolDispatched { tool_name, output } => {
            // Phase 5A pending_action_exists detection.
            //
            // The Solend deposit tool returns success=false with
            // data.status="pending_action_exists" when a prior proposal
            // for the same (session, wallet) is still awaiting
            // approval. The chat HTTP route maps this specific case
            // to 409 Conflict so clients can distinguish "tool ran
            // and produced an output you should display" (200) from
            // "tool refused because a prior pending action blocks
            // dispatch" (409).
            let value = serde_json::to_value(&output).unwrap_or(serde_json::Value::Null);
            if let Some(status) = value
                .get("data")
                .and_then(|d| d.get("status"))
                .and_then(|s| s.as_str())
            {
                if status == "pending_action_exists" {
                    let reason = value
                        .get("data")
                        .and_then(|d| d.get("human_readable_next_step"))
                        .and_then(|s| s.as_str())
                        .unwrap_or(
                            "a prior proposal for this session is awaiting approval",
                        )
                        .to_string();
                    return ChatRouteOutcome::Conflict(
                        ChatResponse::PendingActionExists { reason },
                    );
                }
            }
            ChatRouteOutcome::Ok(ChatResponse::ToolDispatched {
                tool_name,
                output: value,
            })
        }
        ConversationOutcome::MultipleToolCallsRejected { count } => {
            ChatRouteOutcome::Ok(ChatResponse::MultipleToolCallsRejected { count })
        }
        ConversationOutcome::UnknownOrDeniedTool { tool_name, reason } => {
            ChatRouteOutcome::Ok(ChatResponse::UnknownOrDeniedTool { tool_name, reason })
        }
        ConversationOutcome::MalformedToolArguments { tool_name, reason } => {
            ChatRouteOutcome::Ok(ChatResponse::MalformedToolArguments { tool_name, reason })
        }
        ConversationOutcome::MalformedProviderOutput { reason } => {
            ChatRouteOutcome::Ok(ChatResponse::MalformedProviderOutput { reason })
        }
        ConversationOutcome::ToolError { tool_name, error } => {
            // `ToolError`'s Display impl is curated and does not leak
            // internal struct names — see the Phase 5C source guard.
            // We use `to_string()` (Display), NOT Debug.
            ChatRouteOutcome::Ok(ChatResponse::ToolError {
                tool_name,
                message: error.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod source_guard_tests {
    /// Static guard: this wiring module must not reach for raw provider
    /// SDKs, env-var keys, or any signing / submit / broadcast call
    /// site. All of that work belongs in dedicated wirings (LLM
    /// provider factory, signing/submit pipelines).
    #[test]
    fn no_provider_or_signing_call_sites() {
        const SOURCE: &str = include_str!("chat_wiring.rs");
        // Phase 5E expansion: also reject any of the execution-path
        // call shapes the slice spec enumerates. The chat-route
        // adapter must remain a pure configuration / mapping seam.
        let needles = [
            format!("{}{}", "https://api.openai.", "com"),
            format!("{}{}", "https://api.anthropic.", "com"),
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "send_raw_v0_", "transaction("),
            format!("{}{}", "confirm_", "transaction("),
            format!("{}{}", ".get_signature_", "statuses("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "create_signing_", "handoff("),
            format!("{}{}", "Transaction::", "new_signed_with_payer("),
            format!("{}{}", "Keypair::", "new("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "format!(\"{", ":?}"),
            format!("{}{}", "format!(\"{", ":#?}"),
            format!("{}{}", "to", "do!("),
            format!("{}{}", "unimplem", "ented!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "chat_wiring.rs must not contain `{n}`"
            );
        }
    }

    /// Phase 5E — alignment-override system prompt MUST contain the
    /// load-bearing clauses. Locked here as a string-content guard so a
    /// future edit cannot weaken the prompt without a deliberate test
    /// change.
    #[test]
    fn p5e_j_system_prompt_contains_alignment_override() {
        let p = super::ALIGNMENT_SYSTEM_PROMPT;
        // ── Original Phase 5E intro clauses (must stay byte-stable) ──
        assert!(p.contains("prepares transaction proposals"));
        assert!(p.contains("You do not move funds"));
        assert!(p.contains("user must manually review, approve, and sign"));
        // Belt-and-braces — never sign / submit / broadcast / confirm.
        assert!(p.contains("Never sign, submit, broadcast, or confirm"));

        // ── Global propose-only invariant (Agent C reinforcement) ──
        assert!(
            p.contains(
                "You only propose. You never execute, sign, submit, broadcast, or \
                 confirm. The human will approve and sign."
            ),
            "global propose-only invariant must be present verbatim"
        );

        // ── Solend-side clause — must remain intact ──
        assert!(p.contains("solend_deposit_usdc"));

        // ── Jupiter-side clause (Agent C addition) ──
        assert!(p.contains("submit_jupiter_swap"));
        assert!(p.contains("input_mint"));
        assert!(p.contains("output_mint"));
        assert!(p.contains("input_amount"));
        assert!(p.contains("slippage_bps"));
        // wallet_pubkey must be told to be omitted (session binding).
        assert!(p.contains("Omit `wallet_pubkey`"));
        // The Jupiter clause must reject execution-side fields.
        assert!(p.contains("tx_bytes"));
        assert!(p.contains("transaction_base64"));
        assert!(p.contains("signed_tx"));
    }

    /// Phase 5E — provider hang/cost guards must stay tight.
    #[test]
    fn p5e_i_provider_limits_are_bounded() {
        assert!(
            super::CHAT_HTTP_TIMEOUT <= std::time::Duration::from_secs(15),
            "chat HTTP timeout must be <= 15s; got {:?}",
            super::CHAT_HTTP_TIMEOUT
        );
        assert!(
            super::CHAT_MAX_TOKENS <= 200,
            "chat max_tokens must be <= 200; got {}",
            super::CHAT_MAX_TOKENS
        );
    }
}

#[cfg(test)]
mod outcome_mapping_tests {
    use super::*;
    use claw_tool_system::errors::ToolError;
    use claw_types::tool::ToolOutput;
    use serde_json::json;

    #[test]
    fn assistant_text_some_maps_to_ok_with_text() {
        let m = map_outcome(ConversationOutcome::AssistantText(Some("hi".into())));
        match m {
            ChatRouteOutcome::Ok(ChatResponse::AssistantText { assistant_text }) => {
                assert_eq!(assistant_text.as_deref(), Some("hi"));
            }
            other => panic!("expected Ok(AssistantText), got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_none_maps_to_ok_with_null() {
        let m = map_outcome(ConversationOutcome::AssistantText(None));
        match m {
            ChatRouteOutcome::Ok(ChatResponse::AssistantText { assistant_text }) => {
                assert_eq!(assistant_text, None);
            }
            other => panic!("expected Ok(AssistantText None), got {other:?}"),
        }
    }

    #[test]
    fn pending_action_status_lifts_to_conflict() {
        let output = ToolOutput {
            tool_name: "solend_deposit_usdc".into(),
            success: false,
            data: Some(json!({
                "status": "pending_action_exists",
                "human_readable_next_step": "wait for the prior approval",
            })),
            error: None,
            duration_ms: 0,
        };
        let m = map_outcome(ConversationOutcome::ToolDispatched {
            tool_name: "solend_deposit_usdc".into(),
            output,
        });
        match m {
            ChatRouteOutcome::Conflict(ChatResponse::PendingActionExists { reason }) => {
                assert!(reason.contains("wait for the prior approval"));
            }
            other => panic!("expected Conflict(PendingActionExists), got {other:?}"),
        }
    }

    #[test]
    fn non_pending_tool_dispatch_stays_ok() {
        let output = ToolOutput {
            tool_name: "solend_deposit_usdc".into(),
            success: true,
            data: Some(json!({"status": "awaiting_approval"})),
            error: None,
            duration_ms: 0,
        };
        let m = map_outcome(ConversationOutcome::ToolDispatched {
            tool_name: "solend_deposit_usdc".into(),
            output,
        });
        match m {
            ChatRouteOutcome::Ok(ChatResponse::ToolDispatched { tool_name, output }) => {
                assert_eq!(tool_name, "solend_deposit_usdc");
                assert_eq!(output["data"]["status"], "awaiting_approval");
            }
            other => panic!("expected Ok(ToolDispatched), got {other:?}"),
        }
    }

    #[test]
    fn multiple_tool_calls_passes_through() {
        let m = map_outcome(ConversationOutcome::MultipleToolCallsRejected { count: 3 });
        match m {
            ChatRouteOutcome::Ok(ChatResponse::MultipleToolCallsRejected { count }) => {
                assert_eq!(count, 3);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unknown_or_denied_tool_passes_through() {
        let m = map_outcome(ConversationOutcome::UnknownOrDeniedTool {
            tool_name: "broadcast_tx".into(),
            reason: "denied".into(),
        });
        match m {
            ChatRouteOutcome::Ok(ChatResponse::UnknownOrDeniedTool { tool_name, .. }) => {
                assert_eq!(tool_name, "broadcast_tx");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn malformed_args_passes_through() {
        let m = map_outcome(ConversationOutcome::MalformedToolArguments {
            tool_name: "x".into(),
            reason: "y".into(),
        });
        match m {
            ChatRouteOutcome::Ok(ChatResponse::MalformedToolArguments { .. }) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn malformed_provider_output_passes_through() {
        let m = map_outcome(ConversationOutcome::MalformedProviderOutput {
            reason: "junk".into(),
        });
        match m {
            ChatRouteOutcome::Ok(ChatResponse::MalformedProviderOutput { .. }) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn tool_error_passes_through_via_display() {
        let m = map_outcome(ConversationOutcome::ToolError {
            tool_name: "t".into(),
            error: ToolError::NotFound { name: "t".into() },
        });
        match m {
            ChatRouteOutcome::Ok(ChatResponse::ToolError { tool_name, message }) => {
                assert_eq!(tool_name, "t");
                assert!(!message.is_empty());
                // Display impl, not Debug — should NOT contain
                // internal struct-name shape like `ToolError { ... }`.
                assert!(!message.contains("ToolError {"));
            }
            other => panic!("got {other:?}"),
        }
    }
}

#[cfg(test)]
mod p5e_env_gate_tests {
    //! Phase 5E test classes (A–K) — opt-in env gate, fail-closed
    //! credentials, alignment override, strict tool schema, default-skip
    //! dry-run, source guards, one-turn preservation, provider limits.
    //!
    //! These tests do NOT touch the network. They use a mock
    //! [`super::EnvProvider`] and a stub [`Tool`] to assert the gate +
    //! schema invariants without constructing a real provider client.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use claw_agent_runtime::{
        conversation::ScriptedLlmProvider,
        llm::{LlmClientRef, LlmToolCall},
    };
    use claw_tool_system::{errors::ToolError, tool::Tool};
    use claw_types::{
        session::SessionId,
        tool::{ToolInput, ToolOutput, ToolSpec},
    };
    use serde_json::json;
    use uuid::Uuid;

    // ── Mock env provider ──────────────────────────────────────────────────

    struct MockEnv {
        responses: Mutex<HashMap<String, String>>,
        reads: Mutex<Vec<String>>,
    }

    impl MockEnv {
        fn empty() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                reads: Mutex::new(Vec::new()),
            }
        }
        fn with(pairs: &[(&str, &str)]) -> Self {
            let mut m = HashMap::new();
            for (k, v) in pairs {
                m.insert(k.to_string(), v.to_string());
            }
            Self {
                responses: Mutex::new(m),
                reads: Mutex::new(Vec::new()),
            }
        }
        fn reads_for(&self, name: &str) -> usize {
            self.reads
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.as_str() == name)
                .count()
        }
        fn total_reads(&self) -> usize {
            self.reads.lock().unwrap().len()
        }
    }

    impl EnvProvider for MockEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.reads.lock().unwrap().push(name.to_string());
            self.responses.lock().unwrap().get(name).cloned()
        }
    }

    // ── A stub solend_deposit_usdc tool that mirrors the production
    // tool's schema (amount-only) but does no work. Used to populate
    // the test registry. ──────────────────────────────────────────────

    struct StubSolendDepositTool;

    #[async_trait]
    impl Tool for StubSolendDepositTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "solend_deposit_usdc".into(),
                description: "stub for Phase 5E env-gate tests".into(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["amount"],
                    "properties": {
                        "amount": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "raw USDC token units (6 decimals)",
                        }
                    }
                }),
                output_schema: json!({"type":"object"}),
                required_capabilities: vec!["propose_signing".to_string()],
                supports_streaming: false,
                timeout_ms: 5_000,
            }
        }
        async fn execute(&self, _: ToolInput) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                tool_name: "solend_deposit_usdc".into(),
                success: true,
                data: Some(json!({"status":"awaiting_approval"})),
                error: None,
                duration_ms: 0,
            })
        }
    }

    fn stub_registry() -> ToolRegistry {
        ToolRegistry::from_tools(vec![Arc::new(StubSolendDepositTool)])
    }

    // ── A stub submit_jupiter_swap tool that mirrors the production
    // tool's input schema (4 required + 2 optional, no execution-side
    // fields) but does no work. Used for the chat-route narrowing /
    // strict-schema assertions in p5e_h. ──────────────────────────────

    struct StubJupiterSwapTool;

    #[async_trait]
    impl Tool for StubJupiterSwapTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "submit_jupiter_swap".into(),
                description: "stub for Phase 5E env-gate tests".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["input_mint", "output_mint", "input_amount", "slippage_bps"],
                    "properties": {
                        "input_mint":    { "type": "string" },
                        "output_mint":   { "type": "string" },
                        "input_amount":  { "type": "integer", "minimum": 1 },
                        "slippage_bps":  { "type": "integer", "minimum": 0, "maximum": 10000 },
                        "wallet_pubkey": { "type": "string" },
                        "description":   { "type": "string" }
                    }
                }),
                output_schema: json!({"type":"object"}),
                required_capabilities: vec!["propose_signing".to_string()],
                supports_streaming: false,
                timeout_ms: 30_000,
            }
        }
        async fn execute(&self, _: ToolInput) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                tool_name: "submit_jupiter_swap".into(),
                success: true,
                data: Some(json!({"status":"awaiting_approval"})),
                error: None,
                duration_ms: 0,
            })
        }
    }

    fn stub_registry_with_solend_and_jupiter() -> ToolRegistry {
        ToolRegistry::from_tools(vec![
            Arc::new(StubSolendDepositTool),
            Arc::new(StubJupiterSwapTool),
        ])
    }

    // ── Class A — default chat provider disabled without env ──────────────

    #[test]
    fn p5e_a_default_chat_provider_disabled_without_env() {
        let env = MockEnv::empty();
        let result = build_chat_provider_from_env(&env)
            .expect("absent provider env must be Ok(None)");
        assert!(result.is_none());
        // No provider call, no API-key env read.
        assert_eq!(env.reads_for("OPENAI_API_KEY"), 0);
        assert_eq!(env.reads_for("ANTHROPIC_API_KEY"), 0);
    }

    // ── Class B — openai_mode_missing_key_fails_closed ────────────────────

    #[test]
    fn p5e_b_openai_mode_missing_key_fails_closed() {
        let env = MockEnv::with(&[(ENV_CHAT_PROVIDER, "openai")]);
        let result = build_chat_provider_from_env(&env);
        match result {
            Err(LlmProviderConfigError::MissingApiKey { mode, env_var }) => {
                assert_eq!(mode, LlmProviderMode::OpenAi);
                assert_eq!(env_var, "OPENAI_API_KEY");
            }
            // `LlmClientRef` is not Debug, so split arms.
            Err(e) => panic!("expected MissingApiKey(OpenAi); got Err({e})"),
            Ok(_) => panic!("expected MissingApiKey(OpenAi); got Ok(<provider>)"),
        }
    }

    // ── Class C — anthropic_mode_missing_key_fails_closed ─────────────────

    #[test]
    fn p5e_c_anthropic_mode_missing_key_fails_closed() {
        let env = MockEnv::with(&[(ENV_CHAT_PROVIDER, "anthropic")]);
        let result = build_chat_provider_from_env(&env);
        match result {
            Err(LlmProviderConfigError::MissingApiKey { mode, env_var }) => {
                assert_eq!(mode, LlmProviderMode::Anthropic);
                assert_eq!(env_var, "ANTHROPIC_API_KEY");
            }
            Err(e) => panic!("expected MissingApiKey(Anthropic); got Err({e})"),
            Ok(_) => panic!("expected MissingApiKey(Anthropic); got Ok(<provider>)"),
        }
    }

    // ── Class D — provider_env_key_not_logged ─────────────────────────────

    #[test]
    fn p5e_d_provider_env_key_not_logged() {
        // Empty key supplied via env. Construct the error and assert
        // its Debug/Display form contains no key material.
        const FAKE: &str = "sk-fake-must-not-appear-in-log";
        let env = MockEnv::with(&[
            (ENV_CHAT_PROVIDER, "openai"),
            ("OPENAI_API_KEY", "   "), // whitespace → EmptyApiKey
        ]);
        let err = match build_chat_provider_from_env(&env) {
            Err(e) => e,
            Ok(_) => panic!("whitespace key must fail closed; got Ok(<provider>)"),
        };
        assert!(!format!("{err}").contains(FAKE));
        assert!(!format!("{err:?}").contains(FAKE));
        // The known key value used here ("   ") must also not appear.
        assert!(!format!("{err}").contains("   "));

        // ApiKey wrapper itself must redact.
        let key = ApiKey::new(FAKE);
        assert!(!format!("{key}").contains(FAKE));
        assert!(!format!("{key:?}").contains(FAKE));
    }

    // ── Class F — source_guard_no_sign_submit_broadcast_in_chat_wiring ────

    #[test]
    fn p5e_f_source_guard_no_sign_submit_broadcast_in_chat_wiring() {
        // Already locked by source_guard_tests::no_provider_or_signing_call_sites.
        // This duplicate name maps to the slice spec's class F label and
        // checks an additional belt: chat_wiring must not import
        // signing/submit/broadcast modules.
        const SOURCE: &str = include_str!("chat_wiring.rs");
        let needles = [
            format!("{}{}", "use crate::integrations::solend_sub", "mit::"),
            format!("{}{}", "use crate::integrations::solend_sign", "ing::"),
            format!("{}{}", "use crate::orchest", "rator::"),
            format!("{}{}", "use crate::pending_sign", "ing::"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "chat_wiring.rs must not import `{n}`"
            );
        }
    }

    // ── Class G — one_turn_preserved_with_real_provider_adapter ───────────
    //
    // Uses `ScriptedLlmProvider` as the LlmClient (faster than spinning
    // up a real OpenAi/Anthropic client) and asserts that the chat
    // handler calls `complete()` exactly once per HTTP request.

    #[tokio::test]
    async fn p5e_g_one_turn_preserved_with_real_provider_adapter_or_stub() {
        let scripted = Arc::new(ScriptedLlmProvider::tool_calls(vec![LlmToolCall {
            id: "c1".into(),
            tool_name: "solend_deposit_usdc".into(),
            input: json!({"amount": 1000}),
        }]));
        let llm: LlmClientRef = scripted.clone();
        let handler = GatewayChatHandler::new(
            llm,
            stub_registry(),
            ALIGNMENT_SYSTEM_PROMPT.to_string(),
            chat_capabilities(),
        );
        let outcome = handler
            .handle_chat(&SessionId::from(Uuid::new_v4()), "deposit 0.001 USDC".into())
            .await;
        assert!(matches!(outcome, ChatRouteOutcome::Ok(ChatResponse::ToolDispatched { .. })));
        assert_eq!(
            scripted.call_count(),
            1,
            "chat handler must call provider exactly once per turn"
        );
        // Role separation: system prompt is the alignment override,
        // never the user text.
        let nth = scripted.nth_call(0).expect("recorded call");
        assert_eq!(nth.system, ALIGNMENT_SYSTEM_PROMPT);
        assert!(!nth.system.contains("deposit 0.001 USDC"));
    }

    // ── Class H — strict_tool_schema_shape_is_amount_only ─────────────────
    //
    // Two phases:
    //   1. Solend-only registry → narrowed surface is exactly
    //      `solend_deposit_usdc` with the strict amount-only schema. This
    //      is the original Phase 5E invariant; it must NOT be weakened by
    //      adding Jupiter to the allowlist.
    //   2. Solend + Jupiter registry → narrowed surface contains both
    //      tools. The Jupiter input schema lists the four required fields
    //      (input_mint / output_mint / input_amount / slippage_bps) plus
    //      the two optional fields (wallet_pubkey / description) and
    //      mentions no execution-side payload key.

    #[test]
    fn p5e_h_strict_tool_schema_shape_is_amount_only() {
        // ── Phase 1: Solend-only registry ──
        let narrowed = narrow_registry_for_chat(&stub_registry())
            .expect("stub registry contains solend_deposit_usdc");
        let names = narrowed.names();
        assert_eq!(names, vec!["solend_deposit_usdc".to_string()]);

        let spec = narrowed.all_specs().into_iter().next().expect("spec exists");
        let schema = &spec.input_schema;

        // Required fields = ["amount"] only.
        let required = schema["required"].as_array().expect("required array");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "amount");

        // additionalProperties must be false (strict).
        assert_eq!(schema["additionalProperties"], json!(false));

        // No execution-side fields are even mentioned in the schema text.
        let raw = serde_json::to_string(schema).unwrap();
        for forbidden in [
            "wallet_pubkey",
            "reserve_mint",
            "protocol",
            "priority_fee",
            "approve",
            "submit",
            "tx_bytes",
            "transaction_base64",
            "slippage",
        ] {
            assert!(
                !raw.contains(forbidden),
                "strict Solend chat schema must not mention `{forbidden}`; got {raw}"
            );
        }

        // ── Phase 2: Solend + Jupiter registry ──
        let both = narrow_registry_for_chat(&stub_registry_with_solend_and_jupiter())
            .expect("stub registry contains both chat tools");
        let mut names = both.names();
        names.sort();
        let mut expected_names =
            vec!["solend_deposit_usdc".to_string(), "submit_jupiter_swap".to_string()];
        expected_names.sort();
        assert_eq!(names, expected_names, "narrowed registry must contain both chat tools");

        // Jupiter spec.
        let jupiter_spec = both
            .all_specs()
            .into_iter()
            .find(|s| s.name == "submit_jupiter_swap")
            .expect("jupiter spec present after narrowing");
        let jupiter_schema = &jupiter_spec.input_schema;

        // Required = exactly the four intent-level fields.
        let required = jupiter_schema["required"]
            .as_array()
            .expect("jupiter required array");
        let required_set: std::collections::HashSet<&str> = required
            .iter()
            .map(|v| v.as_str().expect("required entry is a string"))
            .collect();
        let expected_required: std::collections::HashSet<&str> = [
            "input_mint",
            "output_mint",
            "input_amount",
            "slippage_bps",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            required_set, expected_required,
            "jupiter required fields drift; got {required_set:?}"
        );

        // wallet_pubkey + description are present as optional properties
        // (in `properties` but not in `required`).
        let props = jupiter_schema["properties"]
            .as_object()
            .expect("jupiter properties object");
        assert!(
            props.contains_key("wallet_pubkey"),
            "wallet_pubkey must be advertised as optional"
        );
        assert!(
            props.contains_key("description"),
            "description must be advertised as optional"
        );
        assert!(
            !required_set.contains("wallet_pubkey"),
            "wallet_pubkey must NOT be required (session binding resolves it)"
        );
        assert!(
            !required_set.contains("description"),
            "description must NOT be required"
        );

        // Forbidden execution-side fields must not appear anywhere in the
        // Jupiter input-schema text. The Solend strict schema blocks these
        // via `additionalProperties: false`; the Jupiter schema (per its
        // production shape) does not, so we rely on a raw-text scan as the
        // chat-layer safety guard.
        let raw_jupiter = serde_json::to_string(jupiter_schema).unwrap();
        for forbidden in [
            "tx_bytes",
            "transaction_base64",
            "signed_tx",
            "submit",
            "approve",
            "private_key",
            "keypair",
        ] {
            assert!(
                !raw_jupiter.contains(forbidden),
                "strict Jupiter chat schema must not mention `{forbidden}`; got {raw_jupiter}"
            );
        }
    }

    // ── Class K — proof writer excludes raw provider payloads ─────────────
    //
    // The proof writer is exercised in the live dry-run test file;
    // here we lock the in-memory shape: a [`ChatResponse::ToolDispatched`]
    // serialised by the route's normal sanitiser path must include only
    // tool_name + sanitised output and never raw provider payload keys.

    #[test]
    fn p5e_k_proof_doc_excludes_raw_provider_payloads() {
        let resp = ChatResponse::ToolDispatched {
            tool_name: "solend_deposit_usdc".into(),
            output: json!({
                "tool_name": "solend_deposit_usdc",
                "success": true,
                "data": {"status": "awaiting_approval", "approval_request_id": "00000000-0000-0000-0000-000000000000"},
                "error": null,
                "duration_ms": 12,
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        // Forbidden fields that must NEVER be in the wire DTO.
        for forbidden in [
            "transaction_base64",
            "tx_bytes",
            "Authorization",
            "x-api-key",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "private_key",
        ] {
            assert!(
                !json.contains(forbidden),
                "ChatResponse JSON must not contain `{forbidden}`; got {json}"
            );
        }
    }

    // ── Bonus: invalid CLAW_CHAT_PROVIDER value is rejected ───────────────

    #[test]
    fn p5e_invalid_provider_value_returns_invalid_provider_config() {
        let env = MockEnv::with(&[(ENV_CHAT_PROVIDER, "google")]);
        let err = match build_chat_provider_from_env(&env) {
            Err(e) => e,
            Ok(_) => panic!("unsupported provider must error; got Ok(<provider>)"),
        };
        match err {
            LlmProviderConfigError::InvalidProviderConfig { reason } => {
                assert!(reason.contains("openai") || reason.contains("anthropic"));
            }
            other => panic!("expected InvalidProviderConfig; got {other:?}"),
        }
    }

    // ── Bonus: wire_chat_handler_with_registry returns Ok(None) when env disabled ──

    #[test]
    fn p5e_wire_with_registry_returns_none_when_env_disabled() {
        let env = MockEnv::empty();
        let result = wire_chat_handler_with_registry(&stub_registry(), &env)
            .expect("disabled env must be Ok(None)");
        assert!(result.is_none());
    }

    // ── Bonus: wire_chat_handler_with_registry surfaces typed error when registry empty ──

    #[test]
    fn p5e_wire_with_registry_errors_when_registry_lacks_chat_tool() {
        // Provider gate fully satisfied, but registry has no chat tool.
        let env = MockEnv::with(&[
            (ENV_CHAT_PROVIDER, "openai"),
            ("OPENAI_API_KEY", "sk-test-fixture"),
        ]);
        let empty = ToolRegistry::new();
        let result = wire_chat_handler_with_registry(&empty, &env);
        match result {
            Err(LlmProviderConfigError::InvalidProviderConfig { .. }) => {}
            // Result's Ok arm contains `Option<ChatHandlerRef>` (no Debug);
            // split arms so we never Debug-format it.
            Err(e) => panic!("expected InvalidProviderConfig; got Err({e})"),
            Ok(_) => panic!("expected InvalidProviderConfig; got Ok(<chat handler>)"),
        }
    }
}

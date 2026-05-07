//! Phase 6H-B — AI tool-result explainer.
//!
//! Takes the raw structured output of a read-only ClawSol tool (e.g.
//! `get_solend_position`) and produces a human-readable, audit-grade
//! explanation suitable for a demo UI. The tool output remains the
//! sole source of truth — the explainer may summarise, format, and
//! contextualise but MUST NOT alter, estimate, or invent values.
//!
//! # Scope (Phase 6H-B)
//!
//! - Supported tool: `get_solend_position` only. Other tools resolve
//!   to [`ExplainerOutcome::UnsupportedTool`].
//! - Pure read-only: the module imports nothing related to signing,
//!   submit, broadcast, approval, or wallet management. A source-guard
//!   test in this file enforces that.
//! - The explainer is a post-processing layer — it never invokes
//!   tools, never asks the LLM for a follow-up tool call, and never
//!   mutates state. The LLM call configures `tools = &[]` so the
//!   provider has no surface to call.
//! - Wiring into the chat-route response is intentionally deferred to
//!   a follow-up slice (6H-C). See the module-level comment "Wiring
//!   deferred — rationale" below.
//!
//! # Wiring deferred — rationale
//!
//! `claw_api::state::ChatResponse::ToolDispatched` is a struct-style
//! enum variant destructured by ~30 existing test sites and one HTTP
//! mapper. Adding an `explanation: Option<...>` field to that variant
//! breaks every destructure that does not use a `..` rest-pattern,
//! turning a small wire change into a wide test edit. To keep this
//! slice tight and the abstraction landable on its own, the explainer
//! module ships with full unit-test coverage and a stable public API.
//! The follow-up slice (6H-C) will (1) extend `ChatResponse::ToolDispatched`
//! with the optional field, (2) update the existing destructures, and
//! (3) inject an [`Arc<dyn ToolOutputExplainer>`] into
//! `GatewayChatHandler` so the chat HTTP response carries the
//! explanation when the dispatched tool is `get_solend_position`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use claw_agent_runtime::errors::AgentError;
use claw_agent_runtime::llm::{LlmClient, LlmMessage};
use claw_agent_runtime::LlmClientRef;
use claw_types::tool::ToolSpec;

use crate::tools::get_solend_position::TOOL_NAME as GET_SOLEND_POSITION_TOOL_NAME;

// ── Safety prompt ──────────────────────────────────────────────────────────

/// Phase 6H-B explainer system prompt.
///
/// Encodes the ten non-negotiable safety constraints from the slice
/// spec verbatim. Locked here as a public constant so the
/// `system_prompt_contains_all_safety_clauses` test can byte-compare
/// for the load-bearing phrases. A future edit cannot weaken the
/// prompt without a deliberate test change.
pub const EXPLAINER_SYSTEM_PROMPT: &str = "\
You are ClawSol's defensive DeFi explainer. You receive raw JSON \
output from a read-only on-chain tool and produce a structured, \
audit-friendly explanation for the UI. The tool output is the SOLE \
source of truth. You may summarise and format it. You MUST NEVER \
alter, estimate, or invent any value.\n\
\n\
Hard rules:\n\
1. Do not change any numbers, pubkeys, counts, reserve IDs, wallet \
IDs, or status strings. Echo the verbatim values where useful.\n\
2. Do not estimate supplied USDC if `supplied_usdc_estimate_ui` and \
`supplied_usdc_estimate_raw` are null. Treat the value as not \
currently determinable.\n\
3. Do not say \"funds are safe\" as an absolute guarantee. Use \
phrasing such as \"the on-chain owner field of the obligation \
matches the user's wallet, which is consistent with user-owned \
custody\".\n\
4. You may say \"the obligation owner field is the user's wallet\" \
and \"this indicates the position is user-owned on-chain\".\n\
5. When `estimate_unavailable_reason` is present and non-null, you \
MUST mention the uncertainty in `plain_english_summary` AND add an \
entry to `cannot_claim`.\n\
6. You must NOT recommend borrow or repay. Do not suggest borrowing \
strategies or repayment timing.\n\
7. You must NOT say a withdrawal has happened or is in progress.\n\
8. You may list these next safe actions only:\n\
   - \"View the raw JSON for verification.\"\n\
   - \"Render this as a position card.\"\n\
   - \"Prepare a withdraw proposal only after the user explicitly asks.\"\n\
9. If withdrawal is mentioned, state that withdrawal would still \
require the user's Phantom signature.\n\
10. Output ONLY a single JSON object with the exact shape below; no \
markdown fences, no preamble, no trailing commentary:\n\
{\n\
  \"headline\": \"<short title>\",\n\
  \"plain_english_summary\": \"<2-4 sentences>\",\n\
  \"key_facts\": [\"<fact>\"],\n\
  \"risk_notes\": [\"<note>\"],\n\
  \"next_safe_actions\": [\"<action>\"],\n\
  \"cannot_claim\": [\"<unavailable claim>\"]\n\
}\n\
\n\
Discovery vs custody: if `dashboard_visibility_note` is present in the \
tool output, mention in `plain_english_summary` that the visibility \
issue is a discovery / indexing concern, NOT a custody transfer.\n\
\n\
Tone: concise, non-hype, audit-friendly. Do not editorialise.";

// ── Public DTOs ────────────────────────────────────────────────────────────

/// Structured explanation produced by [`ToolOutputExplainer::explain`].
///
/// Every field is plain text. The shape is the wire shape an HTTP
/// client (frontend, audit log, demo card) will see when the wiring
/// slice attaches it to `ChatResponse::ToolDispatched`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplanationDto {
    pub headline: String,
    pub plain_english_summary: String,
    #[serde(default)]
    pub key_facts: Vec<String>,
    #[serde(default)]
    pub risk_notes: Vec<String>,
    #[serde(default)]
    pub next_safe_actions: Vec<String>,
    #[serde(default)]
    pub cannot_claim: Vec<String>,
}

/// Outcome of an explainer call. Each variant maps to a stable status
/// string the wiring slice will surface to the HTTP client.
#[derive(Debug, Clone)]
pub enum ExplainerOutcome {
    /// Explanation generated successfully and passed all scrub checks.
    Ok(ExplanationDto),
    /// Tool name is not in this slice's supported set. Caller should
    /// surface the raw tool output without an explanation.
    UnsupportedTool,
    /// No explainer is configured (env gate disabled, no LLM client).
    /// The tool output still flows; the explanation is simply absent.
    ExplainerUnavailable,
    /// The tool output JSON did not match the supported shape. Caller
    /// should still surface the raw tool output.
    InvalidToolOutput { reason: String },
    /// LLM call failed, returned no text, or returned a body that
    /// could not be safely round-tripped (parse failure, dropped
    /// wallet pubkey, etc.). Caller MUST still surface the raw tool
    /// output — explanation is best-effort.
    ProviderError { reason: String },
}

impl ExplainerOutcome {
    /// Stable wire-shape status string, suitable for an outer
    /// `{ "explanation_status": ... }` envelope when the wiring slice
    /// lands.
    pub fn status_label(&self) -> &'static str {
        match self {
            ExplainerOutcome::Ok(_) => "ok",
            ExplainerOutcome::UnsupportedTool => "unsupported_tool",
            ExplainerOutcome::ExplainerUnavailable => "explainer_unavailable",
            ExplainerOutcome::InvalidToolOutput { .. } => "invalid_tool_output",
            ExplainerOutcome::ProviderError { .. } => "provider_error",
        }
    }
}

// ── Trait + impls ──────────────────────────────────────────────────────────

/// Post-processing seam: convert raw tool output into a structured
/// explanation. The trait is the abstraction the chat-handler wiring
/// slice will inject into [`crate::runtime::chat_wiring::GatewayChatHandler`].
#[async_trait]
pub trait ToolOutputExplainer: Send + Sync {
    /// Produce an [`ExplainerOutcome`] for the given tool name + raw
    /// tool output JSON. Implementations MUST NOT call any tool, MUST
    /// NOT initiate any transaction, and MUST NOT block on user input.
    async fn explain(&self, tool_name: &str, tool_output: &Value) -> ExplainerOutcome;
}

/// "Disabled" implementation — returns
/// [`ExplainerOutcome::ExplainerUnavailable`] for every call. Used when
/// `CLAW_CHAT_PROVIDER` is unset (or the env gate otherwise refuses an
/// LLM client) so the chat path remains operational without an
/// explanation.
pub struct DisabledExplainer;

#[async_trait]
impl ToolOutputExplainer for DisabledExplainer {
    async fn explain(&self, _tool_name: &str, _tool_output: &Value) -> ExplainerOutcome {
        ExplainerOutcome::ExplainerUnavailable
    }
}

/// Production explainer backed by an [`LlmClientRef`].
///
/// The LLM is called with `tools = &[]`, so the provider has no
/// surface to request a follow-up tool call. The response text is
/// parsed strictly as JSON matching [`ExplanationDto`]; non-conforming
/// responses fail closed with `ProviderError` and the caller falls
/// back to the raw tool output.
pub struct LlmToolOutputExplainer {
    llm: LlmClientRef,
}

impl LlmToolOutputExplainer {
    pub fn new(llm: LlmClientRef) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl ToolOutputExplainer for LlmToolOutputExplainer {
    async fn explain(&self, tool_name: &str, tool_output: &Value) -> ExplainerOutcome {
        // 1. Tool support gate.
        if tool_name != GET_SOLEND_POSITION_TOOL_NAME {
            return ExplainerOutcome::UnsupportedTool;
        }

        // 2. Pre-LLM validation. We refuse to ask the LLM about an
        //    output that does not match the supported shape, both to
        //    save provider tokens and to keep the prompt's "facts"
        //    deterministic.
        let inputs = match validate_solend_output(tool_output) {
            Ok(v) => v,
            Err(reason) => return ExplainerOutcome::InvalidToolOutput { reason },
        };

        // 3. Build the user message. The system prompt is the
        //    immutable [`EXPLAINER_SYSTEM_PROMPT`] constant.
        let user_prompt = build_user_prompt(tool_output);
        let messages = vec![LlmMessage::text("user", user_prompt)];

        // 4. Call the LLM. `tools = &[]` denies any tool-use surface.
        let response = match self
            .llm
            .complete(EXPLAINER_SYSTEM_PROMPT, &messages, EMPTY_TOOLS)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ExplainerOutcome::ProviderError {
                    reason: format!("provider call failed: {e}"),
                }
            }
        };

        // 5. Defence — refuse any tool_calls in the response. The
        //    explainer must never trigger a follow-up dispatch.
        if !response.tool_calls.is_empty() {
            return ExplainerOutcome::ProviderError {
                reason: "explainer model emitted tool_calls; refused".into(),
            };
        }

        let text = match response.text.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                return ExplainerOutcome::ProviderError {
                    reason: "explainer returned no text".into(),
                }
            }
        };

        // 6. Parse strict JSON.
        let dto: ExplanationDto = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(e) => {
                return ExplainerOutcome::ProviderError {
                    reason: format!("explainer JSON parse failed: {e}"),
                }
            }
        };

        // 7. Scrub — load-bearing facts must round-trip. The wallet
        //    pubkey from the tool output must appear in the explanation
        //    text; if it does not, the LLM dropped the most important
        //    custody-evidence fact and we treat the explanation as
        //    untrusted. When the tool output has at least one deposit
        //    with `estimate_unavailable_reason`, `cannot_claim` must
        //    not be empty — this is the load-bearing acknowledgment
        //    that a supplied USDC value was NOT provided.
        if let Some(wallet) = inputs.wallet_pubkey.as_deref() {
            if !dto_mentions(&dto, wallet) {
                return ExplainerOutcome::ProviderError {
                    reason: "explanation does not preserve wallet_pubkey".into(),
                };
            }
        }
        if inputs.deposits_with_estimate_unavailable > 0 && dto.cannot_claim.is_empty() {
            return ExplainerOutcome::ProviderError {
                reason:
                    "explanation must list the unavailable supplied USDC estimate in cannot_claim"
                        .into(),
            };
        }

        ExplainerOutcome::Ok(dto)
    }
}

/// Empty tool-spec slice. Constructed once so the call site cannot
/// accidentally pass a non-empty list.
const EMPTY_TOOLS: &[ToolSpec] = &[];

// ── Internal helpers ───────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ToolOutputInputs {
    /// Echo of the wallet pubkey from the raw tool output, used by the
    /// post-LLM scrubber to verify the explanation preserved it.
    wallet_pubkey: Option<String>,
    /// Number of deposit entries whose `estimate_unavailable_reason`
    /// field is non-null. Used to enforce the cannot_claim acknowledgment
    /// rule (slice rule 5).
    deposits_with_estimate_unavailable: usize,
}

fn validate_solend_output(value: &Value) -> Result<ToolOutputInputs, String> {
    let status = value
        .get("status")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "tool output is missing the `status` field".to_string())?;
    let wallet_pubkey = value
        .get("wallet_pubkey")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    match status {
        "ok" => {
            let deposits_with_estimate_unavailable = value
                .get("deposits")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|d| {
                            d.get("estimate_unavailable_reason")
                                .map(|v| !v.is_null())
                                .unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0);
            Ok(ToolOutputInputs {
                wallet_pubkey,
                deposits_with_estimate_unavailable,
            })
        }
        "no_position" => Ok(ToolOutputInputs {
            wallet_pubkey,
            deposits_with_estimate_unavailable: 0,
        }),
        other => Err(format!(
            "unsupported status `{other}` for explainer (expected `ok` or `no_position`)"
        )),
    }
}

fn build_user_prompt(tool_output: &Value) -> String {
    let pretty = serde_json::to_string_pretty(tool_output)
        .unwrap_or_else(|_| tool_output.to_string());
    format!(
        "Tool: get_solend_position\n\n\
         Raw tool output (the SOURCE OF TRUTH — do not alter any \
         numbers, pubkeys, counts, reserve IDs, or wallet IDs):\n\n\
         ```json\n{pretty}\n```\n\n\
         Produce ONLY the JSON object described in the system prompt. \
         No markdown fences around it, no commentary outside it."
    )
}

fn dto_mentions(dto: &ExplanationDto, needle: &str) -> bool {
    let appears = |s: &str| s.contains(needle);
    appears(&dto.headline)
        || appears(&dto.plain_english_summary)
        || dto.key_facts.iter().any(|s| appears(s))
        || dto.risk_notes.iter().any(|s| appears(s))
        || dto.next_safe_actions.iter().any(|s| appears(s))
        || dto.cannot_claim.iter().any(|s| appears(s))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use claw_agent_runtime::llm::LlmResponse;
    use serde_json::json;

    // ── Fake LLM ────────────────────────────────────────────────────────

    /// Configurable fake LLM client. Captures the system + user content
    /// the explainer sent so tests can assert prompt construction. The
    /// response is a fixed text or a fixed error.
    struct FakeLlm {
        response: FakeResponse,
        captured_system: Mutex<Option<String>>,
        captured_user: Mutex<Option<String>>,
        captured_tools_len: Mutex<Option<usize>>,
        call_count: Mutex<usize>,
    }

    enum FakeResponse {
        Text(String),
        EmptyText,
        Error(String),
        WithToolCalls { text: String },
    }

    impl FakeLlm {
        fn returning_text(text: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                response: FakeResponse::Text(text.into()),
                captured_system: Mutex::new(None),
                captured_user: Mutex::new(None),
                captured_tools_len: Mutex::new(None),
                call_count: Mutex::new(0),
            })
        }
        fn returning_empty() -> Arc<Self> {
            Arc::new(Self {
                response: FakeResponse::EmptyText,
                captured_system: Mutex::new(None),
                captured_user: Mutex::new(None),
                captured_tools_len: Mutex::new(None),
                call_count: Mutex::new(0),
            })
        }
        fn returning_error(reason: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                response: FakeResponse::Error(reason.into()),
                captured_system: Mutex::new(None),
                captured_user: Mutex::new(None),
                captured_tools_len: Mutex::new(None),
                call_count: Mutex::new(0),
            })
        }
        fn returning_text_with_tool_calls(text: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                response: FakeResponse::WithToolCalls {
                    text: text.into(),
                },
                captured_system: Mutex::new(None),
                captured_user: Mutex::new(None),
                captured_tools_len: Mutex::new(None),
                call_count: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl LlmClient for FakeLlm {
        async fn complete(
            &self,
            system: &str,
            messages: &[LlmMessage],
            tools: &[ToolSpec],
        ) -> Result<LlmResponse, AgentError> {
            *self.captured_system.lock().unwrap() = Some(system.to_string());
            *self.captured_user.lock().unwrap() =
                messages.first().map(|m| m.content_text());
            *self.captured_tools_len.lock().unwrap() = Some(tools.len());
            *self.call_count.lock().unwrap() += 1;

            match &self.response {
                FakeResponse::Text(t) => Ok(LlmResponse {
                    text: Some(t.clone()),
                    tool_calls: vec![],
                    stop_reason: "end_turn".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                }),
                FakeResponse::EmptyText => Ok(LlmResponse {
                    text: Some(String::new()),
                    tool_calls: vec![],
                    stop_reason: "end_turn".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                }),
                FakeResponse::Error(reason) => Err(AgentError::Llm(reason.clone())),
                FakeResponse::WithToolCalls { text } => Ok(LlmResponse {
                    text: Some(text.clone()),
                    tool_calls: vec![claw_agent_runtime::llm::LlmToolCall {
                        id: "x".into(),
                        tool_name: "broadcast_tx".into(),
                        input: json!({}),
                    }],
                    stop_reason: "tool_use".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                }),
            }
        }
    }

    // ── Fixtures ────────────────────────────────────────────────────────

    const TEST_WALLET_BS58: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const TEST_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";

    fn solend_ok_output_with_n_positions(n: usize) -> Value {
        let deposits: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "obligation_pubkey": format!("Obg{i}aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    "owner_pubkey": TEST_WALLET_BS58,
                    "reserve_pubkey": TEST_RESERVE_BS58,
                    "is_main_pool_usdc": true,
                    "supplied_usdc_estimate_raw": Value::Null,
                    "supplied_usdc_estimate_ui": Value::Null,
                    "estimate_unavailable_reason":
                        "Solend reserve liquidity index is required to convert obligation \
                         collateral shares into USDC; not yet wired into the read path.",
                })
            })
            .collect();
        json!({
            "status": "ok",
            "wallet_pubkey": TEST_WALLET_BS58,
            "obligation_count": n,
            "deposits": deposits,
            "borrows": [],
            "dashboard_visibility_note":
                "Solend's public dashboard may not display this position because ClawSol \
                 discovers obligations by owner via getProgramAccounts while the dashboard \
                 may rely on deterministic obligation discovery. This is a discovery / \
                 indexing concern, NOT a custody transfer.",
        })
    }

    /// Canned, well-formed explanation produced by the (faked) LLM.
    /// The wallet pubkey is preserved verbatim so the scrubber accepts it.
    fn well_formed_explanation_text(wallet: &str, count: usize) -> String {
        json!({
            "headline": format!("{count} Solend / Save USDC obligation(s) found for your wallet"),
            "plain_english_summary": format!(
                "I found {count} Solend obligation(s) whose on-chain owner field is {wallet}. \
                 They all point to the Main Pool USDC reserve. Solend's public dashboard may \
                 not display them; that is a discovery / indexing concern, not a custody \
                 transfer. The supplied USDC value is not currently determinable.",
            ),
            "key_facts": [
                format!("Owner pubkey on-chain: {wallet}"),
                format!("Obligation count: {count}"),
                format!("Reserve: {TEST_RESERVE_BS58} (Main Pool USDC)"),
            ],
            "risk_notes": [
                "On-chain ownership is consistent with user-owned custody; visibility issue is dashboard-side."
            ],
            "next_safe_actions": [
                "View the raw JSON for verification.",
                "Render this as a position card.",
                "Prepare a withdraw proposal only after the user explicitly asks."
            ],
            "cannot_claim": [
                "The supplied USDC value cannot be reported because supplied_usdc_estimate_ui is null."
            ]
        })
        .to_string()
    }

    fn build_explainer(llm: Arc<FakeLlm>) -> LlmToolOutputExplainer {
        LlmToolOutputExplainer::new(llm as LlmClientRef)
    }

    // ── Tests ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn unsupported_tool_returns_unsupported_tool() {
        let llm = FakeLlm::returning_text("never used");
        let explainer = build_explainer(llm.clone());
        let outcome = explainer
            .explain("get_wallet_balances", &json!({"status": "ok"}))
            .await;
        assert!(matches!(outcome, ExplainerOutcome::UnsupportedTool));
        assert_eq!(*llm.call_count.lock().unwrap(), 0, "LLM not invoked");
    }

    #[tokio::test]
    async fn missing_status_returns_invalid_tool_output() {
        let llm = FakeLlm::returning_text("never used");
        let explainer = build_explainer(llm.clone());
        let outcome = explainer
            .explain("get_solend_position", &json!({}))
            .await;
        match outcome {
            ExplainerOutcome::InvalidToolOutput { reason } => {
                assert!(reason.contains("status"));
            }
            other => panic!("expected InvalidToolOutput, got {other:?}"),
        }
        assert_eq!(*llm.call_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn unsupported_status_returns_invalid_tool_output() {
        let llm = FakeLlm::returning_text("never used");
        let explainer = build_explainer(llm.clone());
        let outcome = explainer
            .explain("get_solend_position", &json!({"status": "rpc_error"}))
            .await;
        match outcome {
            ExplainerOutcome::InvalidToolOutput { reason } => {
                assert!(reason.contains("rpc_error"));
            }
            other => panic!("expected InvalidToolOutput, got {other:?}"),
        }
        assert_eq!(*llm.call_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn ok_output_with_5_positions_produces_explanation_dto() {
        let tool_output = solend_ok_output_with_n_positions(5);
        let llm = FakeLlm::returning_text(well_formed_explanation_text(TEST_WALLET_BS58, 5));
        let explainer = build_explainer(llm.clone());

        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        let dto = match outcome {
            ExplainerOutcome::Ok(d) => d,
            other => panic!("expected Ok, got {other:?}"),
        };

        // Wallet pubkey preserved verbatim somewhere in the dto.
        assert!(dto_mentions(&dto, TEST_WALLET_BS58));
        // Obligation count + reserve pubkey preserved.
        assert!(dto_mentions(&dto, "5"));
        assert!(dto_mentions(&dto, TEST_RESERVE_BS58));
        // The cannot_claim list mentions the unavailable supplied USDC estimate.
        assert!(
            dto.cannot_claim
                .iter()
                .any(|s| s.contains("supplied_usdc_estimate_ui")),
            "cannot_claim must mention the unavailable estimate; got {:?}",
            dto.cannot_claim
        );
        // Headline + summary are non-empty.
        assert!(!dto.headline.is_empty());
        assert!(!dto.plain_english_summary.is_empty());
        // LLM was invoked exactly once.
        assert_eq!(*llm.call_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn explanation_does_not_invent_supplied_usdc_when_null() {
        let tool_output = solend_ok_output_with_n_positions(3);
        // The well-formed canned response correctly names the
        // unavailability — a regression that fabricated a number would
        // either drop the cannot_claim entry or invent a digit string.
        let llm = FakeLlm::returning_text(well_formed_explanation_text(TEST_WALLET_BS58, 3));
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        let dto = match outcome {
            ExplainerOutcome::Ok(d) => d,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(
            !dto.plain_english_summary.contains("USDC supplied"),
            "summary must not assert a supplied USDC quantity; got {}",
            dto.plain_english_summary
        );
        assert!(
            dto.cannot_claim
                .iter()
                .any(|s| s.contains("supplied_usdc_estimate_ui") || s.contains("not currently determinable") || s.contains("null")),
            "cannot_claim must capture the unavailable estimate"
        );
    }

    #[tokio::test]
    async fn provider_error_returns_provider_error_outcome() {
        let tool_output = solend_ok_output_with_n_positions(1);
        let llm = FakeLlm::returning_error("rate limited");
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        match outcome {
            ExplainerOutcome::ProviderError { reason } => {
                assert!(reason.contains("rate limited"));
            }
            other => panic!("expected ProviderError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_text_returns_provider_error() {
        let tool_output = solend_ok_output_with_n_positions(1);
        let llm = FakeLlm::returning_empty();
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        match outcome {
            ExplainerOutcome::ProviderError { reason } => {
                assert!(reason.contains("no text"));
            }
            other => panic!("expected ProviderError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparseable_json_returns_provider_error() {
        let tool_output = solend_ok_output_with_n_positions(1);
        let llm = FakeLlm::returning_text("this is not json at all");
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        match outcome {
            ExplainerOutcome::ProviderError { reason } => {
                assert!(reason.contains("parse failed"));
            }
            other => panic!("expected ProviderError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dropped_wallet_pubkey_returns_provider_error() {
        let tool_output = solend_ok_output_with_n_positions(2);
        // Canned response does NOT mention the wallet pubkey anywhere.
        let dto = json!({
            "headline": "two positions",
            "plain_english_summary": "the user has two obligations.",
            "key_facts": ["obligation count: 2"],
            "risk_notes": [],
            "next_safe_actions": ["View the raw JSON for verification."],
            "cannot_claim": []
        })
        .to_string();
        let llm = FakeLlm::returning_text(dto);
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        match outcome {
            ExplainerOutcome::ProviderError { reason } => {
                assert!(reason.contains("preserve wallet_pubkey"));
            }
            other => panic!("expected ProviderError for missing wallet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_cannot_claim_when_estimate_unavailable_returns_provider_error() {
        // Tool output has 2 deposits, both with `estimate_unavailable_reason`.
        // The canned LLM response forgets to acknowledge this in
        // `cannot_claim` (the list is empty). Scrubber must reject so
        // the audit-grade UI never claims a value the source did not
        // provide.
        let tool_output = solend_ok_output_with_n_positions(2);
        let dto = json!({
            "headline": "two positions",
            "plain_english_summary": format!(
                "Wallet {TEST_WALLET_BS58} has two Solend obligations on chain."
            ),
            "key_facts": [format!("Owner pubkey on-chain: {TEST_WALLET_BS58}"), "Obligation count: 2"],
            "risk_notes": [],
            "next_safe_actions": ["View the raw JSON for verification."],
            "cannot_claim": []
        })
        .to_string();
        let llm = FakeLlm::returning_text(dto);
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        match outcome {
            ExplainerOutcome::ProviderError { reason } => {
                assert!(
                    reason.contains("cannot_claim"),
                    "rejection must mention cannot_claim; got: {reason}"
                );
            }
            other => panic!("expected ProviderError for empty cannot_claim, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_calls_in_response_are_refused_as_provider_error() {
        let tool_output = solend_ok_output_with_n_positions(1);
        let llm = FakeLlm::returning_text_with_tool_calls(
            well_formed_explanation_text(TEST_WALLET_BS58, 1),
        );
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        match outcome {
            ExplainerOutcome::ProviderError { reason } => {
                assert!(reason.contains("tool_calls"));
            }
            other => panic!("expected ProviderError for tool_calls, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn user_prompt_contains_dashboard_visibility_note_when_present() {
        let tool_output = solend_ok_output_with_n_positions(1);
        let llm = FakeLlm::returning_text(well_formed_explanation_text(TEST_WALLET_BS58, 1));
        let explainer = build_explainer(llm.clone());
        let _ = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        let user = llm
            .captured_user
            .lock()
            .unwrap()
            .clone()
            .expect("user message captured");
        assert!(
            user.contains("dashboard_visibility_note"),
            "user prompt must include dashboard_visibility_note text"
        );
        assert!(
            user.contains("estimate_unavailable_reason"),
            "user prompt must include estimate_unavailable_reason text"
        );
    }

    #[tokio::test]
    async fn system_prompt_passed_verbatim_and_tools_empty() {
        let tool_output = solend_ok_output_with_n_positions(1);
        let llm = FakeLlm::returning_text(well_formed_explanation_text(TEST_WALLET_BS58, 1));
        let explainer = build_explainer(llm.clone());
        let _ = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        assert_eq!(
            llm.captured_system.lock().unwrap().clone(),
            Some(EXPLAINER_SYSTEM_PROMPT.to_string()),
            "system prompt must be the explainer constant verbatim"
        );
        assert_eq!(
            *llm.captured_tools_len.lock().unwrap(),
            Some(0),
            "tools must be empty — explainer never asks for follow-up tool calls"
        );
    }

    #[tokio::test]
    async fn no_position_status_is_supported() {
        let tool_output = json!({
            "status": "no_position",
            "wallet_pubkey": TEST_WALLET_BS58,
        });
        let dto = json!({
            "headline": "no Solend positions found",
            "plain_english_summary": format!(
                "Wallet {TEST_WALLET_BS58} has zero Solend obligations on chain."
            ),
            "key_facts": [format!("Wallet: {TEST_WALLET_BS58}"), "Obligation count: 0"],
            "risk_notes": [],
            "next_safe_actions": ["View the raw JSON for verification."],
            "cannot_claim": []
        })
        .to_string();
        let llm = FakeLlm::returning_text(dto);
        let explainer = build_explainer(llm);
        let outcome = explainer
            .explain("get_solend_position", &tool_output)
            .await;
        let dto = match outcome {
            ExplainerOutcome::Ok(d) => d,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(dto_mentions(&dto, TEST_WALLET_BS58));
    }

    #[tokio::test]
    async fn disabled_explainer_returns_explainer_unavailable() {
        let explainer = DisabledExplainer;
        let outcome = explainer
            .explain("get_solend_position", &json!({"status": "ok"}))
            .await;
        assert!(matches!(outcome, ExplainerOutcome::ExplainerUnavailable));
    }

    #[test]
    fn explainer_outcome_status_labels_are_stable() {
        assert_eq!(
            ExplainerOutcome::Ok(ExplanationDto::default()).status_label(),
            "ok"
        );
        assert_eq!(
            ExplainerOutcome::UnsupportedTool.status_label(),
            "unsupported_tool"
        );
        assert_eq!(
            ExplainerOutcome::ExplainerUnavailable.status_label(),
            "explainer_unavailable"
        );
        assert_eq!(
            ExplainerOutcome::InvalidToolOutput {
                reason: "x".into()
            }
            .status_label(),
            "invalid_tool_output"
        );
        assert_eq!(
            ExplainerOutcome::ProviderError {
                reason: "x".into()
            }
            .status_label(),
            "provider_error"
        );
    }

    #[test]
    fn system_prompt_contains_all_safety_clauses() {
        let p = EXPLAINER_SYSTEM_PROMPT;
        // Rule 1: do-not-change clause (numbers, pubkeys, etc.)
        assert!(p.contains("Do not change any numbers, pubkeys"));
        // Rule 2: do not estimate when null
        assert!(p.contains("supplied_usdc_estimate_ui"));
        assert!(p.contains("not currently determinable"));
        // Rule 3: no absolute "funds are safe" guarantee
        assert!(p.contains("Do not say \"funds are safe\""));
        // Rule 4: allowed phrasings
        assert!(p.contains("the obligation owner field is the user's wallet"));
        assert!(p.contains("user-owned on-chain"));
        // Rule 5: estimate_unavailable_reason -> mention uncertainty + cannot_claim
        assert!(p.contains("estimate_unavailable_reason"));
        assert!(p.contains("cannot_claim"));
        // Rule 6: do not recommend borrow / repay (capitalised NOT in prompt for emphasis)
        assert!(p.contains("NOT recommend borrow or repay"));
        // Rule 7: do not say a withdrawal has happened (capitalised NOT)
        assert!(p.contains("NOT say a withdrawal has happened"));
        // Rule 8: allowed next_safe_actions
        assert!(p.contains("View the raw JSON for verification."));
        assert!(p.contains("Render this as a position card."));
        assert!(p.contains("Prepare a withdraw proposal only after"));
        // Rule 9: withdrawal still requires Phantom signature
        assert!(p.contains("Phantom signature"));
        // Rule 10: JSON-only output, no markdown fences
        assert!(p.contains("Output ONLY a single JSON object"));
        assert!(p.contains("no markdown fences"));
        // Discovery vs custody clause
        assert!(p.contains("dashboard_visibility_note"));
        assert!(p.contains("discovery / indexing concern"));
        assert!(p.contains("NOT a custody transfer"));
    }

    #[test]
    fn source_guard_no_signing_or_broadcast_or_secrets() {
        const SOURCE: &str = include_str!("tool_output_explainer.rs");
        // Needles split at compile-time so this test does not match
        // its own source text.
        let needles: Vec<String> = vec![
            format!("{}{}", "send_", "transaction("),
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", "send_raw_v0_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "confirm_", "transaction("),
            format!("{}{}", "create_signing_", "handoff("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "Keypair::", "new("),
            format!("{}{}", "Keypair::", "from"),
            format!("{}{}", "private_", "key"),
            format!("{}{}", "tx_", "bytes"),
            format!("{}{}", "transaction_", "base64"),
            format!("{}{}", "signed_", "bytes"),
            format!("{}{}", "to", "do!("),
            format!("{}{}", "unimplem", "ented!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "tool_output_explainer.rs must not contain `{n}`"
            );
        }
    }
}

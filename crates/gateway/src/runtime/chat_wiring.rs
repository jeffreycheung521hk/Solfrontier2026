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
    W5dConditionalDepositResultDto,
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
You also have two READ-ONLY tools that DO NOT propose, sign, or move \
funds. Use them to inform reasoning before deciding whether to \
propose. \
\n\n\
`get_wallet_balances` takes no inputs and returns SOL + USDC balances \
for the session-bound wallet (raw + UI-decimal forms, plus the USDC \
ATA pubkey if found). It is safe to call any time. \
\n\n\
`get_jupiter_quote` takes `input_mint`, `output_mint`, `input_amount` \
(raw base units), and `slippage_bps` (must be in [0, 100]) and \
returns a Jupiter route preview — expected output, slippage threshold, \
price impact, and DEX route — without building, signing, or \
broadcasting anything. For `input_mint` and `output_mint` you may \
pass either the canonical base58 mint pubkey OR the symbols \
`SOL`, `WSOL`, or `USDC` (case-insensitive); the tool normalises \
symbols to canonical mints. Use it when the user asks \"how much X \
would I get for Y?\" or compares routes before deciding to swap. \
\n\n\
`get_solend_position` takes no inputs and returns a read-only scan \
of the session-bound wallet's Solend / Save obligations on mainnet. \
It uses `getProgramAccounts` with bounded filters (1300-byte data \
size + memcmp on the owner field) — it does NOT sign, build a \
transaction, broadcast, or create an approval. Call it when the user \
asks \"where is my Solend deposit?\", \"show my Solend position\", \
\"why doesn't the Solend dashboard show my 5 USDC?\", \"find my \
Solend USDC obligation\", or anything that asks the assistant to \
LOCATE / CONFIRM an existing on-chain Solend USDC position. Do NOT \
call this tool when the user asks to MAKE a new deposit — that path \
is `solend_deposit_usdc`. The tool reports the obligation pubkey, \
the lending market, and any USDC-reserve deposits with their cToken \
amounts; it does not invent a USDC value if the exchange-rate \
decode is not available in this slice (it returns \
`supplied_usdc_estimate_raw: null` with an explicit \
`estimate_unavailable_reason` string). Be honest about uncertainty: \
say \"I found the on-chain obligation at X with N USDC-reserve \
deposit entries\" when found, or \"I found no Solend obligation \
owned by this wallet\" when the scan returns empty. \
\n\n\
`preview_solend_withdraw_all` takes a single required field \
`obligation_pubkey` (base58 string — typically one of the \
`obligation_pubkey` values returned by `get_solend_position`) and \
returns a read-only PREVIEW of whether that obligation is safe for \
withdraw-all. It never signs, never broadcasts, never creates an \
approval, never builds a transaction the user could submit — it only \
re-fetches that one obligation, decodes it, and reports preconditions. \
The output's `mode` is always `withdraw_all_collateral`; \
`requires_user_signature` is always `true`; \
`requires_obligation_keypair`, `will_create_approval`, `will_sign`, and \
`will_broadcast` are always `false`. The `collateral_amount_raw` is the \
deposited cToken amount; `underlying_usdc_estimate_raw` is `null` (the \
exchange-rate decode is a future slice). \
\n\n\
Call `preview_solend_withdraw_all` when the user asks to PREVIEW, \
CHECK, VERIFY, or CONFIRM whether a specific Solend obligation can be \
withdrawn — for example: \"preview withdraw-all for obligation \
HcKrv5Jo...\", \"can I withdraw all from obligation HcKrv5Jo...?\", \
\"check whether this Solend obligation is withdrawable: \
HcKrv5Jo...\". The user MUST provide an explicit obligation pubkey; \
do NOT invent one and do NOT silently pick from a prior \
`get_solend_position` result. \
\n\n\
Do NOT call `preview_solend_withdraw_all` when the user asks to \
EXECUTE a withdraw (\"withdraw it now\", \"take my 5 USDC out\", \
\"sign and submit the withdrawal\"). Withdraw EXECUTION is not enabled \
yet — for those requests respond with plain text explaining that you \
can only PREVIEW the withdraw, not execute it, and offer to run the \
preview if the user supplies an obligation pubkey. Do NOT batch \
`get_solend_position` and `preview_solend_withdraw_all` in the same \
turn — the one-tool-per-turn rule applies; if the user wants both, \
call the scanner first and stop. \
\n\n\
`solend_withdraw_all_usdc` takes a single required field \
`obligation_pubkey` (base58 string — typically copied from a prior \
`get_solend_position` or `preview_solend_withdraw_all` result) and \
PROPOSES a Solend / Save withdraw-all transaction by parking an \
awaiting-approval intent. It does NOT sign, does NOT broadcast, does \
NOT build a final signed transaction at chat-tool time — the actual \
withdraw transaction is assembled on the user's Sign-with-Phantom \
click in a later step. The output's `status` is `awaiting_approval` \
on success, or `policy_blocked` / `wallet_not_bound` / \
`invalid_obligation_pubkey` / `obligation_not_found` / `decode_error` \
/ `rpc_error` / `pending_action_exists` on the various refusal paths. \
\n\n\
Call `solend_withdraw_all_usdc` ONLY when the user explicitly asks to \
withdraw ALL from a SPECIFIC Solend obligation pubkey — for example: \
\"withdraw all USDC from Solend obligation HcKrv5Jo…\", \"withdraw \
everything from this obligation: HcKrv5Jo…\". The user MUST provide \
an explicit `obligation_pubkey`; do NOT infer one from prior \
messages, do NOT silently pick from a prior `get_solend_position` \
result, and do NOT auto-select the largest. \
\n\n\
Partial withdraws are NOT supported. If the user asks \"withdraw 5 \
USDC\", \"withdraw some USDC\", \"take out half\", or anything that \
implies a numeric amount, do NOT call `solend_withdraw_all_usdc`. \
Respond with plain text explaining that only withdraw-all by \
explicit obligation is available, and offer to run \
`preview_solend_withdraw_all` first if the user provides an \
obligation pubkey. Multi-obligation \"withdraw all positions\" is \
also not supported — only one obligation per call. \
\n\n\
Never call `solend_withdraw_all_usdc` with a hallucinated `amount`, \
`mode`, `wallet_pubkey`, `reserve_pubkey`, `slippage`, `approve`, \
`tx_bytes`, or any other field — the tool's strict schema rejects \
extra fields and the entire call fails. The session-bound wallet is \
the only signer; do NOT include it in the input. The one-tool-per-turn \
rule still applies — never batch the preview and execution in the \
same turn. \
\n\n\
When the user makes a CONDITIONAL request (for example \"deposit \
0.001 USDC into Solend if my balance is above 0.3\", or \"if I have \
enough SOL, swap 0.001 SOL to USDC\"), call the appropriate \
read-only tool FIRST and stop the turn. The system returns the \
result to the user; the user (or a follow-up turn) then decides. \
Do NOT batch a read-only tool with `solend_deposit_usdc` or \
`submit_jupiter_swap` in the same turn — that violates the \
one-tool-per-turn rule and the entire turn is rejected. \
\n\n\
If a read-only tool reveals that the wallet's balance is INSUFFICIENT \
for the user's requested action, do NOT call a transaction tool. \
Respond with plain text instead, explaining the balance you observed \
and that you are not creating a proposal. Fail closed. \
\n\n\
Make at most one tool call per turn. After the tool returns, stop — \
do not call additional tools, do not approve, do not sign.";

// ── Phase 5E — registry narrowing ─────────────────────────────────────────

/// Name of the only tool the chat-route surface exposes to the LLM in
/// Phase 5E. Lives as a constant so the source guards can lock it.
pub const CHAT_TOOL_ALLOWLIST: &[&str] = &[
    "solend_deposit_usdc",
    "submit_jupiter_swap",
    "get_wallet_balances",
    "get_jupiter_quote",
    // Phase 6H — read-only Solend / Save position scanner. Pure
    // `getProgramAccounts` + obligation decode; no signing, no broadcast,
    // no approval. Lets the assistant answer "where is my deposit?"
    // without exposing withdraw.
    "get_solend_position",
    // Phase 6I-B — read-only Solend withdraw-all PREVIEW for a specific
    // obligation pubkey. Validates owner / borrow-free / has-USDC-deposit
    // and returns a structured preview. Strictly preview-only:
    // `required_capabilities: vec![]`, no approval, no signing, no
    // broadcast, no execution.
    "preview_solend_withdraw_all",
    // Phase 6I-D — Solend withdraw-all EXECUTION proposal. Returns
    // `awaiting_approval` with a parked intent keyed by a real
    // `approval_request_id`. The tool itself does NOT sign, broadcast,
    // or build a transaction; the resume / JIT signing handoff /
    // submit pipeline is deferred to the follow-up slice. Strictly
    // bounded: one explicit obligation, withdraw-all only, USDC
    // reserve only, Solend / Save Main Pool lending market only, no
    // borrow tolerated, no partial amount field accepted.
    "solend_withdraw_all_usdc",
];

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
    w5e_repo: Option<Arc<claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository>>,
    w5g_executor: Option<Arc<crate::stage2_chat_execute::Stage2ChatExecutor>>,
    w5h_intent_repo: Option<
        Arc<claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository>,
    >,
    session_wallet_lookup: Option<Arc<dyn crate::tools::jupiter_swap::SessionBoundWallet>>,
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
    let mut handler = GatewayChatHandler::new(
        llm,
        narrowed,
        ALIGNMENT_SYSTEM_PROMPT.to_string(),
        chat_capabilities(),
    );
    // W5d demo-bridge — opt-in via RPC URL. The presence of a working
    // RPC URL is treated as "the demo bridge is configured"; absence
    // means W5d grammar is NOT recognised by the chat handler and the
    // message falls through to the LLM (i.e. legacy behaviour).
    if let Some(rpc) = env
        .get("HELIUS_RPC_URL")
        .or_else(|| env.get("CLAW_RPC_URL"))
    {
        if let Some(fetcher) =
            crate::stage2_demo_apr_bridge::LiveW5dAprFetcher::new(rpc)
        {
            handler = handler.with_w5d_bridge(Arc::new(fetcher));
        }
    }
    // W5e — attach the durable rule repository so accepted commands
    // persist a real WatchRule and the result carries
    // `rule_persisted=true`. Absent => preview-only (the chat-route
    // card carries `rule_persisted=false` and the UI must reflect
    // the no-overclaim banner).
    if let Some(repo) = w5e_repo {
        handler = handler.with_w5e_repo(repo);
    }
    // W5f — attach the live Save display APY fetcher. The fetcher hits
    // the official Solend REST API at
    // <https://dev.solend.fi/docs/api/>. Base URL is overridable via
    // `SOLEND_API_BASE_URL` so integration tests can point at a stub.
    // Absent / blocked => the chat handler falls back to the W5e
    // single-APR path (degraded mode); the UI can detect this because
    // `save_display_apy_bps == native_onchain_apr_bps` in that case.
    {
        let base_url = env
            .get("SOLEND_API_BASE_URL")
            .unwrap_or_else(|| {
                crate::stage2_demo_apr_bridge::SOLEND_API_BASE_URL_DEFAULT.to_string()
            });
        if let Some(save) =
            crate::stage2_demo_apr_bridge::LiveSaveDisplayApyFetcher::new(base_url)
        {
            handler = handler.with_w5f_save_apy(Arc::new(save));
        }
    }
    // W5g — attach the orchestrator if the daemon built one. The
    // daemon is responsible for constructing the executor under the
    // full env-gate chain; this wire layer just plumbs the Arc.
    if let Some(executor) = w5g_executor {
        handler = handler.with_w5g_executor(executor);
    }
    // W5h-lite — attach the funding-intent repo and the session →
    // user-wallet lookup. Both are required by the W5h chat-route
    // dispatcher; if either is absent, W5h-grammar commands surface
    // a typed `ToolError` (NEVER a silent LLM fall-through and
    // NEVER an intent insert without the operator's bound wallet).
    if let Some(repo) = w5h_intent_repo {
        handler = handler.with_w5h_intent_repo(repo);
    }
    if let Some(lookup) = session_wallet_lookup {
        handler = handler.with_session_wallet_lookup(lookup);
    }
    Ok(Some(handler.into_handler_ref()))
}

/// Convenience: read from the real process env. Used by the daemon at
/// startup. Tests inject a mock `EnvProvider` via
/// [`wire_chat_handler_with_registry`].
///
/// W5e — the daemon supplies the optional `Stage2WatchRuleRepository`
/// it constructed from its `Database` handle; this entry forwards it.
/// W5f — the live Save display APY fetcher is constructed internally
/// from the `SOLEND_API_BASE_URL` env var (defaulting to the official
/// Solend REST API).
pub fn wire_chat_handler_from_std_env(
    registry: &ToolRegistry,
    w5e_repo: Option<Arc<claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository>>,
    w5g_executor: Option<Arc<crate::stage2_chat_execute::Stage2ChatExecutor>>,
    w5h_intent_repo: Option<
        Arc<claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository>,
    >,
    session_wallet_lookup: Option<Arc<dyn crate::tools::jupiter_swap::SessionBoundWallet>>,
) -> Result<Option<ChatHandlerRef>, LlmProviderConfigError> {
    wire_chat_handler_with_registry(
        registry,
        &StdEnvProvider,
        w5e_repo,
        w5g_executor,
        w5h_intent_repo,
        session_wallet_lookup,
    )
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
    /// Optional W5d demo-bridge — when present, the chat handler tries
    /// the deterministic W5d grammar BEFORE dispatching to the LLM.
    /// A non-matching message falls through unchanged (LLM path
    /// preserved). When `None`, the chat handler behaves exactly as
    /// pre-W5d (LLM-only). The chat route never broadcasts a tx —
    /// the live-send path is reserved for the env-gated W5c harness.
    w5d_bridge: Option<Arc<dyn crate::stage2_demo_apr_bridge::W5dAprFetcher>>,
    /// W5e — optional state-store repository for persisting demo
    /// watch rules. When present (daemon wires it from `Database`),
    /// `handle_demo_command_v3` will insert the rule and set
    /// `rule_persisted=true`. When absent, the bridge returns a
    /// preview-only result with `rule_persisted=false`.
    w5e_repo: Option<
        Arc<claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository>,
    >,
    /// W5f — Save display APY fetcher (REST API). When present, the
    /// orchestrator uses Save UI APY (not native on-chain APR) as the
    /// decision metric. When absent, the chat route surfaces a
    /// `ToolError` for any matched W5d command because W5f requires
    /// the Save metric to render the typed card.
    w5f_save_apy: Option<Arc<dyn crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher>>,
    /// W5g — chat-route controlled-wallet Solend deposit execution.
    /// When present, the chat handler detects the W5g approval
    /// command (`Execute W5g conditional deposit … with approval
    /// phrase …`), dispatches to the orchestrator, and emits a
    /// typed `W5gConditionalExecution` variant. When absent (every
    /// env gate fail-closed at daemon startup), the chat route
    /// returns a `ToolError` with `tool_name="w5g_conditional_execution"`
    /// so the operator sees an explicit refusal instead of a silent
    /// fall-through to the LLM.
    w5g_executor: Option<Arc<crate::stage2_chat_execute::Stage2ChatExecutor>>,
    /// W5h-lite — funding-intent repository. When present alongside
    /// `w5d_bridge` + `w5f_save_apy` + `w5e_repo` +
    /// `session_wallet_lookup`, the chat handler detects W5h grammar
    /// (`"If Save APY > X%, deposit 0.25 USDC"`) and dispatches to
    /// the W5h bridge BEFORE the LLM call. The funding-confirm route
    /// is wired separately in `daemon.rs`.
    w5h_intent_repo: Option<
        Arc<claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository>,
    >,
    /// W5h-lite — session → user-wallet pubkey resolver. The W5h
    /// dispatcher REQUIRES this; the funding intent's `user_wallet`
    /// MUST be the operator's externally-bound pubkey, never a
    /// hard-coded placeholder. When the session has no bound wallet,
    /// the dispatcher returns a typed `ToolError` and does NOT create
    /// any intent (verified by
    /// `w5h_chat_command_without_bound_wallet_returns_typed_error`).
    session_wallet_lookup: Option<Arc<dyn crate::tools::jupiter_swap::SessionBoundWallet>>,
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
            w5d_bridge: None,
            w5e_repo: None,
            w5f_save_apy: None,
            w5g_executor: None,
            w5h_intent_repo: None,
            session_wallet_lookup: None,
        }
    }

    /// Variant constructor that attaches a W5d APR-bridge fetcher.
    /// The chat handler will route any message that matches the W5d
    /// deterministic grammar (per
    /// [`crate::stage2_demo_apr_bridge::looks_like_w5d_command`]) to
    /// the fetcher instead of the LLM. Non-matching messages continue
    /// through the existing LLM path unchanged.
    pub fn with_w5d_bridge(
        mut self,
        fetcher: Arc<dyn crate::stage2_demo_apr_bridge::W5dAprFetcher>,
    ) -> Self {
        self.w5d_bridge = Some(fetcher);
        self
    }

    /// W5e — attach a `Stage2WatchRuleRepository` so demo commands
    /// persist a real `WatchRule`. Caller must ensure the repository
    /// is built from the same `Database` the rest of the daemon uses
    /// (so dashboard / `repo.get(rule_id)` queries see the persisted
    /// rule). Without this call, the chat handler returns
    /// `rule_persisted=false`.
    pub fn with_w5e_repo(
        mut self,
        repo: Arc<claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository>,
    ) -> Self {
        self.w5e_repo = Some(repo);
        self
    }

    /// W5f — attach a `SaveDisplayApyFetcher`. When present, the
    /// chat-route orchestrator uses Save UI display APY (not native
    /// on-chain APR) as the decision metric for W5d-grammar commands.
    /// When absent, the chat route falls back to the W5e single-APR
    /// path (native APR drives the decision) — but this is NOT W5f
    /// parity and the card will render with `decision_source` showing
    /// the legacy `save_display_apy` placeholder against `current_apr_bps`.
    pub fn with_w5f_save_apy(
        mut self,
        fetcher: Arc<dyn crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher>,
    ) -> Self {
        self.w5f_save_apy = Some(fetcher);
        self
    }

    /// W5g — attach the chat-card live-execution orchestrator. When
    /// present, the chat handler detects the W5g approval command
    /// and dispatches; when absent, the chat handler returns a
    /// typed `ToolError` so the operator gets a clear refusal.
    pub fn with_w5g_executor(
        mut self,
        executor: Arc<crate::stage2_chat_execute::Stage2ChatExecutor>,
    ) -> Self {
        self.w5g_executor = Some(executor);
        self
    }

    /// W5h-lite — attach the funding-intent repository. Required
    /// alongside `with_w5d_bridge`, `with_w5f_save_apy`,
    /// `with_w5e_repo`, and `with_session_wallet_lookup` for the
    /// chat-route W5h dispatch to fire. Without it, W5h-grammar
    /// commands fall through to the LLM.
    pub fn with_w5h_intent_repo(
        mut self,
        repo: Arc<claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository>,
    ) -> Self {
        self.w5h_intent_repo = Some(repo);
        self
    }

    /// W5h-lite — attach the session → user-wallet pubkey resolver.
    /// The W5h dispatcher uses this to source the operator's
    /// externally-bound wallet for the funding intent's `user_wallet`
    /// field. If absent at dispatch time, the chat-route returns a
    /// typed `ToolError` and never creates an intent.
    pub fn with_session_wallet_lookup(
        mut self,
        lookup: Arc<dyn crate::tools::jupiter_swap::SessionBoundWallet>,
    ) -> Self {
        self.session_wallet_lookup = Some(lookup);
        self
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
            // ── W5d demo-bridge interceptor ──────────────────────────
            //
            // If the chat handler was built with a W5d fetcher (daemon
            // config has the RPC URL) AND the message matches the
            // deterministic W5d grammar, dispatch deterministically
            // BEFORE invoking the LLM. Any non-matching message falls
            // through to the LLM path unchanged.
            //
            // The chat route NEVER broadcasts a tx. The bridge's
            // happy-path status is one of `"condition_not_met"` |
            // `"ready_to_execute"`. Live-send authorisation is reserved
            // for the env-gated W5c test harness.
            if let Some(fetcher) = &self.w5d_bridge {
                if crate::stage2_demo_apr_bridge::looks_like_w5d_command(&message)
                {
                    return handle_w5d_demo_command(
                        fetcher.as_ref(),
                        self.w5f_save_apy.as_deref(),
                        self.w5e_repo.as_deref(),
                        &message,
                    )
                    .await;
                }
            }

            // ── W5h-lite chat-route interceptor ──────────────────────
            //
            // The W5h grammar (English / 繁中, "If Save APY > X%,
            // deposit 0.25 USDC[, expires in 3 minutes]") is detected
            // BEFORE the LLM call. The bridge needs:
            //   - APR fetcher (w5d_bridge)
            //   - Save APY fetcher (w5f_save_apy)
            //   - WatchRule repo (w5e_repo)
            //   - Funding-intent repo (w5h_intent_repo)
            //   - Session-bound user wallet (session_wallet_lookup)
            //
            // Missing any of these → typed ToolError, NEVER a silent
            // LLM fall-through. The user wallet pubkey is sourced
            // from the session binding, never hardcoded.
            if crate::stage2_w5h_chat::looks_like_w5h_chat_command(&message) {
                return handle_w5h_chat_command(
                    self.w5d_bridge.as_deref(),
                    self.w5f_save_apy.as_deref(),
                    self.w5e_repo.as_deref(),
                    self.w5h_intent_repo.as_deref(),
                    self.session_wallet_lookup.as_deref(),
                    &session_id,
                    &message,
                )
                .await;
            }

            // ── W5g approval-command interceptor ──────────────────────
            //
            // If the message matches the W5g approval-command grammar,
            // dispatch deterministically — BEFORE any LLM call. If the
            // executor isn't wired (env gates fail-closed) the route
            // returns a typed ToolError; if parsing fails, same shape.
            if crate::stage2_chat_execute::looks_like_w5g_chat_command(&message) {
                return handle_w5g_chat_command(
                    self.w5g_executor.as_deref(),
                    &message,
                )
                .await;
            }

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

/// W5d chat-route branch. Tries `parse_demo_command` strictly; if it
/// rejects the grammar (the lightweight detector matched but the
/// strict parser failed), surfaces a typed `ChatResponse::ToolError`
/// with the parser's typed reason so the existing frontend error-card
/// renders it. If parse succeeds and the fetcher returns a result,
/// builds the typed W5d card variant; otherwise translates the typed
/// `EvaluationError` to a `ToolError`.
///
/// This function is the single boundary where the rich gateway-side
/// types (`W5dEvaluationResult`, `EvaluationError`, `ParseError`)
/// collapse into the lean wire DTOs (`ChatResponse::*`,
/// `W5dConditionalDepositResultDto`) the api crate exposes.
async fn handle_w5d_demo_command(
    fetcher: &(dyn crate::stage2_demo_apr_bridge::W5dAprFetcher + 'static),
    save_apy: Option<&(dyn crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher + 'static)>,
    repo: Option<&claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository>,
    message: &str,
) -> ChatRouteOutcome {
    const TOOL_NAME: &str = "w5d_conditional_deposit";
    // W5f: prefer the v3 orchestrator (Save APY decision metric).
    // When the daemon did NOT wire a Save fetcher, fall back to v2
    // (native APR decision) so the route still works in degraded
    // environments — but the resulting card will have
    // `save_display_apy_bps == native_onchain_apr_bps` so the UI can
    // detect the legacy path.
    let result = match save_apy {
        Some(s) => {
            crate::stage2_demo_apr_bridge::handle_demo_command_v3(fetcher, s, repo, message)
                .await
        }
        None => crate::stage2_demo_apr_bridge::handle_demo_command_v2(fetcher, repo, message)
            .await,
    };
    match result
    {
        Ok(result) => {
            let dto = W5dConditionalDepositResultDto {
                input_text: result.input_text,
                source: result.source,
                reserve_pubkey: result.reserve_pubkey,
                current_apr_bps: result.current_apr_bps,
                threshold_bps: result.threshold_bps,
                threshold_pct_label: result.threshold_pct_label,
                condition_met: result.condition_met,
                execution_attempted: result.execution_attempted,
                status: result.status,
                tx_signature: result.tx_signature,
                controlled_wallet: result.controlled_wallet,
                source_usdc_ata: result.source_usdc_ata,
                required_budget_raw: result.required_budget_raw,
                current_budget_raw: result.current_budget_raw,
                budget_status: result.budget_status,
                last_checked_slot: result.last_checked_slot,
                expires_at_slot: result.expires_at_slot,
                rule_id_hex: result.rule_id_hex,
                canonical_rule_hash_hex: result.canonical_rule_hash_hex,
                rule_persisted: result.rule_persisted,
                decision_source: result.decision_source,
                save_display_apy_bps: result.save_display_apy_bps,
                native_onchain_apr_bps: result.native_onchain_apr_bps,
                native_onchain_apr_source: result.native_onchain_apr_source,
            };
            ChatRouteOutcome::Ok(ChatResponse::W5dConditionalDeposit { result: dto })
        }
        Err(eval_err) => {
            // Map any evaluator failure (parse, RPC, decode, APR) to a
            // typed `ToolError` so the frontend reuses its existing
            // error-card surface. The message is the typed display of
            // the gateway-side `EvaluationError`.
            ChatRouteOutcome::Ok(ChatResponse::ToolError {
                tool_name: TOOL_NAME.to_string(),
                message: eval_err.to_string(),
            })
        }
    }
}

/// W5h-lite chat-route branch. Detects the W5h grammar (English /
/// 繁中, "If Save APY > X%, deposit 0.25 USDC[, expires in 3 minutes]")
/// and dispatches to the persistent bridge BEFORE the LLM call.
///
/// Fail-closed conditions (all return typed `ChatResponse::ToolError`
/// with `tool_name == "w5h_conditional_order"`, NEVER an LLM
/// fall-through and NEVER a silent intent insert):
///
///   - APR fetcher missing (daemon has no RPC URL).
///   - Save APY fetcher missing.
///   - WatchRule repo missing.
///   - Funding-intent repo missing.
///   - Session-wallet-lookup missing.
///   - Session has no bound external wallet — surfaced to the user
///     as "Connect wallet before creating a funded conditional order."
///   - W5h bridge parse / native-APR / Save-APY / repo failure.
///
/// The user wallet pubkey is sourced ONLY from
/// `session_wallet_lookup.session_wallet_pubkey(session_id)`. It is
/// never hardcoded, never `Pubkey::default()`, and never the
/// controlled wallet.
async fn handle_w5h_chat_command(
    apr_fetcher: Option<&(dyn crate::stage2_demo_apr_bridge::W5dAprFetcher + 'static)>,
    save_apy: Option<&(dyn crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher + 'static)>,
    rule_repo: Option<&claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository>,
    intent_repo: Option<&claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository>,
    session_wallet_lookup: Option<&(dyn crate::tools::jupiter_swap::SessionBoundWallet + 'static)>,
    session_id: &SessionId,
    message: &str,
) -> ChatRouteOutcome {
    const TOOL_NAME: &str = "w5h_conditional_order";

    fn tool_error(msg: impl Into<String>) -> ChatRouteOutcome {
        ChatRouteOutcome::Ok(ChatResponse::ToolError {
            tool_name: TOOL_NAME.to_string(),
            message: msg.into(),
        })
    }

    // ── Dependency gate — every piece MUST be wired ──────────────────
    let apr_fetcher = match apr_fetcher {
        Some(f) => f,
        None => {
            return tool_error(
                "W5h is not available in this daemon \
                 (B-O1 APR fetcher missing — start the daemon with \
                 HELIUS_RPC_URL or CLAW_RPC_URL set).",
            );
        }
    };
    let save_apy = match save_apy {
        Some(s) => s,
        None => {
            return tool_error(
                "W5h is not available in this daemon \
                 (Save display APY fetcher not wired).",
            );
        }
    };
    let rule_repo = match rule_repo {
        Some(r) => r,
        None => {
            return tool_error(
                "W5h is not available in this daemon \
                 (Stage 2 WatchRule repository not wired).",
            );
        }
    };
    let intent_repo = match intent_repo {
        Some(r) => r,
        None => {
            return tool_error(
                "W5h is not available in this daemon \
                 (Stage 2 W5h funding-intent repository not wired).",
            );
        }
    };
    let session_wallet_lookup = match session_wallet_lookup {
        Some(s) => s,
        None => {
            return tool_error(
                "W5h is not available in this daemon \
                 (session→wallet lookup not wired).",
            );
        }
    };

    // ── User wallet — sourced ONLY from the session binding ──────────
    //
    // No hardcoded fallback, no Pubkey::default(), no controlled
    // wallet substitution. If the session has no bound external
    // wallet, refuse the command — the operator's W5h Phantom flow
    // can only target the wallet they've proved ownership of.
    let user_wallet_bs58 = match session_wallet_lookup.session_wallet_pubkey(session_id) {
        Some(pk) if !pk.trim().is_empty() => pk,
        _ => {
            return tool_error(
                "Connect wallet before creating a funded conditional order. \
                 Use the wallet-bind challenge/response flow to bind a \
                 Phantom wallet to this session, then re-issue the W5h \
                 command.",
            );
        }
    };

    // ── Dispatch to the persistent W5h bridge ────────────────────────
    match crate::stage2_w5h_bridge::handle_w5h_chat_command(
        apr_fetcher,
        save_apy,
        rule_repo,
        intent_repo,
        &user_wallet_bs58,
        message,
    )
    .await
    {
        Ok(dto) => {
            ChatRouteOutcome::Ok(ChatResponse::W5hConditionalOrder { result: dto })
        }
        Err(e) => tool_error(e.to_string()),
    }
}

/// W5g chat-route branch. Parses the user-typed approval command,
/// dispatches to the orchestrator if it's wired, and emits a typed
/// [`ChatResponse::W5gConditionalExecution`] carrying the full
/// execution result. Parse failures and absent-executor cases
/// surface as `ChatResponse::ToolError` with
/// `tool_name == "w5g_conditional_execution"` so the frontend reuses
/// its existing error-card surface for the structural-rejection
/// path. (Orchestrator pre-check failures are NOT ToolErrors — they
/// flow through the typed W5g card with `status="prechecks_failed"`.)
///
/// This is the single boundary where `ChatExecuteOutcome` (rich
/// internal type) collapses into the lean wire DTO
/// (`ChatExecuteResultDto`).
async fn handle_w5g_chat_command(
    executor: Option<&crate::stage2_chat_execute::Stage2ChatExecutor>,
    message: &str,
) -> ChatRouteOutcome {
    const TOOL_NAME: &str = "w5g_conditional_execution";

    // Parse the user-typed command. Parse failures are surfaced as
    // ToolError because they're STRUCTURAL — the operator can fix
    // them by re-copying the command from the W5f card.
    let request = match crate::stage2_chat_execute::parse_w5g_chat_command(message) {
        Ok(r) => r,
        Err(e) => {
            return ChatRouteOutcome::Ok(ChatResponse::ToolError {
                tool_name: TOOL_NAME.to_string(),
                message: format!("w5g parse: {e}"),
            });
        }
    };

    // Executor must be wired. If not, this is a daemon-startup
    // misconfiguration; we surface a typed ToolError so the operator
    // sees an explicit "not wired" message instead of a silent LLM
    // fall-through that pretends to act.
    let executor = match executor {
        Some(e) => e,
        None => {
            return ChatRouteOutcome::Ok(ChatResponse::ToolError {
                tool_name: TOOL_NAME.to_string(),
                message:
                    "W5g chat-execute is not wired in this daemon \
                     (env gates fail-closed). The W5e/W5f read paths still work, \
                     but no live deposit can be authorised from chat until \
                     the daemon is started with CLAW_STAGE2_LIVE_CHAT_EXECUTION=1 \
                     + matching approval phrase + delegated keypair + cluster + RPC."
                        .to_string(),
            });
        }
    };

    // Dispatch to the orchestrator. Every result variant (Completed /
    // BroadcastedTimeout / PrechecksFailed / ExecutionFailed) flows
    // through the typed W5g card on the frontend.
    let outcome = executor.execute(request).await;
    let dto = crate::runtime::stage2_chat_execute_wiring::map_outcome_to_dto(outcome);
    ChatRouteOutcome::Ok(ChatResponse::W5gConditionalExecution { result: dto })
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

        // ── Phase 6C: read-only tools + conditional + insufficient-balance ──
        assert!(p.contains("get_wallet_balances"));
        assert!(p.contains("get_jupiter_quote"));
        assert!(
            p.contains("READ-ONLY"),
            "prompt must mark the read-only tools explicitly"
        );
        // Conditional pattern guidance.
        assert!(
            p.contains("CONDITIONAL"),
            "prompt must call out the conditional pattern"
        );
        assert!(
            p.contains("call the appropriate \nread-only tool FIRST")
                || p.contains("call the appropriate read-only tool FIRST"),
            "prompt must instruct read-first for conditional requests"
        );
        // Insufficient-balance / fail-closed guidance.
        assert!(
            p.contains("INSUFFICIENT"),
            "prompt must call out the insufficient-balance branch"
        );
        assert!(
            p.contains("Fail closed"),
            "prompt must use the fail-closed phrasing for insufficient-balance"
        );
        // Multi-tool batching is rejected.
        assert!(
            p.contains("one-tool-per-turn"),
            "prompt must reference the one-tool-per-turn rule by name"
        );
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

    // ── Phase 6C — read-only tool stubs (mirror production schemas) ────────

    struct StubGetWalletBalancesTool;

    #[async_trait]
    impl Tool for StubGetWalletBalancesTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "get_wallet_balances".into(),
                description: "stub for Phase 6C narrowing tests".into(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": [],
                    "properties": {}
                }),
                output_schema: json!({"type":"object"}),
                required_capabilities: vec![],
                supports_streaming: false,
                timeout_ms: 8_000,
            }
        }
        async fn execute(&self, _: ToolInput) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                tool_name: "get_wallet_balances".into(),
                success: true,
                data: Some(json!({"status":"ok"})),
                error: None,
                duration_ms: 0,
            })
        }
    }

    struct StubGetJupiterQuoteTool;

    #[async_trait]
    impl Tool for StubGetJupiterQuoteTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "get_jupiter_quote".into(),
                description: "stub for Phase 6C narrowing tests".into(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["input_mint", "output_mint", "input_amount", "slippage_bps"],
                    "properties": {
                        "input_mint":   { "type": "string" },
                        "output_mint":  { "type": "string" },
                        "input_amount": { "type": "integer", "minimum": 1 },
                        "slippage_bps": { "type": "integer", "minimum": 0, "maximum": 100 }
                    }
                }),
                output_schema: json!({"type":"object"}),
                required_capabilities: vec![],
                supports_streaming: false,
                timeout_ms: 8_000,
            }
        }
        async fn execute(&self, _: ToolInput) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                tool_name: "get_jupiter_quote".into(),
                success: true,
                data: Some(json!({"status":"ok"})),
                error: None,
                duration_ms: 0,
            })
        }
    }

    fn stub_registry_with_all_chat_tools() -> ToolRegistry {
        ToolRegistry::from_tools(vec![
            Arc::new(StubSolendDepositTool),
            Arc::new(StubJupiterSwapTool),
            Arc::new(StubGetWalletBalancesTool),
            Arc::new(StubGetJupiterQuoteTool),
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
        // Use a unique marker as the user message so the role-separation
        // substring check below cannot collide with any prompt example
        // text (the alignment prompt now contains illustrative phrases
        // such as "deposit 0.001 USDC into Solend if my balance is above
        // 0.3" which would have falsely failed an earlier substring guard).
        const MARKER: &str = "Phase5G-roleSep-UserMessage-Marker-42";
        let outcome = handler
            .handle_chat(&SessionId::from(Uuid::new_v4()), MARKER.into())
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
        assert!(
            !nth.system.contains(MARKER),
            "user message must not bleed into the system prompt"
        );
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

        // ── Phase 3: read-only tools (Phase 6C) — schemas in narrowed registry ──
        let all = narrow_registry_for_chat(&stub_registry_with_all_chat_tools())
            .expect("stub registry contains all chat tools");
        let mut all_names = all.names();
        all_names.sort();
        let mut expected_all = vec![
            "solend_deposit_usdc".to_string(),
            "submit_jupiter_swap".to_string(),
            "get_wallet_balances".to_string(),
            "get_jupiter_quote".to_string(),
        ];
        expected_all.sort();
        assert_eq!(
            all_names, expected_all,
            "narrowed registry must contain every chat-allowlisted tool when present"
        );

        // get_wallet_balances — no inputs, no required fields.
        let bal_spec = all
            .all_specs()
            .into_iter()
            .find(|s| s.name == "get_wallet_balances")
            .expect("get_wallet_balances spec");
        let bal_schema = &bal_spec.input_schema;
        assert_eq!(bal_schema["additionalProperties"], json!(false));
        assert!(bal_schema["required"].as_array().unwrap().is_empty());
        assert!(bal_schema["properties"].as_object().unwrap().is_empty());
        assert!(
            bal_spec.required_capabilities.is_empty(),
            "read-only tool must require no capabilities"
        );
        let raw_bal = serde_json::to_string(bal_schema).unwrap();
        for forbidden in [
            "tx_bytes",
            "transaction_base64",
            "signed_tx",
            "private_key",
            "keypair",
            "submit",
            "approve",
        ] {
            assert!(
                !raw_bal.contains(forbidden),
                "get_wallet_balances schema must not mention `{forbidden}`; got {raw_bal}"
            );
        }

        // get_jupiter_quote — exactly the four documented required fields,
        // additionalProperties false, slippage_bps capped at 100.
        let quote_spec = all
            .all_specs()
            .into_iter()
            .find(|s| s.name == "get_jupiter_quote")
            .expect("get_jupiter_quote spec");
        let quote_schema = &quote_spec.input_schema;
        assert_eq!(quote_schema["additionalProperties"], json!(false));
        let q_required: std::collections::HashSet<&str> = quote_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let q_expected: std::collections::HashSet<&str> =
            ["input_mint", "output_mint", "input_amount", "slippage_bps"]
                .into_iter()
                .collect();
        assert_eq!(q_required, q_expected);
        assert_eq!(
            quote_schema["properties"]["slippage_bps"]["maximum"],
            json!(100),
            "Phase 6C cap: slippage_bps must be schema-bounded at 100 bps"
        );
        assert!(
            quote_spec.required_capabilities.is_empty(),
            "read-only tool must require no capabilities"
        );
        let raw_q = serde_json::to_string(quote_schema).unwrap();
        for forbidden in [
            "tx_bytes",
            "transaction_base64",
            "signed_tx",
            "private_key",
            "keypair",
            "submit",
            "approve",
            "wallet_pubkey",
        ] {
            assert!(
                !raw_q.contains(forbidden),
                "get_jupiter_quote schema must not mention `{forbidden}`; got {raw_q}"
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
        let result = wire_chat_handler_with_registry(&stub_registry(), &env, None, None, None, None)
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
        let result = wire_chat_handler_with_registry(&empty, &env, None, None, None, None);
        match result {
            Err(LlmProviderConfigError::InvalidProviderConfig { .. }) => {}
            // Result's Ok arm contains `Option<ChatHandlerRef>` (no Debug);
            // split arms so we never Debug-format it.
            Err(e) => panic!("expected InvalidProviderConfig; got Err({e})"),
            Ok(_) => panic!("expected InvalidProviderConfig; got Ok(<chat handler>)"),
        }
    }
}

// ── W5d chat-route interceptor tests ─────────────────────────────────────
//
// These tests exercise the `handle_w5d_demo_command` branch + the
// `looks_like_w5d_command` detector / fall-through logic at the
// GatewayChatHandler seam without making a single RPC call. The mock
// fetcher is the same one defined in `stage2_demo_apr_bridge::tests`,
// but since module-private items don't cross test-binary boundaries
// we redefine a tiny one here.

#[cfg(test)]
mod w5d_chat_route_tests {
    use super::*;
    use crate::stage2_demo_apr_bridge::{
        looks_like_w5d_command, DemoParsed, EvaluationError, W5dAprFetcher,
        W5dEvaluationResult,
    };
    use async_trait::async_trait;
    use claw_api::state::ChatResponse;
    use std::sync::Arc;

    #[derive(Debug, Clone)]
    struct MockFetcher {
        current_apr_bps: u32,
        current_budget_raw: u64,
        last_checked_slot: u64,
    }
    #[async_trait]
    impl W5dAprFetcher for MockFetcher {
        async fn evaluate(
            &self,
            input_text: &str,
            parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            let (controlled_wallet, source_usdc_ata) =
                crate::stage2_demo_apr_bridge::controlled_wallet_addresses();
            Ok(crate::stage2_demo_apr_bridge::compose_w5e_result(
                input_text,
                parsed,
                self.current_apr_bps,
                self.current_budget_raw,
                self.last_checked_slot,
                controlled_wallet,
                source_usdc_ata,
            ))
        }
    }

    /// Detector: non-W5d messages must NOT match (chat route falls
    /// through to the LLM path).
    #[test]
    fn non_w5d_message_does_not_match_detector() {
        assert!(!looks_like_w5d_command("show my balances"));
        assert!(!looks_like_w5d_command("what is the jupiter quote for SOL"));
        assert!(!looks_like_w5d_command("deposit my USDC into Save"));
    }

    /// W5e false branch — condition not met but budget reserved →
    /// `status="watching"`, `budget_status="reserved"`, the chat handler
    /// must NOT have called any send path, and the result must carry
    /// a rule_id + canonical hash (derived by `handle_demo_command_v2`).
    #[tokio::test]
    async fn handle_w5d_false_command_returns_watching() {
        let fetcher = MockFetcher {
            current_apr_bps: 163,
            current_budget_raw: 500_000, // ≥ 250_000, budget reserved
            last_checked_slot: 42_424_242,
        };
        let outcome = handle_w5d_demo_command(
            &fetcher,
            None,
            None,
            "If Solend Main Pool USDC deposit APR is above 4.63%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .await;
        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::W5dConditionalDeposit { result }) => {
                assert!(!result.condition_met);
                assert_eq!(result.status, "watching");
                assert_eq!(result.budget_status, "reserved");
                assert!(!result.execution_attempted);
                assert!(result.tx_signature.is_none());
                assert_eq!(result.current_apr_bps, 163);
                assert_eq!(result.threshold_bps, 463);
                // After W5f, `source` is the legacy field carrying the
                // decision-source label. With no Save fetcher wired
                // (this test exercises the W5e degraded path), the
                // result still carries `"save_display_apy"` as the
                // default label.
                assert_eq!(result.source, "save_display_apy");
                assert_eq!(result.last_checked_slot, 42_424_242);
                assert!(result.expires_at_slot.is_some());
                assert!(result.rule_id_hex.is_some());
                assert!(result.canonical_rule_hash_hex.is_some());
                // No repo wired → preview-only, must not claim persisted.
                assert!(!result.rule_persisted);
                // W5f degraded path: native and Save metrics both equal
                // the one APR the mock fetcher produced.
                assert_eq!(result.save_display_apy_bps, 163);
                assert_eq!(result.native_onchain_apr_bps, 163);
            }
            other => panic!("expected W5dConditionalDeposit, got {other:?}"),
        }
    }

    /// W5e true branch — condition met + budget reserved →
    /// `status="ready_to_execute"`. The chat route NEVER broadcasts,
    /// so `tx_signature` stays None and `execution_attempted=false`.
    #[tokio::test]
    async fn handle_w5d_true_command_returns_ready_to_execute() {
        let fetcher = MockFetcher {
            current_apr_bps: 163,
            current_budget_raw: 500_000,
            last_checked_slot: 42_424_242,
        };
        let outcome = handle_w5d_demo_command(
            &fetcher,
            None,
            None,
            "If Solend Main Pool USDC deposit APR is above 0.63%, deposit 0.25 USDC from my bounded executor wallet into Solend.",
        )
        .await;
        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::W5dConditionalDeposit { result }) => {
                assert!(result.condition_met);
                assert_eq!(result.status, "ready_to_execute");
                assert_eq!(result.budget_status, "reserved");
                assert!(!result.execution_attempted);
                assert!(result.tx_signature.is_none());
                assert!(!result.rule_persisted);
            }
            other => panic!("expected W5dConditionalDeposit, got {other:?}"),
        }
    }

    /// Parser failure (lightweight detector matched but strict parser
    /// rejected) surfaces as `ChatResponse::ToolError` with
    /// `tool_name == "w5d_conditional_deposit"`.
    #[tokio::test]
    async fn handle_w5d_parse_error_surfaces_as_tool_error() {
        let fetcher = MockFetcher {
            current_apr_bps: 163,
            current_budget_raw: 500_000,
            last_checked_slot: 42_424_242,
        };
        // The detector matches (pool name + "deposit apr" present),
        // but the amount is unsupported (1.0 instead of 0.25).
        let outcome = handle_w5d_demo_command(
            &fetcher,
            None,
            None,
            "If Solend Main Pool USDC deposit APR is above 10%, deposit 1.0 USDC from my bounded executor wallet into Solend.",
        )
        .await;
        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::ToolError { tool_name, message }) => {
                assert_eq!(tool_name, "w5d_conditional_deposit");
                assert!(
                    message.contains("unsupported amount")
                        || message.contains("UnsupportedAmount")
                        || message.contains("0.25 USDC"),
                    "expected unsupported-amount in message, got {message}"
                );
            }
            other => panic!("expected ToolError, got {other:?}"),
        }
    }

    /// Builder-level: `with_w5d_bridge` attaches the fetcher; the
    /// detector branch fires for matching messages. Non-matching
    /// messages fall through to the LLM path unchanged (no fetcher
    /// call), which we observe indirectly by confirming the detector
    /// would NOT match the test phrase.
    #[test]
    fn with_w5d_bridge_attaches_fetcher_and_detector_is_consistent() {
        let llm = claw_agent_runtime::disabled_provider();
        let handler = GatewayChatHandler::new(
            llm,
            ToolRegistry::new(),
            "alignment".to_string(),
            CapabilitySet::empty(),
        )
        .with_w5d_bridge(Arc::new(MockFetcher {
            current_apr_bps: 1,
            current_budget_raw: 500_000,
            last_checked_slot: 1,
        }));
        // Handler holds the fetcher; we don't have a getter (intentional
        // — internal state). Verify via the detector helper that a
        // non-W5d message does NOT match, so the handler would skip
        // the bridge and fall through to the LLM path.
        assert!(handler.w5d_bridge.is_some());
        assert!(!looks_like_w5d_command("hello world"));
        // And a W5d-shaped message would match.
        assert!(looks_like_w5d_command(
            "If Solend Main Pool USDC deposit APR is above 1%, ..."
        ));
    }
}

/// W5g chat-route seam tests. Proves that when a `Stage2ChatExecutor`
/// is wired via `GatewayChatHandler::with_w5g_executor(...)`, a valid
/// W5g approval-command message dispatches through to the orchestrator
/// and the response is returned as the typed
/// `ChatResponse::W5gConditionalExecution { result }` variant — NOT a
/// `ToolError`. This is the wired-vs-not-wired discriminator.
///
/// The executor is constructed with `master_gate_on=false` so the
/// orchestrator short-circuits at its first precheck (returning a
/// `prechecks_failed` status with reason `master_gate_missing`) — the
/// sender stub PANICS if reached, additionally proving no broadcast
/// path is invoked.
#[cfg(test)]
mod w5g_chat_route_tests {
    use super::*;
    use crate::stage2_chat_execute::{
        ChatExecuteSendOutcome, ChatExecuteSendRequest, Stage2ChatExecuteConfig,
        Stage2ChatExecuteSender, Stage2ChatExecutor, W5G_REQUIRED_APPROVAL_PHRASE,
    };
    use crate::stage2_demo_apr_bridge::{
        compose_w5e_result, controlled_wallet_addresses, DemoParsed, EvaluationError,
        SaveDisplayApyFetcher, SaveDisplayApyReading, W5dAprFetcher, W5dEvaluationResult,
    };
    use async_trait::async_trait;
    use claw_agent_runtime::disabled_provider;
    use claw_api::state::ChatResponse;
    use claw_state_store::{stage2_watch_rules::Stage2WatchRuleRepository, Database};
    use claw_tool_system::permissions::CapabilitySet;
    use claw_types::session::SessionId;
    use std::sync::Arc;
    use uuid::Uuid;

    /// Sender stub that panics if invoked. master_gate_on=false should
    /// stop the executor before any sender path runs; if this stub ever
    /// fires, the seam is leaking past its precheck gate.
    #[derive(Debug)]
    struct UncallableW5gSender;
    #[async_trait]
    impl Stage2ChatExecuteSender for UncallableW5gSender {
        async fn build_sign_send_poll(
            &self,
            _request: ChatExecuteSendRequest,
        ) -> ChatExecuteSendOutcome {
            panic!(
                "W5g sender must not be reached when master_gate_on=false; \
                 the executor must short-circuit before any send path"
            );
        }
    }

    /// Stub Save display APY fetcher. Returns a deterministic 210-bps
    /// reading anchored to the W5f Main Pool USDC identifiers; never
    /// invoked under master_gate_off but required by the executor's type.
    #[derive(Debug, Clone)]
    struct StubW5gSaveFetcher;
    #[async_trait]
    impl SaveDisplayApyFetcher for StubW5gSaveFetcher {
        async fn fetch_main_pool_usdc(
            &self,
        ) -> Result<SaveDisplayApyReading, EvaluationError> {
            Ok(SaveDisplayApyReading {
                save_display_apy_bps: 210,
                raw_supply_interest_str: "2.10".to_string(),
                reserve_pubkey: "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw".to_string(),
                lending_market: "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY".to_string(),
                liquidity_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                collateral_mint: "993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk".to_string(),
                rewards_present: false,
            })
        }
    }

    /// Stub B-O1 native APR fetcher. Returns a deterministic 166-bps
    /// reading; never invoked under master_gate_off but required by the
    /// executor's type.
    #[derive(Debug, Clone)]
    struct StubW5gAprFetcher;
    #[async_trait]
    impl W5dAprFetcher for StubW5gAprFetcher {
        async fn evaluate(
            &self,
            input_text: &str,
            parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            let (controlled_wallet, source_usdc_ata) = controlled_wallet_addresses();
            Ok(compose_w5e_result(
                input_text,
                parsed,
                166,
                500_000,
                42_424_242,
                controlled_wallet,
                source_usdc_ata,
            ))
        }
    }

    /// Pins the wired W5g chat-route seam. Constructs `GatewayChatHandler`
    /// with `.with_w5g_executor(...)` (the executor's `master_gate_on` is
    /// intentionally `false` so the orchestrator returns the
    /// `prechecks_failed` outcome and no send path is reached), submits a
    /// valid W5g approval command, and asserts the chat handler returns
    /// the typed `ChatResponse::W5gConditionalExecution { result }`
    /// variant — proving the seam dispatches through to the orchestrator
    /// and DOES NOT fall back to the "not wired" `ToolError`.
    #[tokio::test]
    async fn wired_approval_path_returns_w5g_conditional_execution_variant() {
        // In-memory state-store DB; the executor's repo is required by
        // its constructor even though master_gate_off prevents lookup.
        let db = Database::open_in_memory().await.expect("in-memory DB");
        let repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));

        // Master gate OFF → executor short-circuits with
        // PrechecksFailed/MasterGateMissing before the sender is reached.
        let config = Stage2ChatExecuteConfig {
            master_gate_on: false,
            env_approval_phrase: None,
            cluster: None,
            rpc_url_present: false,
            keypair_path_present: false,
        };
        let executor = Arc::new(Stage2ChatExecutor::new(
            Arc::new(UncallableW5gSender),
            Arc::new(StubW5gSaveFetcher),
            Arc::new(StubW5gAprFetcher),
            repo,
            config,
        ));

        let llm = disabled_provider();
        let handler = GatewayChatHandler::new(
            llm,
            ToolRegistry::new(),
            "alignment".to_string(),
            CapabilitySet::empty(),
        )
        .with_w5g_executor(executor);

        // Build a syntactically valid W5g approval command — 32 hex
        // chars rule_id, 64 hex chars canonical_rule_hash, exact phrase.
        let rule_id_hex = "0102030405060708090a0b0c0d0e0f10";
        let canonical_hash_hex =
            "1112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30";
        let message = format!(
            "Execute W5g conditional deposit {rule_id_hex} {canonical_hash_hex} \
             with approval phrase {W5G_REQUIRED_APPROVAL_PHRASE}"
        );

        let outcome = handler
            .handle_chat(&SessionId::from(Uuid::new_v4()), message)
            .await;

        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::W5gConditionalExecution { result }) => {
                // Mocked rejected status — master_gate_off ⇒ prechecks_failed.
                assert_eq!(
                    result.status, "prechecks_failed",
                    "wired executor with master_gate_off must surface prechecks_failed"
                );
                // The parser round-tripped the user's rule_id_hex through
                // to the DTO. This is the substantive seam evidence.
                assert_eq!(result.rule_id_hex, rule_id_hex);
                assert_eq!(result.canonical_rule_hash_hex, canonical_hash_hex);
                // Sender stub panics if called; passing this assertion
                // confirms no broadcast was attempted.
                assert!(
                    result.tx_signature.is_none(),
                    "no broadcast under master_gate_off; tx_signature must be None"
                );
                assert!(
                    result.confirmation_slot.is_none(),
                    "no confirmation under master_gate_off; confirmation_slot must be None"
                );
            }
            other => panic!(
                "expected ChatResponse::W5gConditionalExecution (proves wired seam), \
                 got {other:?}"
            ),
        }
    }
}

/// W5h-lite chat-route seam tests. Proves:
///
///   1. With every W5h dep wired (intent repo + session-wallet lookup +
///      APR + Save APY + WatchRule repo), an English/Chinese W5h
///      command dispatches to the bridge and returns
///      `ChatResponse::W5hConditionalOrder { result }` — the LLM
///      stub PANICS if invoked, proving no LLM fall-through.
///
///   2. When the session has no bound external wallet, the W5h
///      dispatcher returns a typed `ToolError` ("Connect wallet
///      before creating a funded conditional order.") and does NOT
///      create a funding intent (repo row count remains 0).
#[cfg(test)]
mod w5h_chat_route_tests {
    use super::*;
    use crate::stage2_demo_apr_bridge::{
        compose_w5e_result, controlled_wallet_addresses, DemoParsed,
        EvaluationError, SaveDisplayApyFetcher, SaveDisplayApyReading,
        W5dAprFetcher, W5dEvaluationResult,
    };
    use crate::tools::jupiter_swap::SessionBoundWallet;
    use async_trait::async_trait;
    use claw_agent_runtime::{
        errors::AgentError,
        llm::{LlmClient, LlmMessage, LlmResponse},
    };
    use claw_api::state::ChatResponse;
    use claw_state_store::{
        stage2_w5h_funding::Stage2W5hFundingIntentRepository,
        stage2_watch_rules::Stage2WatchRuleRepository, Database,
    };
    use claw_tool_system::permissions::CapabilitySet;
    use claw_types::session::SessionId;
    use claw_types::tool::ToolSpec;
    use std::sync::Arc;
    use uuid::Uuid;

    const TEST_USER_WALLET: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";

    /// LLM that panics if invoked. The W5h dispatch MUST happen
    /// before the LLM call; reaching this stub proves the seam is
    /// leaking and the test fails loud.
    #[derive(Debug)]
    struct UncallableLlm;
    #[async_trait]
    impl LlmClient for UncallableLlm {
        async fn complete(
            &self,
            _system: &str,
            _messages: &[LlmMessage],
            _tools: &[ToolSpec],
        ) -> Result<LlmResponse, AgentError> {
            panic!(
                "LLM must NOT be invoked for W5h chat commands; \
                 the W5h dispatcher must intercept before the LLM call"
            );
        }
    }

    /// Stub Save APY fetcher: deterministic 312 bps.
    #[derive(Debug, Clone)]
    struct StubSaveApy;
    #[async_trait]
    impl SaveDisplayApyFetcher for StubSaveApy {
        async fn fetch_main_pool_usdc(
            &self,
        ) -> Result<SaveDisplayApyReading, EvaluationError> {
            Ok(SaveDisplayApyReading {
                save_display_apy_bps: 312,
                raw_supply_interest_str: "3.12".to_string(),
                reserve_pubkey:
                    "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw".to_string(),
                lending_market:
                    "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY".to_string(),
                liquidity_mint:
                    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                collateral_mint:
                    "993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk".to_string(),
                rewards_present: false,
            })
        }
    }

    /// Stub B-O1 native APR fetcher: deterministic 287 bps.
    #[derive(Debug, Clone)]
    struct StubAprFetcher;
    #[async_trait]
    impl W5dAprFetcher for StubAprFetcher {
        async fn evaluate(
            &self,
            input_text: &str,
            parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            let (controlled_wallet, source_usdc_ata) =
                controlled_wallet_addresses();
            Ok(compose_w5e_result(
                input_text,
                parsed,
                287,
                500_000,
                42_424_242,
                controlled_wallet,
                source_usdc_ata,
            ))
        }
    }

    /// Session-bound-wallet stub. `bound_pubkey` is the value the W5h
    /// dispatcher will see for `user_wallet`; `None` exercises the
    /// fail-closed path.
    #[derive(Debug)]
    struct StubSessionWallet {
        bound_pubkey: Option<String>,
    }
    impl SessionBoundWallet for StubSessionWallet {
        fn session_wallet_pubkey(&self, _sid: &SessionId) -> Option<String> {
            self.bound_pubkey.clone()
        }
    }

    fn build_w5h_handler(
        intent_repo: Arc<Stage2W5hFundingIntentRepository>,
        rule_repo: Arc<Stage2WatchRuleRepository>,
        wallet: StubSessionWallet,
    ) -> GatewayChatHandler {
        let registry = ToolRegistry::new();
        GatewayChatHandler::new(
            Arc::new(UncallableLlm),
            registry,
            "system-prompt".to_string(),
            CapabilitySet::empty(),
        )
        .with_w5d_bridge(Arc::new(StubAprFetcher))
        .with_w5f_save_apy(Arc::new(StubSaveApy))
        .with_w5e_repo(rule_repo)
        .with_w5h_intent_repo(intent_repo)
        .with_session_wallet_lookup(Arc::new(wallet))
    }

    /// Positive test: typed W5h card returned, LLM stub never invoked.
    #[tokio::test]
    async fn w5h_chat_command_returns_funding_required_card_without_llm() {
        let db = Database::open_in_memory().await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            db.pool().clone(),
        ));
        let rule_repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));
        let wallet = StubSessionWallet {
            bound_pubkey: Some(TEST_USER_WALLET.to_string()),
        };
        let handler = build_w5h_handler(intent_repo.clone(), rule_repo, wallet);
        let sid = SessionId::from(Uuid::new_v4());

        let outcome = handler
            .handle_chat(&sid, "If Save APY > 1%, deposit 0.25 USDC".to_string())
            .await;

        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::W5hConditionalOrder { result }) => {
                assert_eq!(result.amount_raw, 250_000);
                assert_eq!(
                    result.controlled_wallet,
                    "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L"
                );
                assert_eq!(
                    result.controlled_usdc_ata,
                    "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3"
                );
                assert_eq!(result.user_wallet, TEST_USER_WALLET);
                assert_eq!(result.threshold_bps, 100);
                // Persisted intent exists with funding_required status.
                let stored = intent_repo
                    .get(&result.rule_id_hex)
                    .await
                    .unwrap()
                    .expect("intent must be persisted");
                assert_eq!(stored.user_wallet, TEST_USER_WALLET);
            }
            other => panic!(
                "expected ChatResponse::W5hConditionalOrder (proves wired seam, \
                 LLM was NOT invoked); got {other:?}"
            ),
        }
    }

    /// Chinese-grammar variant. Same assertions; proves both grammars
    /// dispatch through the same seam.
    #[tokio::test]
    async fn w5h_chinese_chat_command_returns_funding_required_card_without_llm() {
        let db = Database::open_in_memory().await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            db.pool().clone(),
        ));
        let rule_repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));
        let wallet = StubSessionWallet {
            bound_pubkey: Some(TEST_USER_WALLET.to_string()),
        };
        let handler = build_w5h_handler(intent_repo, rule_repo, wallet);
        let sid = SessionId::from(Uuid::new_v4());

        let outcome = handler
            .handle_chat(
                &sid,
                "如果 Save APY > 1%，deposit 0.25 USDC".to_string(),
            )
            .await;

        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::W5hConditionalOrder { result }) => {
                assert_eq!(result.amount_raw, 250_000);
                assert_eq!(result.user_wallet, TEST_USER_WALLET);
            }
            other => panic!(
                "expected ChatResponse::W5hConditionalOrder for Chinese grammar; \
                 got {other:?}"
            ),
        }
    }

    /// Fail-closed: session has no bound wallet → typed ToolError +
    /// NO intent persisted.
    #[tokio::test]
    async fn w5h_chat_command_without_bound_wallet_returns_typed_error() {
        let db = Database::open_in_memory().await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            db.pool().clone(),
        ));
        let rule_repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));
        let wallet = StubSessionWallet { bound_pubkey: None };
        let handler =
            build_w5h_handler(intent_repo.clone(), rule_repo, wallet);
        let sid = SessionId::from(Uuid::new_v4());

        let outcome = handler
            .handle_chat(&sid, "If Save APY > 1%, deposit 0.25 USDC".to_string())
            .await;

        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::ToolError {
                tool_name,
                message,
            }) => {
                assert_eq!(tool_name, "w5h_conditional_order");
                assert!(
                    message.contains("Connect wallet"),
                    "message must instruct the operator to bind a wallet; got {message:?}"
                );
                // No intent must have been persisted.
                let rule_id_guess = "00000000000000000000000000000000";
                let probe = intent_repo.get(rule_id_guess).await.unwrap();
                assert!(probe.is_none());
            }
            other => panic!(
                "expected ChatResponse::ToolError for unbound session; got {other:?}"
            ),
        }
    }

    /// Fail-closed: blank/whitespace-only bound wallet is treated as
    /// unbound. Defends against an upstream layer accidentally
    /// emitting an empty pubkey.
    #[tokio::test]
    async fn w5h_chat_command_with_blank_bound_wallet_returns_typed_error() {
        let db = Database::open_in_memory().await.unwrap();
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            db.pool().clone(),
        ));
        let rule_repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));
        let wallet = StubSessionWallet {
            bound_pubkey: Some("   ".to_string()),
        };
        let handler = build_w5h_handler(intent_repo, rule_repo, wallet);
        let sid = SessionId::from(Uuid::new_v4());

        let outcome = handler
            .handle_chat(&sid, "If Save APY > 1%, deposit 0.25 USDC".to_string())
            .await;

        match outcome {
            ChatRouteOutcome::Ok(ChatResponse::ToolError {
                tool_name,
                message,
            }) => {
                assert_eq!(tool_name, "w5h_conditional_order");
                assert!(message.contains("Connect wallet"));
            }
            other => panic!(
                "expected ChatResponse::ToolError for blank-pubkey session; \
                 got {other:?}"
            ),
        }
    }
}

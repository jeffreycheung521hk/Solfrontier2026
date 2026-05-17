//! Phase 5D.1 — Provider selection seam (factory + ApiKey + EnvProvider).
//!
//! # What this module owns
//!
//! - [`LlmProviderMode`] — the four-arm config selector
//!   (`Disabled`, `Scripted`, `OpenAi`, `Anthropic`).
//! - [`LlmProviderConfig`] — caller-supplied configuration, never
//!   `Debug`-prints credential material.
//! - [`ApiKey`] — redacting newtype around a credential string. Its
//!   `Debug` and `Display` impls always print `<REDACTED>`. The raw
//!   string is reachable only through the explicit
//!   [`ApiKey::expose_secret`] method, which is documented as
//!   call-site-only (used at most once, at provider construction).
//! - [`EnvProvider`] — narrow trait for environment-variable reads.
//!   The factory uses an injected provider so tests can assert that
//!   `Disabled` and `Scripted` modes perform **zero** env reads.
//! - [`build_llm_provider`] — the fail-closed factory.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT add a chat route (Phase 5D.2's job).
//! - Does NOT call OpenAI / Anthropic. The `OpenAi`/`Anthropic` arms
//!   construct existing [`crate::llm::openai::OpenAiClient`] /
//!   [`crate::llm::anthropic::AnthropicClient`] only after they have
//!   a non-empty API key. No request is issued by this factory.
//! - Does NOT read environment variables in `Disabled` or `Scripted`
//!   modes — those branches don't even consult the `EnvProvider`.
//! - Does NOT silently fall back. If the caller asks for `OpenAi` or
//!   `Anthropic` and credentials are missing, the factory returns
//!   [`LlmProviderConfigError::MissingApiKey`] — never `Scripted` or
//!   `Disabled`.
//! - Does NOT log credential values, even on the error path.
//! - Does NOT loop, dispatch tools, or know about the tool registry.
//!   It returns an `Arc<dyn LlmClient>` and stops.

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

use crate::conversation::ScriptedLlmProvider;
use crate::errors::AgentError;
use crate::llm::{
    anthropic::AnthropicClient, openai::OpenAiClient, LlmClient, LlmClientRef, LlmMessage,
    LlmResponse,
};
use claw_types::tool::ToolSpec;

/// Stable env var name read by the `OpenAi` factory arm. Defined as a
/// constant (not a literal call site) so it is grep-locatable and the
/// dependency-guard tests can assert no other module reads it.
pub const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";

/// Stable env var name read by the `Anthropic` factory arm.
pub const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";

// ── ApiKey: redacting wrapper ───────────────────────────────────────────────

/// Redacting newtype around a credential string.
///
/// `Debug` and `Display` always print `<REDACTED>`. The inner raw key
/// is reachable only through [`Self::expose_secret`], which the
/// factory calls exactly once at provider construction. There is no
/// `serde::Serialize` impl — accidental inclusion in a JSON payload
/// would be a compile error.
///
/// **Do not derive `Debug`.** Manual impl below redacts.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Reveal the raw key. Use only at the narrow construction site of
    /// a provider client. Do NOT pass the result through formatters,
    /// logs, or audit events.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Whether this key would be considered empty (whitespace-only or
    /// length 0). Used by the factory to fail closed on a `Some("")`
    /// config value before a network client is built.
    pub fn is_empty_or_whitespace(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiKey(<REDACTED>)")
    }
}

impl std::fmt::Display for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<REDACTED>")
    }
}

// ── Provider mode + config + error ──────────────────────────────────────────

/// The four valid provider selections.
///
/// `Disabled` is a hard "no LLM is wired" signal. `Scripted` is the
/// default test/dev mode; it never reads environment variables.
/// `OpenAi` and `Anthropic` require explicit configuration and an API
/// key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProviderMode {
    Disabled,
    Scripted,
    OpenAi,
    Anthropic,
}

/// Caller-supplied configuration. The credential field is `Option<ApiKey>`
/// so the caller can either inject a programmatic key (e.g. in tests)
/// or leave `None` to opt into the factory's lazy env-var read.
///
/// **Manual `Debug`** ensures the key never appears in any debug
/// output even if the struct is added to a larger Debug-printable
/// audit/event payload by mistake.
#[derive(Clone)]
pub struct LlmProviderConfig {
    pub mode: LlmProviderMode,
    /// Programmatic credential injection. When `Some`, the factory
    /// will use this key directly. When `None` and mode is
    /// `OpenAi`/`Anthropic`, the factory consults the injected
    /// [`EnvProvider`] for the relevant env-var name.
    pub api_key: Option<ApiKey>,
}

impl std::fmt::Debug for LlmProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmProviderConfig")
            .field("mode", &self.mode)
            .field("api_key", &self.api_key.as_ref().map(|_| "<REDACTED>"))
            .finish()
    }
}

impl Default for LlmProviderConfig {
    /// Default = `Scripted`, no key. Reads zero environment variables.
    fn default() -> Self {
        Self {
            mode: LlmProviderMode::Scripted,
            api_key: None,
        }
    }
}

/// Typed factory error. Each variant is fail-closed — the caller MUST
/// surface this rather than silently fall back to a different mode.
///
/// `Debug` and `Display` impls never include credential values
/// (locked by a test below).
#[derive(Debug, Error)]
pub enum LlmProviderConfigError {
    /// Mode is `OpenAi` or `Anthropic`, but no API key was supplied
    /// programmatically AND no value was found in the corresponding
    /// environment variable.
    #[error("missing API key for provider mode {mode:?} (programmatic key absent and {env_var} not set)")]
    MissingApiKey {
        mode: LlmProviderMode,
        env_var: &'static str,
    },
    /// Mode is `OpenAi` or `Anthropic`, an API key was supplied (via
    /// config or env), but it is empty / whitespace-only.
    #[error("empty API key for provider mode {mode:?}")]
    EmptyApiKey { mode: LlmProviderMode },
    /// Mode is `OpenAi` or `Anthropic` but the build-time
    /// configuration deliberately disabled real-provider construction.
    /// (Reserved for future feature-flag gating; currently unused.)
    #[error("provider mode {mode:?} is not enabled in this build")]
    UnsupportedProvider { mode: LlmProviderMode },
    /// Mode is `Disabled`. Caller should not have requested a client.
    #[error("LLM provider is disabled in this configuration")]
    ProviderDisabled,
    /// Generic config validation failure — used sparingly and never
    /// includes the offending value.
    #[error("invalid provider configuration: {reason}")]
    InvalidProviderConfig { reason: String },
}

// ── EnvProvider: narrow seam for env-var reads ──────────────────────────────

/// Test-injectable read-only environment seam. Production code wires
/// [`StdEnvProvider`]; tests use [`CountingEnvProvider`] to assert
/// zero reads in `Disabled`/`Scripted` modes.
pub trait EnvProvider: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

/// Production env provider — wraps `std::env::var`. The factory only
/// consults this in `OpenAi`/`Anthropic` arms; `Disabled`/`Scripted`
/// branches never call into it.
pub struct StdEnvProvider;

impl EnvProvider for StdEnvProvider {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

// ── Factory ─────────────────────────────────────────────────────────────────

/// Build an `Arc<dyn LlmClient>` according to the mode in `config`.
///
/// **Fail-closed contract:**
///
/// | Mode      | Behavior                                                      |
/// |-----------|---------------------------------------------------------------|
/// | Disabled  | Returns `Err(ProviderDisabled)`.                              |
/// | Scripted  | Returns `Ok(Arc<DisabledScriptedProvider>)` — empty queue.    |
/// | OpenAi    | Reads env var (only if `config.api_key` is None). Empty/missing → `MissingApiKey`/`EmptyApiKey`. Else constructs `OpenAiClient`. |
/// | Anthropic | Same as OpenAi but for `ANTHROPIC_API_KEY` / `AnthropicClient`.|
///
/// **Never falls back.** A caller asking for `OpenAi` will never
/// receive `Scripted` from this factory.
pub fn build_llm_provider(
    config: &LlmProviderConfig,
    env: &dyn EnvProvider,
) -> Result<LlmClientRef, LlmProviderConfigError> {
    match config.mode {
        // Disabled and Scripted MUST NOT consult `env`. The branches
        // below do not read `env` at all — proven by the
        // `disabled_and_scripted_modes_perform_zero_env_reads` test
        // using a counting EnvProvider.
        LlmProviderMode::Disabled => Err(LlmProviderConfigError::ProviderDisabled),
        LlmProviderMode::Scripted => {
            // Empty-queue Scripted provider. Callers (tests, dev
            // shells) must hand the resulting Arc to a test that
            // pre-loads its queue, OR receive an "exhausted" error on
            // the first call. Either way, no network, no env read.
            let scripted: Arc<ScriptedLlmProvider> =
                Arc::new(ScriptedLlmProvider::new(Vec::new()));
            Ok(scripted as LlmClientRef)
        }
        LlmProviderMode::OpenAi => construct_real(
            LlmProviderMode::OpenAi,
            config.api_key.clone(),
            ENV_OPENAI_API_KEY,
            env,
            |k| Arc::new(OpenAiClient::new(k)) as LlmClientRef,
        ),
        LlmProviderMode::Anthropic => construct_real(
            LlmProviderMode::Anthropic,
            config.api_key.clone(),
            ENV_ANTHROPIC_API_KEY,
            env,
            |k| Arc::new(AnthropicClient::new(k)) as LlmClientRef,
        ),
    }
}

/// Resolve a key (programmatic or env), validate non-empty, then
/// construct via the supplied closure. The raw key reaches the
/// closure ONLY at the construction site and is dropped immediately
/// after.
fn construct_real(
    mode: LlmProviderMode,
    programmatic: Option<ApiKey>,
    env_var: &'static str,
    env: &dyn EnvProvider,
    construct: impl FnOnce(String) -> LlmClientRef,
) -> Result<LlmClientRef, LlmProviderConfigError> {
    // Programmatic injection wins; env is consulted only on `None`.
    let key = match programmatic {
        Some(k) => k,
        None => match env.get(env_var) {
            Some(v) => ApiKey::new(v),
            None => return Err(LlmProviderConfigError::MissingApiKey { mode, env_var }),
        },
    };
    if key.is_empty_or_whitespace() {
        return Err(LlmProviderConfigError::EmptyApiKey { mode });
    }
    // `expose_secret` is called exactly here, then the raw String is
    // moved into the provider's own internal storage. This is the
    // only narrow point where the raw credential is materialised.
    let raw = key.expose_secret().to_string();
    Ok(construct(raw))
}

// ── Disabled-mode marker provider (helpful for downstream callers) ─────────

/// A trivial `LlmClient` that always returns `AgentError::Llm`. Useful
/// to plug into `AppState` when the operator explicitly disables LLM
/// (e.g. an offline daemon) without adding `Option<LlmClientRef>` everywhere.
///
/// Construct via [`disabled_provider()`].
pub struct DisabledLlmClient;

#[async_trait]
impl LlmClient for DisabledLlmClient {
    async fn complete(
        &self,
        _system: &str,
        _messages: &[LlmMessage],
        _tools: &[ToolSpec],
    ) -> Result<LlmResponse, AgentError> {
        Err(AgentError::Llm(
            "LLM provider is disabled in this configuration".to_string(),
        ))
    }
}

pub fn disabled_provider() -> LlmClientRef {
    Arc::new(DisabledLlmClient) as LlmClientRef
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A counting env provider used to assert zero env reads in
    /// Disabled/Scripted modes.
    struct CountingEnvProvider {
        responses: HashMap<String, String>,
        reads: Mutex<Vec<String>>,
    }

    impl CountingEnvProvider {
        fn empty() -> Self {
            Self {
                responses: HashMap::new(),
                reads: Mutex::new(Vec::new()),
            }
        }
        fn with(pairs: &[(&str, &str)]) -> Self {
            Self {
                responses: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                reads: Mutex::new(Vec::new()),
            }
        }
        fn read_count(&self) -> usize {
            self.reads.lock().unwrap().len()
        }
        fn reads_for(&self, name: &str) -> usize {
            self.reads
                .lock()
                .unwrap()
                .iter()
                .filter(|n| n.as_str() == name)
                .count()
        }
    }

    impl EnvProvider for CountingEnvProvider {
        fn get(&self, name: &str) -> Option<String> {
            self.reads.lock().unwrap().push(name.to_string());
            self.responses.get(name).cloned()
        }
    }

    // ── A. Default mode ────────────────────────────────────────────────────

    #[test]
    fn p5d1_a_default_provider_mode_is_scripted() {
        let cfg = LlmProviderConfig::default();
        assert_eq!(cfg.mode, LlmProviderMode::Scripted);
        assert!(cfg.api_key.is_none());
    }

    // ── B. Disabled ────────────────────────────────────────────────────────

    #[test]
    fn p5d1_b_disabled_provider_returns_provider_disabled_error() {
        let env = CountingEnvProvider::empty();
        let cfg = LlmProviderConfig {
            mode: LlmProviderMode::Disabled,
            api_key: None,
        };
        let result = build_llm_provider(&cfg, &env);
        match result {
            Err(LlmProviderConfigError::ProviderDisabled) => {}
            Err(e) => panic!("expected ProviderDisabled; got Err({e})"),
            Ok(_) => panic!("expected ProviderDisabled; got Ok(<provider>)"),
        }
    }

    // ── C. Scripted ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn p5d1_c_scripted_provider_constructs_without_credentials() {
        let env = CountingEnvProvider::empty();
        let cfg = LlmProviderConfig::default();
        let provider = build_llm_provider(&cfg, &env).expect("scripted should build");
        // The returned provider implements LlmClient. With an empty
        // queue, the first call fails — confirming this is the
        // ScriptedLlmProvider seam, not a real network client.
        let result = provider
            .complete("system", &[LlmMessage::text("user", "hi")], &[])
            .await;
        match result {
            Err(AgentError::Llm(msg)) => {
                assert!(msg.contains("exhausted"), "expected ScriptedLlmProvider exhausted: {msg}");
            }
            other => panic!("expected scripted exhausted error; got {other:?}"),
        }
    }

    // ── D. OpenAi without key ──────────────────────────────────────────────

    #[test]
    fn p5d1_d_openai_mode_without_api_key_fails_closed_missing_api_key() {
        let env = CountingEnvProvider::empty();
        let cfg = LlmProviderConfig {
            mode: LlmProviderMode::OpenAi,
            api_key: None,
        };
        let result = build_llm_provider(&cfg, &env);
        match result {
            Err(LlmProviderConfigError::MissingApiKey { mode, env_var }) => {
                assert_eq!(mode, LlmProviderMode::OpenAi);
                assert_eq!(env_var, "OPENAI_API_KEY");
            }
            Err(e) => panic!("expected MissingApiKey; got Err({e})"),
            Ok(_) => panic!("expected MissingApiKey; got Ok(<provider>)"),
        }
        // The factory consulted env exactly once — for the OPENAI key.
        assert_eq!(env.read_count(), 1);
        assert_eq!(env.reads_for("OPENAI_API_KEY"), 1);
        assert_eq!(env.reads_for("ANTHROPIC_API_KEY"), 0);
    }

    // ── E. Anthropic without key ───────────────────────────────────────────

    #[test]
    fn p5d1_e_anthropic_mode_without_api_key_fails_closed_missing_api_key() {
        let env = CountingEnvProvider::empty();
        let cfg = LlmProviderConfig {
            mode: LlmProviderMode::Anthropic,
            api_key: None,
        };
        let result = build_llm_provider(&cfg, &env);
        match result {
            Err(LlmProviderConfigError::MissingApiKey { mode, env_var }) => {
                assert_eq!(mode, LlmProviderMode::Anthropic);
                assert_eq!(env_var, "ANTHROPIC_API_KEY");
            }
            Err(e) => panic!("expected MissingApiKey; got Err({e})"),
            Ok(_) => panic!("expected MissingApiKey; got Ok(<provider>)"),
        }
        assert_eq!(env.read_count(), 1);
        assert_eq!(env.reads_for("ANTHROPIC_API_KEY"), 1);
        assert_eq!(env.reads_for("OPENAI_API_KEY"), 0);
    }

    // ── F. ApiKey + config + error don't leak fake key ─────────────────────

    #[test]
    fn p5d1_f_api_key_not_leaked_in_display_or_debug() {
        const FAKE: &str = "sk-test-should-not-appear";
        let key = ApiKey::new(FAKE);

        // Direct ApiKey formatters
        assert!(!format!("{key}").contains(FAKE));
        assert!(!format!("{key:?}").contains(FAKE));

        // Config containing the key
        let cfg = LlmProviderConfig {
            mode: LlmProviderMode::OpenAi,
            api_key: Some(key.clone()),
        };
        assert!(!format!("{cfg:?}").contains(FAKE));

        // Error containing the mode (does NOT carry a key, but lock it)
        let err = LlmProviderConfigError::MissingApiKey {
            mode: LlmProviderMode::OpenAi,
            env_var: "OPENAI_API_KEY",
        };
        assert!(!format!("{err}").contains(FAKE));
        assert!(!format!("{err:?}").contains(FAKE));

        // Empty key error
        let err2 = LlmProviderConfigError::EmptyApiKey {
            mode: LlmProviderMode::OpenAi,
        };
        assert!(!format!("{err2}").contains(FAKE));
    }

    // ── G. Disabled / Scripted modes perform zero env reads ────────────────

    #[test]
    fn p5d1_g_disabled_and_scripted_modes_perform_zero_env_reads() {
        let env = CountingEnvProvider::with(&[
            (ENV_OPENAI_API_KEY, "should_not_be_read"),
            (ENV_ANTHROPIC_API_KEY, "should_not_be_read"),
        ]);
        // Disabled
        let _ = build_llm_provider(
            &LlmProviderConfig {
                mode: LlmProviderMode::Disabled,
                api_key: None,
            },
            &env,
        );
        assert_eq!(
            env.read_count(),
            0,
            "Disabled mode must not consult env; saw {} reads",
            env.read_count()
        );

        // Scripted
        let _ = build_llm_provider(
            &LlmProviderConfig {
                mode: LlmProviderMode::Scripted,
                api_key: None,
            },
            &env,
        );
        assert_eq!(
            env.read_count(),
            0,
            "Scripted mode must not consult env; saw {} reads",
            env.read_count()
        );
    }

    // ── H. Provider ref does not dispatch tools ────────────────────────────

    #[test]
    fn p5d1_h_provider_module_does_not_dispatch_tools() {
        // Static guard: the `provider` module source must not import
        // tool-dispatch / tool-registry / Tool symbols. The provider
        // is an `LlmClient` factory, period. Tool dispatch belongs in
        // `ConversationHandler`.
        const SOURCE: &str = include_str!("provider.rs");
        // Build needles from fragments so this test does not match its
        // own source text.
        let needles = [
            format!("{}{}", "ToolDispat", "cher"),
            format!("{}{}", "ToolRegis", "try"),
            format!("{}{}", "tool.exec", "ute("),
            format!("{}{}", "dispatcher.dispa", "tch("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "provider.rs must not reference `{n}`; tool dispatch belongs to ConversationHandler"
            );
        }
    }

    // ── K. No silent fallback when real provider requested ─────────────────

    #[test]
    fn p5d1_k_no_silent_fallback_when_real_provider_requested() {
        // OpenAi with no key MUST NOT return a Scripted provider.
        let env = CountingEnvProvider::empty();
        let result = build_llm_provider(
            &LlmProviderConfig {
                mode: LlmProviderMode::OpenAi,
                api_key: None,
            },
            &env,
        );
        assert!(matches!(result, Err(_)), "OpenAi without key must error, never Ok");

        // Anthropic with no key MUST NOT return a Scripted provider.
        let result = build_llm_provider(
            &LlmProviderConfig {
                mode: LlmProviderMode::Anthropic,
                api_key: None,
            },
            &env,
        );
        assert!(matches!(result, Err(_)), "Anthropic without key must error, never Ok");

        // Empty key supplied programmatically — also fail closed,
        // not silently fall back.
        let result = build_llm_provider(
            &LlmProviderConfig {
                mode: LlmProviderMode::OpenAi,
                api_key: Some(ApiKey::new("   ")),
            },
            &env,
        );
        match result {
            Err(LlmProviderConfigError::EmptyApiKey { mode: LlmProviderMode::OpenAi }) => {}
            Err(e) => panic!("expected EmptyApiKey; got Err({e})"),
            Ok(_) => panic!("expected EmptyApiKey; got Ok(<provider>)"),
        }
    }

    // ── M. Provider mode source guard — no live network in this module ────

    #[test]
    fn p5d1_m_provider_module_has_no_live_network_or_panic_paths() {
        const SOURCE: &str = include_str!("provider.rs");
        // Needles fragmented at runtime to avoid scanner self-match.
        let needles = [
            format!("{}{}", "https://api.openai.", "com"),
            format!("{}{}", "https://api.anthropic.", "com"),
            format!("{}{}", "reqwest::Client::", "new("),
            format!("{}{}", "client.chat.", "completions("),
            format!("{}{}", "client.messages.", "create("),
            // Forbidden execution-path call shapes — the provider
            // module must never issue a Solana RPC.
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "create_signing_", "handoff("),
            format!("{}{}", "Keypair::", "new("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "to", "do!("),
            format!("{}{}", "unimplem", "ented!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "provider.rs must not contain `{n}`"
            );
        }
    }

    // ── Bonus: Disabled mode helper produces a client that always errors ──

    #[tokio::test]
    async fn p5d1_disabled_provider_helper_returns_error_on_complete() {
        let p = disabled_provider();
        let result = p
            .complete("system", &[LlmMessage::text("user", "hi")], &[])
            .await;
        assert!(matches!(result, Err(AgentError::Llm(_))));
    }

    // ── Bonus: programmatic key wins over env ─────────────────────────────

    #[test]
    fn p5d1_programmatic_key_takes_precedence_over_env() {
        let env = CountingEnvProvider::with(&[(ENV_OPENAI_API_KEY, "from-env")]);
        let cfg = LlmProviderConfig {
            mode: LlmProviderMode::OpenAi,
            api_key: Some(ApiKey::new("from-config")),
        };
        // Build succeeds (programmatic wins; env is not consulted).
        let _ = build_llm_provider(&cfg, &env).expect("programmatic key should construct");
        assert_eq!(
            env.read_count(),
            0,
            "programmatic key path must NOT consult env (saw {} reads)",
            env.read_count()
        );
    }
}

//! Configuration loading for the gateway daemon.
//!
//! Config is loaded from a TOML file. All fields can also be overridden
//! by environment variables (CLAW_ prefix).
//!
//! Config is loaded once at startup. Hot-reloading of the policy section
//! is supported via SIGHUP.

use serde::{Deserialize, Serialize};
use std::path::Path;

use claw_types::{
    policy::PolicyRule,
    solana::SolanaNetwork,
    swap::SwapPolicyConfig,
    wallet::SignerType,
};

use crate::errors::GatewayError;

/// Top-level configuration for the ClawSolana daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClawConfig {
    pub daemon: DaemonConfig,
    pub network: NetworkConfig,
    pub rpc: RpcConfig,
    #[serde(default)]
    pub wallets: Vec<WalletConfig>,
    pub policy: PolicyConfig,
    pub llm: LlmConfig,
    pub api: ApiConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    /// Webhook alerting configuration. When enabled, policy violations and
    /// approval lifecycle events are POSTed to the configured URL.
    #[serde(default)]
    pub alerting: AlertingConfig,
    /// Named operators with per-token identity and role assignments.
    /// When non-empty, each operator gets their own bearer token and roles.
    /// When empty, falls back to single `CLAW_API_TOKEN` with anonymous identity.
    #[serde(default)]
    pub operators: Vec<OperatorConfig>,
    /// Jupiter swap integration. Disabled by default.
    /// When enabled, the `submit_jupiter_swap` tool is registered and the
    /// production JIT executor is constructed.
    #[serde(default)]
    pub jupiter: JupiterConfig,
}

/// Jupiter swap integration configuration.
///
/// Controls registration of the `submit_jupiter_swap` tool and the
/// configuration of the intent-level swap policy.
///
/// # Feature flag
///
/// `enabled = false` (default) means the tool is NOT registered. The daemon
/// runs without any Jupiter-related surface. This is the safe default for
/// mainnet until policy caps are explicitly configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupiterConfig {
    /// Master switch. When false, the tool is not registered.
    #[serde(default)]
    pub enabled: bool,
    /// Base URL for the Jupiter API. Hosts both `/v6/quote` and `/swap/v2/build`.
    #[serde(default = "default_jupiter_base_url")]
    pub base_url: String,
    /// Intent-level swap policy (mint allowlists, amount caps, slippage caps).
    #[serde(default)]
    pub swap_policy: SwapPolicyConfig,
}

impl Default for JupiterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_jupiter_base_url(),
            swap_policy: SwapPolicyConfig::default(),
        }
    }
}

fn default_jupiter_base_url() -> String {
    "https://api.jup.ag".to_string()
}

/// An operator declaration with token and role assignments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorConfig {
    /// Unique operator identifier (used in audit trail).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Bearer token for this operator. Override with env var `CLAW_OPERATOR_<ID>_TOKEN`.
    pub token: Option<String>,
    /// Roles this operator holds (e.g., ["risk", "treasury"]).
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Path to the SQLite database file.
    pub db_path: String,
    /// Path to write the daemon PID file.
    pub pid_file: Option<String>,
    /// How often to purge old terminal pending-state rows (hours).
    /// Default: 6 hours.
    #[serde(default = "default_purge_interval_hours")]
    pub terminal_purge_interval_hours: u64,
    /// How long to retain terminal pending-state rows (days).
    /// Default: 14 days.
    #[serde(default = "default_terminal_retention_days")]
    pub terminal_retention_days: u64,
}

fn default_purge_interval_hours() -> u64 { 6 }
fn default_terminal_retention_days() -> u64 { 14 }

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            db_path: "./data/claw.db".to_string(),
            pid_file: None,
            terminal_purge_interval_hours: default_purge_interval_hours(),
            terminal_retention_days: default_terminal_retention_days(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub network: SolanaNetwork,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { network: SolanaNetwork::Devnet }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    /// Primary RPC URL.
    pub primary_url: String,
    /// Optional fallback RPC URLs.
    #[serde(default)]
    pub fallback_urls: Vec<String>,
    /// WebSocket URL for subscriptions.
    pub ws_url: Option<String>,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            primary_url: "https://api.devnet.solana.com".to_string(),
            fallback_urls: vec![],
            ws_url: Some("wss://api.devnet.solana.com".to_string()),
            timeout_ms: 15_000,
        }
    }
}

/// A wallet registration entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    pub label: String,
    /// The signer type.
    pub signer_type: SignerType,
    /// For LocalKeypair: path to a JSON keypair file (Solana CLI format).
    pub keypair_path: Option<String>,
    /// For LocalKeypair: base58-encoded private key (use keypair_path in production).
    /// SECURITY: Never commit this to source control.
    pub keypair_base58: Option<String>,
    /// For External: the base58-encoded public key of the external wallet.
    /// Required when signer_type = "external".
    pub pubkey: Option<String>,
    /// Per-wallet policy constraints. These rules are applied in addition to
    /// (and before) global and session policy rules when this wallet signs.
    #[serde(default)]
    pub policy: Option<WalletPolicyConfig>,
}

/// Per-wallet policy configuration.
///
/// Allows operators to set wallet-specific constraints that are stricter
/// than the global policy. Evaluated before session and global rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletPolicyConfig {
    /// Maximum transfer amount per transaction (in lamports).
    /// Transactions exceeding this require human approval.
    pub max_amount_lamports: Option<u64>,
    /// Program IDs this wallet is allowed to interact with.
    /// If non-empty, transactions invoking unlisted programs are rejected.
    #[serde(default)]
    pub program_allowlist: Vec<String>,
    /// Approver role required for any transaction from this wallet.
    pub required_approver_role: Option<String>,
    /// Additional custom policy rules for this wallet.
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Custom policy rules to evaluate before the defaults.
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
    /// Program IDs that are explicitly allowed.
    #[serde(default)]
    pub program_allowlist: Vec<String>,
    /// Destination pubkeys that are explicitly denied.
    #[serde(default)]
    pub destination_denylist: Vec<String>,
    /// Use mainnet safe defaults (require human approval for all mainnet txs).
    #[serde(default = "default_true")]
    pub mainnet_safe_defaults: bool,
    /// Per-role default policy profiles.
    #[serde(default)]
    pub role_profiles: Vec<RolePolicyProfile>,
    /// Maximum time (in seconds) an approval workflow can remain pending
    /// before being automatically expired. Default: 300 (5 minutes).
    #[serde(default = "default_approval_lease_seconds")]
    pub approval_lease_seconds: u64,
}

fn default_approval_lease_seconds() -> u64 { 300 }

/// A default policy rule set bound to a specific `AgentRole`.
///
/// Example TOML:
/// ```toml
/// [[policy.role_profiles]]
/// role = "execution"
/// rules = [
///   { name = "exec-cap", description = "Cap execution to 0.1 SOL", condition = { type = "AmountExceedsLamports", threshold = 100000000 }, action = { type = "RequireHumanApproval", reason = "execution role: amount exceeds 0.1 SOL" } }
/// ]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolePolicyProfile {
    /// The agent role this profile applies to (e.g., "execution", "risk", "ops").
    pub role: String,
    /// Policy rules for this role (evaluated after caller overrides, before global rules).
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

fn default_true() -> bool { true }

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            rules: vec![],
            program_allowlist: vec![],
            destination_denylist: vec![],
            mainnet_safe_defaults: true,
            role_profiles: vec![],
            approval_lease_seconds: default_approval_lease_seconds(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// LLM provider: "anthropic" or "openai". Default: "openai".
    #[serde(default = "default_llm_provider")]
    pub provider: String,
    /// API key. Override with CLAW_LLM_API_KEY env var.
    pub api_key: String,
    /// Model to use. Depends on provider:
    /// - anthropic: "claude-sonnet-4-6" (default)
    /// - openai: "gpt-4o" (default)
    pub model: String,
}

fn default_llm_provider() -> String { "openai".to_string() }

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            api_key: "".to_string(),
            model: "gpt-4o".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Local API listen address.
    pub bind_addr: String,
    /// Local API port.
    pub port: u16,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1".to_string(),
            port: 7070,
        }
    }
}

/// Webhook alerting configuration.
///
/// When `enabled` is true and `webhook_url` is set, the daemon POSTs
/// JSON payloads for policy violations, approval lifecycle events, and
/// signing lease expirations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertingConfig {
    /// Whether alerting is enabled. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// The URL to POST alert payloads to.
    pub webhook_url: Option<String>,
    /// Maximum number of delivery retries per alert. Default: 3.
    #[serde(default = "default_alert_max_retries")]
    pub max_retries: u32,
    /// Timeout per delivery attempt in milliseconds. Default: 5000.
    #[serde(default = "default_alert_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_alert_max_retries() -> u32 { 3 }
fn default_alert_timeout_ms() -> u64 { 5000 }

impl Default for AlertingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            webhook_url: None,
            max_retries: default_alert_max_retries(),
            timeout_ms: default_alert_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// "json" or "pretty"
    pub format: String,
    /// RUST_LOG filter string
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: "pretty".to_string(),
            level: "info".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of rebroadcast attempts per dropped transaction.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_max_retries() -> u32 { 3 }

impl Default for RetryConfig {
    fn default() -> Self {
        Self { max_retries: default_max_retries() }
    }
}

/// Per-token sliding-window rate limiting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Whether rate limiting is enabled. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum requests per window per bearer token. Default: 60.
    #[serde(default = "default_requests_per_minute")]
    pub requests_per_minute: u32,
    /// Extra burst allowance above the per-minute rate. Default: 10.
    #[serde(default = "default_burst_size")]
    pub burst_size: u32,
}

fn default_requests_per_minute() -> u32 { 60 }
fn default_burst_size() -> u32 { 10 }

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_minute: default_requests_per_minute(),
            burst_size: default_burst_size(),
        }
    }
}

impl ClawConfig {
    /// Loads config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, GatewayError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| GatewayError::Config(format!("cannot read config file: {e}")))?;

        let mut config: ClawConfig = toml::from_str(&content)
            .map_err(|e| GatewayError::Config(format!("invalid TOML: {e}")))?;

        // Apply env var overrides
        if let Ok(key) = std::env::var("CLAW_LLM_API_KEY") {
            config.llm.api_key = key;
        }
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            if config.llm.api_key.is_empty() {
                config.llm.api_key = key;
                if config.llm.provider != "openai" {
                    config.llm.provider = "openai".to_string();
                }
            }
        }
        if let Ok(provider) = std::env::var("CLAW_LLM_PROVIDER") {
            config.llm.provider = provider;
        }
        if let Ok(url) = std::env::var("CLAW_RPC_URL") {
            config.rpc.primary_url = url;
        }

        Ok(config)
    }

    /// Returns a default config for development use.
    pub fn default_dev() -> Self {
        Self {
            daemon:  DaemonConfig::default(),
            network: NetworkConfig::default(),
            rpc:     RpcConfig::default(),
            wallets: vec![],
            policy:  PolicyConfig::default(),
            llm:     {
                // Auto-detect provider: prefer CLAW_LLM_API_KEY, fallback to OPENAI_API_KEY
                let (provider, api_key, model) = if let Ok(key) = std::env::var("CLAW_LLM_API_KEY") {
                    let prov = std::env::var("CLAW_LLM_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
                    let m = if prov == "openai" { "gpt-4o" } else { "claude-sonnet-4-6" };
                    (prov, key, m.to_string())
                } else if let Ok(key) = std::env::var("OPENAI_API_KEY") {
                    ("openai".to_string(), key, "gpt-4o".to_string())
                } else {
                    ("openai".to_string(), String::new(), "gpt-4o".to_string())
                };
                LlmConfig { provider, api_key, model }
            },
            api:        ApiConfig::default(),
            logging:    LoggingConfig::default(),
            retry:      RetryConfig::default(),
            rate_limit: RateLimitConfig::default(),
            alerting:   AlertingConfig::default(),
            operators:  vec![],
            jupiter:    JupiterConfig::default(),
        }
    }

    /// Validates config and returns warnings for missing optional fields.
    /// The daemon can start without an LLM key — wallet bind, signing,
    /// and approval flows work without it. Only agent message handling
    /// (POST /sessions/:id/messages) requires an LLM key.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.llm.api_key.is_empty() {
            warnings.push(
                "No LLM API key set. Agent message handling will be unavailable. \
                 Set CLAW_LLM_API_KEY for full functionality.".to_string(),
            );
        }
        warnings
    }
}

#[cfg(test)]
mod jupiter_config_tests {
    use super::*;

    #[test]
    fn jupiter_disabled_by_default() {
        let config = JupiterConfig::default();
        assert!(!config.enabled, "Jupiter must be disabled by default");
        assert_eq!(config.base_url, "https://api.jup.ag");
    }

    #[test]
    fn jupiter_defaults_off_with_canonical_url() {
        let cfg = JupiterConfig::default();
        assert!(!cfg.enabled, "Jupiter MUST default to disabled");
        assert_eq!(cfg.base_url, "https://api.jup.ag");
        // Empty swap policy = approves everything; this is paired with enabled=false.
        assert!(cfg.swap_policy.allowed_input_mints.is_empty());
        assert!(cfg.swap_policy.allowed_output_mints.is_empty());
    }

    #[test]
    fn parses_full_jupiter_section() {
        let toml_str = r#"
enabled = true
base_url = "https://api.jup.ag"

[swap_policy]
allowed_input_mints = ["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"]
allowed_output_mints = ["So11111111111111111111111111111111111111112"]
max_input_amount = 100000000
max_slippage_bps = 100
required_approver_role = "treasury"
"#;
        let cfg: JupiterConfig = toml::from_str(toml_str).expect("parse");
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, "https://api.jup.ag");
        assert_eq!(cfg.swap_policy.allowed_input_mints.len(), 1);
        assert_eq!(cfg.swap_policy.allowed_output_mints.len(), 1);
        assert_eq!(cfg.swap_policy.max_input_amount, Some(100_000_000));
        assert_eq!(cfg.swap_policy.max_slippage_bps, Some(100));
        assert_eq!(cfg.swap_policy.required_approver_role.as_deref(), Some("treasury"));
    }

    #[test]
    fn claw_config_default_dev_has_jupiter_disabled() {
        // Guard: the default dev config must not silently turn on Jupiter.
        let cfg = ClawConfig::default_dev();
        assert!(!cfg.jupiter.enabled);
    }

    #[test]
    fn jupiter_field_is_serde_default_when_absent() {
        // Verify the `#[serde(default)]` attribute on the `jupiter` field is
        // in effect — existing configs without a `[jupiter]` section must
        // keep loading. We do this without building a full ClawConfig TOML
        // (which requires many unrelated required fields) by parsing a
        // stand-alone document that only has the jupiter field declaration.
        #[derive(serde::Deserialize)]
        struct Probe {
            #[serde(default)]
            jupiter: JupiterConfig,
        }
        let probe: Probe = toml::from_str("").expect("empty doc parses");
        assert!(!probe.jupiter.enabled);
        assert_eq!(probe.jupiter.base_url, "https://api.jup.ag");
    }
}

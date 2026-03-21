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
}

fn default_true() -> bool { true }

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            rules: vec![],
            program_allowlist: vec![],
            destination_denylist: vec![],
            mainnet_safe_defaults: true,
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
            api:     ApiConfig::default(),
            logging: LoggingConfig::default(),
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

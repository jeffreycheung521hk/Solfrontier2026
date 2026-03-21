// =============================================================================
// ClawSolana — Domain Types + Configuration (V1.5 STEP 1)
// For external review. File paths shown as comments before each section.
// Key addition: AgentRole is now used to construct per-session CapabilitySet
// via CapabilitySet::for_role(role) — see CORE_CODE.rs for implementation.
// =============================================================================

// =============================================================================
// FILE: crates/types/src/lib.rs
// The shared domain vocabulary — zero intra-workspace deps.
// Every significant semantic boundary in the system is encoded here as
// a named type. Stringly-typed patterns are explicitly prohibited.
// =============================================================================

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod agent;
pub mod alert;
pub mod errors;
pub mod events;
pub mod messages;
pub mod policy;
pub mod session;
pub mod solana;
pub mod tool;
pub mod transaction;
pub mod wallet;

pub use agent::{AgentCommand, AgentResponse, AgentRole};
pub use alert::{Alert, AlertSeverity};
pub use errors::ClawError;
pub use events::GatewayEvent;
pub use messages::{InboundMessage, MessageContent, OutboundContent, OutboundMessage};
pub use policy::{PolicyRule, PolicyVerdict};
pub use session::{SessionId, SessionState, SessionSummary};
pub use solana::{CommitmentLevel, SolanaEvent, SolanaNetwork};
pub use tool::{ToolInput, ToolOutput, ToolSpec, ToolTrace};
pub use transaction::{TransactionProposal, TransactionRecord, TransactionStatus};
pub use wallet::{SignerType, WalletRef};


// =============================================================================
// FILE: crates/types/src/agent.rs
// Agent roles, commands, responses. Roles enforce principle of least privilege.
// =============================================================================

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// The role of an agent determines its system prompt, default tool set,
/// and capability grants. Roles are deliberately narrow to enforce
/// the principle of least privilege at the agent level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Reads chain state, fetches account info, decodes transactions.
    /// Cannot sign or send transactions.
    Research,
    /// Can build, simulate, and (with approval) send transactions.
    Execution,
    /// Monitors subscriptions, evaluates risk conditions, emits alerts.
    Risk,
    /// Manages system configuration. Elevated capabilities, no signing by default.
    Ops,
}

/// A structured command sent to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCommand {
    pub id:          Uuid,
    pub text:        String,
    #[serde(default)]
    pub parameters:  HashMap<String, serde_json::Value>,
    pub target_role: Option<AgentRole>,
}

impl AgentCommand {
    pub fn new(text: impl Into<String>) -> Self {
        Self { id: Uuid::new_v4(), text: text.into(), parameters: HashMap::new(), target_role: None }
    }
}

/// The structured response produced by an agent after processing a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub command_id:  Uuid,
    pub text:        String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:        Option<serde_json::Value>,
    pub status:      AgentResponseStatus,
    #[serde(default)]
    pub tool_calls:  Vec<ToolCallSummary>,
}

/// Status of an agent execution cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentResponseStatus {
    Completed,
    AwaitingApproval,
    Failed,
    Cancelled,
}

/// Brief summary of a single tool call (full detail is in tool_traces table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub tool_name:   String,
    pub status:      String,
    pub duration_ms: u64,
}


// =============================================================================
// FILE: crates/types/src/policy.rs
// PolicyVerdict: never a boolean. Every verdict carries rule_name + reason.
// =============================================================================

use serde::{Deserialize, Serialize};

/// The verdict produced by evaluating a transaction against the active policy.
/// NEVER a boolean — every verdict is fully auditable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum PolicyVerdict {
    /// All checks passed. Transaction may proceed automatically.
    Approved { rule_name: String },
    /// Operator must explicitly confirm before signing.
    RequiresHumanApproval { reason: String, rule_name: String },
    /// A policy rule explicitly blocked this transaction.
    Rejected { reason: String, rule_name: String },
    /// Simulation has not been run yet, and the active policy requires it.
    SimulationRequired,
    /// Simulation ran but returned an error.
    SimulationFailed { simulation_error: String },
}

impl PolicyVerdict {
    pub fn is_auto_approved(&self) -> bool { matches!(self, PolicyVerdict::Approved { .. }) }
    pub fn is_blocked(&self) -> bool {
        matches!(self, PolicyVerdict::Rejected { .. } | PolicyVerdict::SimulationFailed { .. })
    }
    pub fn requires_human(&self) -> bool { matches!(self, PolicyVerdict::RequiresHumanApproval { .. }) }
    pub fn label(&self) -> &'static str {
        match self {
            PolicyVerdict::Approved { .. }               => "approved",
            PolicyVerdict::RequiresHumanApproval { .. }  => "requires_human_approval",
            PolicyVerdict::Rejected { .. }               => "rejected",
            PolicyVerdict::SimulationRequired             => "simulation_required",
            PolicyVerdict::SimulationFailed { .. }       => "simulation_failed",
        }
    }
}

/// A single policy rule. Rules are evaluated in order; first match wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    pub name:        String,
    pub description: String,
    pub condition:   PolicyCondition,
    pub action:      PolicyAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyCondition {
    NetworkIn(Vec<crate::solana::SolanaNetwork>),
    ProgramNotInAllowlist,
    DestinationInDenylist,
    CostExceedsSol(f64),
    DailySpendExceedsSol(f64),
    SimulationNotPassed,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Approve,
    RequireHumanApproval { reason: String },
    Reject { reason: String },
}


// =============================================================================
// FILE: crates/types/src/session.rs
// Session identity and lifecycle types.
// Session = top-level isolation unit. Every tool call/tx is scoped to one.
// =============================================================================

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Opaque session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
    pub fn as_uuid(&self) -> &Uuid { &self.0 }
}

impl Default for SessionId { fn default() -> Self { Self::new() } }
impl fmt::Display for SessionId { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) } }
impl From<Uuid> for SessionId { fn from(uuid: Uuid) -> Self { Self(uuid) } }

/// Session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState { Active, Closing, Closed, Degraded }

/// Summary view of a session (for API responses and audit records).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id:              SessionId,
    pub state:           SessionState,
    pub agent_role:      AgentRole,   // (AgentRole from agent.rs)
    pub channel:         String,
    pub created_at:      DateTime<Utc>,
    pub closed_at:       Option<DateTime<Utc>>,
    pub message_count:   u64,
    pub tool_call_count: u64,
}


// =============================================================================
// FILE: crates/types/src/tool.rs
// Tool system types: spec, input, output, trace.
// =============================================================================

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Static specification of a tool. Registered at startup.
/// The `input_schema` field is passed verbatim to the LLM as tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name:                  String,
    pub description:           String,
    pub input_schema:          Value,
    pub output_schema:         Value,
    pub required_capabilities: Vec<String>,
    pub supports_streaming:    bool,
    pub timeout_ms:            u64,
}

/// A validated tool invocation input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInput {
    pub tool_name:      String,
    pub parameters:     Value,
    pub session_id:     SessionId,  // (SessionId from session.rs)
    pub correlation_id: Uuid,
}

/// The result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub tool_name:   String,
    pub success:     bool,
    pub data:        Option<Value>,
    pub error:       Option<String>,
    pub duration_ms: u64,
}

/// Persisted audit record for a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTrace {
    pub id:             Uuid,
    pub session_id:     SessionId,
    pub correlation_id: Uuid,
    pub tool_name:      String,
    pub started_at:     DateTime<Utc>,
    pub finished_at:    Option<DateTime<Utc>>,
    pub status:         ToolTraceStatus,
    pub input:          Value,   // sanitized (no secrets)
    pub output:         Option<Value>,
    pub error:          Option<String>,
    pub duration_ms:    Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTraceStatus { Running, Succeeded, Failed, TimedOut, Cancelled }


// =============================================================================
// FILE: crates/types/src/transaction.rs
// Transaction lifecycle: each pipeline stage is a distinct type.
// Policy verdict is embedded, never bypassed.
// =============================================================================

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A transaction proposal at the start of the pipeline (built, not yet simulated/signed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionProposal {
    pub id:                   Uuid,
    pub session_id:           SessionId,
    pub wallet_pubkey:        String,
    pub network:              crate::solana::SolanaNetwork,
    pub description:          String,
    pub transaction_b64:      String,   // base64-encoded unsigned tx
    #[serde(default)]
    pub instructions_summary: Vec<InstructionSummary>,
    pub created_at:           DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionSummary {
    pub program_id:   String,
    pub program_name: Option<String>,
    pub description:  String,
    pub accounts:     Vec<AccountRole>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRole {
    pub pubkey:      String,
    pub label:       Option<String>,
    pub is_signer:   bool,
    pub is_writable: bool,
}

/// Result of simulating a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResult {
    pub success:            bool,
    pub error:              Option<String>,
    pub compute_units_used: Option<u64>,
    pub logs:               Vec<String>,
    pub return_data:        Option<String>,
    pub account_diffs:      Vec<AccountDiff>,
    pub fee_lamports:       Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountDiff {
    pub pubkey:          String,
    pub lamports_before: Option<u64>,
    pub lamports_after:  Option<u64>,
    pub data_changed:    bool,
}

/// Transaction lifecycle status — each is a distinct, named stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    Proposed, Simulated, PolicyChecked, AwaitingApproval,
    Approved, Rejected, Signed, Sent, Confirmed, Finalized,
    Failed, Expired,
}

/// Authoritative audit record tracking a transaction through its full lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub id:                Uuid,
    pub session_id:        SessionId,
    pub wallet_pubkey:     String,
    pub network:           crate::solana::SolanaNetwork,
    pub status:            TransactionStatus,
    pub description:       String,
    pub proposal:          TransactionProposal,
    pub simulation_result: Option<SimulationResult>,
    pub policy_verdict:    Option<PolicyVerdict>,  // always present before signing
    pub signature:         Option<String>,          // on-chain signature after send
    pub created_at:        DateTime<Utc>,
    pub updated_at:        DateTime<Utc>,
}


// =============================================================================
// FILE: crates/types/src/wallet.rs
// Wallet identity types. Private key material is NEVER stored here.
// Key material lives in claw-wallet-engine behind SecretKeystore.
// =============================================================================

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A wallet reference — identifier and metadata only, never key material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletRef {
    pub pubkey:      String,         // base58 public key
    pub label:       Option<String>,
    pub signer_type: SignerType,
    pub network:     crate::solana::SolanaNetwork,
    pub created_at:  DateTime<Utc>,
    pub is_active:   bool,
}

/// How signing requests are routed for this wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerType {
    LocalKeypair,   // in-process keypair, key material in locked memory
    Ledger,         // hardware wallet, key material never leaves device
    External,       // multisig program / Squads / callback service
    ReadOnly,       // can inspect state, cannot sign
}


// =============================================================================
// FILE: crates/types/src/events.rs  (excerpt — key variants shown)
// GatewayEvent: the single enum flowing through the broadcast channel.
// Events are facts, not commands. Every significant action emits one.
// =============================================================================

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

/// All events that flow through the EventBus (broadcast channel, capacity 1024).
/// Subscribers filter by pattern-matching on variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum GatewayEvent {
    // Session lifecycle
    SessionOpened(SessionOpenedEvent),
    SessionStateChanged(SessionStateChangedEvent),
    SessionClosed(SessionClosedEvent),
    // Agent lifecycle
    AgentTaskStarted(AgentTaskEvent),
    AgentTaskCompleted(AgentTaskEvent),
    AgentTaskFailed(AgentTaskFailedEvent),
    // Tool execution
    ToolInvoked(ToolLifecycleEvent),
    ToolCompleted(ToolLifecycleEvent),
    ToolFailed(ToolLifecycleEvent),
    // Transaction pipeline
    TransactionProposed(TransactionLifecycleEvent),
    TransactionSimulated(TransactionLifecycleEvent),
    PolicyEvaluated(PolicyEvaluatedEvent),
    ApprovalRequested(ApprovalLifecycleEvent),
    ApprovalReceived(ApprovalLifecycleEvent),
    TransactionSigned(TransactionLifecycleEvent),
    TransactionSent(TransactionLifecycleEvent),
    TransactionConfirmed(TransactionLifecycleEvent),
    TransactionFailed(TransactionFailedEvent),
    // Solana chain events
    SolanaEvent(SolanaEvent),
    // System
    AlertEmitted(Alert),
    HealthCheckCompleted(HealthCheckEvent),
    ConfigReloaded(ConfigReloadedEvent),
    DaemonShuttingDown,
}

/// Standard event header — every event carries correlation + timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventHeader {
    pub id:             Uuid,
    pub correlation_id: Uuid,
    pub session_id:     Option<SessionId>,
    pub occurred_at:    DateTime<Utc>,
}


// =============================================================================
// FILE: crates/types/src/messages.rs  (excerpt)
// Unified message schema for all channel adapters.
// Every channel normalizes to InboundMessage before the gateway sees it.
// =============================================================================

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

/// Inbound message arriving from any channel into the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id:             Uuid,
    pub correlation_id: Uuid,
    pub session_id:     SessionId,
    pub channel:        String,     // which channel adapter delivered this
    pub received_at:    DateTime<Utc>,
    pub content:        MessageContent,
    pub auth:           AuthContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    Command(AgentCommand),
    Approval(ApprovalResponse),
    SystemControl(SystemControlMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthContext {
    Local,
    Token { token_id: String },
}

/// Outbound message sent from the gateway to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id:             Uuid,
    pub correlation_id: Uuid,
    pub session_id:     SessionId,
    pub sent_at:        DateTime<Utc>,
    pub content:        OutboundContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundContent {
    Text(String),
    AgentResponse(AgentResponse),
    ApprovalRequest(TransactionApprovalRequest),
    Alert(Alert),
    StreamChunk(StreamChunk),
    Error(ErrorResponse),
}

/// Human-facing approval request for a transaction.
/// The operator must respond before the request expires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionApprovalRequest {
    pub request_id:        Uuid,
    pub proposal:          TransactionProposal,
    pub simulation_result: Option<SimulationResult>,
    pub policy_verdict:    PolicyVerdict,
    pub explanation:       String,    // plain-language "what this tx will do"
    pub expires_in_ms:     u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: Uuid,
    pub approved:   bool,
    pub note:       Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemControlMessage {
    Shutdown,
    ReloadConfig,
    ListSessions,
    KillSession { session_id: SessionId },
    ListSubscriptions,
    ListWallets,
}


// =============================================================================
// FILE: bins/clawd/src/main.rs
// Daemon binary entry point.
// =============================================================================

use std::path::PathBuf;
use anyhow::Context;
use clap::Parser;
use tracing::info;
use claw_gateway::{ClawConfig, GatewayDaemon};
use claw_observability::{init_tracing_with_filter, init::LogFormat};

#[derive(Parser, Debug)]
#[command(name = "clawd", about = "ClawSolana daemon — Solana-native AI agent control plane", version)]
struct Args {
    #[arg(short, long, value_name = "FILE", env = "CLAW_CONFIG")]
    config: Option<PathBuf>,

    #[arg(short, long, env = "RUST_LOG")]
    log_level: Option<String>,

    #[arg(long, env = "CLAW_LOG_JSON")]
    log_json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config = if let Some(path) = &args.config {
        ClawConfig::load(path)
            .with_context(|| format!("failed to load config from {}", path.display()))?
    } else {
        eprintln!("clawd: no --config provided, using dev defaults");
        ClawConfig::default_dev()
    };

    let log_format = if args.log_json || config.logging.format == "json" {
        LogFormat::Json
    } else {
        LogFormat::Pretty
    };

    // Resolve effective filter — priority: CLI flag (= RUST_LOG env via clap) > config > "info".
    // Passed directly to init_tracing_with_filter; no unsafe env::set_var.
    let filter_str: Option<String> = args.log_level
        .or_else(|| {
            let lvl = config.logging.level.clone();
            if lvl.is_empty() { None } else { Some(lvl) }
        });

    // Zero unsafe — no environment mutation.
    init_tracing_with_filter(log_format, filter_str.as_deref());

    info!(
        version = env!("CARGO_PKG_VERSION"),
        network = %config.network.network,
        rpc     = %config.rpc.primary_url,
        "clawd starting"
    );

    GatewayDaemon::run(config)
        .await
        .context("gateway daemon exited with error")?;

    info!("clawd stopped cleanly");
    Ok(())
}


// =============================================================================
// FILE: config/default.toml
// Reference configuration — copy to claw.toml and edit for your environment.
// Sensitive values should be passed via environment variables.
// =============================================================================

/*
# ClawSolana — default development configuration

# ── Daemon ─────────────────────────────────────────────────────────────────
[daemon]
db_path = "./data/claw.db"

# ── Network ────────────────────────────────────────────────────────────────
[network]
network = "devnet"   # devnet | testnet | mainnet-beta | localnet

# ── RPC ────────────────────────────────────────────────────────────────────
[rpc]
primary_url = "https://api.devnet.solana.com"
fallback_urls = []
ws_url = "wss://api.devnet.solana.com"
timeout_ms = 15000

# ── Wallets ─────────────────────────────────────────────────────────────────
# [[wallets]]
# label = "devnet-hot-wallet"
# signer_type = "local_keypair"
# keypair_path = "~/.config/solana/devnet.json"

# ── Policy ─────────────────────────────────────────────────────────────────
[policy]
mainnet_safe_defaults = true  # STRONGLY recommended — requires human approval on mainnet
program_allowlist     = []    # empty = allow all programs (appropriate for devnet)
destination_denylist  = []

# ── LLM ────────────────────────────────────────────────────────────────────
[llm]
api_key = ""  # Set via CLAW_LLM_API_KEY env var — NEVER commit a real key here
model   = "claude-sonnet-4-6"

# ── API ─────────────────────────────────────────────────────────────────────
[api]
bind_addr = "127.0.0.1"  # NEVER change to 0.0.0.0 without a TLS reverse proxy + auth
port      = 7070

# ── Logging ─────────────────────────────────────────────────────────────────
[logging]
format = "pretty"   # "json" for production/containers
level  = "info,claw_gateway=debug,claw_agent_runtime=debug"
*/

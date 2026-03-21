//! Agent domain types: roles, commands, and responses.
//!
//! Agent roles define the capability envelope and system persona.
//! Commands are the structured instructions routed to an agent.
//! Responses are the structured outputs an agent produces.

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
    /// Has access to the full transaction pipeline.
    Execution,

    /// Monitors subscriptions, evaluates risk conditions, emits alerts.
    /// Cannot sign or send transactions.
    Risk,

    /// Manages system configuration, subscriptions, wallet registry.
    /// Operator-only. Has elevated capabilities but no signing by default.
    Ops,
}

impl std::fmt::Display for AgentRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentRole::Research  => write!(f, "research"),
            AgentRole::Execution => write!(f, "execution"),
            AgentRole::Risk      => write!(f, "risk"),
            AgentRole::Ops       => write!(f, "ops"),
        }
    }
}

/// A structured command sent to an agent.
/// The agent interprets `text` as the user intent and uses
/// `parameters` for any structured hints the channel layer can provide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCommand {
    /// Unique ID for this command (used in correlation).
    pub id: Uuid,

    /// The human-readable instruction or question.
    pub text: String,

    /// Optional structured parameters (e.g., a wallet address the CLI parsed).
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,

    /// If set, the command is directed at a specific agent role.
    /// If None, the gateway routes based on intent detection.
    pub target_role: Option<AgentRole>,
}

impl AgentCommand {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            text: text.into(),
            parameters: HashMap::new(),
            target_role: None,
        }
    }
}

/// The structured response produced by an agent after processing a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    /// Correlates back to the originating `AgentCommand.id`.
    pub command_id: Uuid,

    /// The agent's textual explanation or answer.
    pub text: String,

    /// Any structured data produced (e.g., a transaction summary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,

    /// The final status of the agent's execution.
    pub status: AgentResponseStatus,

    /// Tool calls made during this response (for transparency).
    #[serde(default)]
    pub tool_calls: Vec<ToolCallSummary>,
}

/// Status of an agent execution cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentResponseStatus {
    /// The agent completed successfully.
    Completed,
    /// The agent requires human input to continue (e.g., approval).
    AwaitingApproval,
    /// The agent failed with an unrecoverable error.
    Failed,
    /// The agent was cancelled (session closed, timeout, etc.).
    Cancelled,
}

/// A brief summary of a single tool call within an agent response.
/// Full detail is in the `tool_traces` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub tool_name:   String,
    pub status:      String,
    pub duration_ms: u64,
}

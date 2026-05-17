//! Tool system types.
//!
//! Tools are the atomic actions agents can take. Each tool has a typed spec,
//! typed input/output, and a trace record for the audit log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::session::SessionId;

/// The static specification of a tool, used for registration and
/// JSON Schema validation of inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Unique tool name. Must be stable — used in audit records.
    pub name: String,

    /// Human-readable description (also shown to the LLM).
    pub description: String,

    /// JSON Schema for the tool's input object.
    pub input_schema: Value,

    /// JSON Schema for the tool's output object.
    pub output_schema: Value,

    /// The capabilities required to invoke this tool.
    pub required_capabilities: Vec<String>,

    /// Whether this tool can produce streaming output chunks.
    pub supports_streaming: bool,

    /// Maximum allowed execution time in milliseconds.
    pub timeout_ms: u64,
}

/// A tool invocation input (after validation against the tool's schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInput {
    pub tool_name: String,
    pub parameters: Value,
    pub session_id: SessionId,
    pub correlation_id: Uuid,
}

/// The result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub tool_name:    String,
    pub success:      bool,
    pub data:         Option<Value>,
    pub error:        Option<String>,
    pub duration_ms:  u64,
}

/// A persisted record of a single tool execution, written to the state store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTrace {
    pub id:             Uuid,
    pub session_id:     SessionId,
    pub correlation_id: Uuid,
    pub tool_name:      String,
    pub started_at:     DateTime<Utc>,
    pub finished_at:    Option<DateTime<Utc>>,
    pub status:         ToolTraceStatus,
    /// Sanitized (no secrets) input parameters.
    pub input:          Value,
    /// Sanitized output. May be truncated for large outputs.
    pub output:         Option<Value>,
    pub error:          Option<String>,
    pub duration_ms:    Option<u64>,
}

/// Status of a tool execution trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTraceStatus {
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

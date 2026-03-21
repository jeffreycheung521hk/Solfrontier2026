//! Action planning types.
//!
//! In V1 the agent uses a simple ReAct-style loop (reason → act → observe).
//! The `ActionPlan` type is a stub for the structured planning capability
//! that will be expanded in V2.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A planned action step for the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionStep {
    pub id:          Uuid,
    pub description: String,
    pub tool_name:   Option<String>,
    pub tool_input:  Option<serde_json::Value>,
}

/// A high-level plan consisting of ordered steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPlan {
    pub id:    Uuid,
    pub steps: Vec<ActionStep>,
}

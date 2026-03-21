//! Agent session: tracks the state of one agent execution context.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use claw_types::{agent::AgentRole, session::SessionId};

use crate::llm::context::ContextManager;

/// The execution context for a single agent session.
pub struct AgentSession {
    pub id:          SessionId,
    pub agent_role:  AgentRole,
    pub channel:     String,
    pub created_at:  DateTime<Utc>,
    pub context:     ContextManager,
    pub task_count:  u64,
    pub tool_calls:  u64,
}

impl AgentSession {
    pub fn new(id: SessionId, role: AgentRole, channel: impl Into<String>) -> Self {
        Self {
            id,
            agent_role:  role,
            channel:     channel.into(),
            created_at:  Utc::now(),
            context:     ContextManager::new(100_000), // ~100k tokens max
            task_count:  0,
            tool_calls:  0,
        }
    }
}

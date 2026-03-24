//! Agent router — routes commands to per-session agent executors.
//!
//! The `AgentRouter` owns the live `AgentSession` map and provides
//! a single `dispatch()` method that:
//!
//!   1. Looks up the session by ID
//!   2. Runs `Agent::run(command, session)` with the session's mutable context
//!   3. Returns the `AgentResponse`
//!
//! Sessions are created here (not in SessionManager) because `AgentSession`
//! holds LLM conversation history — runtime state that does not belong in
//! the persistence layer.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use claw_state_store::tool_traces::ToolTraceRepository;
use claw_tool_system::ToolDispatcher;
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    session::SessionId,
};

use crate::{
    agent::Agent,
    errors::AgentError,
    session::AgentSession,
};

/// Routes commands to the appropriate session/agent by session ID.
pub struct AgentRouter {
    sessions: Arc<RwLock<HashMap<SessionId, AgentSession>>>,
}

impl AgentRouter {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Creates a new `AgentSession` for the given session ID.
    /// Called when the gateway opens a new session.
    pub async fn open_session(
        &self,
        id: SessionId,
        role: AgentRole,
        channel: impl Into<String>,
        wallet_context: Option<String>,
    ) -> SessionId {
        let mut session = AgentSession::new(id.clone(), role, channel);
        session.wallet_context = wallet_context;
        self.sessions.write().await.insert(id.clone(), session);
        id
    }

    /// Removes the session from the router.
    /// Called when the gateway closes a session.
    pub async fn close_session(&self, id: &SessionId) {
        self.sessions.write().await.remove(id);
    }

    /// Returns the number of active sessions.
    pub async fn active_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Returns the `AgentRole` for the given session, or `None` if not found.
    pub async fn session_role(&self, id: &SessionId) -> Option<AgentRole> {
        self.sessions.read().await.get(id).map(|s| s.agent_role)
    }

    /// Dispatches an `AgentCommand` to the session identified by `session_id`.
    ///
    /// `dispatcher` must already be narrowed to the session's capability set.
    /// The caller (typically `GatewayMessageHandler`) is responsible for
    /// building the per-role dispatcher before calling this method.
    ///
    /// Returns `AgentError::SessionNotFound` if the session does not exist.
    pub async fn dispatch(
        &self,
        session_id: &SessionId,
        command: AgentCommand,
        agent: &Agent,
        dispatcher: ToolDispatcher,
        tool_traces: &ToolTraceRepository,
    ) -> Result<AgentResponse, AgentError> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| AgentError::SessionNotFound(session_id.to_string()))?;

        agent.run(command, session, dispatcher, tool_traces).await
    }
}

impl Default for AgentRouter {
    fn default() -> Self {
        Self::new()
    }
}

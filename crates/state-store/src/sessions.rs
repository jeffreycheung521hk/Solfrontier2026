//! Session repository.
//!
//! Provides typed read/write access to the `sessions` table.
//! Uses runtime sqlx queries (not compile-time macros) for offline buildability.

use chrono::Utc;
use sqlx::{FromRow, SqlitePool};
use tracing::instrument;

use claw_types::{
    agent::AgentRole,
    session::{SessionId, SessionState},
};

use crate::errors::StoreError;

/// Data-access object for sessions.
#[derive(Clone, Debug)]
pub struct SessionRepository {
    pool: SqlitePool,
}

impl SessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Persists a new session record.
    #[instrument(skip(self), fields(session_id = %id))]
    pub async fn create(
        &self,
        id:         &SessionId,
        agent_role: AgentRole,
        channel:    &str,
    ) -> Result<(), StoreError> {
        let id_str   = id.to_string();
        let role_str = agent_role.to_string();
        let now      = Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO sessions (id, state, agent_role, channel, created_at)
             VALUES (?, 'active', ?, ?, ?)",
        )
        .bind(id_str)
        .bind(role_str)
        .bind(channel)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Updates the session state.
    #[instrument(skip(self), fields(session_id = %id))]
    pub async fn update_state(
        &self,
        id:    &SessionId,
        state: SessionState,
    ) -> Result<(), StoreError> {
        let id_str    = id.to_string();
        let state_str = state.to_string();
        let closed_at: Option<i64> = if state == SessionState::Closed {
            Some(Utc::now().timestamp_millis())
        } else {
            None
        };

        sqlx::query(
            "UPDATE sessions SET state = ?, closed_at = ? WHERE id = ?",
        )
        .bind(state_str)
        .bind(closed_at)
        .bind(id_str)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Increments the session's message and tool call counters.
    pub async fn increment_counters(
        &self,
        id:         &SessionId,
        messages:   i64,
        tool_calls: i64,
    ) -> Result<(), StoreError> {
        let id_str = id.to_string();

        sqlx::query(
            "UPDATE sessions
             SET message_count   = message_count   + ?,
                 tool_call_count = tool_call_count + ?
             WHERE id = ?",
        )
        .bind(messages)
        .bind(tool_calls)
        .bind(id_str)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Lists all active sessions.
    pub async fn list_active(&self) -> Result<Vec<SessionRow>, StoreError> {
        let rows = sqlx::query_as::<_, SessionRow>(
            "SELECT id, state, agent_role, channel, created_at, closed_at,
                    message_count, tool_call_count
             FROM sessions
             WHERE state = 'active'
             ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

/// Raw row from the sessions table.
#[derive(Debug, FromRow)]
pub struct SessionRow {
    pub id:              String,
    pub state:           String,
    pub agent_role:      String,
    pub channel:         String,
    pub created_at:      i64,
    pub closed_at:       Option<i64>,
    pub message_count:   i64,
    pub tool_call_count: i64,
}

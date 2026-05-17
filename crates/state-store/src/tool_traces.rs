//! Tool trace repository — records of every tool invocation.

use chrono::Utc;
use serde_json::Value;
use sqlx::SqlitePool;

use claw_types::tool::ToolTraceStatus;

use crate::errors::StoreError;

#[derive(Clone, Debug)]
pub struct ToolTraceRepository {
    pool: SqlitePool,
}

impl ToolTraceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn start(
        &self,
        id:             &str,
        session_id:     &str,
        correlation_id: &str,
        tool_name:      &str,
        input:          &Value,
    ) -> Result<(), StoreError> {
        let input_str = serde_json::to_string(input)?;
        let now       = Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO tool_traces
                 (id, session_id, correlation_id, tool_name, started_at, status, input)
             VALUES (?, ?, ?, ?, ?, 'running', ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(correlation_id)
        .bind(tool_name)
        .bind(now)
        .bind(input_str)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn finish(
        &self,
        id:          &str,
        status:      ToolTraceStatus,
        output:      Option<&Value>,
        error:       Option<&str>,
        duration_ms: u64,
    ) -> Result<(), StoreError> {
        let status_str = match status {
            ToolTraceStatus::Succeeded => "succeeded",
            ToolTraceStatus::Failed    => "failed",
            ToolTraceStatus::TimedOut  => "timed_out",
            ToolTraceStatus::Cancelled => "cancelled",
            ToolTraceStatus::Running   => "running",
        };
        let output_str = output.map(|o| serde_json::to_string(o)).transpose()?;
        let now        = Utc::now().timestamp_millis();
        let duration   = duration_ms as i64;

        sqlx::query(
            "UPDATE tool_traces
             SET status = ?, output = ?, error = ?, finished_at = ?, duration_ms = ?
             WHERE id = ?",
        )
        .bind(status_str)
        .bind(output_str)
        .bind(error)
        .bind(now)
        .bind(duration)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

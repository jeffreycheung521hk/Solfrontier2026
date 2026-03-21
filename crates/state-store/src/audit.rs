//! Audit log repository — append-only.
//!
//! The audit log is intentionally append-only: no `update` or `delete` methods.

use chrono::Utc;
use serde_json::Value;
use sqlx::{FromRow, SqlitePool};
use tracing::instrument;
use uuid::Uuid;

use crate::errors::StoreError;

/// Severity of an audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

impl AuditSeverity {
    fn as_str(self) -> &'static str {
        match self {
            AuditSeverity::Info     => "info",
            AuditSeverity::Warning  => "warning",
            AuditSeverity::Error    => "error",
            AuditSeverity::Critical => "critical",
        }
    }
}

/// The append-only audit repository.
#[derive(Clone, Debug)]
pub struct AuditRepository {
    pool: SqlitePool,
}

impl AuditRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Appends a new audit event. This is the ONLY write operation.
    #[instrument(skip(self, payload), fields(event_type = %event_type))]
    pub async fn append(
        &self,
        session_id:     Option<&str>,
        correlation_id: &str,
        event_type:     &str,
        actor:          &str,
        payload:        &Value,
        severity:       AuditSeverity,
    ) -> Result<String, StoreError> {
        let id           = Uuid::new_v4().to_string();
        let now          = Utc::now().timestamp_millis();
        let payload_str  = serde_json::to_string(payload)?;
        let severity_str = severity.as_str();

        sqlx::query(
            "INSERT INTO audit_events
                 (id, session_id, correlation_id, occurred_at, event_type, actor, payload, severity)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(session_id)
        .bind(correlation_id)
        .bind(now)
        .bind(event_type)
        .bind(actor)
        .bind(payload_str)
        .bind(severity_str)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Returns recent audit events for a session.
    pub async fn list_for_session(
        &self,
        session_id: &str,
        limit:      i64,
    ) -> Result<Vec<AuditRow>, StoreError> {
        let rows = sqlx::query_as::<_, AuditRow>(
            "SELECT id, session_id, correlation_id, occurred_at, event_type, actor, payload, severity
             FROM audit_events
             WHERE session_id = ?
             ORDER BY occurred_at DESC
             LIMIT ?",
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Returns recent security-relevant audit events.
    pub async fn list_security_events(&self, limit: i64) -> Result<Vec<AuditRow>, StoreError> {
        let rows = sqlx::query_as::<_, AuditRow>(
            "SELECT id, session_id, correlation_id, occurred_at, event_type, actor, payload, severity
             FROM audit_events
             WHERE event_type IN (
                 'policy_rejected', 'permission_denied', 'signing_failed',
                 'human_rejected', 'suspicious_instruction_detected'
             )
             ORDER BY occurred_at DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

/// Raw row from the audit_events table.
#[derive(Debug, FromRow)]
pub struct AuditRow {
    pub id:             String,
    pub session_id:     Option<String>,
    pub correlation_id: String,
    pub occurred_at:    i64,
    pub event_type:     String,
    pub actor:          String,
    pub payload:        String,
    pub severity:       String,
}

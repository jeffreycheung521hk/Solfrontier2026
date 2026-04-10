//! Repository for approval workflow persistence (Step 4).
//!
//! Each approval request has an associated workflow row that tracks the
//! full lifecycle: `pending` → `approved` | `rejected` | `expired`.
//!
//! Rows are never deleted — they serve as a durable audit record of the
//! approval lifecycle (complementing the append-only audit trail).

use sqlx::SqlitePool;

use crate::errors::StoreError;

/// A persisted approval workflow row.
#[derive(Debug, Clone)]
pub struct ApprovalWorkflowRow {
    pub request_id:  String,
    pub session_id:  String,
    pub state:       String,
    pub stages_json: String,
    pub created_at:  i64,
    pub updated_at:  i64,
}

/// Repository for approval workflow state persistence.
#[derive(Clone, Debug)]
pub struct ApprovalWorkflowRepository {
    pool: SqlitePool,
}

impl ApprovalWorkflowRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Create a new workflow row. Idempotent (INSERT OR IGNORE).
    pub async fn create(
        &self,
        request_id:  &str,
        session_id:  &str,
        state:       &str,
        stages_json: &str,
        created_at:  i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT OR IGNORE INTO approval_workflows
                 (request_id, session_id, state, stages_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(request_id)
        .bind(session_id)
        .bind(state)
        .bind(stages_json)
        .bind(created_at)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the workflow state and stages after a decision.
    pub async fn update(
        &self,
        request_id:  &str,
        state:       &str,
        stages_json: &str,
        updated_at:  i64,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE approval_workflows
             SET state = ?, stages_json = ?, updated_at = ?
             WHERE request_id = ?",
        )
        .bind(state)
        .bind(stages_json)
        .bind(updated_at)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Load a single workflow by request ID.
    pub async fn load(
        &self,
        request_id: &str,
    ) -> Result<Option<ApprovalWorkflowRow>, StoreError> {
        let row = sqlx::query_as::<_, (String, String, String, String, i64, i64)>(
            "SELECT request_id, session_id, state, stages_json, created_at, updated_at
             FROM approval_workflows
             WHERE request_id = ?",
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(request_id, session_id, state, stages_json, created_at, updated_at)| {
            ApprovalWorkflowRow {
                request_id,
                session_id,
                state,
                stages_json,
                created_at,
                updated_at,
            }
        }))
    }

    /// Load all workflows in a given state (e.g., "pending" for restart recovery).
    pub async fn load_by_state(
        &self,
        state: &str,
    ) -> Result<Vec<ApprovalWorkflowRow>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, String, String, i64, i64)>(
            "SELECT request_id, session_id, state, stages_json, created_at, updated_at
             FROM approval_workflows
             WHERE state = ?",
        )
        .bind(state)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(request_id, session_id, state, stages_json, created_at, updated_at)| {
                ApprovalWorkflowRow {
                    request_id,
                    session_id,
                    state,
                    stages_json,
                    created_at,
                    updated_at,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    async fn test_repo() -> ApprovalWorkflowRepository {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        ApprovalWorkflowRepository::new(db.pool().clone())
    }

    #[tokio::test]
    async fn create_and_load() {
        let repo = test_repo().await;

        repo.create("req-1", "sess-1", "pending", r#"[{"index":0}]"#, 1000)
            .await.unwrap();

        let row = repo.load("req-1").await.unwrap().expect("should exist");
        assert_eq!(row.request_id, "req-1");
        assert_eq!(row.session_id, "sess-1");
        assert_eq!(row.state, "pending");
        assert_eq!(row.created_at, 1000);
        assert_eq!(row.updated_at, 1000);
    }

    #[tokio::test]
    async fn create_is_idempotent() {
        let repo = test_repo().await;

        repo.create("req-1", "sess-1", "pending", "[]", 1000).await.unwrap();
        repo.create("req-1", "sess-1", "pending", "[]", 2000).await.unwrap(); // ignored

        let row = repo.load("req-1").await.unwrap().unwrap();
        assert_eq!(row.created_at, 1000, "first insert wins");
    }

    #[tokio::test]
    async fn update_transitions_state() {
        let repo = test_repo().await;

        repo.create("req-1", "sess-1", "pending", "[]", 1000).await.unwrap();

        let affected = repo.update("req-1", "approved", r#"[{"decided":true}]"#, 2000)
            .await.unwrap();
        assert_eq!(affected, 1);

        let row = repo.load("req-1").await.unwrap().unwrap();
        assert_eq!(row.state, "approved");
        assert_eq!(row.updated_at, 2000);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let repo = test_repo().await;
        assert!(repo.load("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_by_state_filters_correctly() {
        let repo = test_repo().await;

        repo.create("req-1", "s1", "pending", "[]", 1000).await.unwrap();
        repo.create("req-2", "s1", "approved", "[]", 1000).await.unwrap();
        repo.create("req-3", "s2", "pending", "[]", 1000).await.unwrap();

        let pending = repo.load_by_state("pending").await.unwrap();
        assert_eq!(pending.len(), 2);

        let approved = repo.load_by_state("approved").await.unwrap();
        assert_eq!(approved.len(), 1);
    }
}

//! Repository for per-session policy override persistence (Step 2).
//!
//! Write-through layer: `SessionPolicyStore` calls these methods alongside
//! its in-memory DashMap mutations so that overrides survive daemon restarts.
//!
//! The `rules_json` column stores the policy rules as a JSON array. This is
//! intentionally opaque to the state-store layer — deserialization happens in
//! the gateway layer, keeping claw-state-store free of policy logic.

use chrono::Utc;
use sqlx::SqlitePool;

use crate::errors::StoreError;

/// A persisted session policy override row.
#[derive(Debug, Clone)]
pub struct SessionPolicyRow {
    /// The session that owns these overrides.
    pub session_id: String,
    /// JSON-serialized `Vec<PolicyRule>`.
    pub rules_json: String,
    /// Unix epoch milliseconds when the overrides were stored.
    pub created_at: i64,
}

/// Repository for per-session policy overrides.
#[derive(Clone, Debug)]
pub struct SessionPolicyRepository {
    pool: SqlitePool,
}

impl SessionPolicyRepository {
    /// Create a new repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Persist policy overrides for a session. Replaces any existing row.
    ///
    /// Uses INSERT OR REPLACE so that re-opening a session with different
    /// overrides is idempotent.
    pub async fn save(
        &self,
        session_id: &str,
        rules_json: &str,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR REPLACE INTO session_policy_overrides
                 (session_id, rules_json, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(session_id)
        .bind(rules_json)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load the policy overrides for a single session.
    pub async fn load(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionPolicyRow>, StoreError> {
        let row = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT session_id, rules_json, created_at
             FROM session_policy_overrides
             WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(session_id, rules_json, created_at)| SessionPolicyRow {
            session_id,
            rules_json,
            created_at,
        }))
    }

    /// Load ALL persisted overrides (for startup recovery).
    pub async fn load_all(&self) -> Result<Vec<SessionPolicyRow>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT session_id, rules_json, created_at
             FROM session_policy_overrides",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(session_id, rules_json, created_at)| SessionPolicyRow {
                session_id,
                rules_json,
                created_at,
            })
            .collect())
    }

    /// Remove overrides for a session (called on session close).
    pub async fn delete_session(
        &self,
        session_id: &str,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "DELETE FROM session_policy_overrides WHERE session_id = ?",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Returns the number of persisted override rows.
    pub async fn count(&self) -> Result<i64, StoreError> {
        let (count,) = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM session_policy_overrides",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    async fn test_repo() -> SessionPolicyRepository {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        SessionPolicyRepository::new(db.pool().clone())
    }

    #[tokio::test]
    async fn save_and_load() {
        let repo = test_repo().await;

        let rules = r#"[{"name":"test","description":"d","condition":"Always","action":"Approve"}]"#;
        repo.save("s1", rules).await.unwrap();

        let row = repo.load("s1").await.unwrap().expect("should find row");
        assert_eq!(row.session_id, "s1");
        assert_eq!(row.rules_json, rules);
        assert!(row.created_at > 0);
    }

    #[tokio::test]
    async fn save_is_idempotent_replaces() {
        let repo = test_repo().await;

        repo.save("s1", r#"[{"name":"v1"}]"#).await.unwrap();
        repo.save("s1", r#"[{"name":"v2"}]"#).await.unwrap();

        let row = repo.load("s1").await.unwrap().unwrap();
        assert_eq!(row.rules_json, r#"[{"name":"v2"}]"#);

        assert_eq!(repo.count().await.unwrap(), 1, "should have exactly 1 row");
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let repo = test_repo().await;
        assert!(repo.load("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_session_removes_row() {
        let repo = test_repo().await;

        repo.save("s1", "[]").await.unwrap();
        repo.save("s2", "[]").await.unwrap();

        let removed = repo.delete_session("s1").await.unwrap();
        assert_eq!(removed, 1);

        assert!(repo.load("s1").await.unwrap().is_none());
        assert!(repo.load("s2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn load_all_returns_all_rows() {
        let repo = test_repo().await;

        repo.save("s1", r#"[{"name":"r1"}]"#).await.unwrap();
        repo.save("s2", r#"[{"name":"r2"}]"#).await.unwrap();
        repo.save("s3", r#"[{"name":"r3"}]"#).await.unwrap();

        let all = repo.load_all().await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_zero() {
        let repo = test_repo().await;
        let removed = repo.delete_session("nonexistent").await.unwrap();
        assert_eq!(removed, 0);
    }
}

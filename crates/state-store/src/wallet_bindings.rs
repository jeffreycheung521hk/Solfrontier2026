//! Repository for session-wallet binding persistence (C4).
//!
//! Write-through layer: `ExternalWalletStore` calls these methods
//! alongside its in-memory DashMap mutations so that bindings survive
//! daemon restarts.

use chrono::Utc;
use sqlx::SqlitePool;

use crate::errors::StoreError;

/// A persisted session-wallet binding row.
#[derive(Debug, Clone)]
pub struct WalletBindingRow {
    /// The session that owns this binding.
    pub session_id:    String,
    /// The base58-encoded wallet pubkey.
    pub wallet_pubkey: String,
    /// Unix epoch milliseconds when the binding was created.
    pub created_at:    i64,
}

/// Repository for session-wallet bindings.
#[derive(Clone, Debug)]
pub struct WalletBindingRepository {
    pool: SqlitePool,
}

impl WalletBindingRepository {
    /// Create a new repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Persist a wallet binding. Idempotent (INSERT OR IGNORE).
    pub async fn bind(
        &self,
        session_id:    &str,
        wallet_pubkey: &str,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR IGNORE INTO session_wallet_bindings
                 (session_id, wallet_pubkey, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(session_id)
        .bind(wallet_pubkey)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all wallet pubkeys bound to a session.
    pub async fn wallets_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT wallet_pubkey FROM session_wallet_bindings WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|(pk,)| pk).collect())
    }

    /// Load ALL bindings (for startup recovery).
    pub async fn load_all(&self) -> Result<Vec<WalletBindingRow>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT session_id, wallet_pubkey, created_at FROM session_wallet_bindings",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(session_id, wallet_pubkey, created_at)| WalletBindingRow {
                session_id,
                wallet_pubkey,
                created_at,
            })
            .collect())
    }

    /// Remove all bindings for a session (on session close).
    pub async fn unbind_session(&self, session_id: &str) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "DELETE FROM session_wallet_bindings WHERE session_id = ?",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    async fn test_repo() -> WalletBindingRepository {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        WalletBindingRepository::new(db.pool().clone())
    }

    #[tokio::test]
    async fn bind_and_load() {
        let repo = test_repo().await;

        repo.bind("s1", "pk-A").await.unwrap();
        repo.bind("s1", "pk-B").await.unwrap();
        repo.bind("s2", "pk-C").await.unwrap();

        let s1 = repo.wallets_for_session("s1").await.unwrap();
        assert_eq!(s1.len(), 2);
        assert!(s1.contains(&"pk-A".to_string()));
        assert!(s1.contains(&"pk-B".to_string()));

        let s2 = repo.wallets_for_session("s2").await.unwrap();
        assert_eq!(s2, vec!["pk-C".to_string()]);
    }

    #[tokio::test]
    async fn bind_is_idempotent() {
        let repo = test_repo().await;

        repo.bind("s1", "pk-A").await.unwrap();
        repo.bind("s1", "pk-A").await.unwrap(); // duplicate — INSERT OR IGNORE

        let wallets = repo.wallets_for_session("s1").await.unwrap();
        assert_eq!(wallets.len(), 1);
    }

    #[tokio::test]
    async fn unbind_session_removes_all() {
        let repo = test_repo().await;

        repo.bind("s1", "pk-A").await.unwrap();
        repo.bind("s1", "pk-B").await.unwrap();
        repo.bind("s2", "pk-C").await.unwrap();

        let removed = repo.unbind_session("s1").await.unwrap();
        assert_eq!(removed, 2);

        assert!(repo.wallets_for_session("s1").await.unwrap().is_empty());
        assert_eq!(repo.wallets_for_session("s2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn load_all_returns_all_bindings() {
        let repo = test_repo().await;

        repo.bind("s1", "pk1").await.unwrap();
        repo.bind("s1", "pk2").await.unwrap();
        repo.bind("s2", "pk3").await.unwrap();

        let all = repo.load_all().await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn empty_session_returns_empty_vec() {
        let repo = test_repo().await;
        assert!(repo.wallets_for_session("nonexistent").await.unwrap().is_empty());
    }
}

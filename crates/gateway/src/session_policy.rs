//! Per-session policy override store with optional SQLite write-through.
//!
//! Stores optional `PolicyRule` overrides scoped to a single session.
//! When present, session rules are evaluated **before** global rules in the
//! policy evaluation pipeline (session rules → global rules → defaults).
//!
//! # Persistence (Step 2)
//!
//! When a `SessionPolicyRepository` is attached via `with_repo()`, all
//! mutations are written through to SQLite so overrides survive daemon
//! restarts. On startup, `load_persisted()` restores in-memory state.
//!
//! Without a repo (test / legacy mode), the store is in-memory only.
//!
//! # Lifecycle
//!
//! - Populated on `POST /sessions` if the request includes `policy_overrides`
//! - Cleaned up on session close
//! - Recovered from SQLite on daemon restart

use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, warn};

use claw_state_store::SessionPolicyRepository;
use claw_types::agent::AgentRole;

use crate::config::RolePolicyProfile;

/// Builds the combined session policy rules from caller overrides + role profile.
///
/// This is the exact logic used by `GatewaySessionAdapter::open()`:
/// 1. Caller overrides (from POST /sessions body) — highest priority
/// 2. Role profile rules (from config `[[policy.role_profiles]]`) — lower priority
///
/// Returns empty Vec if neither applies (no rules to store).
///
/// Extracted as a public function so tests can verify the production wiring
/// without needing to construct the private `GatewaySessionAdapter`.
pub fn build_session_rules(
    role: AgentRole,
    caller_overrides: Option<Vec<claw_types::policy::PolicyRule>>,
    role_profiles: &[RolePolicyProfile],
) -> Vec<claw_types::policy::PolicyRule> {
    let caller_rules = caller_overrides.unwrap_or_default();

    let role_name = serde_json::to_value(role)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    let profile_rules: Vec<claw_types::policy::PolicyRule> = role_profiles
        .iter()
        .find(|p| p.role == role_name)
        .map(|p| p.rules.clone())
        .unwrap_or_default();

    caller_rules.into_iter().chain(profile_rules).collect()
}
use claw_types::{
    policy::PolicyRule,
    session::SessionId,
};

/// Thread-safe store for per-session policy overrides.
///
/// Write-through to SQLite when a `SessionPolicyRepository` is attached.
/// Follows the same pattern as `ExternalWalletStore` with `WalletBindingRepository`.
#[derive(Debug, Clone)]
pub struct SessionPolicyStore {
    overrides: Arc<DashMap<SessionId, Vec<PolicyRule>>>,
    repo: Option<SessionPolicyRepository>,
}

impl SessionPolicyStore {
    /// Creates a new in-memory-only store (no persistence).
    pub fn new() -> Self {
        Self {
            overrides: Arc::new(DashMap::new()),
            repo: None,
        }
    }

    /// Attaches a SQLite repository for durable persistence.
    pub fn with_repo(mut self, repo: SessionPolicyRepository) -> Self {
        self.repo = Some(repo);
        self
    }

    /// Set policy overrides for a session. Replaces any existing overrides.
    ///
    /// Writes through to SQLite if a repo is attached. If the DB write fails,
    /// the in-memory state is still updated (fail-open for overrides — the
    /// worst case is losing overrides on restart, not blocking the session).
    pub async fn set(&self, session_id: SessionId, rules: Vec<PolicyRule>) {
        if rules.is_empty() {
            return; // Don't store empty override lists
        }

        // Memory FIRST — in-memory state is immediately consistent.
        // This prevents race conditions when the signing tool reads before
        // the async SQLite write completes (the set() is called from tokio::spawn).
        self.overrides.insert(session_id.clone(), rules.clone());

        // SQLite second — eventually consistent (write-through).
        if let Some(ref repo) = self.repo {
            match serde_json::to_string(&rules) {
                Ok(json) => {
                    if let Err(e) = repo.save(&session_id.to_string(), &json).await {
                        warn!(
                            session = %session_id,
                            error = %e,
                            "failed to persist session policy overrides — in-memory only"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        session = %session_id,
                        error = %e,
                        "failed to serialize session policy rules — in-memory only"
                    );
                }
            }
        }
    }

    /// Get the policy overrides for a session, if any.
    pub fn get(&self, session_id: &SessionId) -> Option<Vec<PolicyRule>> {
        self.overrides.get(session_id).map(|r| r.value().clone())
    }

    /// Remove overrides for a session (called on session close).
    ///
    /// Removes from both in-memory and SQLite.
    pub async fn remove(&self, session_id: &SessionId) {
        self.overrides.remove(session_id);

        if let Some(ref repo) = self.repo {
            if let Err(e) = repo.delete_session(&session_id.to_string()).await {
                warn!(
                    session = %session_id,
                    error = %e,
                    "failed to delete persisted session policy overrides"
                );
            }
        }
    }

    /// Returns true if the session has any policy overrides.
    pub fn has_overrides(&self, session_id: &SessionId) -> bool {
        self.overrides.contains_key(session_id)
    }

    /// Loads all persisted session policy overrides into memory.
    ///
    /// Called once at daemon startup. Returns the number of sessions recovered.
    pub async fn load_persisted(&self) -> Result<usize, String> {
        let repo = match &self.repo {
            Some(r) => r,
            None => return Ok(0),
        };

        let rows = repo.load_all().await.map_err(|e| e.to_string())?;
        let mut count = 0;

        for row in rows {
            match serde_json::from_str::<Vec<PolicyRule>>(&row.rules_json) {
                Ok(rules) if !rules.is_empty() => {
                    let uuid = match row.session_id.parse::<uuid::Uuid>() {
                        Ok(u) => u,
                        Err(e) => {
                            warn!(
                                session_id = %row.session_id,
                                error = %e,
                                "invalid session_id in persisted policy row — skipping"
                            );
                            continue;
                        }
                    };
                    let sid = SessionId::from(uuid);
                    debug!(
                        session = %sid,
                        rules = rules.len(),
                        "recovered session policy overrides"
                    );
                    self.overrides.insert(sid, rules);
                    count += 1;
                }
                Ok(_) => {
                    // Empty rules — skip (shouldn't happen, but defensive)
                }
                Err(e) => {
                    warn!(
                        session = %row.session_id,
                        error = %e,
                        "failed to deserialize persisted session policy rules — skipping"
                    );
                }
            }
        }

        Ok(count)
    }
}

impl Default for SessionPolicyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_types::policy::{PolicyAction, PolicyCondition};

    fn test_rule(name: &str) -> PolicyRule {
        PolicyRule {
            name: name.to_string(),
            description: format!("test rule {name}"),
            condition: PolicyCondition::Always,
            action: PolicyAction::Approve,
        }
    }

    // ── In-memory tests (no repo) ──────────────────────────────────────────

    #[tokio::test]
    async fn set_and_get_overrides() {
        let store = SessionPolicyStore::new();
        let sid = SessionId::new();

        assert!(store.get(&sid).is_none());
        assert!(!store.has_overrides(&sid));

        store.set(sid.clone(), vec![test_rule("a"), test_rule("b")]).await;

        assert!(store.has_overrides(&sid));
        let rules = store.get(&sid).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name, "a");
    }

    #[tokio::test]
    async fn empty_overrides_not_stored() {
        let store = SessionPolicyStore::new();
        let sid = SessionId::new();

        store.set(sid.clone(), vec![]).await;
        assert!(!store.has_overrides(&sid));
    }

    #[tokio::test]
    async fn remove_cleans_up() {
        let store = SessionPolicyStore::new();
        let sid = SessionId::new();

        store.set(sid.clone(), vec![test_rule("x")]).await;
        assert!(store.has_overrides(&sid));

        store.remove(&sid).await;
        assert!(!store.has_overrides(&sid));
        assert!(store.get(&sid).is_none());
    }

    #[tokio::test]
    async fn sessions_are_isolated() {
        let store = SessionPolicyStore::new();
        let s1 = SessionId::new();
        let s2 = SessionId::new();

        store.set(s1.clone(), vec![test_rule("s1-rule")]).await;
        store.set(s2.clone(), vec![test_rule("s2-rule")]).await;

        assert_eq!(store.get(&s1).unwrap()[0].name, "s1-rule");
        assert_eq!(store.get(&s2).unwrap()[0].name, "s2-rule");

        store.remove(&s1).await;
        assert!(!store.has_overrides(&s1));
        assert!(store.has_overrides(&s2));
    }

    // ── Write-through tests (with SQLite repo) ────────────────────────────

    async fn store_with_repo() -> SessionPolicyStore {
        let db = claw_state_store::Database::open_in_memory()
            .await
            .expect("in-memory DB");
        let repo = SessionPolicyRepository::new(db.pool().clone());
        SessionPolicyStore::new().with_repo(repo)
    }

    #[tokio::test]
    async fn write_through_persists_to_sqlite() {
        let store = store_with_repo().await;
        let sid = SessionId::new();

        store.set(sid.clone(), vec![test_rule("persisted")]).await;

        // Verify it's in the DB via the repo.
        let repo = store.repo.as_ref().unwrap();
        let row = repo.load(&sid.to_string()).await.unwrap();
        assert!(row.is_some(), "should be persisted in SQLite");
        let row = row.unwrap();
        assert!(row.rules_json.contains("persisted"));
    }

    #[tokio::test]
    async fn remove_deletes_from_sqlite() {
        let store = store_with_repo().await;
        let sid = SessionId::new();

        store.set(sid.clone(), vec![test_rule("temp")]).await;
        store.remove(&sid).await;

        let repo = store.repo.as_ref().unwrap();
        assert!(repo.load(&sid.to_string()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_persisted_recovers_from_sqlite() {
        let db = claw_state_store::Database::open_in_memory()
            .await
            .expect("in-memory DB");
        let repo = SessionPolicyRepository::new(db.pool().clone());

        // Simulate a previous daemon run: write directly to repo.
        let sid = SessionId::new();
        let rules = vec![test_rule("recovered")];
        let json = serde_json::to_string(&rules).unwrap();
        repo.save(&sid.to_string(), &json).await.unwrap();

        // Create a fresh store with the same repo (simulating restart).
        let store = SessionPolicyStore::new().with_repo(repo);
        assert!(!store.has_overrides(&sid), "should be empty before load");

        let count = store.load_persisted().await.unwrap();
        assert_eq!(count, 1);
        assert!(store.has_overrides(&sid));

        let loaded = store.get(&sid).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "recovered");
    }

    #[tokio::test]
    async fn load_persisted_skips_corrupt_rows() {
        let db = claw_state_store::Database::open_in_memory()
            .await
            .expect("in-memory DB");
        let repo = SessionPolicyRepository::new(db.pool().clone());

        let sid = SessionId::new();
        // Write corrupt JSON directly.
        repo.save(&sid.to_string(), "NOT VALID JSON").await.unwrap();

        let store = SessionPolicyStore::new().with_repo(repo);
        let count = store.load_persisted().await.unwrap();
        assert_eq!(count, 0, "corrupt row should be skipped, not crash");
    }
}

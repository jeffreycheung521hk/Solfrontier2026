//! N24 — Session policy lifecycle: role profile auto-application and precedence.
//!
//! Verifies:
//! 1. When a session is opened with a role that has a matching `RolePolicyProfile`,
//!    the profile rules are stored in the `SessionPolicyStore`.
//! 2. When caller overrides are also provided, caller rules appear before profile rules
//!    (caller-first precedence).

use claw_types::{
    agent::AgentRole,
    policy::{PolicyAction, PolicyCondition, PolicyRule},
    session::SessionId,
};
use claw_gateway::config::RolePolicyProfile;
use claw_gateway::session_policy::SessionPolicyStore;

fn make_rule(name: &str, threshold: u64) -> PolicyRule {
    PolicyRule {
        name: name.to_string(),
        description: format!("test rule {name}"),
        condition: PolicyCondition::AmountExceedsLamports(threshold),
        action: PolicyAction::RequireHumanApproval {
            reason: format!("{name} triggered"),
            required_approver_role: None,
        },
    }
}

/// Calls the exact production function used by GatewaySessionAdapter::open(),
/// then stores the result. This is NOT a simulation — it's the real function.
async fn production_session_open(
    store: &SessionPolicyStore,
    sid: SessionId,
    role: AgentRole,
    policy_overrides: Option<Vec<PolicyRule>>,
    role_profiles: &[RolePolicyProfile],
) {
    // Call the EXACT production function extracted from daemon.rs.
    let combined = claw_gateway::session_policy::build_session_rules(
        role,
        policy_overrides,
        role_profiles,
    );

    if !combined.is_empty() {
        store.set(sid, combined).await;
    }
}

#[tokio::test]
async fn role_profile_is_auto_applied_when_session_has_no_overrides() {
    let store = SessionPolicyStore::new();
    let sid = SessionId::new();

    let profile_rule = make_rule("exec-cap", 100_000_000);

    let profiles = vec![RolePolicyProfile {
        role: "execution".to_string(),
        rules: vec![profile_rule.clone()],
    }];

    // Open session with role=Execution, no caller overrides
    production_session_open(&store, sid.clone(), AgentRole::Execution, None, &profiles).await;

    // Assert: profile rules are stored
    assert!(store.has_overrides(&sid), "session should have overrides from profile");

    let rules = store.get(&sid).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "exec-cap");
}

#[tokio::test]
async fn no_profile_match_stores_nothing() {
    let store = SessionPolicyStore::new();
    let sid = SessionId::new();

    // Profile exists only for "risk", but session uses Execution
    let profiles = vec![RolePolicyProfile {
        role: "risk".to_string(),
        rules: vec![make_rule("risk-cap", 50_000_000)],
    }];

    production_session_open(&store, sid.clone(), AgentRole::Execution, None, &profiles).await;

    assert!(!store.has_overrides(&sid), "no matching profile → no overrides");
}

#[tokio::test]
async fn precedence_is_caller_then_profile() {
    let store = SessionPolicyStore::new();
    let sid = SessionId::new();

    let caller_rule = make_rule("caller-cap", 10_000_000);
    let profile_rule = make_rule("profile-cap", 200_000_000);

    let profiles = vec![RolePolicyProfile {
        role: "execution".to_string(),
        rules: vec![profile_rule],
    }];

    // Open session with caller overrides AND a matching profile
    production_session_open(
        &store,
        sid.clone(),
        AgentRole::Execution,
        Some(vec![caller_rule]),
        &profiles,
    )
    .await;

    let rules = store.get(&sid).unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].name, "caller-cap", "caller rules come first");
    assert_eq!(rules[1].name, "profile-cap", "profile rules come second");
}

#[tokio::test]
async fn caller_overrides_alone_are_stored_even_without_profile() {
    let store = SessionPolicyStore::new();
    let sid = SessionId::new();

    let caller_rule = make_rule("caller-only", 5_000_000);

    // No profiles at all
    production_session_open(
        &store,
        sid.clone(),
        AgentRole::Execution,
        Some(vec![caller_rule]),
        &[],
    )
    .await;

    let rules = store.get(&sid).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "caller-only");
}

// ── Persistence tests (SQLite write-through) ────────────────────────────────

#[tokio::test]
async fn session_policy_overrides_are_persisted_and_recovered_after_restart() {
    use claw_state_store::{Database, SessionPolicyRepository};

    let db = Database::open_in_memory().await.expect("in-memory DB");
    let repo = SessionPolicyRepository::new(db.pool().clone());

    let sid = SessionId::new();
    let rules = vec![make_rule("rule-alpha", 100_000), make_rule("rule-beta", 200_000)];

    // Phase 1: create store, set rules, then drop it.
    {
        let store = SessionPolicyStore::new().with_repo(repo.clone());
        store.set(sid.clone(), rules.clone()).await;
    }
    // Store is dropped here; only SQLite survives.

    // Phase 2: create a NEW store with the SAME repo (simulating restart).
    let store2 = SessionPolicyStore::new().with_repo(repo);
    assert!(
        store2.get(&sid).is_none(),
        "new store should be empty before load_persisted"
    );

    let count = store2.load_persisted().await.expect("load should succeed");
    assert_eq!(count, 1, "should recover exactly 1 session");

    let recovered = store2.get(&sid).expect("session should be in store after recovery");
    assert_eq!(recovered.len(), 2, "should recover 2 rules");
    assert_eq!(recovered[0].name, "rule-alpha");
    assert_eq!(recovered[1].name, "rule-beta");
}

#[tokio::test]
async fn closing_session_removes_persisted_overrides() {
    use claw_state_store::{Database, SessionPolicyRepository};

    let db = Database::open_in_memory().await.expect("in-memory DB");
    let repo = SessionPolicyRepository::new(db.pool().clone());

    let sid = SessionId::new();
    let store = SessionPolicyStore::new().with_repo(repo.clone());

    store.set(sid.clone(), vec![make_rule("temp-rule", 50_000)]).await;
    assert!(store.get(&sid).is_some(), "rules should exist after set");

    store.remove(&sid).await;
    assert!(store.get(&sid).is_none(), "rules should be gone after remove");

    // Verify SQLite is clean: new store with same repo should find nothing.
    let store2 = SessionPolicyStore::new().with_repo(repo);
    let count = store2.load_persisted().await.expect("load should succeed");
    assert_eq!(count, 0, "SQLite should have no rows after remove");
}

#[tokio::test]
async fn corrupt_persisted_policy_row_is_skipped_without_crash() {
    use claw_state_store::{Database, SessionPolicyRepository};

    let db = Database::open_in_memory().await.expect("in-memory DB");
    let repo = SessionPolicyRepository::new(db.pool().clone());

    // 1. Write one VALID row.
    let valid_sid = SessionId::new();
    let valid_rules = vec![make_rule("valid-rule", 100)];
    let valid_json = serde_json::to_string(&valid_rules).unwrap();
    repo.save(&valid_sid.to_string(), &valid_json).await.unwrap();

    // 2. Write one row with CORRUPT JSON (valid UUID, bad JSON).
    let corrupt_sid = SessionId::new();
    repo.save(&corrupt_sid.to_string(), "NOT JSON AT ALL").await.unwrap();

    // 3. Write one row with INVALID UUID but valid JSON.
    repo.save("not-a-uuid", &valid_json).await.unwrap();

    // 4. Load into a fresh store.
    let store = SessionPolicyStore::new().with_repo(repo);
    let result = store.load_persisted().await;

    // After the bug fix, load_persisted should succeed and skip bad rows.
    assert!(result.is_ok(), "load_persisted should not crash on bad rows: {:?}", result);
    let count = result.unwrap();
    assert_eq!(count, 1, "only the valid row should be recovered");

    assert!(store.has_overrides(&valid_sid), "valid session should be recovered");
    assert!(!store.has_overrides(&corrupt_sid), "corrupt JSON session should be skipped");
}

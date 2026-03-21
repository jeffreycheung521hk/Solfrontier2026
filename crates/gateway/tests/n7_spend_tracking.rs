//! N7 Spend Tracking — Validation Tests
//!
//! Proves:
//! 1. Accumulated spend starts at zero
//! 2. Spend is recorded correctly after `record_spend()`
//! 3. Daily accumulation aggregates multiple transactions
//! 4. Cap check passes when cumulative spend is below cap
//! 5. Cap check blocks when cumulative spend exceeds cap
//! 6. Duplicate transaction_id does not double-count (INSERT OR IGNORE)
//! 7. Multi-transaction stability for the same wallet across sessions
//!
//! Uses: in-memory SQLite, no HTTP listener, no real RPC.

use uuid::Uuid;

use claw_state_store::{
    db::Database,
    spend::SpendRepository,
};
use claw_risk_engine::rules::spend_cap::{exceeds_cap, sol_to_lamports};

async fn setup() -> SpendRepository {
    let db = Database::open_in_memory().await.expect("in-memory DB");
    SpendRepository::new(db.pool().clone())
}

const WALLET_A: &str = "WalletAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const WALLET_B: &str = "WalletBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

// ── Test 1: Accumulated spend starts at zero ─────────────────────────────────

#[tokio::test]
async fn spend_starts_at_zero() {
    let spend = setup().await;
    let daily = spend.wallet_daily_spend(WALLET_A, &today()).await.unwrap();
    assert_eq!(daily, 0, "fresh wallet daily spend must be zero");

    let session = spend.session_spend("session-new").await.unwrap();
    assert_eq!(session, 0, "fresh session spend must be zero");

    let count = spend.wallet_daily_count(WALLET_A, &today()).await.unwrap();
    assert_eq!(count, 0, "fresh wallet daily count must be zero");
}

// ── Test 2: Spend recorded correctly ─────────────────────────────────────────

#[tokio::test]
async fn spend_recorded_correctly() {
    let spend = setup().await;
    let session_id = Uuid::new_v4().to_string();
    let tx_id = Uuid::new_v4().to_string();

    let inserted = spend
        .record_spend(WALLET_A, &session_id, &tx_id, 5000)
        .await
        .unwrap();
    assert!(inserted, "first insert should return true");

    let daily = spend.wallet_daily_spend(WALLET_A, &today()).await.unwrap();
    assert_eq!(daily, 5000, "daily spend should reflect recorded amount");

    let session = spend.session_spend(&session_id).await.unwrap();
    assert_eq!(session, 5000, "session spend should reflect recorded amount");
}

// ── Test 3: Daily accumulation across multiple transactions ──────────────────

#[tokio::test]
async fn daily_accumulation() {
    let spend = setup().await;
    let session_id = Uuid::new_v4().to_string();

    let amounts = [1_000u64, 2_000, 3_000, 4_000];
    for (i, &amt) in amounts.iter().enumerate() {
        let tx_id = format!("tx-{i}");
        spend
            .record_spend(WALLET_A, &session_id, &tx_id, amt)
            .await
            .unwrap();
    }

    let daily = spend.wallet_daily_spend(WALLET_A, &today()).await.unwrap();
    let expected: u64 = amounts.iter().sum();
    assert_eq!(daily, expected, "daily spend must sum all transactions");

    let count = spend.wallet_daily_count(WALLET_A, &today()).await.unwrap();
    assert_eq!(count, 4, "should have 4 spend entries");
}

// ── Test 4: Cap check passes when below cap ──────────────────────────────────

#[tokio::test]
async fn cap_check_passes_below_cap() {
    let spend = setup().await;
    let session_id = Uuid::new_v4().to_string();

    // Record 0.5 SOL
    spend
        .record_spend(WALLET_A, &session_id, "tx-1", sol_to_lamports(0.5))
        .await
        .unwrap();

    let daily = spend.wallet_daily_spend(WALLET_A, &today()).await.unwrap();
    let cap = sol_to_lamports(1.0);
    let proposed = sol_to_lamports(0.3);

    assert!(
        !exceeds_cap(daily, proposed, cap),
        "0.5 + 0.3 = 0.8 SOL should not exceed 1.0 SOL cap"
    );
}

// ── Test 5: Cap check blocks when above cap ──────────────────────────────────

#[tokio::test]
async fn cap_check_blocks_above_cap() {
    let spend = setup().await;
    let session_id = Uuid::new_v4().to_string();

    // Record 0.8 SOL
    spend
        .record_spend(WALLET_A, &session_id, "tx-1", sol_to_lamports(0.8))
        .await
        .unwrap();

    let daily = spend.wallet_daily_spend(WALLET_A, &today()).await.unwrap();
    let cap = sol_to_lamports(1.0);
    let proposed = sol_to_lamports(0.3);

    assert!(
        exceeds_cap(daily, proposed, cap),
        "0.8 + 0.3 = 1.1 SOL should exceed 1.0 SOL cap"
    );
}

// ── Test 6: Duplicate transaction_id does not double-count ───────────────────

#[tokio::test]
async fn no_double_count_on_duplicate_tx_id() {
    let spend = setup().await;
    let session_id = Uuid::new_v4().to_string();
    let tx_id = "unique-tx-id";

    let first = spend
        .record_spend(WALLET_A, &session_id, tx_id, 10_000)
        .await
        .unwrap();
    assert!(first, "first insert should succeed");

    let second = spend
        .record_spend(WALLET_A, &session_id, tx_id, 10_000)
        .await
        .unwrap();
    assert!(!second, "duplicate insert should return false");

    let daily = spend.wallet_daily_spend(WALLET_A, &today()).await.unwrap();
    assert_eq!(daily, 10_000, "daily spend must NOT double-count");

    let count = spend.wallet_daily_count(WALLET_A, &today()).await.unwrap();
    assert_eq!(count, 1, "count must be 1 despite duplicate attempt");
}

// ── Test 7: Multi-transaction stability across wallets and sessions ──────────

#[tokio::test]
async fn multi_tx_stability() {
    let spend = setup().await;
    let session_1 = Uuid::new_v4().to_string();
    let session_2 = Uuid::new_v4().to_string();

    // Wallet A: 2 txns in session 1
    spend.record_spend(WALLET_A, &session_1, "tx-a1", 1_000).await.unwrap();
    spend.record_spend(WALLET_A, &session_1, "tx-a2", 2_000).await.unwrap();

    // Wallet A: 1 txn in session 2
    spend.record_spend(WALLET_A, &session_2, "tx-a3", 3_000).await.unwrap();

    // Wallet B: 1 txn in session 1
    spend.record_spend(WALLET_B, &session_1, "tx-b1", 5_000).await.unwrap();

    // Daily spend per wallet
    let a_daily = spend.wallet_daily_spend(WALLET_A, &today()).await.unwrap();
    assert_eq!(a_daily, 6_000, "wallet A daily: 1k + 2k + 3k = 6k");

    let b_daily = spend.wallet_daily_spend(WALLET_B, &today()).await.unwrap();
    assert_eq!(b_daily, 5_000, "wallet B daily: 5k");

    // Session spend
    let s1_spend = spend.session_spend(&session_1).await.unwrap();
    assert_eq!(s1_spend, 8_000, "session 1: 1k + 2k + 5k = 8k (both wallets)");

    let s2_spend = spend.session_spend(&session_2).await.unwrap();
    assert_eq!(s2_spend, 3_000, "session 2: 3k");

    // Cross-wallet isolation: wallet B's spend doesn't affect wallet A's daily
    assert_eq!(a_daily, 6_000);
    assert_eq!(b_daily, 5_000);
}

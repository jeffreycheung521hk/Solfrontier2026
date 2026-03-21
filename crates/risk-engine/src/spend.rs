//! Spend tracking for per-session and per-wallet-per-day spend caps.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::Utc;

/// Tracks cumulative spend for spend cap policy checks.
#[derive(Clone, Default)]
pub struct SpendTracker {
    /// session_id → lamports spent this session
    session_spend: Arc<RwLock<HashMap<String, u64>>>,
    /// (wallet_pubkey, utc_date_string) → lamports spent today
    daily_spend: Arc<RwLock<HashMap<(String, String), u64>>>,
}

impl SpendTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn session_spend(&self, session_id: &str) -> u64 {
        *self.session_spend.read().await.get(session_id).unwrap_or(&0)
    }

    pub async fn daily_spend(&self, wallet_pubkey: &str) -> u64 {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let key = (wallet_pubkey.to_string(), today);
        *self.daily_spend.read().await.get(&key).unwrap_or(&0)
    }

    pub async fn record_spend(&self, session_id: &str, wallet_pubkey: &str, lamports: u64) {
        let mut ss = self.session_spend.write().await;
        *ss.entry(session_id.to_string()).or_insert(0) += lamports;

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let key = (wallet_pubkey.to_string(), today);
        let mut ds = self.daily_spend.write().await;
        *ds.entry(key).or_insert(0) += lamports;
    }

    pub async fn clear_session(&self, session_id: &str) {
        self.session_spend.write().await.remove(session_id);
    }
}

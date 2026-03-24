//! Websocket subscription manager.
//!
//! The `SubscriptionManager` opens websocket connections to the Solana
//! pubsub endpoint and normalizes incoming frames into typed `SolanaEvent`s
//! that are sent to the gateway's event bus.
//!
//! V1 implementation: account subscription only, with reconnect logic.
//! V2 will add program, logs, and slot subscriptions.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};
use tracing::info;
use uuid::Uuid;

use claw_types::solana::SolanaEvent;

use crate::errors::SolanaError;

/// A registered subscription.
#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: Uuid,
    pub sub_type: SubscriptionType,
    pub target: String,
    pub ws_url: String,
}

/// The type of subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionType {
    Account,
    Program,
    Logs,
    Slot,
    Signature(String),
}

/// Manages active websocket subscriptions and routes normalized events
/// to the event bus.
///
/// Note: Full implementation requires `solana-pubsub-client` which has
/// complex async streaming. This is the structural skeleton; complete
/// implementation is in the integration phase.
#[derive(Clone)]
pub struct SubscriptionManager {
    ws_url: String,
    event_tx: broadcast::Sender<SolanaEvent>,
    subscriptions: Arc<RwLock<HashMap<Uuid, Subscription>>>,
}

impl SubscriptionManager {
    pub fn new(ws_url: impl Into<String>, event_tx: broadcast::Sender<SolanaEvent>) -> Self {
        Self {
            ws_url: ws_url.into(),
            event_tx,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Registers an account subscription.
    pub async fn subscribe_account(&self, pubkey: &str) -> Result<Uuid, SolanaError> {
        let id = Uuid::new_v4();
        let sub = Subscription {
            id,
            sub_type: SubscriptionType::Account,
            target: pubkey.to_string(),
            ws_url: self.ws_url.clone(),
        };

        self.subscriptions.write().await.insert(id, sub.clone());
        info!(id = %id, pubkey, "account subscription registered");

        // Background task to maintain the WS connection.
        // Full implementation in Phase 3.
        let tx = self.event_tx.clone();
        let ws_url = self.ws_url.clone();
        let target = pubkey.to_string();
        tokio::spawn(async move {
            Self::run_account_subscription(id, target, ws_url, tx).await;
        });

        Ok(id)
    }

    async fn run_account_subscription(
        id: Uuid,
        pubkey: String,
        _ws_url: String,
        _tx: broadcast::Sender<SolanaEvent>,
    ) {
        // TODO: Phase 3 — implement with solana_pubsub_client::PubsubClient
        // with exponential backoff reconnect logic.
        info!(id = %id, pubkey = %pubkey, "account subscription task started (stub)");
    }

    pub async fn unsubscribe(&self, id: Uuid) {
        self.subscriptions.write().await.remove(&id);
        info!(id = %id, "subscription removed");
    }

    pub async fn list(&self) -> Vec<Subscription> {
        self.subscriptions.read().await.values().cloned().collect()
    }
}

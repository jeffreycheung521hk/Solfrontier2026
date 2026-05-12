//! W5i — gateway-side adapter for the
//! `GET /sessions/:id/stage2/w5h/order/:rule_id_hex` route.
//!
//! Reads a single `W5hFundingIntent` row from the state-store and
//! collapses it into the wire DTO. No RPC, no signing, no
//! background side effects.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use claw_api::state::{
    W5hOrderStatusDto, W5hOrderStatusHandler, W5hOrderStatusHandlerRef,
    W5hOrderStatusRouteOutcome,
};
use claw_state_store::stage2_w5h_funding::{
    Stage2W5hFundingIntentRepository, W5hFundingIntent, W5hIntentStatus,
};
use claw_types::session::SessionId;

#[derive(Clone)]
pub struct GatewayW5hOrderStatusAdapter {
    intent_repo: Arc<Stage2W5hFundingIntentRepository>,
    auto_execution_enabled: bool,
}

impl std::fmt::Debug for GatewayW5hOrderStatusAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayW5hOrderStatusAdapter")
            .field("auto_execution_enabled", &self.auto_execution_enabled)
            .finish()
    }
}

impl GatewayW5hOrderStatusAdapter {
    pub fn new(
        intent_repo: Arc<Stage2W5hFundingIntentRepository>,
        auto_execution_enabled: bool,
    ) -> Self {
        Self {
            intent_repo,
            auto_execution_enabled,
        }
    }

    pub fn into_handler_ref(self) -> W5hOrderStatusHandlerRef {
        W5hOrderStatusHandlerRef::new(Arc::new(self))
    }
}

#[async_trait]
impl W5hOrderStatusHandler for GatewayW5hOrderStatusAdapter {
    fn get(
        &self,
        _session_id: &SessionId,
        rule_id_hex: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = W5hOrderStatusRouteOutcome> + Send + '_>> {
        let intent_repo = self.intent_repo.clone();
        let auto_execution_enabled = self.auto_execution_enabled;
        let rule_id = rule_id_hex.to_string();
        Box::pin(async move {
            match intent_repo.get(&rule_id).await {
                Ok(Some(intent)) => W5hOrderStatusRouteOutcome::Ok(
                    intent_to_dto(intent, auto_execution_enabled),
                ),
                Ok(None) => W5hOrderStatusRouteOutcome::NotFound(format!(
                    "no W5h funding intent for rule_id_hex={rule_id}"
                )),
                Err(e) => W5hOrderStatusRouteOutcome::BadRequest(format!(
                    "repo.get failed: {e}"
                )),
            }
        })
    }
}

fn intent_to_dto(
    intent: W5hFundingIntent,
    auto_execution_enabled: bool,
) -> W5hOrderStatusDto {
    let status_label = intent.status.as_str().to_string();
    let budget_status = derive_budget_status(intent.status).to_string();
    let solscan_url = intent
        .execution_signature
        .as_ref()
        .map(|s| format!("https://solscan.io/tx/{s}"));
    W5hOrderStatusDto {
        rule_id_hex: intent.rule_id_hex,
        canonical_rule_hash_hex: intent.canonical_rule_hash_hex,
        status: status_label,
        budget_status,
        auto_execution_enabled,
        user_wallet: intent.user_wallet,
        user_usdc_ata: intent.user_usdc_ata,
        controlled_wallet: intent.controlled_wallet,
        controlled_usdc_ata: intent.controlled_usdc_ata,
        amount_raw: intent.amount_raw,
        threshold_bps: intent.threshold_bps,
        save_display_apy_bps_at_creation: intent.save_display_apy_bps_at_creation,
        native_onchain_apr_bps_at_creation: intent.native_onchain_apr_bps_at_creation,
        created_at_ms: intent.created_at_ms,
        expires_at_ms: intent.expires_at_ms,
        funding_signature: intent.funding_signature,
        funding_confirmation_slot: intent.funding_finalized_slot,
        execution_signature: intent.execution_signature,
        solscan_url,
        refund_signature: intent.refund_signature,
        last_error: intent.last_error,
    }
}

fn derive_budget_status(status: W5hIntentStatus) -> &'static str {
    match status {
        W5hIntentStatus::FundingRequired => "needs_funding",
        W5hIntentStatus::FundingSubmitted => "pending",
        W5hIntentStatus::FundingInvalid => "needs_funding",
        W5hIntentStatus::BudgetReserved => "reserved",
        W5hIntentStatus::Executing => "executing",
        W5hIntentStatus::Completed => "completed",
        W5hIntentStatus::Expired => "expired",
        W5hIntentStatus::Refunding => "refunding",
        W5hIntentStatus::Refunded => "refunded",
        W5hIntentStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_state_store::db::Database;
    use claw_state_store::stage2_w5h_funding::NewW5hFundingIntent;

    fn fixture(intent_id: &str) -> NewW5hFundingIntent {
        let now = chrono::Utc::now().timestamp_millis();
        NewW5hFundingIntent {
            intent_id: intent_id.to_string(),
            rule_id_hex: intent_id.to_string(),
            canonical_rule_hash_hex: "00".repeat(32),
            user_wallet: "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".into(),
            user_usdc_ata: "TestUserUsdcAta1111111111111111111111111111".into(),
            controlled_wallet:
                "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L".into(),
            controlled_usdc_ata:
                "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3".into(),
            amount_raw: 250_000,
            threshold_bps: 100,
            save_display_apy_bps_at_creation: 312,
            native_onchain_apr_bps_at_creation: 287,
            created_at_ms: now,
            expires_at_ms: now + 180_000,
        }
    }

    #[tokio::test]
    async fn not_found_when_rule_id_unknown() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        let adapter = GatewayW5hOrderStatusAdapter::new(repo, true);
        let session_id = SessionId::from(uuid::Uuid::new_v4());
        let outcome = adapter.get(&session_id, "ff".repeat(16).as_str()).await;
        match outcome {
            W5hOrderStatusRouteOutcome::NotFound(_) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ok_dto_carries_completed_status_and_solscan_url() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        let intent = fixture(&"a".repeat(32));
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "FundSig")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "FundSig", 100)
            .await
            .unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        repo.lease_execution_if_budget_reserved(&intent.intent_id, now)
            .await
            .unwrap();
        repo.mark_completed_if_executing(&intent.intent_id, "ExecSig")
            .await
            .unwrap();

        let adapter = GatewayW5hOrderStatusAdapter::new(repo, true);
        let session_id = SessionId::from(uuid::Uuid::new_v4());
        let outcome = adapter.get(&session_id, &intent.intent_id).await;
        match outcome {
            W5hOrderStatusRouteOutcome::Ok(dto) => {
                assert_eq!(dto.status, "completed");
                assert_eq!(dto.budget_status, "completed");
                assert_eq!(dto.execution_signature.as_deref(), Some("ExecSig"));
                assert_eq!(
                    dto.solscan_url.as_deref(),
                    Some("https://solscan.io/tx/ExecSig")
                );
                assert!(dto.auto_execution_enabled);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ok_dto_for_budget_reserved_intent() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        let intent = fixture(&"b".repeat(32));
        repo.insert(&intent).await.unwrap();
        repo.mark_funding_submitted_if_required(&intent.intent_id, "FundSig")
            .await
            .unwrap();
        repo.mark_budget_reserved_if_submitted(&intent.intent_id, "FundSig", 100)
            .await
            .unwrap();
        let adapter = GatewayW5hOrderStatusAdapter::new(repo, false);
        let session_id = SessionId::from(uuid::Uuid::new_v4());
        let outcome = adapter.get(&session_id, &intent.intent_id).await;
        match outcome {
            W5hOrderStatusRouteOutcome::Ok(dto) => {
                assert_eq!(dto.status, "budget_reserved");
                assert_eq!(dto.budget_status, "reserved");
                assert!(dto.execution_signature.is_none());
                assert!(dto.solscan_url.is_none());
                // Manual W5g path: watcher gates not all on.
                assert!(!dto.auto_execution_enabled);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }
}

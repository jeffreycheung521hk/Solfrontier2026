//! W5g — gateway-side adapter that implements the api crate's
//! [`claw_api::state::ChatExecuteHandler`] over the internal
//! [`crate::stage2_chat_execute::Stage2ChatExecutor`].
//!
//! This module is the single boundary where the rich gateway-side
//! types (`ChatExecuteOutcome`, `ChatExecuteErrorCode`,
//! `ChatExecuteStatus`) collapse into the lean wire DTOs
//! (`ChatExecuteResultDto`) the api crate exposes. The api crate has
//! NO build-time dependency on the gateway crate; this adapter is
//! the one place that knows both shapes.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use claw_api::state::{
    ChatExecuteHandler, ChatExecuteHandlerRef, ChatExecuteRequestDto,
    ChatExecuteResultDto, ChatExecuteRouteOutcome,
};
use claw_types::session::SessionId;

use crate::stage2_chat_execute::{
    ChatExecuteErrorCode, ChatExecuteOutcome, ChatExecuteRequest,
    ChatExecuteStatus, Stage2ChatExecutor,
};

/// Adapter that bridges `Stage2ChatExecutor` (gateway internal) to
/// the API trait. Cloneable — internally just an `Arc` over the
/// executor.
#[derive(Clone, Debug)]
pub struct GatewayChatExecuteAdapter {
    executor: Arc<Stage2ChatExecutor>,
}

impl GatewayChatExecuteAdapter {
    pub fn new(executor: Arc<Stage2ChatExecutor>) -> Self {
        Self { executor }
    }

    /// Build the API-layer ref consumed by `AppState.chat_execute`.
    pub fn into_handler_ref(self) -> ChatExecuteHandlerRef {
        ChatExecuteHandlerRef::new(Arc::new(self))
    }
}

#[async_trait]
impl ChatExecuteHandler for GatewayChatExecuteAdapter {
    fn execute(
        &self,
        _session_id: &SessionId,
        request: ChatExecuteRequestDto,
    ) -> Pin<Box<dyn std::future::Future<Output = ChatExecuteRouteOutcome> + Send + '_>> {
        let executor = self.executor.clone();
        Box::pin(async move {
            // Map the wire DTO straight into the orchestrator's input.
            // String length / structural validation already happened at
            // the route handler boundary; this layer only translates.
            let internal = ChatExecuteRequest {
                rule_id_hex: request.rule_id_hex,
                canonical_rule_hash_hex: request.canonical_rule_hash_hex,
                approval_phrase: request.approval_phrase,
            };
            let outcome = executor.execute(internal).await;
            ChatExecuteRouteOutcome::Ok(map_outcome_to_dto(outcome))
        })
    }
}

/// Pure mapping from the rich [`ChatExecuteOutcome`] to the lean
/// [`ChatExecuteResultDto`]. Extracted so the test module can drive
/// it with hand-built outcomes and assert the wire surface without
/// spinning up the executor.
pub fn map_outcome_to_dto(o: ChatExecuteOutcome) -> ChatExecuteResultDto {
    ChatExecuteResultDto {
        status: status_label(o.status).to_string(),
        rule_id_hex: o.rule_id_hex,
        canonical_rule_hash_hex: o.canonical_rule_hash_hex,
        tx_signature: o.tx_signature,
        solscan_url: o.solscan_url,
        confirmation_slot: o.confirmation_slot,
        used_save_display_apy_bps: o.used_save_display_apy_bps,
        used_native_onchain_apr_bps: o.used_native_onchain_apr_bps,
        used_threshold_bps: o.used_threshold_bps,
        before_usdc_raw: o.before_usdc_raw,
        after_usdc_raw: o.after_usdc_raw,
        usdc_delta_raw: o.usdc_delta_raw,
        before_ctoken_amount: o.before_ctoken_amount,
        after_ctoken_amount: o.after_ctoken_amount,
        ctoken_delta_raw: o.ctoken_delta_raw,
        serialized_tx_bytes: o.serialized_tx_bytes.map(|n| n as u64),
        instruction_count: o.instruction_count.map(|n| n as u64),
        ctoken_ata_create_included: o.ctoken_ata_create_included,
        error_code: o.error.map(error_label_owned),
        error_reason: o.error_reason,
    }
}

fn status_label(s: ChatExecuteStatus) -> &'static str {
    match s {
        ChatExecuteStatus::Completed => "completed",
        ChatExecuteStatus::BroadcastedTimeout => "broadcasted_timeout",
        ChatExecuteStatus::PrechecksFailed => "prechecks_failed",
        ChatExecuteStatus::ExecutionFailed => "execution_failed",
    }
}

fn error_label_owned(code: ChatExecuteErrorCode) -> String {
    match code {
        ChatExecuteErrorCode::MasterGateMissing => "master_gate_missing",
        ChatExecuteErrorCode::EnvApprovalMismatch => "env_approval_mismatch",
        ChatExecuteErrorCode::RequestApprovalMismatch => "request_approval_mismatch",
        ChatExecuteErrorCode::ClusterMismatch => "cluster_mismatch",
        ChatExecuteErrorCode::KeypairPathMissing => "keypair_path_missing",
        ChatExecuteErrorCode::KeypairLoadFailed => "keypair_load_failed",
        ChatExecuteErrorCode::RpcUrlMissing => "rpc_url_missing",
        ChatExecuteErrorCode::RuleNotFound => "rule_not_found",
        ChatExecuteErrorCode::CanonicalHashMismatch => "canonical_hash_mismatch",
        ChatExecuteErrorCode::RuleNotExecutable => "rule_not_executable",
        ChatExecuteErrorCode::BudgetInsufficient => "budget_insufficient",
        ChatExecuteErrorCode::SaveApyBelowThreshold => "save_apy_below_threshold",
        ChatExecuteErrorCode::MarketDataUnavailable => "market_data_unavailable",
        ChatExecuteErrorCode::NativeAprFetchFailed => "native_apr_fetch_failed",
        ChatExecuteErrorCode::AmountMismatch => "amount_mismatch",
        ChatExecuteErrorCode::TxBuildFailed => "tx_build_failed",
        ChatExecuteErrorCode::TxSizeExceeded => "tx_size_exceeded",
        ChatExecuteErrorCode::BroadcastFailed => "broadcast_failed",
        ChatExecuteErrorCode::OnChainFailure => "on_chain_failure",
        ChatExecuteErrorCode::PollExhausted => "poll_exhausted",
        ChatExecuteErrorCode::PostTxFetchFailed => "post_tx_fetch_failed",
        ChatExecuteErrorCode::RepoUpdateFailed => "repo_update_failed",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_outcome_completed() -> ChatExecuteOutcome {
        ChatExecuteOutcome {
            status: ChatExecuteStatus::Completed,
            rule_id_hex: "00112233445566778899aabbccddeeff".into(),
            canonical_rule_hash_hex:
                "0000000000000000000000000000000000000000000000000000000000000000".into(),
            tx_signature: Some("AaaaaaaaSSSSSSSSiiiiiiiigggggggg".into()),
            solscan_url: Some("https://solscan.io/tx/AaaaaaaaSSSSSSSSiiiiiiiigggggggg".into()),
            confirmation_slot: Some(419_048_388),
            used_save_display_apy_bps: Some(210),
            used_native_onchain_apr_bps: Some(165),
            used_threshold_bps: Some(180),
            before_usdc_raw: Some(403_487),
            after_usdc_raw: Some(153_487),
            usdc_delta_raw: Some(-250_000),
            before_ctoken_amount: Some(0),
            after_ctoken_amount: Some(192_876),
            ctoken_delta_raw: Some(192_876),
            serialized_tx_bytes: Some(900),
            instruction_count: Some(5),
            ctoken_ata_create_included: Some(true),
            error: None,
            error_reason: None,
        }
    }

    #[test]
    fn dto_round_trip_for_completed_outcome_strings_all_u64_fields() {
        let dto = map_outcome_to_dto(fixture_outcome_completed());
        let json = serde_json::to_string(&dto).unwrap();
        // The serde_str helpers must emit JSON strings, not numbers,
        // for every u64 / i64 raw field. Spot-check the key ones.
        assert!(json.contains(r#""confirmation_slot":"419048388""#));
        assert!(json.contains(r#""before_usdc_raw":"403487""#));
        assert!(json.contains(r#""usdc_delta_raw":"-250000""#));
        assert!(json.contains(r#""after_ctoken_amount":"192876""#));
        assert!(json.contains(r#""ctoken_delta_raw":"192876""#));
        assert!(json.contains(r#""serialized_tx_bytes":"900""#));
        assert!(json.contains(r#""instruction_count":"5""#));
        // bps stays as a number per the addendum's "u64 only" rule.
        assert!(json.contains(r#""used_save_display_apy_bps":210"#));
        assert!(json.contains(r#""used_threshold_bps":180"#));
        // Round-trip.
        let back: ChatExecuteResultDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dto);
    }

    #[test]
    fn error_labels_round_trip_via_snake_case() {
        assert_eq!(error_label_owned(ChatExecuteErrorCode::MasterGateMissing), "master_gate_missing");
        assert_eq!(error_label_owned(ChatExecuteErrorCode::TxSizeExceeded), "tx_size_exceeded");
        assert_eq!(error_label_owned(ChatExecuteErrorCode::SaveApyBelowThreshold), "save_apy_below_threshold");
        assert_eq!(error_label_owned(ChatExecuteErrorCode::PollExhausted), "poll_exhausted");
    }

    #[test]
    fn status_label_round_trip() {
        assert_eq!(status_label(ChatExecuteStatus::Completed), "completed");
        assert_eq!(status_label(ChatExecuteStatus::BroadcastedTimeout), "broadcasted_timeout");
        assert_eq!(status_label(ChatExecuteStatus::PrechecksFailed), "prechecks_failed");
        assert_eq!(status_label(ChatExecuteStatus::ExecutionFailed), "execution_failed");
    }
}

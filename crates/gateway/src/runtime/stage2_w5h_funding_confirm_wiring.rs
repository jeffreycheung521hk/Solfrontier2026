//! W5h-lite — gateway-side adapter that implements the api crate's
//! [`claw_api::state::W5hFundingConfirmHandler`] over the internal
//! [`crate::stage2_w5h_funding_confirm::Stage2W5hFundingConfirmExecutor`].
//!
//! This module is the single boundary where the rich gateway-side
//! types ([`W5hFundingConfirmOutcome`], [`W5hFundingConfirmStatus`],
//! [`W5hFundingConfirmErrorCode`]) collapse into the lean wire DTOs
//! ([`claw_api::state::W5hFundingConfirmResultDto`]). The api crate
//! has NO build-time dependency on the gateway crate; this adapter is
//! the one place that knows both shapes.
//!
//! The HTTP route enforces "no long sleeps" at the source-guard
//! level; this adapter is a pure mapper.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use claw_api::state::{
    W5hFundingConfirmHandler, W5hFundingConfirmHandlerRef,
    W5hFundingConfirmRequestDto, W5hFundingConfirmResultDto,
    W5hFundingConfirmRouteOutcome,
};
use claw_types::session::SessionId;

use crate::stage2_w5h_funding_confirm::{
    Stage2W5hFundingConfirmExecutor, W5hFundingConfirmErrorCode,
    W5hFundingConfirmOutcome, W5hFundingConfirmRequest,
    W5hFundingConfirmStatus,
};

/// Adapter bridging `Stage2W5hFundingConfirmExecutor` (gateway
/// internal) to the API trait. Cloneable — internally just an `Arc`
/// over the executor.
#[derive(Clone, Debug)]
pub struct GatewayW5hFundingConfirmAdapter {
    executor: Arc<Stage2W5hFundingConfirmExecutor>,
}

impl GatewayW5hFundingConfirmAdapter {
    pub fn new(executor: Arc<Stage2W5hFundingConfirmExecutor>) -> Self {
        Self { executor }
    }

    /// Build the API-layer ref consumed by `AppState.chat_funding_confirm`.
    pub fn into_handler_ref(self) -> W5hFundingConfirmHandlerRef {
        W5hFundingConfirmHandlerRef::new(Arc::new(self))
    }
}

/// Daemon-startup builder. Returns `Some(handler_ref)` when the W5h
/// funding-confirm route should be wired (RPC URL present + fetcher
/// constructs), or `None` otherwise (route will return 503).
///
/// Kept in the wiring module so the daemon stays a thin orchestrator
/// and so unit tests can drive the helper deterministically.
///
/// The function NEVER panics, NEVER reads env, NEVER blocks.
pub fn build_w5h_funding_confirm_handler(
    rpc_url: Option<&str>,
    intent_repo: Arc<
        claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository,
    >,
) -> Option<W5hFundingConfirmHandlerRef> {
    let rpc_url = rpc_url
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let tx_fetcher = crate::stage2_w5h_funding_confirm::LiveTxFetcher::new(
        rpc_url.to_string(),
    )?;
    let tx_fetcher_arc: Arc<dyn crate::stage2_w5h_funding_confirm::TxFetcher> =
        Arc::new(tx_fetcher);
    let save_apy_fetcher: Arc<
        dyn crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher,
    > = Arc::new(
        crate::stage2_demo_apr_bridge::LiveSaveDisplayApyFetcher::with_default_base_url(),
    );
    let apr_fetcher: Option<Arc<dyn crate::stage2_demo_apr_bridge::W5dAprFetcher>> =
        crate::stage2_demo_apr_bridge::LiveW5dAprFetcher::new(rpc_url.to_string())
            .map(|f| {
                Arc::new(f) as Arc<dyn crate::stage2_demo_apr_bridge::W5dAprFetcher>
            });
    let executor = Arc::new(
        crate::stage2_w5h_funding_confirm::Stage2W5hFundingConfirmExecutor::new(
            intent_repo,
            tx_fetcher_arc,
            Some(save_apy_fetcher),
            apr_fetcher,
        ),
    );
    Some(GatewayW5hFundingConfirmAdapter::new(executor).into_handler_ref())
}

#[async_trait]
impl W5hFundingConfirmHandler for GatewayW5hFundingConfirmAdapter {
    fn execute(
        &self,
        _session_id: &SessionId,
        request: W5hFundingConfirmRequestDto,
    ) -> Pin<
        Box<dyn std::future::Future<Output = W5hFundingConfirmRouteOutcome> + Send + '_>,
    > {
        let executor = self.executor.clone();
        Box::pin(async move {
            // Translate the wire DTO into the orchestrator's input.
            // Structural validation (empty / too-long strings) already
            // happened at the route handler boundary; this layer is a
            // pure shape mapping.
            let internal = W5hFundingConfirmRequest {
                rule_id_hex: request.rule_id_hex,
                funding_signature: request.funding_signature,
                user_wallet: request.user_wallet,
                user_usdc_ata: request.user_usdc_ata,
                controlled_wallet: request.controlled_wallet,
                controlled_usdc_ata: request.controlled_usdc_ata,
                amount_raw: request.amount_raw,
            };
            let outcome = executor.execute(internal).await;
            W5hFundingConfirmRouteOutcome::Ok(map_outcome_to_dto(outcome))
        })
    }
}

/// Pure mapping from the rich [`W5hFundingConfirmOutcome`] to the
/// lean [`W5hFundingConfirmResultDto`]. Extracted so the test module
/// can drive it with hand-built outcomes and assert the wire surface
/// without spinning up the executor.
pub fn map_outcome_to_dto(o: W5hFundingConfirmOutcome) -> W5hFundingConfirmResultDto {
    let budget_status = derive_budget_status(o.status);
    W5hFundingConfirmResultDto {
        status: status_label(o.status).to_string(),
        rule_id_hex: o.rule_id_hex,
        canonical_rule_hash_hex: o.canonical_rule_hash_hex,
        funding_signature: o.funding_signature,
        funding_confirmation_slot: o.funding_finalized_slot,
        expires_at_ms: o.expires_at_ms,
        save_display_apy_bps: o.save_display_apy_bps,
        native_onchain_apr_bps: o.native_onchain_apr_bps,
        threshold_bps: o.threshold_bps,
        amount_raw: o.amount_raw,
        user_wallet: o.user_wallet,
        user_usdc_ata: o.user_usdc_ata,
        controlled_wallet: o.controlled_wallet,
        controlled_usdc_ata: o.controlled_usdc_ata,
        budget_status: budget_status.to_string(),
        // The funding-confirm route NEVER executes the Solend deposit
        // — that's W5g. Hard-pin to None so the frontend never
        // accidentally renders a Solscan link off this route.
        tx_signature: None,
        error_code: o.error.map(error_label_owned),
        error_reason: o.error_reason,
    }
}

fn status_label(s: W5hFundingConfirmStatus) -> &'static str {
    match s {
        W5hFundingConfirmStatus::BudgetReserved => "budget_reserved",
        W5hFundingConfirmStatus::FundingPending => "funding_pending",
        W5hFundingConfirmStatus::FundingInvalid => "funding_invalid",
        W5hFundingConfirmStatus::AlreadyTerminal => "already_terminal",
        W5hFundingConfirmStatus::IntentNotFound => "intent_not_found",
        W5hFundingConfirmStatus::RequestMismatch => "request_mismatch",
        W5hFundingConfirmStatus::RepoFailed => "repo_failed",
    }
}

fn derive_budget_status(s: W5hFundingConfirmStatus) -> &'static str {
    match s {
        W5hFundingConfirmStatus::BudgetReserved => "reserved",
        W5hFundingConfirmStatus::FundingPending => "pending",
        W5hFundingConfirmStatus::FundingInvalid => "needs_funding",
        // Terminal / not-found / mismatch / repo-failed paths leave
        // the user unable to act on this rule; surface `unknown` so
        // the card renders a neutral chip.
        W5hFundingConfirmStatus::AlreadyTerminal
        | W5hFundingConfirmStatus::IntentNotFound
        | W5hFundingConfirmStatus::RequestMismatch
        | W5hFundingConfirmStatus::RepoFailed => "unknown",
    }
}

fn error_label_owned(code: W5hFundingConfirmErrorCode) -> String {
    match code {
        W5hFundingConfirmErrorCode::TxNotYetAvailable => "tx_not_yet_available",
        W5hFundingConfirmErrorCode::OnChainError => "on_chain_error",
        W5hFundingConfirmErrorCode::WrongMint => "wrong_mint",
        W5hFundingConfirmErrorCode::DestinationNotControlledUsdcAta => {
            "destination_not_controlled_usdc_ata"
        }
        W5hFundingConfirmErrorCode::AmountDeltaMismatch => "amount_delta_mismatch",
        W5hFundingConfirmErrorCode::RequestMismatch => "request_mismatch",
        W5hFundingConfirmErrorCode::IntentNotFound => "intent_not_found",
        W5hFundingConfirmErrorCode::IntentAlreadyTerminal => "intent_already_terminal",
        W5hFundingConfirmErrorCode::RepoFailed => "repo_failed",
        W5hFundingConfirmErrorCode::VerifierUnsupported => "verifier_unsupported",
        W5hFundingConfirmErrorCode::MemoMissingOrMismatch => "memo_missing_or_mismatch",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_state_store::db::Database;
    use claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository;

    fn fixture_outcome_budget_reserved() -> W5hFundingConfirmOutcome {
        W5hFundingConfirmOutcome {
            status: W5hFundingConfirmStatus::BudgetReserved,
            rule_id_hex: "6400000090d00300000000009a62dace".into(),
            canonical_rule_hash_hex:
                "962ae352baa3f4d494d4723f6391d40643271d1fd63b9085968ca892a60df69d".into(),
            funding_signature:
                "4M4ezLgmAbcDefGhiJkLMnoPqRstUvWxYz0123456789Py3y".into(),
            funding_finalized_slot: Some(419_571_964),
            expires_at_ms: 1_715_500_000_180,
            threshold_bps: 100,
            save_display_apy_bps: Some(312),
            native_onchain_apr_bps: Some(287),
            amount_raw: 250_000,
            user_wallet: "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW".into(),
            user_usdc_ata: "TestUserUsdcAta1111111111111111111111111111".into(),
            controlled_wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L".into(),
            controlled_usdc_ata: "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3".into(),
            error: None,
            error_reason: None,
        }
    }

    #[test]
    fn dto_strings_all_u64_fields_for_budget_reserved() {
        let dto = map_outcome_to_dto(fixture_outcome_budget_reserved());
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains(r#""status":"budget_reserved""#));
        assert!(json.contains(r#""amount_raw":"250000""#));
        assert!(json.contains(r#""funding_confirmation_slot":"419571964""#));
        assert!(json.contains(r#""expires_at_ms":"1715500000180""#));
        assert!(json.contains(r#""budget_status":"reserved""#));
        // tx_signature is null for the funding-confirm route.
        assert!(json.contains(r#""tx_signature":null"#));
        // bps fields stay numeric.
        assert!(json.contains(r#""save_display_apy_bps":312"#));
        assert!(json.contains(r#""threshold_bps":100"#));
        // Round-trip.
        let back: W5hFundingConfirmResultDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dto);
    }

    #[test]
    fn pending_outcome_maps_to_funding_pending_with_pending_budget_chip() {
        let mut o = fixture_outcome_budget_reserved();
        o.status = W5hFundingConfirmStatus::FundingPending;
        o.error = Some(W5hFundingConfirmErrorCode::TxNotYetAvailable);
        o.error_reason = Some("getTransaction returned null".into());
        let dto = map_outcome_to_dto(o);
        assert_eq!(dto.status, "funding_pending");
        assert_eq!(dto.budget_status, "pending");
        assert_eq!(dto.error_code.as_deref(), Some("tx_not_yet_available"));
        assert_eq!(dto.tx_signature, None);
    }

    #[test]
    fn verifier_unsupported_maps_to_funding_pending_not_funding_invalid() {
        // Addendum §1: verifier-side limitations must NEVER mark a
        // real funding tx invalid. The status is funding_pending and
        // the error_code surfaces verifier_unsupported.
        let mut o = fixture_outcome_budget_reserved();
        o.status = W5hFundingConfirmStatus::FundingPending;
        o.error = Some(W5hFundingConfirmErrorCode::VerifierUnsupported);
        let dto = map_outcome_to_dto(o);
        assert_eq!(dto.status, "funding_pending");
        assert_eq!(dto.error_code.as_deref(), Some("verifier_unsupported"));
    }

    #[test]
    fn invalid_outcome_maps_to_needs_funding_budget_chip() {
        let mut o = fixture_outcome_budget_reserved();
        o.status = W5hFundingConfirmStatus::FundingInvalid;
        o.error = Some(W5hFundingConfirmErrorCode::AmountDeltaMismatch);
        o.error_reason = Some("controlled ATA delta = +100000 raw, expected +250000 raw".into());
        let dto = map_outcome_to_dto(o);
        assert_eq!(dto.status, "funding_invalid");
        assert_eq!(dto.budget_status, "needs_funding");
        assert_eq!(dto.error_code.as_deref(), Some("amount_delta_mismatch"));
    }

    #[tokio::test]
    async fn build_helper_returns_none_when_rpc_url_missing() {
        // None input → 503 path: daemon will leave
        // chat_funding_confirm = None.
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        assert!(build_w5h_funding_confirm_handler(None, repo).is_none());
    }

    #[tokio::test]
    async fn build_helper_returns_none_when_rpc_url_blank() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        assert!(build_w5h_funding_confirm_handler(Some(""), repo.clone()).is_none());
        assert!(build_w5h_funding_confirm_handler(Some("   "), repo).is_none());
    }

    #[tokio::test]
    async fn build_helper_returns_some_when_rpc_url_present() {
        // Any non-blank URL → Some(handler). The handler doesn't
        // perform any RPC call at build time; per-request RPC errors
        // surface as `funding_pending` via the orchestrator's typed
        // outcomes, never as a panic / startup failure. This proves
        // the "daemon wiring creates Some(chat_funding_confirm)" bullet
        // of the W5h-lite brief.
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        let h = build_w5h_funding_confirm_handler(
            Some("https://api.invalid.example.com/rpc"),
            repo,
        );
        assert!(h.is_some(), "Some(handler) expected for non-blank RPC URL");
    }

    #[test]
    fn all_error_codes_have_snake_case_labels() {
        let codes = [
            W5hFundingConfirmErrorCode::TxNotYetAvailable,
            W5hFundingConfirmErrorCode::OnChainError,
            W5hFundingConfirmErrorCode::WrongMint,
            W5hFundingConfirmErrorCode::DestinationNotControlledUsdcAta,
            W5hFundingConfirmErrorCode::AmountDeltaMismatch,
            W5hFundingConfirmErrorCode::RequestMismatch,
            W5hFundingConfirmErrorCode::IntentNotFound,
            W5hFundingConfirmErrorCode::IntentAlreadyTerminal,
            W5hFundingConfirmErrorCode::RepoFailed,
            W5hFundingConfirmErrorCode::VerifierUnsupported,
            W5hFundingConfirmErrorCode::MemoMissingOrMismatch,
        ];
        for code in codes {
            let lbl = error_label_owned(code);
            assert!(
                lbl.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label {lbl:?} is not snake_case",
            );
        }
    }
}

//! Stage 2 W5h — chat-route bridge: parser → WatchRule + funding intent.
//!
//! Responsibilities:
//!
//!  1. Parse the W5h chat command (English / Chinese, 0.25 USDC,
//!     3 minutes).
//!  2. Fetch the live decision-metric snapshot (Save APY + B-O1
//!     native APR + controlled-wallet budget). Reuses the W5d/W5f
//!     fetchers; no new RPC plumbing.
//!  3. Persist a `WatchRule` keyed by deterministic `rule_id`
//!     (same id-derivation as W5e/W5f). This lets the W5g
//!     execution path look up the rule by `rule_id` without any
//!     new schema.
//!  4. Persist a `W5hFundingIntent` keyed by the same `rule_id`.
//!     Status = `funding_required`; expires_at_ms = now + 180 000.
//!  5. Return a typed `W5hConditionalOrderResult` carrying the
//!     funding affordances (`controlled_usdc_ata`, `amount_raw`)
//!     and the decision-metric snapshot.
//!
//! The bridge runs only when the chat-route detector
//! `looks_like_w5h_chat_command` matches. Non-matching messages
//! continue to the W5g (W5d/W5e/W5f) bridge or the LLM path.
//!
//! # What this bridge does NOT do
//!
//! - It does NOT broadcast any transaction (read-only).
//! - It does NOT load the controlled-wallet keypair.
//! - It does NOT consult the W5g executor — that runs only after
//!   `funding_confirm` succeeds.

use std::str::FromStr;

use claw_api::state::W5hConditionalOrderResultDto;
use claw_state_store::stage2_w5h_funding::{
    NewW5hFundingIntent, Stage2W5hFundingIntentRepository, W5hFundingIntent,
};
use claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository;
use claw_types::canonical_intent::PubkeyBytes;
use claw_types::stage2_watch_rule::canonical_rule_hash;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;

use crate::stage2_demo_apr_bridge::{
    build_w5e_watch_rule, controlled_wallet_addresses, hex_id_16,
    hex_id_32, persist_rule_idempotent, DemoParsed, PersistOutcome,
    SaveDisplayApyFetcher, W5dAprFetcher, CONTROLLED_WALLET_BS58,
};
use crate::stage2_w5h_chat::{
    parse_w5h_chat_command, W5hParseError, W5hParsed, W5H_EXPIRY_SECONDS,
};

/// USDC mint (mainnet-beta).
const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Typed errors the bridge surfaces. The chat handler maps these to
/// `ChatResponse::ToolError { tool_name = "w5h_conditional_order" }`
/// so the frontend reuses its existing error-card surface.
#[derive(Debug, Clone)]
pub enum W5hBridgeError {
    Parse(W5hParseError),
    SaveApyUnavailable(String),
    NativeAprUnavailable(String),
    RepoFailed(String),
    UserWalletInvalid(String),
}

impl std::fmt::Display for W5hBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::SaveApyUnavailable(r) => write!(f, "save apy unavailable: {r}"),
            Self::NativeAprUnavailable(r) => write!(f, "native apr unavailable: {r}"),
            Self::RepoFailed(r) => write!(f, "repo failed: {r}"),
            Self::UserWalletInvalid(r) => write!(f, "user wallet invalid: {r}"),
        }
    }
}

/// Entry point — DETERMINISTIC path. Parses the raw user message with
/// the strict W5h grammar; on parse-success delegates to
/// [`handle_w5h_from_parsed`] (the shared-runtime seam).
///
/// `user_wallet_bs58` is the externally-bound wallet pubkey from
/// the session. For test calls the caller can pass any base58 string.
pub async fn handle_w5h_chat_command(
    apr_fetcher: &(dyn W5dAprFetcher + 'static),
    save_apy_fetcher: &(dyn SaveDisplayApyFetcher + 'static),
    rule_repo: &Stage2WatchRuleRepository,
    intent_repo: &Stage2W5hFundingIntentRepository,
    user_wallet_bs58: &str,
    input_text: &str,
) -> Result<W5hConditionalOrderResultDto, W5hBridgeError> {
    let parsed = parse_w5h_chat_command(input_text).map_err(W5hBridgeError::Parse)?;
    handle_w5h_from_parsed(
        apr_fetcher,
        save_apy_fetcher,
        rule_repo,
        intent_repo,
        user_wallet_bs58,
        input_text,
        &parsed,
    )
    .await
}

/// Shared-runtime seam — accepts a pre-parsed [`W5hParsed`] from
/// EITHER the deterministic parser OR the Phase 5 LLM intent
/// extractor and runs the SAME canonical-hash / WatchRule /
/// FundingIntent pipeline.
///
/// This is the single boundary the LLM-assisted extractor crosses
/// to enter the trusted runtime. From canonical hash onward there
/// must be no observable difference between the two paths.
pub async fn handle_w5h_from_parsed(
    apr_fetcher: &(dyn W5dAprFetcher + 'static),
    save_apy_fetcher: &(dyn SaveDisplayApyFetcher + 'static),
    rule_repo: &Stage2WatchRuleRepository,
    intent_repo: &Stage2W5hFundingIntentRepository,
    user_wallet_bs58: &str,
    input_text: &str,
    parsed: &W5hParsed,
) -> Result<W5hConditionalOrderResultDto, W5hBridgeError> {
    // Map W5h parser output → W5d/W5e DemoParsed so we can reuse
    // build_w5e_watch_rule (which expects the W5d shape). The
    // amount + threshold survive unchanged; expires is W5h-specific
    // and applied via build_w5h_watch_rule_with_expires below.
    let demo = DemoParsed {
        threshold_bps: parsed.threshold_bps,
        threshold_pct_label: parsed.threshold_pct_label.clone(),
        amount_raw: parsed.amount_raw,
    };

    // ── Decision-metric snapshot ─────────────────────────────────────
    let native = apr_fetcher
        .evaluate(input_text, &demo)
        .await
        .map_err(|e| W5hBridgeError::NativeAprUnavailable(e.to_string()))?;
    let save = save_apy_fetcher
        .fetch_main_pool_usdc()
        .await
        .map_err(|e| W5hBridgeError::SaveApyUnavailable(e.to_string()))?;

    // ── Wallet topology ─────────────────────────────────────────────
    let user_pk = Pubkey::from_str(user_wallet_bs58)
        .map_err(|e| W5hBridgeError::UserWalletInvalid(e.to_string()))?;
    let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).expect("USDC mint parses");
    let user_usdc_ata = get_associated_token_address(&user_pk, &usdc_mint);
    let (controlled_wallet, controlled_usdc_ata) = controlled_wallet_addresses();

    // ── WatchRule build + persist (idempotent) ──────────────────────
    //
    // Reuse build_w5e_watch_rule for the rule shape (same condition,
    // same action carrier, same destination), then overwrite the
    // expires_at_slot to honor the W5h 3-minute window. Solana slots
    // are ~400 ms so 3 min ≈ 450 slots; we use 480 to be safe.
    let mut rule = build_w5e_watch_rule(&demo, native.last_checked_slot);
    rule.expires_at_slot = native.last_checked_slot.saturating_add(480);

    // Persist the rule. On UNIQUE collision we surface the persisted
    // rule's identity (canonical hash + expires) — same convention as
    // the W5e/W5f collision-fallback fix.
    let outcome = persist_rule_idempotent(rule_repo, &rule).await;
    let (rule_for_response, _rule_persisted) = match outcome {
        PersistOutcome::Inserted => (rule.clone(), true),
        PersistOutcome::AlreadyPresent { persisted } => (persisted, true),
        PersistOutcome::Failed => (rule.clone(), false),
    };

    let rule_id_hex = hex_id_16(&rule_for_response.rule_id);
    let canonical_hash_hex =
        hex_id_32(&canonical_rule_hash(&rule_for_response));

    // ── Funding intent build + insert ───────────────────────────────
    let now_ms = chrono::Utc::now().timestamp_millis();
    let expires_at_ms = now_ms + (W5H_EXPIRY_SECONDS as i64) * 1000;
    let intent = NewW5hFundingIntent {
        intent_id: rule_id_hex.clone(),
        rule_id_hex: rule_id_hex.clone(),
        canonical_rule_hash_hex: canonical_hash_hex.clone(),
        user_wallet: user_pk.to_string(),
        user_usdc_ata: user_usdc_ata.to_string(),
        controlled_wallet: controlled_wallet.clone(),
        controlled_usdc_ata: controlled_usdc_ata.clone(),
        // Phase 5c-lite: thread the variable amount from the parsed
        // shape (deterministic regex still pins 0.25 USDC; LLM-draft
        // path supplies any amount in 0.1–1.0 USDC).
        amount_raw: parsed.amount_raw,
        threshold_bps: parsed.threshold_bps,
        save_display_apy_bps_at_creation: save.save_display_apy_bps,
        native_onchain_apr_bps_at_creation: native.current_apr_bps,
        created_at_ms: now_ms,
        expires_at_ms,
    };

    // Insert. On UNIQUE collision (idempotent re-type) we surface
    // the persisted intent's current status — the user may have
    // already funded, expired, refunded, etc.
    let intent_now = match intent_repo.insert(&intent).await {
        Ok(()) => intent_to_response_template(&intent, parsed),
        Err(_) => match intent_repo.get(&rule_id_hex).await {
            Ok(Some(existing)) => persisted_intent_to_response(&existing),
            Ok(None) => {
                return Err(W5hBridgeError::RepoFailed(
                    "intent_repo.insert failed AND repo.get returned None".to_string(),
                ));
            }
            Err(e) => return Err(W5hBridgeError::RepoFailed(e.to_string())),
        },
    };

    Ok(W5hConditionalOrderResultDto {
        input_text: input_text.to_string(),
        status: intent_now.status,
        rule_id_hex,
        canonical_rule_hash_hex: canonical_hash_hex,

        user_wallet: user_pk.to_string(),
        user_usdc_ata: user_usdc_ata.to_string(),
        controlled_wallet: CONTROLLED_WALLET_BS58.to_string(),
        controlled_usdc_ata,

        // Phase 5c-lite: surface the variable amount from the parsed
        // shape (mirrors the persisted intent and the rule's
        // max_input_amount_raw — single source of truth is the parsed
        // shape).
        amount_raw: parsed.amount_raw,
        threshold_bps: parsed.threshold_bps,
        threshold_pct_label: parsed.threshold_pct_label.clone(),
        save_display_apy_bps_at_creation: save.save_display_apy_bps,
        native_onchain_apr_bps_at_creation: native.current_apr_bps,
        created_at_ms: intent_now.created_at_ms,
        expires_at_ms: intent_now.expires_at_ms,
        funding_signature: intent_now.funding_signature,
        execution_signature: intent_now.execution_signature,
        refund_signature: intent_now.refund_signature,
        last_error: intent_now.last_error,
    })
}

/// Phase 5c-lite seam — accepts a `DraftIntent` that has just passed
/// user finalization and crosses the SAME runtime pipeline used by
/// the deterministic regex path and the (now obsolete) direct Phase 5
/// LLM dispatch.
///
/// The TTL clock for the 3-minute funding window starts at the
/// `finalize_unix_ms` instant supplied by the caller — NOT at draft
/// creation time. The user may take several minutes reviewing the
/// LLM-drafted order card; the funding deadline accrues only from
/// the moment they attest.
pub async fn handle_w5h_from_finalized_intent(
    apr_fetcher: &(dyn W5dAprFetcher + 'static),
    save_apy_fetcher: &(dyn SaveDisplayApyFetcher + 'static),
    rule_repo: &Stage2WatchRuleRepository,
    intent_repo: &Stage2W5hFundingIntentRepository,
    user_wallet_bs58: &str,
    finalize_unix_ms: i64,
    draft: &crate::stage2_phase5c_draft::DraftIntent,
) -> Result<W5hConditionalOrderResultDto, W5hBridgeError> {
    // The draft already passed schema validation upstream; we project
    // it back into the `W5hParsed` shape the shared seam expects.
    let parsed = W5hParsed {
        threshold_bps: draft.threshold_bps,
        threshold_pct_label:
            crate::stage2_phase5c_draft::format_threshold_pct_label(
                draft.threshold_bps,
            ),
        amount_raw: draft.amount_raw,
        expires_seconds: draft.expiry_seconds_after_finalize,
    };
    // `input_text` is the draft's review copy — the canonical
    // server-side rendering of what the user attested to. We do NOT
    // pass the raw user message (which is unbounded prose).
    let mut dto = handle_w5h_from_parsed(
        apr_fetcher,
        save_apy_fetcher,
        rule_repo,
        intent_repo,
        user_wallet_bs58,
        &draft.review_copy,
        &parsed,
    )
    .await?;
    // The shared seam stamps `created_at_ms = now_ms` and
    // `expires_at_ms = now_ms + 180_000`. Phase 5c-lite overrides both
    // with the explicit finalize instant so the TTL clock is rooted
    // there (not implicitly at the moment the bridge happened to run
    // — which is the same for a chat-route immediate dispatch but
    // differs for stalled / queued finalizes).
    dto.created_at_ms = finalize_unix_ms;
    dto.expires_at_ms = finalize_unix_ms
        + (draft.expiry_seconds_after_finalize as i64) * 1000;
    Ok(dto)
}

/// Tiny carrier for the response-time intent shape — same fields as
/// `W5hConditionalOrderResultDto` for status / timing / signatures.
struct IntentResponseTemplate {
    status: String,
    created_at_ms: i64,
    expires_at_ms: i64,
    funding_signature: Option<String>,
    execution_signature: Option<String>,
    refund_signature: Option<String>,
    last_error: Option<String>,
}

fn intent_to_response_template(
    intent: &NewW5hFundingIntent,
    _parsed: &W5hParsed,
) -> IntentResponseTemplate {
    IntentResponseTemplate {
        status: "funding_required".to_string(),
        created_at_ms: intent.created_at_ms,
        expires_at_ms: intent.expires_at_ms,
        funding_signature: None,
        execution_signature: None,
        refund_signature: None,
        last_error: None,
    }
}

fn persisted_intent_to_response(intent: &W5hFundingIntent) -> IntentResponseTemplate {
    IntentResponseTemplate {
        status: intent.status.as_str().to_string(),
        created_at_ms: intent.created_at_ms,
        expires_at_ms: intent.expires_at_ms,
        funding_signature: intent.funding_signature.clone(),
        execution_signature: intent.execution_signature.clone(),
        refund_signature: intent.refund_signature.clone(),
        last_error: intent.last_error.clone(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage2_demo_apr_bridge::{
        compose_w5e_result, EvaluationError, SaveDisplayApyReading, W5dEvaluationResult,
        W5F_MAIN_POOL_COLLATERAL_MINT_BS58, W5F_MAIN_POOL_LENDING_MARKET_BS58,
        W5F_MAIN_POOL_LIQUIDITY_MINT_BS58, W5F_MAIN_POOL_RESERVE_BS58,
    };
    use async_trait::async_trait;
    use claw_state_store::db::Database;

    #[derive(Debug)]
    struct StubAprFetcher {
        apr_bps: u32,
        budget_raw: u64,
        slot: u64,
    }
    #[async_trait]
    impl W5dAprFetcher for StubAprFetcher {
        async fn evaluate(
            &self,
            input_text: &str,
            parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            let (cw, ata) = controlled_wallet_addresses();
            Ok(compose_w5e_result(
                input_text,
                parsed,
                self.apr_bps,
                self.budget_raw,
                self.slot,
                cw,
                ata,
            ))
        }
    }

    #[derive(Debug)]
    struct StubSaveFetcher {
        bps: u32,
    }
    #[async_trait]
    impl SaveDisplayApyFetcher for StubSaveFetcher {
        async fn fetch_main_pool_usdc(
            &self,
        ) -> Result<SaveDisplayApyReading, EvaluationError> {
            Ok(SaveDisplayApyReading {
                save_display_apy_bps: self.bps,
                raw_supply_interest_str: format!("{:.2}", self.bps as f64 / 100.0),
                reserve_pubkey: W5F_MAIN_POOL_RESERVE_BS58.to_string(),
                lending_market: W5F_MAIN_POOL_LENDING_MARKET_BS58.to_string(),
                liquidity_mint: W5F_MAIN_POOL_LIQUIDITY_MINT_BS58.to_string(),
                collateral_mint: W5F_MAIN_POOL_COLLATERAL_MINT_BS58.to_string(),
                rewards_present: false,
            })
        }
    }

    async fn test_setup() -> (
        Database,
        Stage2WatchRuleRepository,
        Stage2W5hFundingIntentRepository,
    ) {
        let db = Database::open_in_memory().await.unwrap();
        let rule_repo = Stage2WatchRuleRepository::new(db.pool().clone());
        let intent_repo = Stage2W5hFundingIntentRepository::new(db.pool().clone());
        (db, rule_repo, intent_repo)
    }

    const TEST_USER_WALLET: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";

    #[tokio::test]
    async fn english_command_creates_funding_required_intent() {
        let (_db, rule_repo, intent_repo) = test_setup().await;
        let apr = StubAprFetcher {
            apr_bps: 165,
            budget_raw: 0, // user hasn't funded yet; not the budget gate's concern in W5h
            slot: 419_000_000,
        };
        let save = StubSaveFetcher { bps: 210 };
        let input = "If Save APY > 1%, deposit 0.25 USDC, expires in 3 minutes";
        let r = handle_w5h_chat_command(
            &apr, &save, &rule_repo, &intent_repo, TEST_USER_WALLET, input,
        )
        .await
        .unwrap();
        assert_eq!(r.status, "funding_required");
        assert_eq!(r.amount_raw, 250_000);
        assert_eq!(r.threshold_bps, 100);
        assert_eq!(r.save_display_apy_bps_at_creation, 210);
        assert_eq!(r.native_onchain_apr_bps_at_creation, 165);
        assert_eq!(r.user_wallet, TEST_USER_WALLET);
        assert_eq!(
            r.controlled_wallet,
            "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L"
        );
        assert!(r.expires_at_ms > r.created_at_ms);
        assert_eq!(r.expires_at_ms - r.created_at_ms, 180_000);
        // Intent must be persisted.
        let stored = intent_repo.get(&r.rule_id_hex).await.unwrap().unwrap();
        assert_eq!(
            stored.status,
            claw_state_store::stage2_w5h_funding::W5hIntentStatus::FundingRequired
        );
    }

    #[tokio::test]
    async fn chinese_command_creates_funding_required_intent() {
        let (_db, rule_repo, intent_repo) = test_setup().await;
        let apr = StubAprFetcher {
            apr_bps: 165,
            budget_raw: 0,
            slot: 419_000_000,
        };
        let save = StubSaveFetcher { bps: 210 };
        let input = "如果 Save APY > 1%，deposit 0.25 USDC，有效期 3 分鐘";
        let r = handle_w5h_chat_command(
            &apr, &save, &rule_repo, &intent_repo, TEST_USER_WALLET, input,
        )
        .await
        .unwrap();
        assert_eq!(r.status, "funding_required");
        assert_eq!(r.amount_raw, 250_000);
        assert_eq!(r.threshold_bps, 100);
    }

    #[tokio::test]
    async fn wrong_amount_surfaces_parse_error() {
        let (_db, rule_repo, intent_repo) = test_setup().await;
        let apr = StubAprFetcher {
            apr_bps: 165,
            budget_raw: 0,
            slot: 1,
        };
        let save = StubSaveFetcher { bps: 210 };
        let err = handle_w5h_chat_command(
            &apr,
            &save,
            &rule_repo,
            &intent_repo,
            TEST_USER_WALLET,
            "If Save APY > 1%, deposit 1 USDC, expires in 3 minutes",
        )
        .await
        .unwrap_err();
        match err {
            W5hBridgeError::Parse(W5hParseError::UnsupportedAmount { .. }) => {}
            other => panic!("expected Parse/UnsupportedAmount, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wrong_expiry_surfaces_parse_error() {
        let (_db, rule_repo, intent_repo) = test_setup().await;
        let apr = StubAprFetcher {
            apr_bps: 165,
            budget_raw: 0,
            slot: 1,
        };
        let save = StubSaveFetcher { bps: 210 };
        let err = handle_w5h_chat_command(
            &apr,
            &save,
            &rule_repo,
            &intent_repo,
            TEST_USER_WALLET,
            "If Save APY > 1%, deposit 0.25 USDC, expires in 5 minutes",
        )
        .await
        .unwrap_err();
        match err {
            W5hBridgeError::Parse(W5hParseError::UnsupportedExpiry { .. }) => {}
            other => panic!("expected Parse/UnsupportedExpiry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn idempotent_retype_surfaces_persisted_status() {
        let (_db, rule_repo, intent_repo) = test_setup().await;
        let apr = StubAprFetcher {
            apr_bps: 165,
            budget_raw: 0,
            slot: 419_000_000,
        };
        let save = StubSaveFetcher { bps: 210 };
        let input = "If Save APY > 1%, deposit 0.25 USDC, expires in 3 minutes";
        let r1 = handle_w5h_chat_command(
            &apr, &save, &rule_repo, &intent_repo, TEST_USER_WALLET, input,
        )
        .await
        .unwrap();
        // Advance the intent past funding_required to budget_reserved
        // (simulate the funding-confirm path landing).
        intent_repo
            .mark_funding_submitted_if_required(&r1.rule_id_hex, "Sig1")
            .await
            .unwrap();
        intent_repo
            .mark_budget_reserved_if_submitted(&r1.rule_id_hex, "Sig1", 12_345)
            .await
            .unwrap();

        // Second chat call — bridge should surface the PERSISTED
        // status, not a fresh `funding_required`.
        let r2 = handle_w5h_chat_command(
            &apr, &save, &rule_repo, &intent_repo, TEST_USER_WALLET, input,
        )
        .await
        .unwrap();
        assert_eq!(r2.status, "budget_reserved");
        assert_eq!(r2.funding_signature.as_deref(), Some("Sig1"));
        assert_eq!(r2.rule_id_hex, r1.rule_id_hex);
        assert_eq!(r2.canonical_rule_hash_hex, r1.canonical_rule_hash_hex);
    }
}

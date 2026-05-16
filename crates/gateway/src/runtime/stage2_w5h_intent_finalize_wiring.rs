//! Phase 5c-lite — gateway-side adapter that implements the api
//! crate's [`claw_api::state::W5hIntentFinalizeHandler`] over the
//! in-process [`crate::stage2_phase5c_draft::DraftIntentStore`] +
//! the existing W5h bridge pipeline.
//!
//! Trust boundary anchor:
//!
//!  * **Confirm** consumes the draft on hash match and crosses
//!    [`crate::stage2_w5h_bridge::handle_w5h_from_finalized_intent`] —
//!    the SAME canonical-hash / WatchRule / W5hFundingIntent pipeline
//!    the deterministic regex path uses, but with the TTL clock
//!    rooted at `finalize_unix_ms` (NOT at draft creation).
//!
//!  * **Reject** drops the draft and returns `status="rejected"`.
//!    Zero DB write on this path.
//!
//!  * **Hash mismatch** leaves the draft entry in place so the user
//!    can re-fetch the canonical hash from the frontend and retry.
//!
//!  * **Not-found / expired / already-consumed** are surfaced as
//!    typed outcomes the HTTP route maps to 404 or 409 with explicit
//!    `error_code` payloads.

use std::pin::Pin;
use std::sync::Arc;

use claw_api::state::{
    W5hIntentFinalizationAuditDto, W5hIntentFinalizeHandler,
    W5hIntentFinalizeHandlerRef, W5hIntentFinalizeRequestDto,
    W5hIntentFinalizeResultDto, W5hIntentFinalizeRouteOutcome,
};
use claw_state_store::stage2_w5h_funding::Stage2W5hFundingIntentRepository;
use claw_state_store::stage2_watch_rules::Stage2WatchRuleRepository;
use claw_types::session::SessionId;

use crate::stage2_demo_apr_bridge::{SaveDisplayApyFetcher, W5dAprFetcher};
use crate::stage2_phase5c_draft::{
    session_id_hex, DraftConsumeOutcome, DraftIntent, DraftIntentStore,
};
use crate::stage2_w5h_bridge::{
    handle_w5h_from_finalized_intent, W5hBridgeError,
};
use crate::tools::jupiter_swap::SessionBoundWallet;

/// Phase 5c-lite finalize handler adapter. Wraps the shared
/// `Arc<DraftIntentStore>` (same Arc the chat handler inserts drafts
/// into) and the full W5h substrate the bridge needs to mint a
/// funding-required row on confirm.
///
/// Cloneable — internally Arcs over its dependencies. Construction
/// happens at daemon startup; per-request work runs on a clone.
#[derive(Clone)]
pub struct Phase5cIntentFinalizeAdapter {
    draft_store: Arc<DraftIntentStore>,
    apr_fetcher: Arc<dyn W5dAprFetcher>,
    save_apy_fetcher: Arc<dyn SaveDisplayApyFetcher>,
    rule_repo: Arc<Stage2WatchRuleRepository>,
    intent_repo: Arc<Stage2W5hFundingIntentRepository>,
    session_wallet_lookup: Arc<dyn SessionBoundWallet>,
}

impl std::fmt::Debug for Phase5cIntentFinalizeAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Phase5cIntentFinalizeAdapter")
            .field("draft_store_len", &self.draft_store.len())
            .finish()
    }
}

impl Phase5cIntentFinalizeAdapter {
    pub fn new(
        draft_store: Arc<DraftIntentStore>,
        apr_fetcher: Arc<dyn W5dAprFetcher>,
        save_apy_fetcher: Arc<dyn SaveDisplayApyFetcher>,
        rule_repo: Arc<Stage2WatchRuleRepository>,
        intent_repo: Arc<Stage2W5hFundingIntentRepository>,
        session_wallet_lookup: Arc<dyn SessionBoundWallet>,
    ) -> Self {
        Self {
            draft_store,
            apr_fetcher,
            save_apy_fetcher,
            rule_repo,
            intent_repo,
            session_wallet_lookup,
        }
    }

    /// Build the API-layer ref consumed by `AppState.chat_intent_finalize`.
    pub fn into_handler_ref(self) -> W5hIntentFinalizeHandlerRef {
        W5hIntentFinalizeHandlerRef::new(Arc::new(self))
    }

    /// Execute the typed outcome. Pulled out from the trait impl so
    /// unit tests can drive it directly (the trait return type is a
    /// `Pin<Box<dyn Future>>` that complicates direct calling).
    pub async fn run(
        &self,
        session_id: &SessionId,
        request: W5hIntentFinalizeRequestDto,
    ) -> W5hIntentFinalizeRouteOutcome {
        if request.user_confirmed {
            self.run_confirm(session_id, &request).await
        } else {
            self.run_reject(session_id, &request).await
        }
    }

    async fn run_confirm(
        &self,
        session_id: &SessionId,
        request: &W5hIntentFinalizeRequestDto,
    ) -> W5hIntentFinalizeRouteOutcome {
        let sid_hex = session_id_hex(session_id);

        // Atomic consume-or-tombstone-hit. The store distinguishes:
        //   - `Ok` → first confirm wins; entry is replaced with a
        //     tombstone in the same critical section.
        //   - `AlreadyConsumed` → a tombstone is present; this is
        //     either a retried confirm or the concurrent loser. Map
        //     to HTTP 409 `draft_already_finalized_or_missing`.
        //   - `NotFoundOrExpired` → never minted or both the draft
        //     and its tombstone aged out. Map to HTTP 404.
        //   - `HashMismatch` → draft preserved; user can retry.
        let draft = match self.draft_store.consume_if_match(
            &sid_hex,
            &request.draft_id,
            &request.draft_hash,
        ) {
            DraftConsumeOutcome::Ok(d) => d,
            DraftConsumeOutcome::NotFoundOrExpired => {
                tracing::info!(
                    parser_source = "llm_extractor",
                    finalize_result = "not_found_or_expired",
                    draft_id = %request.draft_id,
                    "phase5c-lite finalize: draft not found or expired"
                );
                return W5hIntentFinalizeRouteOutcome::NotFoundOrExpired;
            }
            DraftConsumeOutcome::AlreadyConsumed => {
                tracing::info!(
                    parser_source = "llm_extractor",
                    finalize_result = "already_finalized_or_missing",
                    draft_id = %request.draft_id,
                    "phase5c-lite finalize: draft already finalized (tombstone hit)"
                );
                return W5hIntentFinalizeRouteOutcome::AlreadyFinalizedOrMissing;
            }
            DraftConsumeOutcome::HashMismatch { provided, backend } => {
                tracing::warn!(
                    parser_source = "llm_extractor",
                    finalize_result = "hash_mismatch",
                    draft_id = %request.draft_id,
                    "phase5c-lite finalize: draft_hash mismatch; entry preserved"
                );
                return W5hIntentFinalizeRouteOutcome::HashMismatch {
                    provided,
                    backend,
                };
            }
        };

        // Defense-in-depth: the draft store keys by session_id, so a
        // confirm targeting a draft from another session can never
        // resolve here. Cross-check anyway.
        if draft.session_id_hex != sid_hex {
            tracing::warn!(
                parser_source = "llm_extractor",
                finalize_result = "session_mismatch",
                draft_id = %request.draft_id,
                "phase5c-lite finalize: draft's session_id_hex differs from caller"
            );
            return W5hIntentFinalizeRouteOutcome::NotFoundOrExpired;
        }

        // The W5h bridge requires the user's externally-bound wallet
        // pubkey (the FROM-address for the Phantom funding transfer).
        // Source it from the session binding at finalize time — never
        // from the draft, the user's chat message, or any cached
        // copy. If the user re-bound between draft and finalize, the
        // new binding wins.
        let user_wallet_bs58 = match self
            .session_wallet_lookup
            .session_wallet_pubkey(session_id)
        {
            Some(pk) if !pk.trim().is_empty() => pk,
            _ => {
                // The draft was consumed; we can't put it back atomically.
                // Surface a typed BadRequest so the operator sees a clear
                // reason. Re-issuing the chat command mints a fresh draft.
                return W5hIntentFinalizeRouteOutcome::BadRequest(
                    "session has no bound external wallet — \
                     bind a Phantom wallet via the challenge/response \
                     flow before finalizing"
                        .to_string(),
                );
            }
        };

        let finalize_unix_ms = chrono::Utc::now().timestamp_millis();
        let dto = match handle_w5h_from_finalized_intent(
            self.apr_fetcher.as_ref(),
            self.save_apy_fetcher.as_ref(),
            self.rule_repo.as_ref(),
            self.intent_repo.as_ref(),
            &user_wallet_bs58,
            finalize_unix_ms,
            &draft,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    parser_source = "llm_extractor",
                    finalize_result = "bridge_error",
                    draft_id = %request.draft_id,
                    error = %e,
                    "phase5c-lite finalize: W5h bridge failed"
                );
                return finalize_bridge_error_to_outcome(e, &draft, request, finalize_unix_ms);
            }
        };

        tracing::info!(
            parser_source = "llm_extractor",
            finalize_result = "confirmed",
            draft_id = %request.draft_id,
            rule_id = %dto.rule_id_hex,
            amount_raw = dto.amount_raw,
            threshold_bps = dto.threshold_bps,
            "phase5c-lite finalize: confirm → W5h funding_required minted"
        );

        let finalization = audit_envelope(&draft, request, finalize_unix_ms);
        W5hIntentFinalizeRouteOutcome::Ok(W5hIntentFinalizeResultDto {
            status: dto.status.clone(),
            funding: Some(dto),
            finalization,
        })
    }

    async fn run_reject(
        &self,
        session_id: &SessionId,
        request: &W5hIntentFinalizeRequestDto,
    ) -> W5hIntentFinalizeRouteOutcome {
        let sid_hex = session_id_hex(session_id);
        // Look up the draft first so we can populate the audit
        // envelope with its `original_user_message_hash`. The reject
        // path is best-effort: a missing draft is not an error
        // (idempotent), but we still report it via the envelope so
        // the frontend's audit trail is consistent.
        let lookup = self.draft_store.get(&sid_hex, &request.draft_id);

        let finalize_unix_ms = chrono::Utc::now().timestamp_millis();
        let finalization = match &lookup {
            Some(draft) => audit_envelope(draft, request, finalize_unix_ms),
            None => W5hIntentFinalizationAuditDto {
                parser_source: DraftIntent::PARSER_SOURCE.to_string(),
                original_user_message_hash: String::new(),
                draft_id: request.draft_id.clone(),
                draft_hash: request.draft_hash.clone(),
                finalized_at_ms: finalize_unix_ms,
            },
        };
        // Drop is idempotent — safe whether the entry existed or not.
        self.draft_store.drop_draft(&sid_hex, &request.draft_id);

        tracing::info!(
            parser_source = "llm_extractor",
            finalize_result = "rejected",
            draft_id = %request.draft_id,
            draft_was_present = lookup.is_some(),
            "phase5c-lite finalize: explicit reject; draft dropped"
        );

        W5hIntentFinalizeRouteOutcome::Ok(W5hIntentFinalizeResultDto {
            status: "rejected".to_string(),
            funding: None,
            finalization,
        })
    }
}

fn audit_envelope(
    draft: &DraftIntent,
    request: &W5hIntentFinalizeRequestDto,
    finalize_unix_ms: i64,
) -> W5hIntentFinalizationAuditDto {
    W5hIntentFinalizationAuditDto {
        parser_source: draft.parser_source.to_string(),
        original_user_message_hash: draft.original_user_message_hash.clone(),
        draft_id: request.draft_id.clone(),
        draft_hash: request.draft_hash.clone(),
        finalized_at_ms: finalize_unix_ms,
    }
}

/// Map a [`W5hBridgeError`] surfaced from
/// [`handle_w5h_from_finalized_intent`] into a finalize outcome.
///
/// The confirm path already consumed the draft (and installed a
/// tombstone) by the time we reach this, so we cannot put it back —
/// the user retries by re-issuing the chat command (which mints a
/// fresh draft).
///
/// Phase 5c-lite contract: bridge / infrastructure failures **must
/// not** be returned as `Ok(status="bridge_error:...")`. That shape
/// causes the frontend to render a misleading funding card. Instead
/// the adapter emits the typed [`W5hIntentFinalizeRouteOutcome::BridgeFailed`]
/// variant which the HTTP route maps to a 500 with
/// `error_code = "bridge_failed"` so the frontend renders a typed
/// error card (and never a fake funding-required card).
///
/// The only client-attributable bridge failure
/// (`UserWalletInvalid` — the session has a bound wallet that
/// fails base58 parse, very unlikely past the bind challenge /
/// response handshake) is still surfaced as `BadRequest` so the
/// client gets a 4xx rather than a 5xx — but the frontend MUST
/// still not render a funding card on that path.
fn finalize_bridge_error_to_outcome(
    err: W5hBridgeError,
    _draft: &DraftIntent,
    _request: &W5hIntentFinalizeRequestDto,
    _finalize_unix_ms: i64,
) -> W5hIntentFinalizeRouteOutcome {
    match err {
        W5hBridgeError::UserWalletInvalid(reason) => {
            W5hIntentFinalizeRouteOutcome::BadRequest(format!(
                "user wallet invalid: {reason}"
            ))
        }
        // Every other variant is an infrastructure / persistence
        // failure that the user cannot fix by retyping. Surface as
        // typed 500.
        other => W5hIntentFinalizeRouteOutcome::BridgeFailed {
            reason: other.to_string(),
        },
    }
}

impl W5hIntentFinalizeHandler for Phase5cIntentFinalizeAdapter {
    fn execute(
        &self,
        session_id: &SessionId,
        request: W5hIntentFinalizeRequestDto,
    ) -> Pin<Box<dyn std::future::Future<Output = W5hIntentFinalizeRouteOutcome> + Send + '_>>
    {
        let session_id = session_id.clone();
        Box::pin(async move { self.run(&session_id, request).await })
    }
}

/// Daemon-startup builder. Returns `Some(handler_ref)` when the W5h
/// substrate is fully wired AND the chat route's draft store is
/// present (so drafts can actually exist). Returns `None` otherwise
/// — the finalize route will then return 503.
///
/// All deps are passed in explicitly (no env reads here); the daemon
/// is responsible for resolving them.
pub fn build_phase5c_intent_finalize_handler(
    draft_store: Option<Arc<DraftIntentStore>>,
    apr_fetcher: Option<Arc<dyn W5dAprFetcher>>,
    save_apy_fetcher: Option<Arc<dyn SaveDisplayApyFetcher>>,
    rule_repo: Option<Arc<Stage2WatchRuleRepository>>,
    intent_repo: Option<Arc<Stage2W5hFundingIntentRepository>>,
    session_wallet_lookup: Option<Arc<dyn SessionBoundWallet>>,
) -> Option<W5hIntentFinalizeHandlerRef> {
    let draft_store = draft_store?;
    let apr_fetcher = apr_fetcher?;
    let save_apy_fetcher = save_apy_fetcher?;
    let rule_repo = rule_repo?;
    let intent_repo = intent_repo?;
    let session_wallet_lookup = session_wallet_lookup?;
    Some(
        Phase5cIntentFinalizeAdapter::new(
            draft_store,
            apr_fetcher,
            save_apy_fetcher,
            rule_repo,
            intent_repo,
            session_wallet_lookup,
        )
        .into_handler_ref(),
    )
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use claw_state_store::db::Database;
    use claw_state_store::stage2_w5h_funding::W5hIntentStatus;

    use crate::stage2_demo_apr_bridge::{
        compose_w5e_result, controlled_wallet_addresses, DemoParsed,
        EvaluationError, SaveDisplayApyReading, W5dEvaluationResult,
        W5F_MAIN_POOL_COLLATERAL_MINT_BS58, W5F_MAIN_POOL_LENDING_MARKET_BS58,
        W5F_MAIN_POOL_LIQUIDITY_MINT_BS58, W5F_MAIN_POOL_RESERVE_BS58,
    };
    use crate::stage2_phase5c_draft::{
        sha256_hex, DraftIntent, PHASE5C_EXPIRY_SECONDS_AFTER_FINALIZE,
    };

    const TEST_USER_WALLET: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";

    // ── Stub fetchers ───────────────────────────────────────────────────

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

    /// Stub session-wallet lookup. Returns the same pubkey for every
    /// session id. Tests that need to model an unbound session return
    /// the empty-pubkey variant.
    #[derive(Debug, Clone)]
    struct StubSessionWalletLookup {
        pubkey: Option<String>,
    }
    impl crate::tools::jupiter_swap::SessionBoundWallet for StubSessionWalletLookup {
        fn session_wallet_pubkey(&self, _: &SessionId) -> Option<String> {
            self.pubkey.clone()
        }
    }

    // ── Fixture builders ────────────────────────────────────────────────

    async fn fixture_adapter(
        bound_wallet: Option<String>,
        store_ttl_seconds: u64,
    ) -> (
        Phase5cIntentFinalizeAdapter,
        Arc<DraftIntentStore>,
        Arc<Stage2W5hFundingIntentRepository>,
        Database, // keep alive for the duration of the test
    ) {
        let db = Database::open_in_memory().await.unwrap();
        let rule_repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        let store = Arc::new(DraftIntentStore::new(store_ttl_seconds));
        let adapter = Phase5cIntentFinalizeAdapter::new(
            store.clone(),
            Arc::new(StubAprFetcher {
                apr_bps: 165,
                budget_raw: 0,
                slot: 419_000_000,
            }),
            Arc::new(StubSaveFetcher { bps: 210 }),
            rule_repo.clone(),
            intent_repo.clone(),
            Arc::new(StubSessionWalletLookup {
                pubkey: bound_wallet,
            }),
        );
        (adapter, store, intent_repo, db)
    }

    fn fixture_session() -> SessionId {
        SessionId::new()
    }

    fn fixture_draft_for(session: &SessionId, raw_user_msg: &str) -> DraftIntent {
        let (cw, ata) = controlled_wallet_addresses();
        DraftIntent {
            action: DraftIntent::ACTION,
            protocol: DraftIntent::PROTOCOL,
            asset: DraftIntent::ASSET,
            display_source: DraftIntent::DISPLAY_SOURCE,
            comparison: DraftIntent::COMPARISON,
            threshold_bps: 100, // 1%
            amount_raw: 500_000, // 0.5 USDC — out-of-deterministic-pin, in-band
            expiry_seconds_after_finalize: PHASE5C_EXPIRY_SECONDS_AFTER_FINALIZE,
            controlled_wallet: cw,
            controlled_usdc_ata: ata,
            original_user_message_hash: sha256_hex(raw_user_msg),
            draft_id: uuid::Uuid::new_v4().simple().to_string(),
            parser_source: DraftIntent::PARSER_SOURCE,
            warnings: Vec::new(),
            review_copy:
                "If Save APY > 1%, deposit 0.5 USDC (= 500000 raw) into Solend"
                    .to_string(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            session_id_hex: session_id_hex(session),
        }
    }

    // ── Happy paths ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn confirm_happy_path_mints_funding_required_and_records_audit() {
        let (adapter, store, intent_repo, _db) =
            fixture_adapter(Some(TEST_USER_WALLET.to_string()), 10 * 60).await;
        let session = fixture_session();
        let draft = fixture_draft_for(&session, "If Save APY > 1%, deposit 0.5 USDC");
        let draft_hash = draft.compute_hash();
        let draft_id = draft.draft_id.clone();
        store.insert(draft.clone());

        let outcome = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id: draft_id.clone(),
                    draft_hash: draft_hash.clone(),
                    user_confirmed: true,
                },
            )
            .await;

        let dto = match outcome {
            W5hIntentFinalizeRouteOutcome::Ok(d) => d,
            other => panic!("expected Ok, got {other:?}"),
        };
        let funding = dto.funding.expect("confirm must carry a funding payload");
        assert_eq!(dto.status, funding.status);
        assert_eq!(funding.status, "funding_required");
        assert_eq!(funding.amount_raw, 500_000);
        assert_eq!(funding.threshold_bps, 100);
        // TTL clock rooted at finalize: expires_at_ms = finalize_unix_ms + 180_000.
        assert_eq!(
            funding.expires_at_ms - funding.created_at_ms,
            (PHASE5C_EXPIRY_SECONDS_AFTER_FINALIZE as i64) * 1000
        );
        // Audit envelope.
        assert_eq!(dto.finalization.parser_source, "llm_extractor");
        assert_eq!(dto.finalization.draft_id, draft_id);
        assert_eq!(dto.finalization.draft_hash, draft_hash);
        assert_eq!(
            dto.finalization.original_user_message_hash,
            draft.original_user_message_hash
        );
        // Draft has been consumed (tombstone installed; no active draft).
        assert_eq!(store.active_len(), 0, "draft must be consumed on confirm-success");
        assert_eq!(store.tombstone_len(), 1, "consume installs a tombstone");
        // DB row exists.
        let stored = intent_repo.get(&funding.rule_id_hex).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::FundingRequired);
        assert_eq!(stored.amount_raw, 500_000);
    }

    #[tokio::test]
    async fn reject_drops_draft_and_writes_no_db_row() {
        let (adapter, store, intent_repo, _db) =
            fixture_adapter(Some(TEST_USER_WALLET.to_string()), 10 * 60).await;
        let session = fixture_session();
        let draft = fixture_draft_for(&session, "If Save APY > 1%, deposit 0.5 USDC");
        let draft_hash = draft.compute_hash();
        let draft_id = draft.draft_id.clone();
        store.insert(draft);

        let outcome = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id: draft_id.clone(),
                    draft_hash,
                    user_confirmed: false,
                },
            )
            .await;

        let dto = match outcome {
            W5hIntentFinalizeRouteOutcome::Ok(d) => d,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(dto.status, "rejected");
        assert!(dto.funding.is_none());
        assert_eq!(dto.finalization.draft_id, draft_id);
        assert!(store.is_empty(), "reject must drop the draft");
        // No DB row for any rule_id.
        let any = intent_repo.list_budget_reserved(10).await.unwrap();
        assert!(any.is_empty());
    }

    // ── Idempotency / not found / hash mismatch ─────────────────────────

    #[tokio::test]
    async fn confirm_idempotency_second_attempt_returns_already_finalized_or_missing() {
        // Phase 5c-lite contract-alignment: the SECOND confirm with
        // the same draft_id MUST hit the tombstone and return the
        // typed `AlreadyFinalizedOrMissing` outcome (HTTP 409). The
        // FIRST confirm wrote one rule + one funding-intent row; the
        // second must NOT create duplicates.
        let (adapter, store, intent_repo, db) =
            fixture_adapter(Some(TEST_USER_WALLET.to_string()), 10 * 60).await;
        let session = fixture_session();
        let draft = fixture_draft_for(&session, "msg");
        let draft_hash = draft.compute_hash();
        let draft_id = draft.draft_id.clone();
        store.insert(draft);

        // First confirm: succeeds.
        let first = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id: draft_id.clone(),
                    draft_hash: draft_hash.clone(),
                    user_confirmed: true,
                },
            )
            .await;
        assert!(matches!(first, W5hIntentFinalizeRouteOutcome::Ok(_)));

        // Snapshot DB row counts after the first confirm.
        let rules_after_first = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM stage2_watch_rules",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        let intents_after_first = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM stage2_w5h_funding_intents",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(rules_after_first, 1);
        assert_eq!(intents_after_first, 1);

        // Second confirm with the same draft_id: AlreadyFinalizedOrMissing.
        let second = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id,
                    draft_hash,
                    user_confirmed: true,
                },
            )
            .await;
        assert!(matches!(
            second,
            W5hIntentFinalizeRouteOutcome::AlreadyFinalizedOrMissing
        ));

        // DB row counts MUST be unchanged.
        let rules_after_second = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM stage2_watch_rules",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        let intents_after_second = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM stage2_w5h_funding_intents",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(rules_after_second, rules_after_first);
        assert_eq!(intents_after_second, intents_after_first);

        // Tombstone is the only thing left in the in-process store.
        assert_eq!(store.active_len(), 0);
        assert_eq!(store.tombstone_len(), 1);
    }

    #[tokio::test]
    async fn concurrent_double_confirm_exactly_one_winner() {
        // Spawn two concurrent confirms targeting the same draft_id +
        // draft_hash. Exactly ONE must observe `Ok`; the other must
        // observe `AlreadyFinalizedOrMissing`. Asserts there is no
        // race window where both consume.
        let (adapter, store, intent_repo, db) =
            fixture_adapter(Some(TEST_USER_WALLET.to_string()), 10 * 60).await;
        let session = fixture_session();
        let draft = fixture_draft_for(&session, "race");
        let draft_hash = draft.compute_hash();
        let draft_id = draft.draft_id.clone();
        store.insert(draft);

        let adapter_a = adapter.clone();
        let adapter_b = adapter.clone();
        let session_a = session.clone();
        let session_b = session.clone();
        let did_a = draft_id.clone();
        let did_b = draft_id.clone();
        let dh_a = draft_hash.clone();
        let dh_b = draft_hash.clone();

        let (a, b) = tokio::join!(
            tokio::spawn(async move {
                adapter_a
                    .run(
                        &session_a,
                        W5hIntentFinalizeRequestDto {
                            draft_id: did_a,
                            draft_hash: dh_a,
                            user_confirmed: true,
                        },
                    )
                    .await
            }),
            tokio::spawn(async move {
                adapter_b
                    .run(
                        &session_b,
                        W5hIntentFinalizeRequestDto {
                            draft_id: did_b,
                            draft_hash: dh_b,
                            user_confirmed: true,
                        },
                    )
                    .await
            }),
        );
        let a = a.unwrap();
        let b = b.unwrap();

        // Exactly one Ok + exactly one AlreadyFinalizedOrMissing —
        // never two Oks (would imply two DB rows), never two losers.
        let ok_count = [&a, &b]
            .iter()
            .filter(|o| matches!(o, W5hIntentFinalizeRouteOutcome::Ok(_)))
            .count();
        let already_count = [&a, &b]
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    W5hIntentFinalizeRouteOutcome::AlreadyFinalizedOrMissing
                )
            })
            .count();
        assert_eq!(
            ok_count, 1,
            "exactly ONE confirm must observe Ok; got a={a:?}, b={b:?}"
        );
        assert_eq!(
            already_count, 1,
            "the LOSER must observe AlreadyFinalizedOrMissing; got a={a:?}, b={b:?}"
        );

        // Single row in each table.
        let rules: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stage2_watch_rules")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let intents: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM stage2_w5h_funding_intents")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(rules, 1);
        assert_eq!(intents, 1);
        let _ = intent_repo; // keep alive
    }

    #[tokio::test]
    async fn confirm_hash_mismatch_does_not_consume_draft() {
        let (adapter, store, intent_repo, _db) =
            fixture_adapter(Some(TEST_USER_WALLET.to_string()), 10 * 60).await;
        let session = fixture_session();
        let draft = fixture_draft_for(&session, "msg");
        let draft_id = draft.draft_id.clone();
        store.insert(draft.clone());

        let outcome = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id: draft_id.clone(),
                    draft_hash: "f".repeat(64),
                    user_confirmed: true,
                },
            )
            .await;
        match outcome {
            W5hIntentFinalizeRouteOutcome::HashMismatch { provided, backend } => {
                assert_eq!(provided, "f".repeat(64));
                assert_ne!(backend, "f".repeat(64));
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
        // Draft entry is still there for a follow-up correct retry.
        assert_eq!(store.len(), 1);
        // No DB row was written.
        let any = intent_repo.list_budget_reserved(10).await.unwrap();
        assert!(any.is_empty());

        // Retry with the correct hash succeeds.
        let retry = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id,
                    draft_hash: draft.compute_hash(),
                    user_confirmed: true,
                },
            )
            .await;
        assert!(matches!(retry, W5hIntentFinalizeRouteOutcome::Ok(_)));
        // Retry consumed the draft → tombstone slot present, no
        // active drafts.
        assert_eq!(store.active_len(), 0);
        assert_eq!(store.tombstone_len(), 1);
    }

    #[tokio::test]
    async fn confirm_not_found_returns_not_found_or_expired() {
        let (adapter, _store, _intent_repo, _db) =
            fixture_adapter(Some(TEST_USER_WALLET.to_string()), 10 * 60).await;
        let session = fixture_session();
        let outcome = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id: "never-existed".to_string(),
                    draft_hash: "0".repeat(64),
                    user_confirmed: true,
                },
            )
            .await;
        assert!(matches!(
            outcome,
            W5hIntentFinalizeRouteOutcome::NotFoundOrExpired
        ));
    }

    // ── Wallet-binding edge ─────────────────────────────────────────────

    #[tokio::test]
    async fn confirm_with_unbound_wallet_returns_bad_request_and_consumes_draft() {
        let (adapter, store, _intent_repo, _db) =
            fixture_adapter(None, 10 * 60).await;
        let session = fixture_session();
        let draft = fixture_draft_for(&session, "msg");
        let draft_hash = draft.compute_hash();
        let draft_id = draft.draft_id.clone();
        store.insert(draft);

        let outcome = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id,
                    draft_hash,
                    user_confirmed: true,
                },
            )
            .await;
        assert!(matches!(
            outcome,
            W5hIntentFinalizeRouteOutcome::BadRequest(_)
        ));
        // Per the implementation comment: draft is consumed before
        // wallet-binding is checked (the consume is atomic). User
        // retries by re-issuing the chat command. Slot now holds a
        // tombstone, not an active draft.
        assert_eq!(store.active_len(), 0);
        assert_eq!(store.tombstone_len(), 1);
    }

    // ── Reject without prior draft ───────────────────────────────────────
    //
    // Phase 5c-lite contract: `user_confirmed` is a strongly-typed
    // `bool` field on the request DTO, so there is no "unknown
    // action" case at the wire layer. The earlier
    // `unknown_action_returns_bad_request` test (which exercised an
    // `action: "approve"` value) no longer applies and was removed
    // when the DTO was renamed from `action: String` to
    // `user_confirmed: bool`.

    #[tokio::test]
    async fn reject_unknown_draft_is_idempotent_no_op() {
        let (adapter, _store, _intent_repo, _db) =
            fixture_adapter(Some(TEST_USER_WALLET.to_string()), 10 * 60).await;
        let session = fixture_session();
        let outcome = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id: "never-existed".to_string(),
                    draft_hash: "0".repeat(64),
                    user_confirmed: false,
                },
            )
            .await;
        // Reject is always Ok(rejected) — idempotent.
        match outcome {
            W5hIntentFinalizeRouteOutcome::Ok(dto) => {
                assert_eq!(dto.status, "rejected");
                assert!(dto.funding.is_none());
            }
            other => panic!("expected Ok(rejected), got {other:?}"),
        }
    }

    // ── Issue 3 — typed BridgeFailed outcome ───────────────────────────
    //
    // Phase 5c-lite contract-alignment: when the post-consume W5h
    // bridge pipeline raises an internal error, the adapter MUST
    // surface a typed `BridgeFailed { reason }` outcome (which the
    // HTTP route maps to a 500 with `error_code = "bridge_failed"`).
    // It MUST NOT emit a successful funding-shaped DTO with status
    // `"bridge_error:..."` — that would cause the frontend to render
    // a misleading funding-required card.

    #[derive(Debug)]
    struct FailingAprFetcher;
    #[async_trait]
    impl W5dAprFetcher for FailingAprFetcher {
        async fn evaluate(
            &self,
            _input_text: &str,
            _parsed: &DemoParsed,
        ) -> Result<W5dEvaluationResult, EvaluationError> {
            Err(EvaluationError::RpcFetchFailed {
                detail: "simulated RPC outage".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn bridge_failure_returns_typed_bridge_failed_outcome_no_funding_dto() {
        // Build an adapter whose APR fetcher is failing. Everything
        // else is wired normally; the failure surfaces at the bridge
        // call after the draft is consumed.
        let db = Database::open_in_memory().await.unwrap();
        let rule_repo = Arc::new(Stage2WatchRuleRepository::new(db.pool().clone()));
        let intent_repo = Arc::new(Stage2W5hFundingIntentRepository::new(db.pool().clone()));
        let store = Arc::new(DraftIntentStore::new(10 * 60));
        let adapter = Phase5cIntentFinalizeAdapter::new(
            store.clone(),
            Arc::new(FailingAprFetcher),
            Arc::new(StubSaveFetcher { bps: 210 }),
            rule_repo,
            intent_repo.clone(),
            Arc::new(StubSessionWalletLookup {
                pubkey: Some(TEST_USER_WALLET.to_string()),
            }),
        );
        let session = fixture_session();
        let draft = fixture_draft_for(&session, "bridge-fail msg");
        let draft_hash = draft.compute_hash();
        let draft_id = draft.draft_id.clone();
        store.insert(draft);

        let outcome = adapter
            .run(
                &session,
                W5hIntentFinalizeRequestDto {
                    draft_id,
                    draft_hash,
                    user_confirmed: true,
                },
            )
            .await;

        // MUST be BridgeFailed, NOT Ok-with-bridge-error-status.
        match outcome {
            W5hIntentFinalizeRouteOutcome::BridgeFailed { reason } => {
                assert!(
                    reason.to_lowercase().contains("rpc")
                        || reason.to_lowercase().contains("apr")
                        || reason.to_lowercase().contains("simulated")
                        || reason.to_lowercase().contains("native"),
                    "bridge failure reason should surface infrastructure detail; got {reason:?}"
                );
            }
            W5hIntentFinalizeRouteOutcome::Ok(dto) => panic!(
                "bridge failure must NOT return Ok; got status={:?}, funding={:?}",
                dto.status, dto.funding
            ),
            other => panic!("expected BridgeFailed, got {other:?}"),
        }
        // No funding-intent row was persisted.
        let any = intent_repo.list_budget_reserved(10).await.unwrap();
        assert!(any.is_empty());
        // Draft was consumed → tombstone present, no active drafts.
        assert_eq!(store.active_len(), 0);
        assert_eq!(store.tombstone_len(), 1);
    }

    // ── Source guards ──────────────────────────────────────────────────

    #[test]
    fn source_guard_no_send_or_keypair_in_module() {
        const SRC: &str = include_str!("stage2_w5h_intent_finalize_wiring.rs");
        let needles = [
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", "send", "Transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "read_keypair_", "file"),
        ];
        for n in &needles {
            assert!(
                !SRC.contains(n.as_str()),
                "stage2_w5h_intent_finalize_wiring.rs must not contain `{n}` — \
                 finalize is read-mostly; signing/broadcast belongs in the \
                 W5g executor path"
            );
        }
    }
}

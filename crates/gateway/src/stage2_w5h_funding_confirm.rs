//! Stage 2 W5h — funding-confirm orchestrator.
//!
//! Owns the verification chain for
//! `POST /sessions/:id/stage2/w5h/funding/confirm`:
//!
//!  1. Look up the W5h intent by `rule_id_hex`.
//!  2. Confirm the caller-supplied `(controlled_wallet,
//!     controlled_usdc_ata, amount_raw)` triple matches the
//!     persisted intent. Bind the caller's `funding_signature`
//!     under the `funding_required → funding_submitted` CAS.
//!  3. Fetch the on-chain transaction via the injected
//!     [`TxFetcher`].
//!     - `NotYetAvailable` → `funding_pending` (RPC-delay
//!       resiliency, per the W5h addendum §1).
//!     - `OnChainError` → `funding_invalid` (terminal).
//!     - `Available` → proceed to delta verification.
//!  4. Verify token-balance deltas (W5h addendum §2):
//!     `post - pre` of the controlled USDC ATA MUST equal
//!     `+amount_raw`, using `uiTokenAmount.amount` (raw integer
//!     string) — NEVER `uiAmount` floats.
//!  5. CAS `funding_submitted → budget_reserved` (binds the
//!     finalized_slot). Idempotent: re-POSTing the same signature
//!     after `budget_reserved` returns the same DTO.
//!
//! The orchestrator NEVER signs / broadcasts / loads a keypair. It
//! is read-only over Solana (one `getTransaction` per request, plus
//! a bounded backoff if the RPC says "not yet visible").

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use claw_state_store::stage2_w5h_funding::{
    Stage2W5hFundingIntentRepository, W5hIntentStatus,
};

// ── USDC mint pin ─────────────────────────────────────────────────────────

/// USDC mint (mainnet-beta). The verifier rejects any tx whose
/// controlled-ATA delta is for a different mint.
pub const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// ── Tx fetcher (injectable for tests) ─────────────────────────────────────

/// What the verifier needs from `getTransaction`. The shape mirrors
/// the relevant Solana JSON-RPC response fields (omitting everything
/// we don't read) so tests can build fixtures by hand.
///
/// W5h-lite addendum §2 — `pubkey` is the base58 pubkey resolved from
/// `account_index` via `transaction.message.accountKeys` AND
/// `meta.loadedAddresses` (address-table lookups for v0 transactions).
/// The verifier matches the destination token account by **pubkey
/// equality** with the persisted `controlled_usdc_ata`, never by
/// `accountIndex` position. Mint + owner are still verified as
/// defense-in-depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedTokenBalance {
    pub account_index: u32,
    /// Base58 pubkey resolved from the message's `accountKeys` list
    /// (and `meta.loadedAddresses` for v0 transactions).
    pub pubkey: String,
    pub mint: String,
    pub owner: String,
    /// `uiTokenAmount.amount` — the RAW integer string. Never use
    /// `uiTokenAmount.uiAmount` (float) for delta checks.
    pub amount_raw_str: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedTx {
    /// Slot the tx confirmed at (also the finalized slot when the
    /// caller polled with `commitment=finalized`).
    pub slot: u64,
    /// `meta.err` — `Some(reason)` means the tx failed on-chain.
    pub err: Option<String>,
    /// Token balances BEFORE the tx executed.
    pub pre_token_balances: Vec<FetchedTokenBalance>,
    /// Token balances AFTER the tx executed.
    pub post_token_balances: Vec<FetchedTokenBalance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxFetchOutcome {
    /// `getTransaction` returned `null` (not yet indexed / finalized).
    /// Return `funding_pending`; let the frontend retry.
    NotYetAvailable,
    /// Tx confirmed AND finalized; the verifier can run the delta
    /// check now.
    Available(FetchedTx),
    /// The fetcher saw the tx but cannot safely verify it — e.g. v0
    /// transactions whose address-table-loaded addresses are not
    /// resolvable, or an unsupported transaction version surfaced by
    /// the RPC. Treat as `funding_pending` (NEVER `funding_invalid`):
    /// returning invalid on a verifier limitation would let a real
    /// funding tx get rejected for backend-side reasons. Frontend
    /// retries; an operator can investigate the reason string.
    VerifierUnsupported(String),
}

/// Trait the verifier depends on. Production uses a thin
/// `getTransaction` JSON-RPC wrapper; tests inject a deterministic
/// stub.
#[async_trait]
pub trait TxFetcher: Send + Sync + std::fmt::Debug {
    async fn fetch(&self, signature: &str) -> Result<TxFetchOutcome, String>;
}

// ── Outcomes / errors ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum W5hFundingConfirmStatus {
    /// Tx confirmed + delta verified. Intent advanced to
    /// `budget_reserved`.
    BudgetReserved,
    /// `getTransaction` returned `null` (or transient RPC error).
    /// Frontend should retry the POST after a short backoff. Intent
    /// stays in `funding_submitted`.
    FundingPending,
    /// Tx finalized AND verification failed (wrong delta, wrong
    /// mint, wrong destination, wrong amount, on-chain err, etc).
    /// Terminal — user must mint a new intent.
    FundingInvalid,
    /// Caller submitted a confirm for an intent that's already in a
    /// non-funding-required terminal/lifecycle state. Includes:
    /// already-completed / already-refunded / wrong-status. The
    /// route returns 200 OK with the current status echo so the
    /// frontend can render the correct card.
    AlreadyTerminal,
    /// Intent not found by `rule_id_hex`. Caller likely lost state;
    /// frontend renders a "mint a new order" card.
    IntentNotFound,
    /// One of the caller-supplied fields didn't match the persisted
    /// intent (wrong user_wallet / wrong controlled_usdc_ata /
    /// wrong amount). Conservative refusal.
    RequestMismatch,
    /// Repo error (lost connection, etc). Retryable.
    RepoFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum W5hFundingConfirmErrorCode {
    TxNotYetAvailable,
    OnChainError,
    WrongMint,
    DestinationNotControlledUsdcAta,
    AmountDeltaMismatch,
    RequestMismatch,
    IntentNotFound,
    IntentAlreadyTerminal,
    RepoFailed,
    /// W5h-lite addendum §1/§2: verifier can't safely process this
    /// transaction (e.g. address-table-lookups not resolvable, or RPC
    /// returned `Transaction version unsupported` because the request
    /// omitted `maxSupportedTransactionVersion`). Returned alongside
    /// `FundingPending` — never `FundingInvalid`.
    VerifierUnsupported,
}

/// Rich orchestrator outcome. The route adapter collapses this into
/// the lean `W5hFundingConfirmResultDto` (api crate).
#[derive(Debug, Clone)]
pub struct W5hFundingConfirmOutcome {
    pub status: W5hFundingConfirmStatus,
    pub rule_id_hex: String,
    pub canonical_rule_hash_hex: String,
    pub funding_signature: String,
    pub funding_finalized_slot: Option<u64>,
    pub expires_at_ms: i64,
    pub threshold_bps: u32,
    pub save_display_apy_bps: Option<u32>,
    pub native_onchain_apr_bps: Option<u32>,
    /// W5h-lite addendum — surfaced verbatim from the persisted
    /// intent so the adapter can fill the wire DTO without a repo
    /// round-trip.
    pub amount_raw: u64,
    pub user_wallet: String,
    pub user_usdc_ata: String,
    pub controlled_wallet: String,
    pub controlled_usdc_ata: String,
    pub error: Option<W5hFundingConfirmErrorCode>,
    pub error_reason: Option<String>,
}

// ── Request ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct W5hFundingConfirmRequest {
    pub rule_id_hex: String,
    pub funding_signature: String,
    pub user_wallet: String,
    pub user_usdc_ata: String,
    pub controlled_wallet: String,
    pub controlled_usdc_ata: String,
    pub amount_raw: u64,
}

// ── Pure verification helper ─────────────────────────────────────────────

/// Verifier — pure function over a fetched tx + the request context.
/// Returns either `Ok(())` (delta passes) or a typed error reason.
///
/// Exposed `pub` so the orchestrator and the unit tests share one
/// authoritative implementation.
pub fn verify_funding_delta(
    tx: &FetchedTx,
    controlled_usdc_ata: &str,
    controlled_wallet: &str,
    amount_raw: u64,
) -> Result<u64, (W5hFundingConfirmErrorCode, String)> {
    if let Some(err) = &tx.err {
        return Err((
            W5hFundingConfirmErrorCode::OnChainError,
            format!("on-chain err: {err}"),
        ));
    }

    // W5h-lite addendum §2 — match the destination token account by
    // EXACT pubkey equality. We do NOT trust `accountIndex` ordering
    // and we do NOT trust `(mint, owner)` alone (an attacker can mint
    // a different USDC ATA owned by the controlled wallet and credit
    // it). Mint + owner are still checked as defense-in-depth once
    // the pubkey row is found.
    let find_balance_by_pubkey =
        |list: &[FetchedTokenBalance]| -> Option<FetchedTokenBalance> {
            list.iter().find(|b| b.pubkey == controlled_usdc_ata).cloned()
        };

    let pre = find_balance_by_pubkey(&tx.pre_token_balances);
    let post = find_balance_by_pubkey(&tx.post_token_balances);

    let pre_raw = match pre.as_ref() {
        Some(b) => {
            assert_balance_identity(b, controlled_wallet)?;
            parse_raw_amount(&b.amount_raw_str).map_err(|e| {
                (
                    W5hFundingConfirmErrorCode::AmountDeltaMismatch,
                    format!("pre amount_raw_str parse: {e}"),
                )
            })?
        }
        // Brand-new ATA: pre balance may be missing (the account
        // didn't exist before the tx). Treat as 0.
        None => 0,
    };
    let post_raw = match post.as_ref() {
        Some(b) => {
            assert_balance_identity(b, controlled_wallet)?;
            parse_raw_amount(&b.amount_raw_str).map_err(|e| {
                (
                    W5hFundingConfirmErrorCode::AmountDeltaMismatch,
                    format!("post amount_raw_str parse: {e}"),
                )
            })?
        }
        None => {
            return Err((
                W5hFundingConfirmErrorCode::DestinationNotControlledUsdcAta,
                format!(
                    "post tx has no token-balance row for pubkey {controlled_usdc_ata} \
                     (resolved via message.accountKeys + meta.loadedAddresses)"
                ),
            ));
        }
    };

    if post_raw < pre_raw {
        return Err((
            W5hFundingConfirmErrorCode::AmountDeltaMismatch,
            format!(
                "controlled ATA balance went down (pre={pre_raw}, post={post_raw}); \
                 funding tx must INCREASE the controlled ATA"
            ),
        ));
    }
    let delta = post_raw - pre_raw;
    if delta != amount_raw {
        return Err((
            W5hFundingConfirmErrorCode::AmountDeltaMismatch,
            format!(
                "controlled ATA delta = +{delta} raw, expected +{amount_raw} raw \
                 (pre={pre_raw}, post={post_raw})"
            ),
        ));
    }

    Ok(tx.slot)
}

/// Defense-in-depth: even though we matched by pubkey, assert the
/// token-balance row is in fact USDC and owned by the controlled
/// wallet. Mismatch → typed funding_invalid.
fn assert_balance_identity(
    b: &FetchedTokenBalance,
    controlled_wallet: &str,
) -> Result<(), (W5hFundingConfirmErrorCode, String)> {
    if b.mint != USDC_MINT_BS58 {
        return Err((
            W5hFundingConfirmErrorCode::WrongMint,
            format!(
                "token-balance row mint {} ≠ USDC mint {}",
                b.mint, USDC_MINT_BS58
            ),
        ));
    }
    if b.owner != controlled_wallet {
        return Err((
            W5hFundingConfirmErrorCode::DestinationNotControlledUsdcAta,
            format!(
                "token-balance row owner {} ≠ controlled wallet {}",
                b.owner, controlled_wallet
            ),
        ));
    }
    Ok(())
}

fn parse_raw_amount(s: &str) -> Result<u64, std::num::ParseIntError> {
    s.parse::<u64>()
}

// ── Orchestrator ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Stage2W5hFundingConfirmExecutor {
    intent_repo: Arc<Stage2W5hFundingIntentRepository>,
    tx_fetcher: Arc<dyn TxFetcher>,
    save_apy_fetcher:
        Option<Arc<dyn crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher>>,
    apr_fetcher: Option<Arc<dyn crate::stage2_demo_apr_bridge::W5dAprFetcher>>,
}

impl std::fmt::Debug for Stage2W5hFundingConfirmExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage2W5hFundingConfirmExecutor").finish()
    }
}

impl Stage2W5hFundingConfirmExecutor {
    pub fn new(
        intent_repo: Arc<Stage2W5hFundingIntentRepository>,
        tx_fetcher: Arc<dyn TxFetcher>,
        save_apy_fetcher: Option<
            Arc<dyn crate::stage2_demo_apr_bridge::SaveDisplayApyFetcher>,
        >,
        apr_fetcher: Option<Arc<dyn crate::stage2_demo_apr_bridge::W5dAprFetcher>>,
    ) -> Self {
        Self {
            intent_repo,
            tx_fetcher,
            save_apy_fetcher,
            apr_fetcher,
        }
    }

    pub async fn execute(
        &self,
        request: W5hFundingConfirmRequest,
    ) -> W5hFundingConfirmOutcome {
        // ── 1. Look up the intent ───────────────────────────────────
        let stored = match self.intent_repo.get(&request.rule_id_hex).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                let rid = request.rule_id_hex.clone();
                return W5hFundingConfirmOutcome {
                    status: W5hFundingConfirmStatus::IntentNotFound,
                    rule_id_hex: rid.clone(),
                    canonical_rule_hash_hex: String::new(),
                    funding_signature: request.funding_signature,
                    funding_finalized_slot: None,
                    expires_at_ms: 0,
                    threshold_bps: 0,
                    save_display_apy_bps: None,
                    native_onchain_apr_bps: None,
                    amount_raw: request.amount_raw,
                    user_wallet: request.user_wallet,
                    user_usdc_ata: request.user_usdc_ata,
                    controlled_wallet: request.controlled_wallet,
                    controlled_usdc_ata: request.controlled_usdc_ata,
                    error: Some(W5hFundingConfirmErrorCode::IntentNotFound),
                    error_reason: Some(format!(
                        "rule_id {rid} not found in W5h funding intent repo"
                    )),
                };
            }
            Err(e) => {
                return W5hFundingConfirmOutcome {
                    status: W5hFundingConfirmStatus::RepoFailed,
                    rule_id_hex: request.rule_id_hex.clone(),
                    canonical_rule_hash_hex: String::new(),
                    funding_signature: request.funding_signature,
                    funding_finalized_slot: None,
                    expires_at_ms: 0,
                    threshold_bps: 0,
                    save_display_apy_bps: None,
                    native_onchain_apr_bps: None,
                    amount_raw: request.amount_raw,
                    user_wallet: request.user_wallet,
                    user_usdc_ata: request.user_usdc_ata,
                    controlled_wallet: request.controlled_wallet,
                    controlled_usdc_ata: request.controlled_usdc_ata,
                    error: Some(W5hFundingConfirmErrorCode::RepoFailed),
                    error_reason: Some(format!("repo.get failed: {e}")),
                };
            }
        };

        // ── 2. Validate caller-supplied identity matches persisted ──
        if stored.user_wallet != request.user_wallet
            || stored.user_usdc_ata != request.user_usdc_ata
            || stored.controlled_wallet != request.controlled_wallet
            || stored.controlled_usdc_ata != request.controlled_usdc_ata
            || stored.amount_raw != request.amount_raw
        {
            return self.terminal_outcome(
                &stored,
                request.funding_signature,
                W5hFundingConfirmStatus::RequestMismatch,
                W5hFundingConfirmErrorCode::RequestMismatch,
                "request wallet/ATA/amount fields do not match persisted intent".to_string(),
            ).await;
        }

        // ── 3. Refuse if intent is already in a non-actionable state ─
        match stored.status {
            W5hIntentStatus::FundingRequired | W5hIntentStatus::FundingSubmitted => {
                // OK to proceed.
            }
            W5hIntentStatus::BudgetReserved
            | W5hIntentStatus::Executing
            | W5hIntentStatus::Completed
            | W5hIntentStatus::Expired
            | W5hIntentStatus::Refunding
            | W5hIntentStatus::Refunded
            | W5hIntentStatus::FundingInvalid
            | W5hIntentStatus::Failed => {
                // Echo current status without doing a tx fetch.
                let mapped_status = match stored.status {
                    W5hIntentStatus::BudgetReserved => {
                        W5hFundingConfirmStatus::BudgetReserved
                    }
                    W5hIntentStatus::FundingInvalid => {
                        W5hFundingConfirmStatus::FundingInvalid
                    }
                    _ => W5hFundingConfirmStatus::AlreadyTerminal,
                };
                let error = if mapped_status
                    == W5hFundingConfirmStatus::AlreadyTerminal
                {
                    Some(W5hFundingConfirmErrorCode::IntentAlreadyTerminal)
                } else {
                    None
                };
                let funding_sig = stored
                    .funding_signature
                    .clone()
                    .unwrap_or(request.funding_signature.clone());
                return self.success_outcome_with_status(
                    &stored,
                    funding_sig,
                    mapped_status,
                    error,
                ).await;
            }
        }

        // ── 4. CAS funding_required → funding_submitted ─────────────
        // Returns 1 row if status flipped OR if status was already
        // funding_submitted AND same signature (idempotent).
        let submit_rows = self
            .intent_repo
            .mark_funding_submitted_if_required(
                &request.rule_id_hex,
                &request.funding_signature,
            )
            .await;
        match submit_rows {
            Ok(0) => {
                // Race-with-a-different-signature: status is
                // `funding_submitted` but with a DIFFERENT
                // funding_signature on the persisted row. Refuse
                // conservatively.
                return self.terminal_outcome(
                    &stored,
                    request.funding_signature,
                    W5hFundingConfirmStatus::RequestMismatch,
                    W5hFundingConfirmErrorCode::RequestMismatch,
                    "intent already submitted under a different funding_signature; \
                     refusing to overwrite".to_string(),
                ).await;
            }
            Ok(_) => {}
            Err(e) => {
                return self.terminal_outcome(
                    &stored,
                    request.funding_signature,
                    W5hFundingConfirmStatus::RepoFailed,
                    W5hFundingConfirmErrorCode::RepoFailed,
                    format!("repo CAS failed: {e}"),
                ).await;
            }
        }

        // ── 5. Fetch the on-chain tx ────────────────────────────────
        let tx_outcome = self
            .tx_fetcher
            .fetch(&request.funding_signature)
            .await;
        let tx = match tx_outcome {
            Ok(TxFetchOutcome::NotYetAvailable) => {
                // Intent stays at `funding_submitted`; frontend
                // retries. No state regression.
                return self.success_outcome_with_status(
                    &stored,
                    request.funding_signature,
                    W5hFundingConfirmStatus::FundingPending,
                    Some(W5hFundingConfirmErrorCode::TxNotYetAvailable),
                ).await;
            }
            Ok(TxFetchOutcome::VerifierUnsupported(reason)) => {
                // Backend-side verifier limitation (unsupported tx
                // version, unresolved address-table lookups). Treat
                // as funding_pending so a real funding tx is not
                // marked invalid because of a configuration gap.
                return self.success_outcome_with_status(
                    &stored,
                    request.funding_signature,
                    W5hFundingConfirmStatus::FundingPending,
                    Some(W5hFundingConfirmErrorCode::VerifierUnsupported),
                ).await
                    .with_reason(format!("verifier_unsupported: {reason}"));
            }
            Ok(TxFetchOutcome::Available(t)) => t,
            Err(e) => {
                // RPC transport error — frontend retries.
                return self.success_outcome_with_status(
                    &stored,
                    request.funding_signature,
                    W5hFundingConfirmStatus::FundingPending,
                    Some(W5hFundingConfirmErrorCode::TxNotYetAvailable),
                ).await
                    .with_reason(format!("tx_fetcher: {e}"));
            }
        };

        // ── 6. Delta-verify (USDC mint + controlled-owner +250 000) ─
        match verify_funding_delta(
            &tx,
            &request.controlled_usdc_ata,
            &request.controlled_wallet,
            request.amount_raw,
        ) {
            Err((code, reason)) => {
                // Terminal funding_invalid — record on repo.
                let _ = self
                    .intent_repo
                    .mark_funding_invalid_if_submitted(
                        &request.rule_id_hex,
                        &request.funding_signature,
                        &reason,
                    )
                    .await;
                return self.terminal_outcome(
                    &stored,
                    request.funding_signature,
                    W5hFundingConfirmStatus::FundingInvalid,
                    code,
                    reason,
                ).await;
            }
            Ok(finalized_slot) => {
                // ── 7. CAS funding_submitted → budget_reserved ──────
                let n = self
                    .intent_repo
                    .mark_budget_reserved_if_submitted(
                        &request.rule_id_hex,
                        &request.funding_signature,
                        finalized_slot,
                    )
                    .await;
                match n {
                    Ok(1) | Ok(_) => {
                        // 1 = transition. 0 = race (already advanced
                        // by another caller); we still report
                        // budget_reserved if the repo confirms it.
                    }
                    Err(e) => {
                        return self.terminal_outcome(
                            &stored,
                            request.funding_signature,
                            W5hFundingConfirmStatus::RepoFailed,
                            W5hFundingConfirmErrorCode::RepoFailed,
                            format!("repo CAS failed: {e}"),
                        ).await;
                    }
                }
                // Re-read the stored row to surface the truth
                // (status, finalized_slot).
                let refreshed = self
                    .intent_repo
                    .get(&request.rule_id_hex)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(stored.clone());
                let live = self.refresh_live_metrics().await;
                W5hFundingConfirmOutcome {
                    status: match refreshed.status {
                        W5hIntentStatus::BudgetReserved => {
                            W5hFundingConfirmStatus::BudgetReserved
                        }
                        _ => W5hFundingConfirmStatus::FundingPending,
                    },
                    rule_id_hex: refreshed.rule_id_hex.clone(),
                    canonical_rule_hash_hex: refreshed.canonical_rule_hash_hex.clone(),
                    funding_signature: request.funding_signature,
                    funding_finalized_slot: Some(finalized_slot),
                    expires_at_ms: refreshed.expires_at_ms,
                    threshold_bps: refreshed.threshold_bps,
                    save_display_apy_bps: live.0,
                    native_onchain_apr_bps: live.1,
                    amount_raw: refreshed.amount_raw,
                    user_wallet: refreshed.user_wallet.clone(),
                    user_usdc_ata: refreshed.user_usdc_ata.clone(),
                    controlled_wallet: refreshed.controlled_wallet.clone(),
                    controlled_usdc_ata: refreshed.controlled_usdc_ata.clone(),
                    error: None,
                    error_reason: None,
                }
            }
        }
    }

    /// Fetch the live decision-metric snapshot for the response. If
    /// either fetcher is missing or errors, return `None` for that
    /// field — the operator already has the at-creation snapshot in
    /// the W5h conditional-order card.
    async fn refresh_live_metrics(&self) -> (Option<u32>, Option<u32>) {
        let save_bps = match &self.save_apy_fetcher {
            Some(f) => f.fetch_main_pool_usdc().await.ok().map(|r| r.save_display_apy_bps),
            None => None,
        };
        let native_bps = match &self.apr_fetcher {
            Some(f) => {
                use crate::stage2_demo_apr_bridge::DemoParsed;
                let dummy = DemoParsed {
                    threshold_bps: 0,
                    threshold_pct_label: "0".to_string(),
                    amount_raw: 0,
                };
                f.evaluate("", &dummy).await.ok().map(|r| r.current_apr_bps)
            }
            None => None,
        };
        (save_bps, native_bps)
    }

    async fn success_outcome_with_status(
        &self,
        stored: &claw_state_store::stage2_w5h_funding::W5hFundingIntent,
        funding_signature: String,
        status: W5hFundingConfirmStatus,
        error: Option<W5hFundingConfirmErrorCode>,
    ) -> W5hFundingConfirmOutcome {
        let live = self.refresh_live_metrics().await;
        W5hFundingConfirmOutcome {
            status,
            rule_id_hex: stored.rule_id_hex.clone(),
            canonical_rule_hash_hex: stored.canonical_rule_hash_hex.clone(),
            funding_signature,
            funding_finalized_slot: stored.funding_finalized_slot,
            expires_at_ms: stored.expires_at_ms,
            threshold_bps: stored.threshold_bps,
            save_display_apy_bps: live.0,
            native_onchain_apr_bps: live.1,
            amount_raw: stored.amount_raw,
            user_wallet: stored.user_wallet.clone(),
            user_usdc_ata: stored.user_usdc_ata.clone(),
            controlled_wallet: stored.controlled_wallet.clone(),
            controlled_usdc_ata: stored.controlled_usdc_ata.clone(),
            error,
            error_reason: None,
        }
    }

    async fn terminal_outcome(
        &self,
        stored: &claw_state_store::stage2_w5h_funding::W5hFundingIntent,
        funding_signature: String,
        status: W5hFundingConfirmStatus,
        code: W5hFundingConfirmErrorCode,
        reason: String,
    ) -> W5hFundingConfirmOutcome {
        let live = self.refresh_live_metrics().await;
        W5hFundingConfirmOutcome {
            status,
            rule_id_hex: stored.rule_id_hex.clone(),
            canonical_rule_hash_hex: stored.canonical_rule_hash_hex.clone(),
            funding_signature,
            funding_finalized_slot: stored.funding_finalized_slot,
            expires_at_ms: stored.expires_at_ms,
            threshold_bps: stored.threshold_bps,
            save_display_apy_bps: live.0,
            native_onchain_apr_bps: live.1,
            amount_raw: stored.amount_raw,
            user_wallet: stored.user_wallet.clone(),
            user_usdc_ata: stored.user_usdc_ata.clone(),
            controlled_wallet: stored.controlled_wallet.clone(),
            controlled_usdc_ata: stored.controlled_usdc_ata.clone(),
            error: Some(code),
            error_reason: Some(reason),
        }
    }
}

// Convenience builder so `.await.with_reason(...)` chains cleanly.
impl W5hFundingConfirmOutcome {
    fn with_reason(mut self, r: String) -> Self {
        self.error_reason = Some(r);
        self
    }
}

// ── Live tx fetcher (production JSON-RPC implementation) ─────────────────
//
// `LiveTxFetcher` wraps Solana JSON-RPC `getTransaction` for the W5h
// funding-confirm route. It is read-only — no signing, no
// `sendTransaction`, no keypair.
//
// W5h-lite addendum §1 requirements baked in:
//
//   - `maxSupportedTransactionVersion: 0` is ALWAYS sent so v0
//     transactions (which Phantom can submit) are returned by the
//     RPC instead of rejected as "Transaction version unsupported".
//
//   - If the RPC surfaces an unsupported-version error anyway, we map
//     it to `VerifierUnsupported` (NOT `funding_invalid`) — a real
//     funding tx must never be rejected because of a verifier-side
//     gap.
//
// W5h-lite addendum §2 requirements:
//
//   - We build the full account-key list from
//     `transaction.message.accountKeys` AND, for v0 transactions,
//     `meta.loadedAddresses.writable` followed by
//     `meta.loadedAddresses.readonly`.
//
//   - Every `meta.preTokenBalances` / `meta.postTokenBalances` entry
//     is resolved to its pubkey via the merged key list. The verifier
//     then matches the destination ATA by pubkey equality, never by
//     `accountIndex` ordering.
//
//   - If a token-balance entry references an `accountIndex` we
//     cannot resolve (the merged key list is shorter than the
//     index — typical when a node returns loaded addresses in a
//     format we did not parse), the fetcher emits
//     `VerifierUnsupported(...)` so the orchestrator returns
//     `funding_pending` rather than silently mismatching.
//
// W5h-lite addendum §3 — polling ownership:
//
//   - This fetcher performs a SINGLE `getTransaction` call. It does
//     NOT sleep, poll, or hold the HTTP request open. If the tx is
//     not yet visible, we return `NotYetAvailable`. The frontend
//     owns the retry loop.

use reqwest::Client as ReqwestClient;
use serde_json::{json, Value as JsonValue};
use std::time::Duration as StdDuration;

const LIVE_TX_FETCHER_TIMEOUT_MS: u64 = 4_000;

#[derive(Clone)]
pub struct LiveTxFetcher {
    rpc_url: String,
    http: ReqwestClient,
}

impl std::fmt::Debug for LiveTxFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveTxFetcher")
            .field("rpc_url", &"<redacted>")
            .finish()
    }
}

impl LiveTxFetcher {
    /// Build a fetcher with the given RPC URL. Returns `None` if the
    /// URL is empty/blank or if reqwest fails to construct a client
    /// (timeout misconfig, native TLS init failure, etc).
    pub fn new(rpc_url: impl Into<String>) -> Option<Self> {
        let rpc_url = rpc_url.into();
        if rpc_url.trim().is_empty() {
            return None;
        }
        let http = ReqwestClient::builder()
            .timeout(StdDuration::from_millis(LIVE_TX_FETCHER_TIMEOUT_MS))
            .build()
            .ok()?;
        Some(Self { rpc_url, http })
    }
}

/// Build the `getTransaction` JSON-RPC request body. Kept pub(crate)
/// so tests can prove the wire shape contains
/// `maxSupportedTransactionVersion: 0`.
pub(crate) fn build_get_transaction_request_body(signature: &str) -> JsonValue {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [
            signature,
            {
                "encoding": "json",
                "commitment": "confirmed",
                "maxSupportedTransactionVersion": 0,
            }
        ]
    })
}

#[async_trait]
impl TxFetcher for LiveTxFetcher {
    async fn fetch(&self, signature: &str) -> Result<TxFetchOutcome, String> {
        let body = build_get_transaction_request_body(signature);
        let resp = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("transport: {e}"))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(format!("http {status}"));
        }
        let v: JsonValue = resp
            .json()
            .await
            .map_err(|e| format!("body decode: {e}"))?;

        // Typed RPC error path. We MUST distinguish
        // "unsupported tx version" from other errors and route the
        // former to VerifierUnsupported (NOT funding_invalid).
        if let Some(err) = v.get("error") {
            let msg = serde_json::to_string(err).unwrap_or_else(|_| "{}".to_string());
            let lower = msg.to_ascii_lowercase();
            if lower.contains("version") && lower.contains("unsupported") {
                return Ok(TxFetchOutcome::VerifierUnsupported(format!(
                    "rpc rejected as unsupported version: {msg}"
                )));
            }
            return Err(format!("rpc error: {msg}"));
        }

        let result = v.get("result").cloned().unwrap_or(JsonValue::Null);
        if result.is_null() {
            return Ok(TxFetchOutcome::NotYetAvailable);
        }
        match parse_get_transaction_result(&result) {
            Ok(outcome) => Ok(outcome),
            Err(e) => Err(format!("getTransaction parse: {e}")),
        }
    }
}

/// Parse a `result` object from `getTransaction` into a typed
/// `TxFetchOutcome`. Kept pub(crate) so tests can run fixture JSON
/// through this function without spinning up an HTTP server.
pub(crate) fn parse_get_transaction_result(
    result: &JsonValue,
) -> Result<TxFetchOutcome, String> {
    let slot = result
        .get("slot")
        .and_then(|s| s.as_u64())
        .ok_or_else(|| "missing/non-u64 slot".to_string())?;

    let meta = result
        .get("meta")
        .ok_or_else(|| "missing meta".to_string())?;
    let err = meta
        .get("err")
        .and_then(|e| {
            if e.is_null() {
                None
            } else {
                Some(serde_json::to_string(e).unwrap_or_else(|_| "<unserializable>".to_string()))
            }
        });

    // ── Resolve the full account-key list ──────────────────────────────
    //
    // For v0 transactions, the JSON-RPC `json` encoding returns the
    // message-level static accountKeys plus, separately,
    // `meta.loadedAddresses.{writable, readonly}` arrays. We MUST
    // concatenate them in this order to match how the runtime
    // interprets `accountIndex`:
    //
    //   [ message.accountKeys ... ] ++
    //   [ meta.loadedAddresses.writable ... ] ++
    //   [ meta.loadedAddresses.readonly ... ]
    //
    // Reference: Solana JSON-RPC docs for `getTransaction`,
    // `encoding=json` + `maxSupportedTransactionVersion=0`. Other
    // encodings (`jsonParsed`, `base64`, etc) have different layouts;
    // we hold the format invariant by always requesting `json`.
    let mut keys: Vec<String> = Vec::new();
    let mut alt_present = false;
    if let Some(arr) = result
        .get("transaction")
        .and_then(|t| t.get("message"))
        .and_then(|m| m.get("accountKeys"))
        .and_then(|a| a.as_array())
    {
        for k in arr {
            if let Some(s) = k.as_str() {
                keys.push(s.to_string());
            }
        }
    }
    if let Some(loaded) = meta.get("loadedAddresses") {
        for field in ["writable", "readonly"] {
            if let Some(arr) = loaded.get(field).and_then(|a| a.as_array()) {
                if !arr.is_empty() {
                    alt_present = true;
                }
                for k in arr {
                    if let Some(s) = k.as_str() {
                        keys.push(s.to_string());
                    }
                }
            }
        }
    }

    // Helper: resolve an `accountIndex` → pubkey. If the merged list
    // is shorter than the index we have NO safe way to identify the
    // destination ATA, so we surface `VerifierUnsupported`. Returning
    // an "unknown" pubkey here would let the verifier silently miss
    // the controlled-ATA row.
    let resolve = |idx: u64| -> Option<&str> {
        keys.get(idx as usize).map(|s| s.as_str())
    };

    let pre_token_balances = match parse_token_balance_array(meta.get("preTokenBalances"), &keys) {
        Ok(v) => v,
        Err(e) => {
            return Ok(TxFetchOutcome::VerifierUnsupported(format!(
                "unresolved preTokenBalances (alt_present={alt_present}): {e}"
            )))
        }
    };
    let post_token_balances = match parse_token_balance_array(meta.get("postTokenBalances"), &keys) {
        Ok(v) => v,
        Err(e) => {
            return Ok(TxFetchOutcome::VerifierUnsupported(format!(
                "unresolved postTokenBalances (alt_present={alt_present}): {e}"
            )))
        }
    };

    // `resolve` is kept above for future per-entry lookups; current
    // call sites use `keys` directly via `parse_token_balance_array`.
    let _ = resolve;

    Ok(TxFetchOutcome::Available(FetchedTx {
        slot,
        err,
        pre_token_balances,
        post_token_balances,
    }))
}

fn parse_token_balance_array(
    arr: Option<&JsonValue>,
    keys: &[String],
) -> Result<Vec<FetchedTokenBalance>, String> {
    let arr = match arr.and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let account_index = entry
            .get("accountIndex")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| "tokenBalance entry missing accountIndex".to_string())?;
        let pubkey = keys
            .get(account_index as usize)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "accountIndex {account_index} out of resolved-keys range (len={}); \
                     unresolved address-table-lookup entry",
                    keys.len()
                )
            })?;
        let mint = entry
            .get("mint")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "tokenBalance entry missing mint".to_string())?
            .to_string();
        let owner = entry
            .get("owner")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "tokenBalance entry missing owner".to_string())?
            .to_string();
        let amount_raw_str = entry
            .get("uiTokenAmount")
            .and_then(|u| u.get("amount"))
            .and_then(|a| a.as_str())
            .ok_or_else(|| "tokenBalance entry missing uiTokenAmount.amount".to_string())?
            .to_string();
        out.push(FetchedTokenBalance {
            account_index: account_index as u32,
            pubkey,
            mint,
            owner,
            amount_raw_str,
        });
    }
    Ok(out)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use claw_state_store::db::Database;
    use claw_state_store::stage2_w5h_funding::NewW5hFundingIntent;

    #[derive(Debug, Clone)]
    struct StubFetcher {
        outcome: Result<TxFetchOutcome, String>,
    }
    #[async_trait]
    impl TxFetcher for StubFetcher {
        async fn fetch(&self, _signature: &str) -> Result<TxFetchOutcome, String> {
            self.outcome.clone()
        }
    }

    const USER_WALLET: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const USER_USDC_ATA: &str = "TestUserUsdcAta1111111111111111111111111111";
    const CONTROLLED_WALLET: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";
    const CONTROLLED_USDC_ATA: &str = "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3";
    const RULE_ID: &str = "6400000090d00300000000009a62dace";
    const CANON_HASH: &str =
        "962ae352baa3f4d494d4723f6391d40643271d1fd63b9085968ca892a60df69d";

    async fn fixture() -> (Database, Arc<Stage2W5hFundingIntentRepository>) {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            db.pool().clone(),
        ));
        let now_ms = 1_000_000_000;
        repo.insert(&NewW5hFundingIntent {
            intent_id: RULE_ID.to_string(),
            rule_id_hex: RULE_ID.to_string(),
            canonical_rule_hash_hex: CANON_HASH.to_string(),
            user_wallet: USER_WALLET.to_string(),
            user_usdc_ata: USER_USDC_ATA.to_string(),
            controlled_wallet: CONTROLLED_WALLET.to_string(),
            controlled_usdc_ata: CONTROLLED_USDC_ATA.to_string(),
            amount_raw: 250_000,
            threshold_bps: 100,
            save_display_apy_bps_at_creation: 210,
            native_onchain_apr_bps_at_creation: 165,
            created_at_ms: now_ms,
            expires_at_ms: now_ms + 180_000,
        })
        .await
        .unwrap();
        (db, repo)
    }

    fn good_request(signature: &str) -> W5hFundingConfirmRequest {
        W5hFundingConfirmRequest {
            rule_id_hex: RULE_ID.to_string(),
            funding_signature: signature.to_string(),
            user_wallet: USER_WALLET.to_string(),
            user_usdc_ata: USER_USDC_ATA.to_string(),
            controlled_wallet: CONTROLLED_WALLET.to_string(),
            controlled_usdc_ata: CONTROLLED_USDC_ATA.to_string(),
            amount_raw: 250_000,
        }
    }

    fn tx_with_delta(pre_raw: u64, post_raw: u64, mint: &str, owner: &str) -> FetchedTx {
        FetchedTx {
            slot: 419_200_000,
            err: None,
            pre_token_balances: vec![FetchedTokenBalance {
                account_index: 2,
                pubkey: CONTROLLED_USDC_ATA.to_string(),
                mint: mint.to_string(),
                owner: owner.to_string(),
                amount_raw_str: pre_raw.to_string(),
            }],
            post_token_balances: vec![FetchedTokenBalance {
                account_index: 2,
                pubkey: CONTROLLED_USDC_ATA.to_string(),
                mint: mint.to_string(),
                owner: owner.to_string(),
                amount_raw_str: post_raw.to_string(),
            }],
        }
    }

    // ── Pure verifier ──────────────────────────────────────────────────

    #[test]
    fn verifier_accepts_exact_plus_250000_delta() {
        let tx = tx_with_delta(0, 250_000, USDC_MINT_BS58, CONTROLLED_WALLET);
        let slot = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap();
        assert_eq!(slot, 419_200_000);
    }

    #[test]
    fn verifier_rejects_wrong_mint() {
        let tx = tx_with_delta(0, 250_000, "WRONGMINT11111111111111111111111111111", CONTROLLED_WALLET);
        let err = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap_err();
        // Pubkey matched but mint identity check fails → WrongMint.
        assert_eq!(err.0, W5hFundingConfirmErrorCode::WrongMint);
    }

    #[test]
    fn verifier_rejects_wrong_delta_amount() {
        let tx = tx_with_delta(0, 100_000, USDC_MINT_BS58, CONTROLLED_WALLET);
        let err = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap_err();
        assert_eq!(err.0, W5hFundingConfirmErrorCode::AmountDeltaMismatch);
        assert!(err.1.contains("+100000 raw") && err.1.contains("+250000"));
    }

    #[test]
    fn verifier_rejects_wrong_destination_owner() {
        let tx = tx_with_delta(
            0,
            250_000,
            USDC_MINT_BS58,
            "SomeOtherOwner111111111111111111111111111111",
        );
        let err = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap_err();
        assert_eq!(
            err.0,
            W5hFundingConfirmErrorCode::DestinationNotControlledUsdcAta
        );
    }

    #[test]
    fn verifier_rejects_on_chain_err() {
        let mut tx = tx_with_delta(0, 250_000, USDC_MINT_BS58, CONTROLLED_WALLET);
        tx.err = Some("InstructionError: Custom(1)".to_string());
        let err = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap_err();
        assert_eq!(err.0, W5hFundingConfirmErrorCode::OnChainError);
    }

    #[test]
    fn verifier_accepts_brand_new_ata_pre_missing() {
        // Pre-tx the controlled ATA didn't exist (was just created
        // by the tx itself). Verifier treats missing pre as 0.
        let tx = FetchedTx {
            slot: 1,
            err: None,
            pre_token_balances: vec![], // no pre entry
            post_token_balances: vec![FetchedTokenBalance {
                account_index: 2,
                pubkey: CONTROLLED_USDC_ATA.to_string(),
                mint: USDC_MINT_BS58.to_string(),
                owner: CONTROLLED_WALLET.to_string(),
                amount_raw_str: "250000".to_string(),
            }],
        };
        let slot = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap();
        assert_eq!(slot, 1);
    }

    // ── W5h-lite addendum §2: accountIndex → pubkey resolution ────────

    #[test]
    fn verifier_accepts_destination_at_non_zero_account_index() {
        // post_token_balances has the controlled ATA at accountIndex 7;
        // other token rows (e.g. user's source ATA) sit at lower
        // indices. The verifier MUST match by pubkey, not by index.
        let tx = FetchedTx {
            slot: 419_201_111,
            err: None,
            pre_token_balances: vec![
                FetchedTokenBalance {
                    account_index: 3,
                    pubkey: "UserUsdcAtaPubkey11111111111111111111111111".to_string(),
                    mint: USDC_MINT_BS58.to_string(),
                    owner: "UserWallet1111111111111111111111111111111111".to_string(),
                    amount_raw_str: "5_000_000".replace('_', ""),
                },
                FetchedTokenBalance {
                    account_index: 7,
                    pubkey: CONTROLLED_USDC_ATA.to_string(),
                    mint: USDC_MINT_BS58.to_string(),
                    owner: CONTROLLED_WALLET.to_string(),
                    amount_raw_str: "0".to_string(),
                },
            ],
            post_token_balances: vec![
                FetchedTokenBalance {
                    account_index: 7,
                    pubkey: CONTROLLED_USDC_ATA.to_string(),
                    mint: USDC_MINT_BS58.to_string(),
                    owner: CONTROLLED_WALLET.to_string(),
                    amount_raw_str: "250000".to_string(),
                },
                FetchedTokenBalance {
                    account_index: 3,
                    pubkey: "UserUsdcAtaPubkey11111111111111111111111111".to_string(),
                    mint: USDC_MINT_BS58.to_string(),
                    owner: "UserWallet1111111111111111111111111111111111".to_string(),
                    amount_raw_str: "4750000".to_string(),
                },
            ],
        };
        let slot = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap();
        assert_eq!(slot, 419_201_111);
    }

    #[test]
    fn verifier_rejects_wrong_pubkey_even_when_mint_and_amount_match() {
        // Attacker scenario: an ATA that's NOT the controlled USDC
        // ATA but still has USDC mint and the same +250 000 delta.
        // Strict pubkey equality MUST reject this.
        let tx = FetchedTx {
            slot: 419_300_000,
            err: None,
            pre_token_balances: vec![FetchedTokenBalance {
                account_index: 1,
                pubkey: "DifferentUsdcAta111111111111111111111111111".to_string(),
                mint: USDC_MINT_BS58.to_string(),
                owner: CONTROLLED_WALLET.to_string(),
                amount_raw_str: "0".to_string(),
            }],
            post_token_balances: vec![FetchedTokenBalance {
                account_index: 1,
                pubkey: "DifferentUsdcAta111111111111111111111111111".to_string(),
                mint: USDC_MINT_BS58.to_string(),
                owner: CONTROLLED_WALLET.to_string(),
                amount_raw_str: "250000".to_string(),
            }],
        };
        let err = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap_err();
        assert_eq!(
            err.0,
            W5hFundingConfirmErrorCode::DestinationNotControlledUsdcAta
        );
    }

    // ── Orchestrator paths ────────────────────────────────────────────

    #[tokio::test]
    async fn verifier_unsupported_returns_funding_pending_not_invalid() {
        // W5h-lite addendum §1/§2: fetcher signals a verifier
        // limitation (e.g. unsupported tx version, unresolved ALT).
        // Intent must NOT regress to funding_invalid — that would
        // mark a real funding tx as failed because of a backend
        // verifier gap. Status stays funding_pending; frontend
        // retries; operator investigates the reason.
        let (_db, repo) = fixture().await;
        let exec = Stage2W5hFundingConfirmExecutor::new(
            repo.clone(),
            Arc::new(StubFetcher {
                outcome: Ok(TxFetchOutcome::VerifierUnsupported(
                    "Transaction version (0) is not supported".to_string(),
                )),
            }),
            None,
            None,
        );
        let o = exec.execute(good_request("Sig1")).await;
        assert_eq!(o.status, W5hFundingConfirmStatus::FundingPending);
        assert_eq!(
            o.error,
            Some(W5hFundingConfirmErrorCode::VerifierUnsupported)
        );
        let stored = repo.get(RULE_ID).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::FundingSubmitted);
    }

    #[tokio::test]
    async fn rpc_delay_returns_funding_pending() {
        let (_db, repo) = fixture().await;
        let exec = Stage2W5hFundingConfirmExecutor::new(
            repo.clone(),
            Arc::new(StubFetcher {
                outcome: Ok(TxFetchOutcome::NotYetAvailable),
            }),
            None,
            None,
        );
        let o = exec.execute(good_request("Sig1")).await;
        assert_eq!(o.status, W5hFundingConfirmStatus::FundingPending);
        assert_eq!(
            o.error,
            Some(W5hFundingConfirmErrorCode::TxNotYetAvailable)
        );
        // Intent stays at funding_submitted (CAS already flipped).
        let stored = repo.get(RULE_ID).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::FundingSubmitted);
    }

    #[tokio::test]
    async fn finalized_with_correct_delta_advances_to_budget_reserved() {
        let (_db, repo) = fixture().await;
        let tx = tx_with_delta(0, 250_000, USDC_MINT_BS58, CONTROLLED_WALLET);
        let exec = Stage2W5hFundingConfirmExecutor::new(
            repo.clone(),
            Arc::new(StubFetcher {
                outcome: Ok(TxFetchOutcome::Available(tx)),
            }),
            None,
            None,
        );
        let o = exec.execute(good_request("Sig1")).await;
        assert_eq!(o.status, W5hFundingConfirmStatus::BudgetReserved);
        assert!(o.error.is_none());
        assert_eq!(o.funding_finalized_slot, Some(419_200_000));
        let stored = repo.get(RULE_ID).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::BudgetReserved);
        assert_eq!(stored.funding_finalized_slot, Some(419_200_000));
    }

    #[tokio::test]
    async fn finalized_with_wrong_amount_marks_funding_invalid() {
        let (_db, repo) = fixture().await;
        let tx = tx_with_delta(0, 1, USDC_MINT_BS58, CONTROLLED_WALLET);
        let exec = Stage2W5hFundingConfirmExecutor::new(
            repo.clone(),
            Arc::new(StubFetcher {
                outcome: Ok(TxFetchOutcome::Available(tx)),
            }),
            None,
            None,
        );
        let o = exec.execute(good_request("Sig1")).await;
        assert_eq!(o.status, W5hFundingConfirmStatus::FundingInvalid);
        assert_eq!(o.error, Some(W5hFundingConfirmErrorCode::AmountDeltaMismatch));
        let stored = repo.get(RULE_ID).await.unwrap().unwrap();
        assert_eq!(stored.status, W5hIntentStatus::FundingInvalid);
        assert!(stored.last_error.is_some());
    }

    #[tokio::test]
    async fn idempotent_resubmit_after_budget_reserved_echoes_status() {
        let (_db, repo) = fixture().await;
        let tx = tx_with_delta(0, 250_000, USDC_MINT_BS58, CONTROLLED_WALLET);
        let exec = Stage2W5hFundingConfirmExecutor::new(
            repo.clone(),
            Arc::new(StubFetcher {
                outcome: Ok(TxFetchOutcome::Available(tx)),
            }),
            None,
            None,
        );
        // First call → budget_reserved.
        let o1 = exec.execute(good_request("Sig1")).await;
        assert_eq!(o1.status, W5hFundingConfirmStatus::BudgetReserved);
        // Same signature again — already in budget_reserved.
        let o2 = exec.execute(good_request("Sig1")).await;
        assert_eq!(o2.status, W5hFundingConfirmStatus::BudgetReserved);
        assert!(o2.error.is_none());
    }

    #[tokio::test]
    async fn intent_not_found_returns_typed_error() {
        let (_db, _repo) = fixture().await;
        let repo = Arc::new(Stage2W5hFundingIntentRepository::new(
            _db.pool().clone(),
        ));
        let exec = Stage2W5hFundingConfirmExecutor::new(
            repo.clone(),
            Arc::new(StubFetcher {
                outcome: Ok(TxFetchOutcome::NotYetAvailable),
            }),
            None,
            None,
        );
        let mut req = good_request("Sig1");
        req.rule_id_hex = "ff".repeat(16); // unknown intent_id
        let o = exec.execute(req).await;
        assert_eq!(o.status, W5hFundingConfirmStatus::IntentNotFound);
        assert_eq!(o.error, Some(W5hFundingConfirmErrorCode::IntentNotFound));
    }

    #[tokio::test]
    async fn request_mismatch_refuses_wrong_user_wallet() {
        let (_db, repo) = fixture().await;
        let exec = Stage2W5hFundingConfirmExecutor::new(
            repo.clone(),
            Arc::new(StubFetcher {
                outcome: Ok(TxFetchOutcome::NotYetAvailable),
            }),
            None,
            None,
        );
        let mut req = good_request("Sig1");
        req.user_wallet = "WrongUser111111111111111111111111111111111111".to_string();
        let o = exec.execute(req).await;
        assert_eq!(o.status, W5hFundingConfirmStatus::RequestMismatch);
        assert_eq!(o.error, Some(W5hFundingConfirmErrorCode::RequestMismatch));
    }

    // ── LiveTxFetcher fixtures (addendum §1/§2) ──────────────────────

    #[test]
    fn get_transaction_request_body_includes_max_supported_transaction_version_0() {
        // W5h-lite addendum §1: Phantom may submit a v0 transaction.
        // The request body MUST include
        // `maxSupportedTransactionVersion: 0` or Solana JSON-RPC
        // returns "Transaction version unsupported" even when the tx
        // exists.
        let body = super::build_get_transaction_request_body(
            "4M4ezLgmAbcDefGhiJkLMnoPqRstUvWxYz0123456789Py3y",
        );
        assert_eq!(body["method"], "getTransaction");
        let cfg = &body["params"][1];
        assert_eq!(cfg["encoding"], "json");
        let commitment = cfg["commitment"].as_str().unwrap_or("");
        assert!(
            commitment == "confirmed" || commitment == "finalized",
            "commitment must be confirmed or finalized; got {commitment:?}"
        );
        assert_eq!(
            cfg["maxSupportedTransactionVersion"].as_i64(),
            Some(0),
            "request MUST set maxSupportedTransactionVersion:0 for v0 txs",
        );
    }

    #[test]
    fn parse_get_transaction_resolves_destination_at_non_zero_account_index() {
        // accountKeys: [user_wallet, mint, controlled_usdc_ata, user_usdc_ata]
        // postTokenBalances entry at accountIndex=2 must resolve to
        // controlled_usdc_ata pubkey.
        let result = serde_json::json!({
            "slot": 419_201_000u64,
            "transaction": {
                "message": {
                    "accountKeys": [
                        "UserWalletPubkey111111111111111111111111111",
                        USDC_MINT_BS58,
                        CONTROLLED_USDC_ATA,
                        "UserUsdcAtaPubkey11111111111111111111111111",
                    ],
                },
            },
            "meta": {
                "err": null,
                "preTokenBalances": [
                    {
                        "accountIndex": 3,
                        "mint": USDC_MINT_BS58,
                        "owner": "UserWalletPubkey111111111111111111111111111",
                        "uiTokenAmount": { "amount": "5000000", "decimals": 6 },
                    },
                ],
                "postTokenBalances": [
                    {
                        "accountIndex": 2,
                        "mint": USDC_MINT_BS58,
                        "owner": CONTROLLED_WALLET,
                        "uiTokenAmount": { "amount": "250000", "decimals": 6 },
                    },
                    {
                        "accountIndex": 3,
                        "mint": USDC_MINT_BS58,
                        "owner": "UserWalletPubkey111111111111111111111111111",
                        "uiTokenAmount": { "amount": "4750000", "decimals": 6 },
                    },
                ],
            },
        });
        let outcome = super::parse_get_transaction_result(&result).unwrap();
        let tx = match outcome {
            TxFetchOutcome::Available(t) => t,
            other => panic!("expected Available, got {other:?}"),
        };
        let post = tx
            .post_token_balances
            .iter()
            .find(|b| b.pubkey == CONTROLLED_USDC_ATA)
            .expect("must resolve controlled ATA pubkey from accountIndex=2");
        assert_eq!(post.account_index, 2);
        assert_eq!(post.mint, USDC_MINT_BS58);
        assert_eq!(post.owner, CONTROLLED_WALLET);
        assert_eq!(post.amount_raw_str, "250000");

        // Run the full verifier on the parsed tx.
        let slot = verify_funding_delta(
            &tx,
            CONTROLLED_USDC_ATA,
            CONTROLLED_WALLET,
            250_000,
        )
        .unwrap();
        assert_eq!(slot, 419_201_000);
    }

    #[test]
    fn parse_get_transaction_includes_loaded_addresses_for_v0() {
        // v0 transaction: static accountKeys is shorter than the
        // controlled ATA's index because the controlled ATA appears
        // in meta.loadedAddresses.writable. Global index 4 = 2 static
        // keys + index 2 in the writable list.
        let result = serde_json::json!({
            "slot": 419_202_500u64,
            "transaction": {
                "message": {
                    "accountKeys": [
                        "UserWalletPubkey111111111111111111111111111",
                        "SystemProgram111111111111111111111111111111",
                    ],
                },
            },
            "meta": {
                "err": null,
                "loadedAddresses": {
                    "writable": [
                        USDC_MINT_BS58,
                        "UserUsdcAtaPubkey11111111111111111111111111",
                        CONTROLLED_USDC_ATA,
                    ],
                    "readonly": [],
                },
                "preTokenBalances": [],
                "postTokenBalances": [
                    {
                        "accountIndex": 4,
                        "mint": USDC_MINT_BS58,
                        "owner": CONTROLLED_WALLET,
                        "uiTokenAmount": { "amount": "250000", "decimals": 6 },
                    },
                ],
            },
        });
        let outcome = super::parse_get_transaction_result(&result).unwrap();
        let tx = match outcome {
            TxFetchOutcome::Available(t) => t,
            other => panic!("expected Available, got {other:?}"),
        };
        let post = tx
            .post_token_balances
            .iter()
            .find(|b| b.pubkey == CONTROLLED_USDC_ATA)
            .expect("ALT-loaded controlled ATA must be resolvable");
        assert_eq!(post.account_index, 4);
    }

    #[test]
    fn parse_get_transaction_unresolved_account_index_returns_verifier_unsupported() {
        // accountIndex 9 references something we cannot resolve (the
        // merged keys list has only 2 entries). The fetcher MUST emit
        // VerifierUnsupported, NOT a "found at fake pubkey" entry.
        let result = serde_json::json!({
            "slot": 419_203_000u64,
            "transaction": {
                "message": {
                    "accountKeys": [
                        "AccountKey0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                        "AccountKey1BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
                    ],
                },
            },
            "meta": {
                "err": null,
                "preTokenBalances": [],
                "postTokenBalances": [
                    {
                        "accountIndex": 9,
                        "mint": USDC_MINT_BS58,
                        "owner": CONTROLLED_WALLET,
                        "uiTokenAmount": { "amount": "250000", "decimals": 6 },
                    },
                ],
            },
        });
        let outcome = super::parse_get_transaction_result(&result).unwrap();
        match outcome {
            TxFetchOutcome::VerifierUnsupported(reason) => {
                assert!(reason.contains("accountIndex"));
                assert!(reason.contains("9"));
            }
            other => panic!("expected VerifierUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn live_tx_fetcher_rejects_empty_url() {
        assert!(super::LiveTxFetcher::new("").is_none());
        assert!(super::LiveTxFetcher::new("   ").is_none());
    }

    #[tokio::test]
    async fn no_send_path_static_check() {
        // Source-scan: this module must NOT contain any
        // sendTransaction / Keypair / from_bytes call site.
        const SRC: &str = include_str!("stage2_w5h_funding_confirm.rs");
        let needles = [
            format!("{}{}", "send", "Transaction("),
            format!("{}{}", "Keypair::", "from_bytes"),
            format!("{}{}", "send_raw_", "transaction("),
        ];
        for n in &needles {
            assert!(
                !SRC.contains(n.as_str()),
                "stage2_w5h_funding_confirm.rs must not contain `{n}`"
            );
        }
    }
}

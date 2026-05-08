//! Phase 6I-D — Solend withdraw-all park store.
//!
//! Companion to [`crate::integrations::solend_park`] (deposit park
//! store) but strictly scoped to the **withdraw-all** flow. Holds the
//! parked intent that a future slice will resume at sign-click time:
//!
//!   1. `solend_withdraw_all_usdc` chat tool validates inputs and parks
//!      a [`ParkedSolendWithdrawAllIntent`] in [`SolendWithdrawAllParkStore`].
//!   2. (Future slice) approval-routing branch fires when the operator
//!      decides; the resume task assembles a fresh withdraw plan via
//!      the still-`#[cfg(test)]`-gated
//!      [`crate::integrations::solend_withdraw_tx_plan::assemble_solend_withdraw_tx_plan`]
//!      and creates a JIT signing handoff.
//!   3. (Future slice) Phantom signs → daemon submits → Phase 6D / 6E
//!      rebroadcast handles confirmation.
//!
//! # What this module owns (Phase 6I-D)
//!
//! - The parked-intent shape (`ParkedSolendWithdrawAllIntent`) — frozen
//!   facts the chat tool gathered: explicit `obligation_pubkey` from
//!   the user, the wallet, the propose-time validation outcome, the
//!   action.
//! - A simple in-memory park store keyed by `approval_request_id`.
//!
//! # Phase 6I-E additions
//!
//! - Park entries now carry a `oneshot::Sender<ApprovalWorkflowState>`
//!   so [`crate::approval_routing::route_approval_outcome`] can deliver
//!   the operator's decision to the resume task.
//! - [`run_solend_withdraw_resume_task`] awaits that decision, performs
//!   a fresh re-fetch + decode + structural re-check of the obligation,
//!   and emits a typed [`SolendWithdrawResumeOutcome`]. NO transaction
//!   is built; NO signing handoff is created; NO broadcast.
//!
//! # What this module deliberately does NOT do (Phase 6I-E scope)
//!
//! - Does NOT build a transaction. The withdraw substrate
//!   ([`crate::integrations::solend::withdraw`] +
//!   [`crate::integrations::solend_withdraw_tx_plan`]) remains
//!   `#[cfg(test)]`-gated. The follow-up slice ungates it together
//!   with adding the JIT signing handoff route.
//! - Does NOT create a JIT signing handoff. The
//!   `RecheckPassedReadyForJit` outcome marks the parked intent as
//!   eligible for the (deferred) prepare HTTP route; the route itself
//!   does not yet exist.
//! - Does NOT sign. No `Keypair`, no signing path, no
//!   `create_signing_handoff` call.
//! - Does NOT broadcast. No `send_transaction` call site is reachable
//!   from this module.
//!
//! # Why a separate park store?
//!
//! Deposit's [`crate::integrations::solend_park::ParkedSolendDepositIntent`]
//! carries deposit-specific facts (`obligation_exists`, `source_ata_exists`,
//! `collateral_ata_exists`, propose-time `LendingSnapshot`). Withdraw's
//! preconditions are different (the obligation MUST already exist; the
//! collateral amount is the on-chain decoded amount, not a user-supplied
//! quantity). Co-locating both shapes in one store would create
//! variant-explosion and force every consumer to handle both kinds.
//! A separate store keeps the deposit pipeline byte-identical and
//! avoids any risk to the live deposit flow.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::oneshot;
use tracing::warn;
use uuid::Uuid;

use claw_types::approval::ApprovalWorkflowState;
use claw_types::session::SessionId;

use crate::lending::ProposedAction;

/// Parked withdraw-all intent. The fields here are the facts the chat
/// tool gathered at propose time AND verified against on-chain state
/// — preserving them makes a future fresh-reassembly slice (the
/// pre-sign re-check) able to detect drift between propose-time and
/// sign-time obligation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedSolendWithdrawAllIntent {
    /// The chat tool's intent id (distinct from `approval_request_id`).
    /// Used for log correlation across propose / approve / sign /
    /// broadcast spans.
    pub intent_id: Uuid,

    /// The session that originated this intent. Cleanup / pending-action
    /// guards key off this.
    pub session_id: SessionId,

    /// The session-bound external wallet — the signer the future JIT
    /// handoff will hand off to Phantom.
    pub session_wallet: Pubkey,

    /// **Explicit** obligation pubkey supplied by the LLM / user. This
    /// is NOT derived; it is the same string the Phase 6H scanner
    /// reported. The future resume / assembler MUST thread this value
    /// verbatim into the withdraw ix at slot 3.
    pub obligation_pubkey: Pubkey,

    /// Validated lending market — frozen at propose time. Sign-time
    /// re-check compares against this to detect on-chain drift.
    pub lending_market: Pubkey,

    /// Reserve pubkey — Solend / Save Main Pool USDC reserve at propose
    /// time. The withdraw substrate is currently scoped to this single
    /// reserve.
    pub reserve_pubkey: Pubkey,

    /// USDC mint — frozen for symmetry with deposit's parked intent.
    pub reserve_mint: Pubkey,

    /// Decoded `deposited_amount` (cToken collateral base units) at
    /// propose time. The future withdraw-all submit uses the
    /// `u64::MAX` sentinel inside the action, but recording the
    /// observed amount lets a sign-time re-check detect a drift to
    /// zero (e.g. the user withdrew via another path between propose
    /// and approve).
    pub propose_time_deposited_collateral_raw: u64,

    /// The `ProposedAction::Withdraw` constructed by the chat tool. The
    /// future resume task uses this verbatim so the policy verdict is
    /// re-evaluated against the same logical action.
    pub action: ProposedAction,

    /// Wallclock timestamps mirror deposit's parked-intent shape. The
    /// `expires_at` field is the propose-time TTL the future
    /// approval-routing layer will enforce.
    pub proposed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// One parked entry: the intent + a oneshot sender that the
/// approval-routing layer fires on terminal decisions
/// (Approved / Rejected / Expired). Mirrors deposit's
/// [`crate::integrations::solend_park::SolendParkStore`] entry shape.
struct WithdrawParkEntry {
    intent: ParkedSolendWithdrawAllIntent,
    /// `None` after a `signal()` consumes it. A second `signal()` for
    /// the same id is a graceful no-op (returns false) — same
    /// idempotency contract as deposit's park store.
    decision_tx: Option<oneshot::Sender<ApprovalWorkflowState>>,
}

/// Phase 6I-D / 6I-E in-memory park store. Cloneable (Arc-backed) so the
/// chat tool, the approval-routing layer, the resume task, and audit /
/// inspection helpers all share one source of truth.
#[derive(Clone, Default)]
pub struct SolendWithdrawAllParkStore {
    inner: Arc<Mutex<HashMap<Uuid, WithdrawParkEntry>>>,
}

impl SolendWithdrawAllParkStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a new intent under `approval_request_id`. Returns the
    /// receiver half of the oneshot decision channel — the caller
    /// (the chat tool) hands it to a freshly-spawned resume task so
    /// only one future ever owns the receiver.
    ///
    /// Panics-free: if a parked entry already exists under that id
    /// (UUID collision is astronomically unlikely), the new park
    /// replaces the old AND drops the old sender. The previous resume
    /// task — if any — observes a dropped channel and exits via the
    /// `ReceiverDropped` outcome.
    pub fn park(
        &self,
        approval_request_id: Uuid,
        intent: ParkedSolendWithdrawAllIntent,
    ) -> oneshot::Receiver<ApprovalWorkflowState> {
        let (tx, rx) = oneshot::channel();
        let mut g = self.inner.lock().expect("withdraw park store mutex");
        g.insert(
            approval_request_id,
            WithdrawParkEntry {
                intent,
                decision_tx: Some(tx),
            },
        );
        rx
    }

    /// Look up a parked intent by `approval_request_id`. Returns a
    /// clone (cheap — fixed-size fields; no transaction bytes).
    pub fn get(
        &self,
        approval_request_id: &Uuid,
    ) -> Option<ParkedSolendWithdrawAllIntent> {
        self.inner
            .lock()
            .expect("withdraw park store mutex")
            .get(approval_request_id)
            .map(|e| e.intent.clone())
    }

    /// Drop a parked intent. Used by the resume task on every terminal
    /// outcome so the store never leaks entries. Returns the removed
    /// intent if present.
    pub fn remove(
        &self,
        approval_request_id: &Uuid,
    ) -> Option<ParkedSolendWithdrawAllIntent> {
        self.inner
            .lock()
            .expect("withdraw park store mutex")
            .remove(approval_request_id)
            .map(|e| e.intent)
    }

    /// Number of currently parked intents. Used by tests.
    pub fn parked_count(&self) -> usize {
        self.inner
            .lock()
            .expect("withdraw park store mutex")
            .len()
    }

    /// Deliver an [`ApprovalWorkflowState`] to the parked resume task.
    ///
    /// Returns `true` if the signal was sent, `false` if either:
    ///   - no parked entry exists under `request_id` (e.g. this id
    ///     belongs to a deposit / Jupiter approval, or the entry was
    ///     already cleaned up — graceful no-op so `route_approval_outcome`
    ///     can broadcast to every store unconditionally), OR
    ///   - the entry's sender was already consumed by a prior signal
    ///     (idempotency).
    ///
    /// Mirrors [`crate::integrations::solend_park::SolendParkStore::signal`].
    pub fn signal(&self, request_id: Uuid, state: ApprovalWorkflowState) -> bool {
        let mut g = self.inner.lock().expect("withdraw park store mutex");
        let Some(entry) = g.get_mut(&request_id) else {
            return false;
        };
        let Some(tx) = entry.decision_tx.take() else {
            return false;
        };
        if tx.send(state).is_err() {
            warn!(
                request_id = %request_id,
                "withdraw_park_store: oneshot receiver dropped before signal delivery"
            );
            return false;
        }
        true
    }

    /// Is there an active parked intent for `(session_id, session_wallet)`?
    /// Used by the chat tool to refuse a duplicate proposal while a
    /// prior one is still in flight (mirroring deposit's Phase 5A
    /// pending-action guard).
    pub fn has_active_for_session_wallet(
        &self,
        session_id: &SessionId,
        session_wallet: &Pubkey,
    ) -> bool {
        let g = self.inner.lock().expect("withdraw park store mutex");
        g.values().any(|e| {
            &e.intent.session_id == session_id && &e.intent.session_wallet == session_wallet
        })
    }
}

// ── Phase 6I-E — resume task ────────────────────────────────────────────────

/// Stable audit-event names emitted by the withdraw resume task.
/// Reordering / renaming is a wire change (downstream audit dashboards
/// match on these exact strings).
pub const AUDIT_EVENT_WITHDRAW_RESUME_DECISION: &str =
    "solend_withdraw_resume_decision_received";
pub const AUDIT_EVENT_WITHDRAW_RECHECK_PASSED_READY_FOR_JIT: &str =
    "solend_withdraw_recheck_passed_ready_for_jit";
pub const AUDIT_EVENT_WITHDRAW_RECHECK_BLOCKED: &str =
    "solend_withdraw_recheck_blocked";
pub const AUDIT_EVENT_WITHDRAW_RECHECK_FETCH_FAILED: &str =
    "solend_withdraw_recheck_fetch_failed";

/// Stable typed reasons used in the `RecheckBlocked` outcome and the
/// `solend_withdraw_recheck_blocked` audit event payload. Keeping them
/// as constants prevents source-text drift between the outcome string,
/// the audit payload, and the future signing-handoff route's policy
/// renderer.
pub const RECHECK_REASON_OBLIGATION_NOT_FOUND: &str = "obligation_not_found";
pub const RECHECK_REASON_DECODE_FAILED: &str = "decode_failed";
pub const RECHECK_REASON_OWNER_MISMATCH: &str = "owner_mismatch";
pub const RECHECK_REASON_LENDING_MARKET_MISMATCH: &str = "lending_market_mismatch";
pub const RECHECK_REASON_NO_USDC_DEPOSIT: &str = "no_usdc_deposit";
pub const RECHECK_REASON_BORROW_APPEARED: &str = "borrow_appeared";
pub const RECHECK_REASON_ZERO_COLLATERAL: &str = "zero_collateral";

/// Terminal outcomes of [`run_solend_withdraw_resume_task`].
///
/// One resume task produces exactly one outcome. Mirrors the design
/// of [`crate::integrations::solend_park::SolendResumeOutcome`] — none
/// of the variants imply a transaction was built, signed, or
/// broadcast. Phase 6I-E is pre-execution by construction; the
/// `RecheckPassedReadyForJit` variant only marks the parked intent
/// as eligible for the (deferred) JIT signing-handoff route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolendWithdrawResumeOutcome {
    /// Operator rejected the approval. No re-check performed.
    Rejected,
    /// Approval lease expired. No re-check performed.
    Expired,
    /// The oneshot receiver was dropped without a state — the parked
    /// entry was removed under the resume task's feet (TTL sweep,
    /// double-park, or daemon shutdown).
    ReceiverDropped,
    /// Approval signal arrived as `Approved`, but the parked intent
    /// was gone from [`SolendWithdrawAllParkStore`] by the time the
    /// resume task looked it up. Idempotent re-entry case — safe
    /// terminal, not an error.
    ParkedIntentMissing,
    /// The fresh `getAccountInfo` call against the obligation pubkey
    /// returned an RPC error. The parked entry is removed; the user
    /// can re-propose.
    RecheckFetchFailed { rpc_error: String },
    /// The fresh fetch succeeded but the bytes did not decode into a
    /// valid Solend obligation (wrong size, malformed). Treated as a
    /// hard block — the obligation may have been closed and reopened
    /// in a different shape between propose and approve.
    RecheckBlocked {
        reason: &'static str,
        detail: Option<String>,
    },
    /// All four invariants held against fresh on-chain state: owner
    /// matches the session wallet, lending market matches, at least
    /// one non-zero USDC-reserve deposit, no non-zero borrow. The
    /// parked intent is now eligible for the JIT signing-handoff
    /// route. **Phase 6I-E does NOT create the handoff** — that work
    /// is the next slice. The fresh-observed `deposited_collateral_raw`
    /// is captured so the future handoff route can detect drift since
    /// re-check (e.g. partial withdraw via another path).
    RecheckPassedReadyForJit {
        fresh_deposited_collateral_raw: u64,
    },
}

impl SolendWithdrawResumeOutcome {
    /// Short stable label for log lines / audit payloads.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::ReceiverDropped => "receiver_dropped",
            Self::ParkedIntentMissing => "parked_intent_missing",
            Self::RecheckFetchFailed { .. } => "recheck_fetch_failed",
            Self::RecheckBlocked { .. } => "recheck_blocked",
            Self::RecheckPassedReadyForJit { .. } => "recheck_passed_ready_for_jit",
        }
    }
}

/// Awaits the operator's decision on a parked withdraw-all intent and,
/// on `Approved`, re-fetches the obligation and re-runs the same four
/// structural checks the chat tool ran at propose time.
///
/// # Forbidden side effects
///
/// - Does NOT build, sign, simulate, or broadcast any transaction.
/// - Does NOT call `create_signing_handoff` or anything in
///   `solend_signing` / `solend_submit`.
/// - Does NOT call into the (still `#[cfg(test)]`-gated) withdraw plan
///   assembler. The future slice that adds the JIT signing-handoff
///   route ungates that module together with constructing a real
///   handoff.
///
/// # Idempotency
///
/// - Consumes `decision_rx`, so a second spawn against the same
///   `request_id` cannot race a duplicate re-check.
/// - On every terminal path, removes the parked entry from
///   `park_store` so the store never leaks.
///
/// # Audit
///
/// Emits `solend_withdraw_resume_decision_received` once on every
/// non-dropped path, then exactly one of:
///   - `solend_withdraw_recheck_passed_ready_for_jit`
///   - `solend_withdraw_recheck_blocked`
///   - `solend_withdraw_recheck_fetch_failed`
/// on the Approved path. Rejected / Expired / Dropped paths emit only
/// the decision event.
pub async fn run_solend_withdraw_resume_task(
    request_id: Uuid,
    park_store: SolendWithdrawAllParkStore,
    reader: Arc<dyn crate::tools::get_solend_position::SolendPositionReader>,
    audit: Arc<dyn crate::integrations::solend_submit::SolendAuditSink>,
    decision_rx: oneshot::Receiver<ApprovalWorkflowState>,
) -> SolendWithdrawResumeOutcome {
    use crate::integrations::solend::raw::{decode_obligation, OBLIGATION_LEN};
    use claw_state_store::audit::AuditSeverity;

    // 1. Await operator decision.
    let state = match decision_rx.await {
        Ok(s) => s,
        Err(_) => {
            park_store.remove(&request_id);
            warn!(
                request_id = %request_id,
                "withdraw_resume: decision channel dropped \u{2014} cleaning up"
            );
            return SolendWithdrawResumeOutcome::ReceiverDropped;
        }
    };

    // Audit decision arrival once.
    let session_id_str = park_store
        .get(&request_id)
        .map(|i| i.session_id.to_string());
    audit
        .append_solend_event(
            AUDIT_EVENT_WITHDRAW_RESUME_DECISION,
            request_id.to_string(),
            session_id_str.clone(),
            AuditSeverity::Info,
            serde_json::json!({
                "approval_workflow_state": format!("{state:?}"),
            }),
        )
        .await;

    match state {
        ApprovalWorkflowState::Rejected => {
            park_store.remove(&request_id);
            return SolendWithdrawResumeOutcome::Rejected;
        }
        ApprovalWorkflowState::Expired => {
            park_store.remove(&request_id);
            return SolendWithdrawResumeOutcome::Expired;
        }
        ApprovalWorkflowState::Pending => {
            park_store.remove(&request_id);
            warn!(
                request_id = %request_id,
                "withdraw_resume: non-terminal Pending state on oneshot \u{2014} cleaning up"
            );
            return SolendWithdrawResumeOutcome::ReceiverDropped;
        }
        ApprovalWorkflowState::Approved => { /* fall through */ }
    }

    // 2. Look up parked intent under the same id.
    let intent = match park_store.get(&request_id) {
        Some(i) => i,
        None => {
            // Idempotent re-entry / TTL sweep — not an error.
            return SolendWithdrawResumeOutcome::ParkedIntentMissing;
        }
    };

    // 3. Fresh fetch the obligation account.
    let bytes = match reader.get_account_data(&intent.obligation_pubkey).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            audit
                .append_solend_event(
                    AUDIT_EVENT_WITHDRAW_RECHECK_BLOCKED,
                    request_id.to_string(),
                    Some(intent.session_id.to_string()),
                    AuditSeverity::Warning,
                    serde_json::json!({
                        "reason": RECHECK_REASON_OBLIGATION_NOT_FOUND,
                        "obligation_pubkey": intent.obligation_pubkey.to_string(),
                    }),
                )
                .await;
            park_store.remove(&request_id);
            return SolendWithdrawResumeOutcome::RecheckBlocked {
                reason: RECHECK_REASON_OBLIGATION_NOT_FOUND,
                detail: None,
            };
        }
        Err(e) => {
            audit
                .append_solend_event(
                    AUDIT_EVENT_WITHDRAW_RECHECK_FETCH_FAILED,
                    request_id.to_string(),
                    Some(intent.session_id.to_string()),
                    AuditSeverity::Warning,
                    serde_json::json!({
                        "rpc_error": e,
                        "obligation_pubkey": intent.obligation_pubkey.to_string(),
                    }),
                )
                .await;
            park_store.remove(&request_id);
            return SolendWithdrawResumeOutcome::RecheckFetchFailed { rpc_error: e };
        }
    };

    // 4. Length + decode.
    if bytes.len() != OBLIGATION_LEN {
        let detail = format!(
            "obligation {} got {} bytes, expected {}",
            intent.obligation_pubkey,
            bytes.len(),
            OBLIGATION_LEN
        );
        audit
            .append_solend_event(
                AUDIT_EVENT_WITHDRAW_RECHECK_BLOCKED,
                request_id.to_string(),
                Some(intent.session_id.to_string()),
                AuditSeverity::Warning,
                serde_json::json!({
                    "reason": RECHECK_REASON_DECODE_FAILED,
                    "detail": detail,
                }),
            )
            .await;
        park_store.remove(&request_id);
        return SolendWithdrawResumeOutcome::RecheckBlocked {
            reason: RECHECK_REASON_DECODE_FAILED,
            detail: Some(detail),
        };
    }
    let decoded = match decode_obligation(&bytes) {
        Ok(d) => d,
        Err(e) => {
            let detail = format!("{e}");
            audit
                .append_solend_event(
                    AUDIT_EVENT_WITHDRAW_RECHECK_BLOCKED,
                    request_id.to_string(),
                    Some(intent.session_id.to_string()),
                    AuditSeverity::Warning,
                    serde_json::json!({
                        "reason": RECHECK_REASON_DECODE_FAILED,
                        "detail": detail,
                    }),
                )
                .await;
            park_store.remove(&request_id);
            return SolendWithdrawResumeOutcome::RecheckBlocked {
                reason: RECHECK_REASON_DECODE_FAILED,
                detail: Some(detail),
            };
        }
    };

    // 5. Four structural re-checks. Order is owner → market → deposit
    //    → borrow, matching the chat tool's propose-time order.
    if decoded.owner != intent.session_wallet {
        return emit_recheck_blocked(
            &audit,
            &park_store,
            request_id,
            &intent.session_id,
            RECHECK_REASON_OWNER_MISMATCH,
            Some(format!(
                "fresh owner {} \u{2260} session wallet {}",
                decoded.owner, intent.session_wallet
            )),
        )
        .await;
    }
    if decoded.lending_market != intent.lending_market {
        return emit_recheck_blocked(
            &audit,
            &park_store,
            request_id,
            &intent.session_id,
            RECHECK_REASON_LENDING_MARKET_MISMATCH,
            Some(format!(
                "fresh market {} \u{2260} parked market {}",
                decoded.lending_market, intent.lending_market
            )),
        )
        .await;
    }
    let usdc_deposit = decoded
        .deposits
        .iter()
        .find(|d| d.deposit_reserve == intent.reserve_pubkey && d.deposited_amount > 0);
    let fresh_deposited = match usdc_deposit {
        Some(d) => d.deposited_amount,
        None => {
            return emit_recheck_blocked(
                &audit,
                &park_store,
                request_id,
                &intent.session_id,
                RECHECK_REASON_NO_USDC_DEPOSIT,
                None,
            )
            .await;
        }
    };
    if fresh_deposited == 0 {
        return emit_recheck_blocked(
            &audit,
            &park_store,
            request_id,
            &intent.session_id,
            RECHECK_REASON_ZERO_COLLATERAL,
            None,
        )
        .await;
    }
    if decoded
        .borrows
        .iter()
        .any(|b| b.borrowed_amount_wads > 0)
    {
        return emit_recheck_blocked(
            &audit,
            &park_store,
            request_id,
            &intent.session_id,
            RECHECK_REASON_BORROW_APPEARED,
            None,
        )
        .await;
    }

    // 6. Fresh re-check passed. Mark ready for the (deferred) JIT
    //    signing-handoff route. Parked entry is RETAINED here so the
    //    future prepare HTTP handler can look it up by request_id.
    audit
        .append_solend_event(
            AUDIT_EVENT_WITHDRAW_RECHECK_PASSED_READY_FOR_JIT,
            request_id.to_string(),
            Some(intent.session_id.to_string()),
            AuditSeverity::Info,
            serde_json::json!({
                "obligation_pubkey": intent.obligation_pubkey.to_string(),
                "reserve_pubkey": intent.reserve_pubkey.to_string(),
                "lending_market": intent.lending_market.to_string(),
                "fresh_deposited_collateral_raw": fresh_deposited,
                "propose_time_deposited_collateral_raw": intent.propose_time_deposited_collateral_raw,
            }),
        )
        .await;
    SolendWithdrawResumeOutcome::RecheckPassedReadyForJit {
        fresh_deposited_collateral_raw: fresh_deposited,
    }
}

/// Helper: emit `solend_withdraw_recheck_blocked` audit, drop parked
/// entry, return the typed `RecheckBlocked` outcome.
async fn emit_recheck_blocked(
    audit: &Arc<dyn crate::integrations::solend_submit::SolendAuditSink>,
    park_store: &SolendWithdrawAllParkStore,
    request_id: Uuid,
    session_id: &SessionId,
    reason: &'static str,
    detail: Option<String>,
) -> SolendWithdrawResumeOutcome {
    use claw_state_store::audit::AuditSeverity;
    audit
        .append_solend_event(
            AUDIT_EVENT_WITHDRAW_RECHECK_BLOCKED,
            request_id.to_string(),
            Some(session_id.to_string()),
            AuditSeverity::Warning,
            serde_json::json!({
                "reason": reason,
                "detail": detail,
            }),
        )
        .await;
    park_store.remove(&request_id);
    SolendWithdrawResumeOutcome::RecheckBlocked { reason, detail }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lending::{CollateralTokenAmount, ProtocolTag};
    use std::str::FromStr;

    fn fixture_intent(
        approval_request_id: Uuid,
        wallet: Pubkey,
        obligation_pk: Pubkey,
    ) -> ParkedSolendWithdrawAllIntent {
        let lending_market =
            Pubkey::from_str("4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY").unwrap();
        let reserve =
            Pubkey::from_str("BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw").unwrap();
        let mint =
            Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();
        let _ = approval_request_id;
        ParkedSolendWithdrawAllIntent {
            intent_id: Uuid::new_v4(),
            session_id: SessionId::new(),
            session_wallet: wallet,
            obligation_pubkey: obligation_pk,
            lending_market,
            reserve_pubkey: reserve,
            reserve_mint: mint,
            propose_time_deposited_collateral_raw: 3_857_506,
            action: ProposedAction::Withdraw {
                protocol: ProtocolTag::Solend,
                reserve_mint: mint,
                collateral_amount: CollateralTokenAmount::new(u64::MAX),
            },
            proposed_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(300),
        }
    }

    #[tokio::test]
    async fn park_get_remove_roundtrip() {
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let obl = Pubkey::new_unique();
        let _rx = store.park(id, fixture_intent(id, wallet, obl));
        assert_eq!(store.parked_count(), 1);
        let got = store.get(&id).expect("present");
        assert_eq!(got.session_wallet, wallet);
        assert_eq!(got.obligation_pubkey, obl);
        let removed = store.remove(&id).expect("remove returns");
        assert_eq!(removed.obligation_pubkey, obl);
        assert_eq!(store.parked_count(), 0);
    }

    #[tokio::test]
    async fn park_explicit_obligation_pubkey_is_stored_verbatim_not_derived() {
        // Phase 6I-D required test #11.
        // The explicit pubkey from the LLM/user MUST land in the
        // parked intent unchanged. Any future "derive obligation from
        // wallet+seed" logic would surface here as a mismatch.
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let wallet =
            Pubkey::from_str("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW").unwrap();
        let phase6h_obligation =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let intent = fixture_intent(id, wallet, phase6h_obligation);
        let _rx = store.park(id, intent);
        let got = store.get(&id).expect("present");
        assert_eq!(
            got.obligation_pubkey, phase6h_obligation,
            "parked intent must store the explicit Phase 6H obligation pubkey"
        );
        assert_ne!(got.obligation_pubkey, wallet, "obligation must not be the wallet");
    }

    #[tokio::test]
    async fn has_active_for_session_wallet_matches_only_same_pair() {
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let other_wallet = Pubkey::new_unique();
        let other_session = SessionId::new();
        let intent = fixture_intent(id, wallet, Pubkey::new_unique());
        let session = intent.session_id.clone();
        let _rx = store.park(id, intent);
        assert!(store.has_active_for_session_wallet(&session, &wallet));
        assert!(!store.has_active_for_session_wallet(&session, &other_wallet));
        assert!(!store.has_active_for_session_wallet(&other_session, &wallet));
    }

    // ── Phase 6I-E — oneshot signal mechanism ─────────────────────────

    #[tokio::test]
    async fn signal_approved_delivers_state_to_receiver() {
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let rx =
            store.park(id, fixture_intent(id, Pubkey::new_unique(), Pubkey::new_unique()));
        let sent = store.signal(id, ApprovalWorkflowState::Approved);
        assert!(sent, "first signal must be delivered");
        let got = rx.await.expect("receiver should receive state");
        assert_eq!(got, ApprovalWorkflowState::Approved);
    }

    #[tokio::test]
    async fn signal_unknown_id_returns_false() {
        let store = SolendWithdrawAllParkStore::new();
        // Unknown id (e.g. a deposit approval) — graceful no-op.
        let sent = store.signal(Uuid::new_v4(), ApprovalWorkflowState::Approved);
        assert!(!sent);
    }

    #[tokio::test]
    async fn double_signal_same_id_is_idempotent() {
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let _rx =
            store.park(id, fixture_intent(id, Pubkey::new_unique(), Pubkey::new_unique()));
        assert!(store.signal(id, ApprovalWorkflowState::Approved));
        assert!(!store.signal(id, ApprovalWorkflowState::Approved));
    }

    #[tokio::test]
    async fn double_park_replaces_old_entry_and_drops_old_sender() {
        // The new park API silently replaces. Old receiver observes a
        // dropped sender (test asserts the channel is closed).
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let rx_first = store.park(
            id,
            fixture_intent(id, Pubkey::new_unique(), Pubkey::new_unique()),
        );
        let _rx_second = store.park(
            id,
            fixture_intent(id, Pubkey::new_unique(), Pubkey::new_unique()),
        );
        // Old rx — second park dropped its sender.
        assert!(rx_first.await.is_err(), "old receiver must observe dropped sender");
        assert_eq!(store.parked_count(), 1);
    }

    #[tokio::test]
    async fn signal_rejected_and_expired_states_pass_through() {
        let store = SolendWithdrawAllParkStore::new();
        let id_r = Uuid::new_v4();
        let rx_r = store.park(
            id_r,
            fixture_intent(id_r, Pubkey::new_unique(), Pubkey::new_unique()),
        );
        store.signal(id_r, ApprovalWorkflowState::Rejected);
        assert_eq!(rx_r.await.unwrap(), ApprovalWorkflowState::Rejected);

        let id_e = Uuid::new_v4();
        let rx_e = store.park(
            id_e,
            fixture_intent(id_e, Pubkey::new_unique(), Pubkey::new_unique()),
        );
        store.signal(id_e, ApprovalWorkflowState::Expired);
        assert_eq!(rx_e.await.unwrap(), ApprovalWorkflowState::Expired);
    }

    // ── Phase 6I-E — resume task tests ────────────────────────────────

    use crate::integrations::solend::raw::{
        synth_obligation, SolendObligationCollateralRaw, SolendObligationLiquidityRaw,
    };
    use crate::integrations::solend_submit::SolendAuditSink;
    use claw_state_store::audit::AuditSeverity;
    use crate::tools::get_solend_position::SolendPositionReader;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tokio::sync::Mutex as TokioMutex;

    // Test stubs ─────────────────────────────────────────────────────────

    struct StubReader {
        accounts: TokioMutex<HashMap<Pubkey, Vec<u8>>>,
        rpc_err: TokioMutex<Option<String>>,
    }
    impl StubReader {
        fn new() -> Self {
            Self {
                accounts: TokioMutex::new(HashMap::new()),
                rpc_err: TokioMutex::new(None),
            }
        }
        async fn put(&self, pk: Pubkey, data: Vec<u8>) {
            self.accounts.lock().await.insert(pk, data);
        }
        async fn set_err(&self, msg: &str) {
            *self.rpc_err.lock().await = Some(msg.to_string());
        }
    }
    #[async_trait]
    impl SolendPositionReader for StubReader {
        async fn find_obligations_for_owner(
            &self,
            _: &Pubkey,
        ) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
            Ok(Vec::new())
        }
        async fn get_account_data(&self, pk: &Pubkey) -> Result<Option<Vec<u8>>, String> {
            if let Some(e) = self.rpc_err.lock().await.clone() {
                return Err(e);
            }
            Ok(self.accounts.lock().await.get(pk).cloned())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingAudit(
        Arc<std::sync::Mutex<Vec<(&'static str, AuditSeverity, serde_json::Value)>>>,
    );
    #[async_trait]
    impl SolendAuditSink for RecordingAudit {
        async fn append_solend_event(
            &self,
            event_name: &'static str,
            _correlation_id: String,
            _session_id: Option<String>,
            severity: AuditSeverity,
            payload: serde_json::Value,
        ) {
            self.0
                .lock()
                .unwrap()
                .push((event_name, severity, payload));
        }
    }
    impl RecordingAudit {
        fn events(&self) -> Vec<(&'static str, AuditSeverity, serde_json::Value)> {
            self.0.lock().unwrap().clone()
        }
        fn names(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().iter().map(|(n, _, _)| *n).collect()
        }
    }

    fn build_park_intent_with_obligation(
        wallet: Pubkey,
        market: Pubkey,
        reserve: Pubkey,
        mint: Pubkey,
        obligation_pk: Pubkey,
        deposited: u64,
    ) -> ParkedSolendWithdrawAllIntent {
        ParkedSolendWithdrawAllIntent {
            intent_id: Uuid::new_v4(),
            session_id: SessionId::new(),
            session_wallet: wallet,
            obligation_pubkey: obligation_pk,
            lending_market: market,
            reserve_pubkey: reserve,
            reserve_mint: mint,
            propose_time_deposited_collateral_raw: deposited,
            action: ProposedAction::Withdraw {
                protocol: ProtocolTag::Solend,
                reserve_mint: mint,
                collateral_amount: CollateralTokenAmount::new(u64::MAX),
            },
            proposed_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(300),
        }
    }

    /// Synthesise an obligation byte-array matching the parked intent
    /// shape — wallet owner, market, single USDC-reserve deposit.
    fn synth_matching_obligation(
        wallet: Pubkey,
        market: Pubkey,
        reserve: Pubkey,
        deposited: u64,
        borrows: &[SolendObligationLiquidityRaw],
    ) -> Vec<u8> {
        let deposits = [SolendObligationCollateralRaw {
            deposit_reserve: reserve,
            deposited_amount: deposited,
        }];
        synth_obligation(wallet, market, 0, false, &deposits, borrows)
    }

    // Tests ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn resume_rejected_cleans_up_and_returns_rejected() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                100,
            ),
        );
        // Signal Rejected before resume awaits.
        park.signal(id, ApprovalWorkflowState::Rejected);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader.clone() as Arc<dyn SolendPositionReader>,
            audit.clone() as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        assert_eq!(outcome, SolendWithdrawResumeOutcome::Rejected);
        assert_eq!(park.parked_count(), 0);
        // Only the decision event was emitted (no re-check on rejected).
        let names = audit.names();
        assert_eq!(names, vec![AUDIT_EVENT_WITHDRAW_RESUME_DECISION]);
    }

    #[tokio::test]
    async fn resume_expired_cleans_up_and_returns_expired() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                100,
            ),
        );
        park.signal(id, ApprovalWorkflowState::Expired);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader as Arc<dyn SolendPositionReader>,
            audit as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        assert_eq!(outcome, SolendWithdrawResumeOutcome::Expired);
        assert_eq!(park.parked_count(), 0);
    }

    #[tokio::test]
    async fn resume_dropped_receiver_returns_receiver_dropped() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                100,
            ),
        );
        // Drop the parked entry without signaling — sender drops, rx
        // observes Err.
        park.remove(&id);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader as Arc<dyn SolendPositionReader>,
            audit as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        assert_eq!(outcome, SolendWithdrawResumeOutcome::ReceiverDropped);
    }

    #[tokio::test]
    async fn resume_approved_with_passing_recheck_returns_ready_for_jit() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let wallet = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let id = Uuid::new_v4();
        // Park the intent.
        let rx = park.park(
            id,
            build_park_intent_with_obligation(wallet, market, reserve, mint, obligation_pk, 100),
        );
        // Pre-load the reader with matching fresh obligation bytes.
        reader
            .put(
                obligation_pk,
                synth_matching_obligation(wallet, market, reserve, 200, &[]),
            )
            .await;
        park.signal(id, ApprovalWorkflowState::Approved);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader.clone() as Arc<dyn SolendPositionReader>,
            audit.clone() as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        match outcome {
            SolendWithdrawResumeOutcome::RecheckPassedReadyForJit {
                fresh_deposited_collateral_raw,
            } => {
                // Resume saw the fresh on-chain amount (200) — not the
                // propose-time amount (100). This is the drift-detection
                // hook.
                assert_eq!(fresh_deposited_collateral_raw, 200);
            }
            other => panic!("expected RecheckPassedReadyForJit, got {other:?}"),
        }
        // Audit: decision + ready_for_jit. Parked entry is RETAINED so
        // the (deferred) JIT route can look it up.
        let names = audit.names();
        assert_eq!(
            names,
            vec![
                AUDIT_EVENT_WITHDRAW_RESUME_DECISION,
                AUDIT_EVENT_WITHDRAW_RECHECK_PASSED_READY_FOR_JIT,
            ]
        );
        assert_eq!(park.parked_count(), 1, "ready-for-jit retains the parked entry");
    }

    #[tokio::test]
    async fn resume_approved_owner_mismatch_blocks() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let wallet = Pubkey::new_unique();
        let other_owner = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(wallet, market, reserve, mint, obligation_pk, 100),
        );
        // Fresh obligation has a DIFFERENT owner.
        reader
            .put(
                obligation_pk,
                synth_matching_obligation(other_owner, market, reserve, 200, &[]),
            )
            .await;
        park.signal(id, ApprovalWorkflowState::Approved);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader as Arc<dyn SolendPositionReader>,
            audit.clone() as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        match outcome {
            SolendWithdrawResumeOutcome::RecheckBlocked { reason, .. } => {
                assert_eq!(reason, RECHECK_REASON_OWNER_MISMATCH);
            }
            other => panic!("expected RecheckBlocked(owner_mismatch), got {other:?}"),
        }
        assert_eq!(park.parked_count(), 0);
        // Audit names must contain the recheck_blocked event with
        // reason = owner_mismatch.
        let events = audit.events();
        let blocked = events
            .iter()
            .find(|(n, _, _)| *n == AUDIT_EVENT_WITHDRAW_RECHECK_BLOCKED)
            .expect("must emit recheck_blocked");
        assert_eq!(blocked.2["reason"], RECHECK_REASON_OWNER_MISMATCH);
    }

    #[tokio::test]
    async fn resume_approved_borrow_appeared_blocks() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let wallet = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(wallet, market, reserve, mint, obligation_pk, 100),
        );
        // Borrow appeared between propose and approve.
        let borrow = SolendObligationLiquidityRaw {
            borrow_reserve: Pubkey::new_unique(),
            borrowed_amount_wads: 1_000_000_000_000_000_000u128,
        };
        reader
            .put(
                obligation_pk,
                synth_matching_obligation(wallet, market, reserve, 200, &[borrow]),
            )
            .await;
        park.signal(id, ApprovalWorkflowState::Approved);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader as Arc<dyn SolendPositionReader>,
            audit as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        match outcome {
            SolendWithdrawResumeOutcome::RecheckBlocked { reason, .. } => {
                assert_eq!(reason, RECHECK_REASON_BORROW_APPEARED);
            }
            other => panic!("expected RecheckBlocked(borrow_appeared), got {other:?}"),
        }
        assert_eq!(park.parked_count(), 0);
    }

    #[tokio::test]
    async fn resume_approved_obligation_not_found_blocks() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                100,
            ),
        );
        // Reader has no obligation account stored.
        park.signal(id, ApprovalWorkflowState::Approved);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader as Arc<dyn SolendPositionReader>,
            audit as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        match outcome {
            SolendWithdrawResumeOutcome::RecheckBlocked { reason, .. } => {
                assert_eq!(reason, RECHECK_REASON_OBLIGATION_NOT_FOUND);
            }
            other => panic!("expected RecheckBlocked(obligation_not_found), got {other:?}"),
        }
        assert_eq!(park.parked_count(), 0);
    }

    #[tokio::test]
    async fn resume_approved_rpc_error_returns_recheck_fetch_failed() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        reader.set_err("503 Service Unavailable").await;
        let audit = Arc::new(RecordingAudit::default());
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                100,
            ),
        );
        park.signal(id, ApprovalWorkflowState::Approved);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader as Arc<dyn SolendPositionReader>,
            audit.clone() as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        match outcome {
            SolendWithdrawResumeOutcome::RecheckFetchFailed { rpc_error } => {
                assert!(rpc_error.contains("503"));
            }
            other => panic!("expected RecheckFetchFailed, got {other:?}"),
        }
        let names = audit.names();
        assert!(names.contains(&AUDIT_EVENT_WITHDRAW_RECHECK_FETCH_FAILED));
    }

    /// Phase 6I-E required: the explicit obligation pubkey supplied by
    /// the LLM/user MUST be the SAME pubkey the resume task fetches.
    /// Mirrors deposit's "explicit obligation" invariant on the
    /// withdraw side.
    #[tokio::test]
    async fn resume_threads_explicit_obligation_pubkey_through_to_fetch() {
        let park = SolendWithdrawAllParkStore::new();
        let reader = Arc::new(StubReader::new());
        let audit = Arc::new(RecordingAudit::default());
        let wallet =
            Pubkey::from_str("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW").unwrap();
        let market = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let phase6h_obligation =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let id = Uuid::new_v4();
        let rx = park.park(
            id,
            build_park_intent_with_obligation(
                wallet,
                market,
                reserve,
                mint,
                phase6h_obligation,
                3_857_506,
            ),
        );
        // The reader has data for the EXACT explicit obligation pubkey,
        // and NOT for any derived pubkey.
        reader
            .put(
                phase6h_obligation,
                synth_matching_obligation(wallet, market, reserve, 3_857_506, &[]),
            )
            .await;
        park.signal(id, ApprovalWorkflowState::Approved);

        let outcome = run_solend_withdraw_resume_task(
            id,
            park.clone(),
            reader as Arc<dyn SolendPositionReader>,
            audit as Arc<dyn SolendAuditSink>,
            rx,
        )
        .await;
        match outcome {
            SolendWithdrawResumeOutcome::RecheckPassedReadyForJit { .. } => {}
            other => panic!("expected ReadyForJit on explicit pubkey path, got {other:?}"),
        }
    }
}

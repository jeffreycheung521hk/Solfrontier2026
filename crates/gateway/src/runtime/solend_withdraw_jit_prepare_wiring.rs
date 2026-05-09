//! Phase 6I-F — Solend WITHDRAW JIT-prepare handler.
//!
//! Concrete implementation of [`SolendWithdrawJitPrepareHandler`] that
//! turns an `Approved + ParkedSolendWithdrawAllIntent` into a fresh
//! signing handoff at user Sign-with-Phantom click time.
//!
//! # Validation chain (mirrors deposit's prepare with withdraw-shaped checks)
//!
//! 1. **Approval workflow exists** for `approval_request_id`. Missing →
//!    `NotFound` (indistinguishable from cross-session probes).
//! 2. **Workflow's `session_id` matches the path session.** Mismatch →
//!    `NotFound` (intentionally indistinguishable from "missing").
//! 3. **Workflow state is `Approved`.** Non-Approved → `NotApproved`.
//! 4. **Parked withdraw intent exists** in
//!    [`SolendWithdrawAllParkStore`]. Missing (different protocol's
//!    approval, lazy-swept, or never persisted) → `WithdrawIntentMissing`.
//! 5. **Currently bound wallet matches the captured wallet.** Rebind to
//!    a different wallet between approval and prepare invalidates the
//!    plan's signer metas; reject explicitly so the operator can fix
//!    the binding rather than discover a verification failure later in
//!    submit.
//! 6. **Fresh re-fetch + decode + four structural re-checks** against
//!    the EXPLICIT obligation pubkey:
//!       - owner == session wallet
//!       - lending market == captured market
//!       - non-zero USDC-reserve deposit
//!       - no non-zero borrow
//!    Any failure → `RecheckBlocked { reason, detail }`.
//! 7. **Fresh snapshot** assembled by the deposit-side
//!    `SolendSnapshotAssembler`. The withdraw plan assembler reuses
//!    `AssembledSolendDepositSnapshot` because the on-chain reserve /
//!    obligation / ATA facts the withdraw plan needs are identical to
//!    the deposit plan's. Any failure → `SnapshotAssembleFailed`.
//! 8. **Plan assembly** via the (now-production) withdraw plan
//!    assembler. Any failure → `PlanAssemblyFailed`.
//! 9. **Handoff creation** via [`create_withdraw_signing_handoff`] —
//!    fresh blockhash fetched, unsigned tx built, parked in the same
//!    [`SolendSigningStore`] the deposit pipeline uses. Any failure →
//!    `HandoffCreateFailed`.
//!
//! # Forbidden side effects
//!
//! - Does NOT sign. Backend never holds the user's wallet keypair.
//! - Does NOT broadcast. Submit owns broadcast (Phase 6D / 6E).
//! - Does NOT touch the parked withdraw intent on validation-failure
//!   paths. The intent is RETAINED so the operator can rebind / refresh
//!   and try `prepare` again without forcing a re-approval.
//! - Does NOT consume the JIT-ready state on success. Calling `prepare`
//!   again creates a fresh `signing_request_id` with a fresh blockhash;
//!   this is how the frontend recovers from an expired handoff.

use std::sync::Arc;

use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use uuid::Uuid;

use claw_api::state::{
    SolendWithdrawJitPrepareHandler, SolendWithdrawJitPrepareResult,
};
use claw_state_store::audit::AuditSeverity;
use claw_types::approval::ApprovalWorkflowState;
use claw_types::session::SessionId;

use crate::approval_store::ApprovalStore;
use crate::integrations::solend::raw::{decode_obligation, OBLIGATION_LEN};
use crate::integrations::solend::{
    AssembledSolendDepositSnapshot, SolendAssemblyError, SolendSnapshotAssembler,
};
use crate::integrations::solend_signing::{
    create_withdraw_signing_handoff, RecentBlockhashProvider, SigningHandoffError,
    SolendSigningStore,
};
use crate::integrations::solend_submit::SolendAuditSink;
use crate::integrations::solend_withdraw_park::{
    ParkedSolendWithdrawAllIntent, SolendWithdrawAllParkStore,
    RECHECK_REASON_BORROW_APPEARED, RECHECK_REASON_DECODE_FAILED,
    RECHECK_REASON_LENDING_MARKET_MISMATCH, RECHECK_REASON_NO_USDC_DEPOSIT,
    RECHECK_REASON_OBLIGATION_NOT_FOUND, RECHECK_REASON_OWNER_MISMATCH,
    RECHECK_REASON_ZERO_COLLATERAL,
};
use crate::integrations::solend_withdraw_tx_plan::{
    assemble_solend_withdraw_tx_plan, SolendWithdrawTxPlanError,
};
use crate::tools::get_solend_position::SolendPositionReader;
use crate::tools::jupiter_swap::SessionBoundWallet;

/// Audit-event names emitted by the withdraw prepare handler. Stable;
/// downstream dashboards match on these strings.
pub const AUDIT_EVENT_WITHDRAW_PREPARE_STARTED: &str =
    "solend_withdraw_signing_prepare_started";
pub const AUDIT_EVENT_WITHDRAW_PREPARE_BLOCKED: &str =
    "solend_withdraw_signing_prepare_blocked";
pub const AUDIT_EVENT_WITHDRAW_HANDOFF_CREATED: &str =
    "solend_withdraw_signing_handoff_created";

// ── Snapshot seam ──────────────────────────────────────────────────────────

/// Narrow trait the prepare handler depends on. Production implements
/// this on [`SolendSnapshotAssembler`]; tests inject a stub returning a
/// pre-built `AssembledSolendDepositSnapshot`.
///
/// The trait is named for the WITHDRAW path even though it returns the
/// deposit-shaped snapshot — that's because the on-chain facts the
/// withdraw plan needs (reserve, obligation, ATAs, lending market,
/// oracle pubkeys) are identical to what the deposit assembler already
/// produces.
#[async_trait]
pub trait SolendWithdrawFreshSnapshot: Send + Sync + 'static {
    async fn assemble(
        &self,
        session_wallet: Pubkey,
        reserve_mint: Pubkey,
        obligation_pubkey: Pubkey,
    ) -> Result<AssembledSolendDepositSnapshot, SolendAssemblyError>;
}

#[async_trait]
impl SolendWithdrawFreshSnapshot for SolendSnapshotAssembler {
    async fn assemble(
        &self,
        session_wallet: Pubkey,
        reserve_mint: Pubkey,
        obligation_pubkey: Pubkey,
    ) -> Result<AssembledSolendDepositSnapshot, SolendAssemblyError> {
        self.assemble_for_deposit(session_wallet, reserve_mint, obligation_pubkey)
            .await
    }
}

// ── Handler ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GatewaySolendWithdrawJitPrepareHandler {
    pub approval_store: ApprovalStore,
    pub external_wallet: Arc<dyn SessionBoundWallet>,
    pub withdraw_park_store: SolendWithdrawAllParkStore,
    pub obligation_reader: Arc<dyn SolendPositionReader>,
    pub fresh_snapshot: Arc<dyn SolendWithdrawFreshSnapshot>,
    pub signing_store: SolendSigningStore,
    pub blockhash_provider: Arc<dyn RecentBlockhashProvider>,
    pub audit: Arc<dyn SolendAuditSink>,
    pub signing_lease_seconds: u64,
}

impl GatewaySolendWithdrawJitPrepareHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        approval_store: ApprovalStore,
        external_wallet: Arc<dyn SessionBoundWallet>,
        withdraw_park_store: SolendWithdrawAllParkStore,
        obligation_reader: Arc<dyn SolendPositionReader>,
        fresh_snapshot: Arc<dyn SolendWithdrawFreshSnapshot>,
        signing_store: SolendSigningStore,
        blockhash_provider: Arc<dyn RecentBlockhashProvider>,
        audit: Arc<dyn SolendAuditSink>,
        signing_lease_seconds: u64,
    ) -> Self {
        Self {
            approval_store,
            external_wallet,
            withdraw_park_store,
            obligation_reader,
            fresh_snapshot,
            signing_store,
            blockhash_provider,
            audit,
            signing_lease_seconds,
        }
    }

    async fn append_event(
        &self,
        name: &'static str,
        approval_request_id: Uuid,
        session_id: &SessionId,
        severity: AuditSeverity,
        extra: serde_json::Value,
    ) {
        let mut payload = serde_json::json!({
            "approval_request_id": approval_request_id,
            "session_id":          session_id.to_string(),
        });
        if let serde_json::Value::Object(extra_obj) = extra {
            if let serde_json::Value::Object(p) = &mut payload {
                for (k, v) in extra_obj {
                    p.insert(k, v);
                }
            }
        }
        self.audit
            .append_solend_event(
                name,
                approval_request_id.to_string(),
                Some(session_id.to_string()),
                severity,
                payload,
            )
            .await;
    }

    async fn append_blocked(
        &self,
        approval_request_id: Uuid,
        session_id: &SessionId,
        reason: &str,
        extra: serde_json::Value,
    ) {
        let mut payload = serde_json::json!({ "reason": reason });
        if let serde_json::Value::Object(extra_obj) = extra {
            if let serde_json::Value::Object(p) = &mut payload {
                for (k, v) in extra_obj {
                    p.insert(k, v);
                }
            }
        }
        self.append_event(
            AUDIT_EVENT_WITHDRAW_PREPARE_BLOCKED,
            approval_request_id,
            session_id,
            AuditSeverity::Warning,
            payload,
        )
        .await;
    }

    /// Run the validation chain + handoff creation. Pure logic; no
    /// HTTP-layer concerns. The route handler boxes this future via the
    /// trait impl below.
    async fn run(
        &self,
        session_id: SessionId,
        approval_request_id: Uuid,
    ) -> SolendWithdrawJitPrepareResult {
        // 1. Approval workflow lookup.
        let workflow = match self.approval_store.get_workflow(approval_request_id) {
            Some(w) => w,
            None => {
                self.append_blocked(
                    approval_request_id,
                    &session_id,
                    "approval_workflow_missing",
                    serde_json::json!({}),
                )
                .await;
                return SolendWithdrawJitPrepareResult::NotFound;
            }
        };
        if workflow.session_id != session_id {
            self.append_blocked(
                approval_request_id,
                &session_id,
                "session_mismatch",
                serde_json::json!({
                    "workflow_session": workflow.session_id.to_string(),
                }),
            )
            .await;
            return SolendWithdrawJitPrepareResult::NotFound;
        }

        // 2. Workflow must be Approved.
        if workflow.state != ApprovalWorkflowState::Approved {
            let state_label = match workflow.state {
                ApprovalWorkflowState::Pending => "pending",
                ApprovalWorkflowState::Approved => "approved",
                ApprovalWorkflowState::Rejected => "rejected",
                ApprovalWorkflowState::Expired => "expired",
            };
            self.append_blocked(
                approval_request_id,
                &session_id,
                "workflow_not_approved",
                serde_json::json!({ "state": state_label }),
            )
            .await;
            return SolendWithdrawJitPrepareResult::NotApproved {
                state: state_label.to_string(),
            };
        }

        // 3. Parked withdraw intent must exist.
        let intent: ParkedSolendWithdrawAllIntent =
            match self.withdraw_park_store.get(&approval_request_id) {
                Some(i) => i,
                None => {
                    self.append_blocked(
                        approval_request_id,
                        &session_id,
                        "withdraw_intent_missing",
                        serde_json::json!({}),
                    )
                    .await;
                    return SolendWithdrawJitPrepareResult::WithdrawIntentMissing;
                }
            };

        // 4. Currently bound wallet must match the captured wallet.
        let expected_wallet = intent.session_wallet.to_string();
        let bound_wallet = self.external_wallet.session_wallet_pubkey(&session_id);
        if bound_wallet.as_deref() != Some(expected_wallet.as_str()) {
            self.append_blocked(
                approval_request_id,
                &session_id,
                "wallet_mismatch",
                serde_json::json!({
                    "expected": expected_wallet.clone(),
                    "bound":    bound_wallet.clone(),
                }),
            )
            .await;
            return SolendWithdrawJitPrepareResult::WalletMismatch {
                expected: expected_wallet,
                bound: bound_wallet,
            };
        }

        self.append_event(
            AUDIT_EVENT_WITHDRAW_PREPARE_STARTED,
            approval_request_id,
            &session_id,
            AuditSeverity::Info,
            serde_json::json!({
                "obligation_pubkey": intent.obligation_pubkey.to_string(),
                "reserve_pubkey":    intent.reserve_pubkey.to_string(),
            }),
        )
        .await;

        // 5. Fresh re-fetch + decode + four structural re-checks.
        let bytes = match self
            .obligation_reader
            .get_account_data(&intent.obligation_pubkey)
            .await
        {
            Ok(Some(b)) => b,
            Ok(None) => {
                return self
                    .recheck_blocked(
                        approval_request_id,
                        &session_id,
                        RECHECK_REASON_OBLIGATION_NOT_FOUND,
                        None,
                    )
                    .await;
            }
            Err(e) => {
                return self
                    .recheck_blocked(
                        approval_request_id,
                        &session_id,
                        RECHECK_REASON_OBLIGATION_NOT_FOUND,
                        Some(format!("rpc_error: {e}")),
                    )
                    .await;
            }
        };
        if bytes.len() != OBLIGATION_LEN {
            return self
                .recheck_blocked(
                    approval_request_id,
                    &session_id,
                    RECHECK_REASON_DECODE_FAILED,
                    Some(format!(
                        "got {} bytes, expected {}",
                        bytes.len(),
                        OBLIGATION_LEN
                    )),
                )
                .await;
        }
        let decoded = match decode_obligation(&bytes) {
            Ok(d) => d,
            Err(e) => {
                return self
                    .recheck_blocked(
                        approval_request_id,
                        &session_id,
                        RECHECK_REASON_DECODE_FAILED,
                        Some(format!("{e}")),
                    )
                    .await;
            }
        };
        if decoded.owner != intent.session_wallet {
            return self
                .recheck_blocked(
                    approval_request_id,
                    &session_id,
                    RECHECK_REASON_OWNER_MISMATCH,
                    Some(format!(
                        "fresh owner {} != session wallet {}",
                        decoded.owner, intent.session_wallet
                    )),
                )
                .await;
        }
        if decoded.lending_market != intent.lending_market {
            return self
                .recheck_blocked(
                    approval_request_id,
                    &session_id,
                    RECHECK_REASON_LENDING_MARKET_MISMATCH,
                    Some(format!(
                        "fresh market {} != parked market {}",
                        decoded.lending_market, intent.lending_market
                    )),
                )
                .await;
        }
        let usdc_deposit = decoded.deposits.iter().find(|d| {
            d.deposit_reserve == intent.reserve_pubkey && d.deposited_amount > 0
        });
        let _fresh_deposited = match usdc_deposit {
            Some(d) => d.deposited_amount,
            None => {
                return self
                    .recheck_blocked(
                        approval_request_id,
                        &session_id,
                        RECHECK_REASON_NO_USDC_DEPOSIT,
                        None,
                    )
                    .await;
            }
        };
        if decoded
            .borrows
            .iter()
            .any(|b| b.borrowed_amount_wads > 0)
        {
            return self
                .recheck_blocked(
                    approval_request_id,
                    &session_id,
                    RECHECK_REASON_BORROW_APPEARED,
                    None,
                )
                .await;
        }
        if let Some(d) = usdc_deposit {
            if d.deposited_amount == 0 {
                return self
                    .recheck_blocked(
                        approval_request_id,
                        &session_id,
                        RECHECK_REASON_ZERO_COLLATERAL,
                        None,
                    )
                    .await;
            }
        }

        // 6. Fresh snapshot for the plan assembler.
        let fresh_snapshot = match self
            .fresh_snapshot
            .assemble(
                intent.session_wallet,
                intent.reserve_mint,
                intent.obligation_pubkey,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let label = solend_assembly_error_label(&e);
                let msg = format!("{e}");
                self.append_blocked(
                    approval_request_id,
                    &session_id,
                    "snapshot_assemble_failed",
                    serde_json::json!({
                        "error_type": label,
                        "message":    msg.clone(),
                    }),
                )
                .await;
                return SolendWithdrawJitPrepareResult::SnapshotAssembleFailed {
                    error_type: label.to_string(),
                    message: msg,
                };
            }
        };

        // Snapshot must report obligation_exists==true; the resume task
        // already verified this. A drift here would be surprising.
        // Surface as a recheck block so the operator can re-propose.
        if !fresh_snapshot.obligation_exists {
            return self
                .recheck_blocked(
                    approval_request_id,
                    &session_id,
                    RECHECK_REASON_OBLIGATION_NOT_FOUND,
                    Some("snapshot reports obligation_exists=false".into()),
                )
                .await;
        }

        // 7. Plan assembly.
        let plan = match assemble_solend_withdraw_tx_plan(
            approval_request_id,
            &intent.action,
            session_id.clone(),
            intent.session_wallet,
            &fresh_snapshot,
            fresh_snapshot.snapshot.fetched_at.observed_slot.raw(),
        ) {
            Ok(p) => p,
            Err(e) => {
                let label = withdraw_plan_error_label(&e);
                let msg = format!("{e}");
                self.append_blocked(
                    approval_request_id,
                    &session_id,
                    "plan_assembly_failed",
                    serde_json::json!({
                        "error_type": label,
                        "message":    msg.clone(),
                    }),
                )
                .await;
                return SolendWithdrawJitPrepareResult::PlanAssemblyFailed {
                    error_type: label.to_string(),
                    message: msg,
                };
            }
        };

        // 8. Create signing handoff.
        let summary = match create_withdraw_signing_handoff(
            &plan,
            &self.signing_store,
            self.blockhash_provider.as_ref(),
            self.audit.as_ref(),
            self.signing_lease_seconds,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                let label = signing_handoff_error_label(&e);
                let msg = format!("{e}");
                self.append_blocked(
                    approval_request_id,
                    &session_id,
                    "handoff_create_failed",
                    serde_json::json!({
                        "error_type": label,
                        "message":    msg.clone(),
                    }),
                )
                .await;
                return SolendWithdrawJitPrepareResult::HandoffCreateFailed {
                    error_type: label.to_string(),
                    message: msg,
                };
            }
        };

        self.append_event(
            AUDIT_EVENT_WITHDRAW_HANDOFF_CREATED,
            approval_request_id,
            &session_id,
            AuditSeverity::Info,
            serde_json::json!({
                "signing_request_id": summary.signing_request_id,
                "intent_id":          summary.intent_id,
                "session_wallet":     summary.session_wallet.to_string(),
                "verified_slot":      summary.verified_slot,
                "last_valid_block_height": summary.last_valid_block_height,
                "obligation_pubkey":  intent.obligation_pubkey.to_string(),
                "reserve_pubkey":     intent.reserve_pubkey.to_string(),
            }),
        )
        .await;

        SolendWithdrawJitPrepareResult::Ready {
            approval_request_id,
            signing_request_id: summary.signing_request_id,
            session_id: session_id.clone(),
            wallet: expected_wallet,
            obligation_pubkey: intent.obligation_pubkey.to_string(),
            reserve_pubkey: intent.reserve_pubkey.to_string(),
            last_valid_block_height: summary.last_valid_block_height,
            verified_slot: summary.verified_slot,
            expires_at_unix_ms: summary.expires_at.timestamp_millis(),
        }
    }

    async fn recheck_blocked(
        &self,
        approval_request_id: Uuid,
        session_id: &SessionId,
        reason: &'static str,
        detail: Option<String>,
    ) -> SolendWithdrawJitPrepareResult {
        self.append_blocked(
            approval_request_id,
            session_id,
            reason,
            serde_json::json!({ "detail": detail }),
        )
        .await;
        SolendWithdrawJitPrepareResult::RecheckBlocked {
            reason: reason.to_string(),
            detail,
        }
    }
}

fn signing_handoff_error_label(e: &SigningHandoffError) -> &'static str {
    match e {
        SigningHandoffError::MissingTransientObligation => "MissingTransientObligation",
        SigningHandoffError::UnexpectedTransientObligation => "UnexpectedTransientObligation",
        SigningHandoffError::BlockhashFetchFailed(_) => "BlockhashFetchFailed",
        SigningHandoffError::SerializationFailed(_) => "SerializationFailed",
        SigningHandoffError::PartialSignFailed(_) => "PartialSignFailed",
        // Stage 1 Tail Agent I — withdraw path does not enable demo
        // record_intent yet, so these arms cannot fire here. They are
        // exhaustively listed to keep the match total in case a future
        // slice routes the demo through the withdraw JIT prepare too.
        SigningHandoffError::IntentExpired { .. } => "intent_expired",
        SigningHandoffError::TxTooLargeWithRecordIntent { .. } => {
            "tx_too_large_with_record_intent"
        }
    }
}

fn withdraw_plan_error_label(e: &SolendWithdrawTxPlanError) -> &'static str {
    match e {
        SolendWithdrawTxPlanError::UnsupportedProtocol { .. } => "UnsupportedProtocol",
        SolendWithdrawTxPlanError::UnsupportedActionKind => "UnsupportedActionKind",
        SolendWithdrawTxPlanError::ReserveNotInSnapshot { .. } => "ReserveNotInSnapshot",
        SolendWithdrawTxPlanError::ObligationDoesNotExist => "ObligationDoesNotExist",
        SolendWithdrawTxPlanError::WithdrawBuildFailed(_) => "WithdrawBuildFailed",
        SolendWithdrawTxPlanError::InternalConstantParseFailed => "InternalConstantParseFailed",
    }
}

fn solend_assembly_error_label(e: &SolendAssemblyError) -> &'static str {
    // Stable labels; mirrors the deposit-side label table.
    let s = format!("{e:?}");
    // Pull the first identifier before `(` or ` `; this is robust to
    // future variant additions without exposing struct contents.
    let before_paren = s.split('(').next().unwrap_or(&s);
    let first_word = before_paren.split_whitespace().next().unwrap_or("Unknown");
    Box::leak(first_word.to_string().into_boxed_str())
}

#[async_trait]
impl SolendWithdrawJitPrepareHandler for GatewaySolendWithdrawJitPrepareHandler {
    fn prepare(
        &self,
        session_id: &SessionId,
        approval_request_id: Uuid,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = SolendWithdrawJitPrepareResult> + Send + '_>,
    > {
        let this = self.clone();
        let sid = session_id.clone();
        Box::pin(async move { this.run(sid, approval_request_id).await })
    }
}

#[cfg(test)]
mod tests {
    //! Phase 6I-F validation-chain tests. The Ready / handoff-create
    //! paths require either a stub blockhash provider OR live RPC; we
    //! cover the pure-data validation branches here and the wire-shape
    //! contract in `routes/solend_withdraw_jit_signing.rs`.
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use solana_sdk::hash::Hash;
    use solana_sdk::pubkey::Pubkey;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex as TokioMutex;

    use claw_types::approval::{ApprovalRequest, ApprovalWorkflow};
    use claw_types::policy::PolicyVerdict;
    use claw_types::transaction::SimulationResult;

    use crate::external_wallet::ExternalWalletStore;
    use crate::integrations::solend::raw::{
        synth_obligation, synth_reserve, SolendObligationCollateralRaw,
        SolendObligationLiquidityRaw, SOLEND_NULL_ORACLE_SENTINEL_BS58,
    };
    use crate::integrations::solend::{
        AssembledSolendDepositSnapshot, ClawRpcPoolAccountFetcher, SolendAssemblyError,
        SolendSnapshotAssembler,
    };
    use crate::integrations::solend_signing::{BlockhashError, SolendSigningStore};
    use crate::integrations::solend_submit::NullSolendAuditSink;
    use crate::integrations::solend_withdraw_park::{
        ParkedSolendWithdrawAllIntent, SolendWithdrawAllParkStore,
    };
    use crate::lending::{CollateralTokenAmount, ProposedAction, ProtocolTag};

    const USDC_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const LENDING_MARKET_BS58: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";

    fn nil_session() -> SessionId {
        SessionId::from(Uuid::new_v4())
    }

    fn approved_workflow_for(session: &SessionId, request_id: Uuid) -> ApprovalWorkflow {
        let mut w = ApprovalWorkflow::single_stage(request_id, session.clone(), None);
        w.state = claw_types::approval::ApprovalWorkflowState::Approved;
        w
    }

    fn pending_workflow_for(session: &SessionId, request_id: Uuid) -> ApprovalWorkflow {
        ApprovalWorkflow::single_stage(request_id, session.clone(), None)
    }

    fn dummy_request(session: &SessionId, request_id: Uuid) -> ApprovalRequest {
        ApprovalRequest {
            id: request_id,
            session_id: session.clone(),
            transaction_id: Uuid::nil(),
            description: "test".into(),
            policy_verdict: PolicyVerdict::Approved {
                rule_name: "test".into(),
            },
            simulation: SimulationResult {
                success: true,
                error: None,
                logs: vec![],
                compute_units_used: None,
                fee_lamports: None,
                return_data: None,
                account_diffs: vec![],
            },
            requested_at: Utc::now(),
            decided: false,
            required_approver_role: None,
        }
    }

    fn parked_intent_for(
        session: &SessionId,
        wallet: Pubkey,
        obligation: Pubkey,
    ) -> ParkedSolendWithdrawAllIntent {
        use std::str::FromStr;
        let market = Pubkey::from_str(LENDING_MARKET_BS58).unwrap();
        let reserve = Pubkey::from_str(USDC_RESERVE_BS58).unwrap();
        let mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        ParkedSolendWithdrawAllIntent {
            intent_id: Uuid::new_v4(),
            session_id: session.clone(),
            session_wallet: wallet,
            obligation_pubkey: obligation,
            lending_market: market,
            reserve_pubkey: reserve,
            reserve_mint: mint,
            propose_time_deposited_collateral_raw: 3_857_506,
            action: ProposedAction::Withdraw {
                protocol: ProtocolTag::Solend,
                reserve_mint: mint,
                collateral_amount: CollateralTokenAmount::new(u64::MAX),
            },
            proposed_at: Utc::now(),
            expires_at: Utc::now() + ChronoDuration::seconds(300),
        }
    }

    // ── Stubs ───────────────────────────────────────────────────────────

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
    }
    #[async_trait]
    impl SolendPositionReader for StubReader {
        async fn find_obligations_for_owner(
            &self,
            _owner: &Pubkey,
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

    struct UnreachableSnapshot;
    #[async_trait]
    impl SolendWithdrawFreshSnapshot for UnreachableSnapshot {
        async fn assemble(
            &self,
            _: Pubkey,
            _: Pubkey,
            _: Pubkey,
        ) -> Result<AssembledSolendDepositSnapshot, SolendAssemblyError> {
            panic!("snapshot assembler should not be reached on validation-failure paths");
        }
    }

    struct UnreachableBlockhash;
    #[async_trait]
    impl RecentBlockhashProvider for UnreachableBlockhash {
        async fn latest_blockhash(&self) -> Result<(Hash, u64), BlockhashError> {
            panic!("blockhash provider should not be reached on validation-failure paths");
        }
    }

    /// Synthesise an obligation byte-array matching the parked intent.
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

    fn make_handler(
        approval_store: ApprovalStore,
        external_wallet: ExternalWalletStore,
        park: SolendWithdrawAllParkStore,
        reader: Arc<StubReader>,
    ) -> GatewaySolendWithdrawJitPrepareHandler {
        let ew_ref: Arc<dyn SessionBoundWallet> = Arc::new(external_wallet);
        GatewaySolendWithdrawJitPrepareHandler::new(
            approval_store,
            ew_ref,
            park,
            reader as Arc<dyn SolendPositionReader>,
            Arc::new(UnreachableSnapshot) as Arc<dyn SolendWithdrawFreshSnapshot>,
            SolendSigningStore::new(),
            Arc::new(UnreachableBlockhash) as Arc<dyn RecentBlockhashProvider>,
            Arc::new(NullSolendAuditSink) as Arc<dyn SolendAuditSink>,
            120,
        )
    }

    // ── Pure validation-failure paths (no snapshot, no blockhash) ───────

    #[tokio::test]
    async fn prepare_returns_not_found_for_unknown_approval_request_id() {
        let handler = make_handler(
            ApprovalStore::new(),
            ExternalWalletStore::new(),
            SolendWithdrawAllParkStore::new(),
            Arc::new(StubReader::new()),
        );
        let result = handler.run(nil_session(), Uuid::new_v4()).await;
        assert_eq!(result, SolendWithdrawJitPrepareResult::NotFound);
    }

    #[tokio::test]
    async fn prepare_returns_not_found_for_cross_session_probe() {
        let owner_session = nil_session();
        let probe_session = nil_session();
        let request_id = Uuid::new_v4();
        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&owner_session, request_id),
            approved_workflow_for(&owner_session, request_id),
        );
        let handler = make_handler(
            approval_store,
            ExternalWalletStore::new(),
            SolendWithdrawAllParkStore::new(),
            Arc::new(StubReader::new()),
        );
        let result = handler.run(probe_session, request_id).await;
        assert_eq!(result, SolendWithdrawJitPrepareResult::NotFound);
    }

    #[tokio::test]
    async fn prepare_returns_not_approved_when_workflow_pending() {
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            pending_workflow_for(&session, request_id),
        );
        let handler = make_handler(
            approval_store,
            ExternalWalletStore::new(),
            SolendWithdrawAllParkStore::new(),
            Arc::new(StubReader::new()),
        );
        let result = handler.run(session, request_id).await;
        match result {
            SolendWithdrawJitPrepareResult::NotApproved { state } => {
                assert_eq!(state, "pending")
            }
            other => panic!("expected NotApproved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_returns_withdraw_intent_missing_when_approved_but_no_park() {
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );
        let handler = make_handler(
            approval_store,
            ExternalWalletStore::new(),
            SolendWithdrawAllParkStore::new(),
            Arc::new(StubReader::new()),
        );
        let result = handler.run(session, request_id).await;
        assert_eq!(result, SolendWithdrawJitPrepareResult::WithdrawIntentMissing);
    }

    #[tokio::test]
    async fn prepare_returns_wallet_mismatch_when_bound_wallet_differs() {
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let captured_wallet = Pubkey::new_unique();
        let other_wallet = Pubkey::new_unique();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );
        let park = SolendWithdrawAllParkStore::new();
        let _rx = park.park(
            request_id,
            parked_intent_for(&session, captured_wallet, Pubkey::new_unique()),
        );
        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &other_wallet.to_string());

        let handler = make_handler(
            approval_store,
            external_wallet,
            park,
            Arc::new(StubReader::new()),
        );
        let result = handler.run(session, request_id).await;
        match result {
            SolendWithdrawJitPrepareResult::WalletMismatch { expected, bound } => {
                assert_eq!(expected, captured_wallet.to_string());
                assert_eq!(bound, Some(other_wallet.to_string()));
            }
            other => panic!("expected WalletMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_returns_recheck_blocked_when_owner_mismatch_at_prepare() {
        use std::str::FromStr;
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let captured_wallet = Pubkey::new_unique();
        let other_owner = Pubkey::new_unique();
        let obligation = Pubkey::new_unique();
        let market = Pubkey::from_str(LENDING_MARKET_BS58).unwrap();
        let reserve = Pubkey::from_str(USDC_RESERVE_BS58).unwrap();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );
        let park = SolendWithdrawAllParkStore::new();
        let _rx = park.park(
            request_id,
            parked_intent_for(&session, captured_wallet, obligation),
        );
        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &captured_wallet.to_string());
        let reader = Arc::new(StubReader::new());
        // Fresh obligation has DIFFERENT owner.
        reader
            .put(
                obligation,
                synth_matching_obligation(other_owner, market, reserve, 200, &[]),
            )
            .await;

        let handler = make_handler(approval_store, external_wallet, park, reader);
        let result = handler.run(session, request_id).await;
        match result {
            SolendWithdrawJitPrepareResult::RecheckBlocked { reason, .. } => {
                assert_eq!(reason, "owner_mismatch");
            }
            other => panic!("expected RecheckBlocked(owner_mismatch), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_returns_recheck_blocked_when_borrow_appears_at_prepare() {
        use std::str::FromStr;
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let obligation = Pubkey::new_unique();
        let market = Pubkey::from_str(LENDING_MARKET_BS58).unwrap();
        let reserve = Pubkey::from_str(USDC_RESERVE_BS58).unwrap();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );
        let park = SolendWithdrawAllParkStore::new();
        let _rx = park.park(request_id, parked_intent_for(&session, wallet, obligation));
        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &wallet.to_string());
        let reader = Arc::new(StubReader::new());
        let borrow = SolendObligationLiquidityRaw {
            borrow_reserve: Pubkey::new_unique(),
            borrowed_amount_wads: 1_000_000_000_000_000_000u128,
        };
        reader
            .put(
                obligation,
                synth_matching_obligation(wallet, market, reserve, 200, &[borrow]),
            )
            .await;

        let handler = make_handler(approval_store, external_wallet, park, reader);
        let result = handler.run(session, request_id).await;
        match result {
            SolendWithdrawJitPrepareResult::RecheckBlocked { reason, .. } => {
                assert_eq!(reason, "borrow_appeared");
            }
            other => panic!("expected RecheckBlocked(borrow_appeared), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_returns_recheck_blocked_when_no_usdc_deposit_at_prepare() {
        use std::str::FromStr;
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let obligation = Pubkey::new_unique();
        let market = Pubkey::from_str(LENDING_MARKET_BS58).unwrap();
        let other_reserve = Pubkey::new_unique();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );
        let park = SolendWithdrawAllParkStore::new();
        let _rx = park.park(request_id, parked_intent_for(&session, wallet, obligation));
        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &wallet.to_string());
        // Fresh obligation is on a different reserve — no USDC deposit.
        let reader = Arc::new(StubReader::new());
        reader
            .put(
                obligation,
                synth_matching_obligation(wallet, market, other_reserve, 200, &[]),
            )
            .await;

        let handler = make_handler(approval_store, external_wallet, park, reader);
        let result = handler.run(session, request_id).await;
        match result {
            SolendWithdrawJitPrepareResult::RecheckBlocked { reason, .. } => {
                assert_eq!(reason, "no_usdc_deposit");
            }
            other => panic!("expected RecheckBlocked(no_usdc_deposit), got {other:?}"),
        }
    }

    /// Phase 6I-F required: parked withdraw intent is RETAINED on
    /// validation-failure paths so the operator can rebind / retry
    /// without re-approval.
    #[tokio::test]
    async fn parked_withdraw_intent_is_NOT_consumed_by_validation_failures() {
        use std::str::FromStr;
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let other_wallet = Pubkey::new_unique();
        let obligation = Pubkey::new_unique();
        let _market = Pubkey::from_str(LENDING_MARKET_BS58).unwrap();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );
        let park = SolendWithdrawAllParkStore::new();
        let _rx = park.park(request_id, parked_intent_for(&session, wallet, obligation));
        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &other_wallet.to_string());

        let handler = make_handler(
            approval_store,
            external_wallet,
            park.clone(),
            Arc::new(StubReader::new()),
        );
        let r1 = handler.run(session.clone(), request_id).await;
        assert!(matches!(r1, SolendWithdrawJitPrepareResult::WalletMismatch { .. }));
        // Intent must still be parked.
        assert!(park.get(&request_id).is_some());
        // Second call with same mismatch — still WalletMismatch, not
        // WithdrawIntentMissing.
        let r2 = handler.run(session, request_id).await;
        assert!(matches!(r2, SolendWithdrawJitPrepareResult::WalletMismatch { .. }));
    }

    // ── Happy-path test (stub blockhash + stub fresh-snapshot) ───────────

    /// Stub blockhash provider that returns a fixed pair.
    struct StubBlockhash {
        hash: Hash,
        last_valid: u64,
    }
    #[async_trait]
    impl RecentBlockhashProvider for StubBlockhash {
        async fn latest_blockhash(&self) -> Result<(Hash, u64), BlockhashError> {
            Ok((self.hash, self.last_valid))
        }
    }

    /// Stub fresh-snapshot. Reuses the deposit assembler's tests's
    /// fixture-build helpers indirectly: it constructs a fully-populated
    /// `AssembledSolendDepositSnapshot` from raw byte arrays (`synth_*`)
    /// + `ClawRpcPoolAccountFetcher`-equivalent — actually simpler: we
    /// re-use the `solend_withdraw_tx_plan` test fixture pattern
    /// (`fresh_withdraw_assembled`) which is private to that module.
    /// Since we can't import it cross-module, build a minimal snapshot
    /// inline via the public `map_snapshot` API.
    struct StubSnapshot {
        snapshot: StdMutex<Option<AssembledSolendDepositSnapshot>>,
    }
    #[async_trait]
    impl SolendWithdrawFreshSnapshot for StubSnapshot {
        async fn assemble(
            &self,
            _: Pubkey,
            _: Pubkey,
            _: Pubkey,
        ) -> Result<AssembledSolendDepositSnapshot, SolendAssemblyError> {
            self.snapshot.lock().unwrap().take().ok_or(
                SolendAssemblyError::RpcFetchFailed {
                    reason: "stub snapshot already consumed".to_string(),
                },
            )
        }
    }

    fn build_fresh_snapshot(
        owner: Pubkey,
        reserve_mint: Pubkey,
        market: Pubkey,
        obligation_pk: Pubkey,
        deposited: u64,
    ) -> AssembledSolendDepositSnapshot {
        use crate::integrations::solend::mapping::{
            map_snapshot, OracleAccountInfo, ReserveInput, SolendAssemblyInputs,
        };
        use crate::integrations::solend::raw;
        use crate::lending::{ChainSlot, FeedPublishFreshness};
        use std::str::FromStr;

        let liquidity_supply = Pubkey::new_unique();
        let collateral_mint = Pubkey::new_unique();
        let collateral_supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey =
            Pubkey::from_str("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ").unwrap();
        let sentinel: Pubkey = Pubkey::from_str(SOLEND_NULL_ORACLE_SENTINEL_BS58).unwrap();

        let deposits = [SolendObligationCollateralRaw {
            deposit_reserve: Pubkey::new_unique(), // reserve_pubkey synth
            deposited_amount: deposited,
        }];
        let reserve_pk = deposits[0].deposit_reserve;
        let real_deposits = [SolendObligationCollateralRaw {
            deposit_reserve: reserve_pk,
            deposited_amount: deposited,
        }];
        let obl_bytes = synth_obligation(owner, market, 100, false, &real_deposits, &[]);
        let res_bytes = synth_reserve(
            market,
            reserve_mint,
            6,
            liquidity_supply,
            pyth,
            sentinel,
            9_999_999,
            collateral_mint,
            collateral_supply,
            100,
            false,
        );

        let snap = map_snapshot(SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: obligation_pk,
            obligation_raw: raw::decode_obligation(&obl_bytes).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(1_000),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: raw::decode_reserve(&res_bytes).unwrap(),
                fetched_at_slot: ChainSlot::new(1_000),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(1_000),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(1_000)),
            }],
            snapshot_observed_slot: ChainSlot::new(1_000),
        })
        .unwrap();

        AssembledSolendDepositSnapshot {
            snapshot: snap,
            obligation_exists: true,
            source_ata_exists: true,
            collateral_ata_exists: true,
            reserve_pubkey: reserve_pk,
            obligation_pubkey: obligation_pk,
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: market,
        }
    }

    #[tokio::test]
    async fn happy_path_creates_handoff_and_threads_explicit_obligation_pubkey() {
        use std::str::FromStr;
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let obligation =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let market = Pubkey::from_str(LENDING_MARKET_BS58).unwrap();
        let reserve = Pubkey::from_str(USDC_RESERVE_BS58).unwrap();
        let mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );
        let park = SolendWithdrawAllParkStore::new();
        let _rx = park.park(request_id, parked_intent_for(&session, wallet, obligation));
        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &wallet.to_string());
        let reader = Arc::new(StubReader::new());
        reader
            .put(
                obligation,
                synth_matching_obligation(wallet, market, reserve, 3_857_506, &[]),
            )
            .await;

        let snapshot = build_fresh_snapshot(wallet, mint, market, obligation, 3_857_506);
        let stub_snapshot = Arc::new(StubSnapshot {
            snapshot: StdMutex::new(Some(snapshot)),
        });
        let stub_blockhash = Arc::new(StubBlockhash {
            hash: Hash::new_unique(),
            last_valid: 1_234_567,
        });
        let signing_store = SolendSigningStore::new();
        let ew_ref: Arc<dyn SessionBoundWallet> = Arc::new(external_wallet);
        let handler = GatewaySolendWithdrawJitPrepareHandler::new(
            approval_store,
            ew_ref,
            park,
            reader as Arc<dyn SolendPositionReader>,
            stub_snapshot as Arc<dyn SolendWithdrawFreshSnapshot>,
            signing_store.clone(),
            stub_blockhash as Arc<dyn RecentBlockhashProvider>,
            Arc::new(NullSolendAuditSink) as Arc<dyn SolendAuditSink>,
            120,
        );

        let result = handler.run(session, request_id).await;
        match result {
            SolendWithdrawJitPrepareResult::Ready {
                approval_request_id: arid,
                signing_request_id,
                obligation_pubkey,
                reserve_pubkey,
                wallet: w,
                last_valid_block_height,
                ..
            } => {
                assert_eq!(arid, request_id);
                assert_eq!(obligation_pubkey, obligation.to_string());
                assert_eq!(reserve_pubkey, reserve.to_string());
                assert_eq!(w, wallet.to_string());
                assert_eq!(last_valid_block_height, 1_234_567);
                // Signing store must have a parked entry with no
                // backend obligation signature (withdraw never has one).
                let summary = signing_store
                    .summary(signing_request_id)
                    .expect("parked");
                assert!(!summary.obligation_signer_backend_partial);
                assert_eq!(summary.session_wallet, wallet);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// Helper to silence the dead-code warning for test-only fetcher.
    #[allow(dead_code)]
    fn _force_imports(_f: ClawRpcPoolAccountFetcher, _a: SolendSnapshotAssembler) {}
}

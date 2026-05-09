//! Phase 6B Window 2 — Solend JIT-prepare handler.
//!
//! Concrete implementation of [`SolendJitPrepareHandler`] that turns
//! an `Approved + JIT-ready` Solend deposit into a fresh signing
//! handoff at user-click time, instead of at approval time.
//!
//! # Validation chain
//!
//! 1. Approval workflow must exist for `approval_request_id`. Missing
//!    → `NotFound` (indistinguishable from cross-session probes).
//! 2. Workflow's `session_id` must match the path session. Mismatch
//!    → `NotFound` (intentionally indistinguishable from "missing").
//! 3. Workflow state must be `Approved`. Non-Approved → `NotApproved`
//!    with the actual state echoed for caller diagnostics.
//! 4. A JIT-ready entry must exist in the `SolendJitReadyStore` keyed
//!    by `approval_request_id`. Missing (incl. lazy-swept expired) →
//!    `JitReadyMissing`.
//! 5. The session's currently-bound external wallet must match the
//!    one captured at re-check time (in the JIT-ready entry).
//!    Mismatch → `WalletMismatch { expected, bound }`.
//! 6. Call [`create_signing_handoff`] which fetches a FRESH blockhash,
//!    generates a fresh obligation Keypair (if required), and parks
//!    the partial-signed transaction in the `SolendSigningStore`.
//!    `Err` → `HandoffCreateFailed { error_type, message }`.
//!
//! # Retry semantics
//!
//! Calling `prepare` more than once for the same approved request is
//! supported and intentional. Each call generates a fresh
//! `signing_request_id` with a fresh blockhash; the JIT-ready entry is
//! NOT consumed by a successful prepare. This is how the frontend
//! recovers from an expired handoff without forcing the operator to
//! re-approve.
//!
//! No internal retry loop. Each `prepare` call is a single user click.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use claw_types::approval::ApprovalWorkflowState;
use claw_types::session::SessionId;

use claw_api::state::{SolendJitPrepareHandler, SolendJitPrepareResult};

use crate::approval_store::ApprovalStore;
use crate::integrations::solend_jit_ready::SolendJitReadyStore;
use crate::integrations::solend_signing::{
    create_signing_handoff, RecentBlockhashProvider, SigningHandoffError,
    SolendSigningStore,
};
use crate::integrations::solend_submit::{
    SolendAuditSink, AUDIT_EVENT_HANDOFF_CREATED, AUDIT_EVENT_HANDOFF_FAILED,
    AUDIT_EVENT_JIT_PREPARE_REJECTED,
};
use crate::tools::jupiter_swap::SessionBoundWallet;

/// Stable label for a [`SigningHandoffError`]. Mirrors the helper that
/// previously lived in `solend_park.rs`; relocated here in Phase 6B
/// because the prepare route is now the only call site.
fn signing_handoff_error_label(e: &SigningHandoffError) -> &'static str {
    match e {
        SigningHandoffError::MissingTransientObligation => "MissingTransientObligation",
        SigningHandoffError::UnexpectedTransientObligation => "UnexpectedTransientObligation",
        SigningHandoffError::BlockhashFetchFailed(_) => "BlockhashFetchFailed",
        SigningHandoffError::SerializationFailed(_) => "SerializationFailed",
        SigningHandoffError::PartialSignFailed(_) => "PartialSignFailed",
        SigningHandoffError::IntentExpired { .. } => "intent_expired",
        SigningHandoffError::TxTooLargeWithRecordIntent { .. } => {
            "tx_too_large_with_record_intent"
        }
    }
}

/// Concrete prepare handler. Holds the dependencies needed to:
///
/// - look up approval workflow state (`approval_store`)
/// - look up the JIT-ready plan + preflight (`jit_ready_store`)
/// - check the session's currently bound external wallet
///   (`external_wallet`)
/// - park the new partial-signed transaction (`signing_store`)
/// - fetch a fresh blockhash (`blockhash_provider`)
/// - emit append-only audit events (`audit`)
///
/// All dependencies are clones of the shared daemon-lifetime stores;
/// the route handler holds an `Arc<dyn SolendJitPrepareHandler>` that
/// dispatches into this struct.
#[derive(Clone)]
pub struct GatewaySolendJitPrepareHandler {
    pub approval_store: ApprovalStore,
    pub external_wallet: Arc<dyn SessionBoundWallet>,
    pub jit_ready_store: SolendJitReadyStore,
    pub signing_store: SolendSigningStore,
    pub blockhash_provider: Arc<dyn RecentBlockhashProvider>,
    pub audit: Arc<dyn SolendAuditSink>,
    pub signing_lease_seconds: u64,
}

impl GatewaySolendJitPrepareHandler {
    pub fn new(
        approval_store: ApprovalStore,
        external_wallet: Arc<dyn SessionBoundWallet>,
        jit_ready_store: SolendJitReadyStore,
        signing_store: SolendSigningStore,
        blockhash_provider: Arc<dyn RecentBlockhashProvider>,
        audit: Arc<dyn SolendAuditSink>,
        signing_lease_seconds: u64,
    ) -> Self {
        Self {
            approval_store,
            external_wallet,
            jit_ready_store,
            signing_store,
            blockhash_provider,
            audit,
            signing_lease_seconds,
        }
    }

    async fn append_rejection(
        &self,
        approval_request_id: Uuid,
        session_id: &SessionId,
        reason: &str,
        extra: serde_json::Value,
    ) {
        let mut payload = serde_json::json!({
            "approval_request_id": approval_request_id,
            "session_id":          session_id.to_string(),
            "reason":              reason,
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
                AUDIT_EVENT_JIT_PREPARE_REJECTED,
                approval_request_id.to_string(),
                Some(session_id.to_string()),
                claw_state_store::audit::AuditSeverity::Warning,
                payload,
            )
            .await;
    }

    /// Inner async body. Split out so the trait impl just boxes the
    /// future; the body itself reads top-down without trait-level
    /// pin/box noise.
    async fn run(
        &self,
        session_id: SessionId,
        approval_request_id: Uuid,
    ) -> SolendJitPrepareResult {
        // 1. Approval workflow must exist.
        let workflow = match self.approval_store.get_workflow(approval_request_id) {
            Some(w) => w,
            None => {
                self.append_rejection(
                    approval_request_id,
                    &session_id,
                    "approval_workflow_missing",
                    serde_json::json!({}),
                )
                .await;
                return SolendJitPrepareResult::NotFound;
            }
        };

        // 2. Workflow must own this session. Mismatch is collapsed
        //    into NotFound so cross-session probes can't distinguish
        //    "missing" from "wrong session".
        if workflow.session_id != session_id {
            self.append_rejection(
                approval_request_id,
                &session_id,
                "session_mismatch",
                serde_json::json!({
                    "workflow_session": workflow.session_id.to_string(),
                }),
            )
            .await;
            return SolendJitPrepareResult::NotFound;
        }

        // 3. Workflow must be Approved.
        if workflow.state != ApprovalWorkflowState::Approved {
            let state_label = match workflow.state {
                ApprovalWorkflowState::Pending => "pending",
                ApprovalWorkflowState::Approved => "approved",
                ApprovalWorkflowState::Rejected => "rejected",
                ApprovalWorkflowState::Expired => "expired",
            };
            self.append_rejection(
                approval_request_id,
                &session_id,
                "workflow_not_approved",
                serde_json::json!({ "state": state_label }),
            )
            .await;
            return SolendJitPrepareResult::NotApproved {
                state: state_label.to_string(),
            };
        }

        // 4. JIT-ready entry must exist (and not be expired).
        let entry = match self.jit_ready_store.get(approval_request_id) {
            Some(e) => e,
            None => {
                self.append_rejection(
                    approval_request_id,
                    &session_id,
                    "jit_ready_missing",
                    serde_json::json!({}),
                )
                .await;
                return SolendJitPrepareResult::JitReadyMissing;
            }
        };

        // 5. Currently bound wallet must match the one captured at
        //    re-check time. A rebind to a different wallet between
        //    approval and prepare invalidates the plan's signer
        //    metas; reject explicitly so the operator can fix the
        //    binding rather than discover a verification failure
        //    later in the submit pipeline.
        let expected_wallet = entry.session_wallet.to_string();
        let bound_wallet = self.external_wallet.session_wallet_pubkey(&session_id);
        if bound_wallet.as_deref() != Some(expected_wallet.as_str()) {
            self.append_rejection(
                approval_request_id,
                &session_id,
                "wallet_mismatch",
                serde_json::json!({
                    "expected": expected_wallet,
                    "bound":    bound_wallet,
                }),
            )
            .await;
            return SolendJitPrepareResult::WalletMismatch {
                expected: expected_wallet,
                bound: bound_wallet,
            };
        }

        // 6. Create a fresh signing handoff. This step fetches the
        //    fresh blockhash, generates the obligation Keypair, and
        //    parks the partial-signed transaction in the signing
        //    store. Multiple successive `prepare` calls are
        //    intentional and produce distinct signing_request_ids;
        //    the JIT-ready entry is NOT consumed by a successful
        //    prepare so a later call (after expiry) still works.
        // Stage 1 Tail Agent I: future slice will populate the
        // `RecordIntentHandoffOptions` from `entry.canonical_metadata`
        // + `entry.canonical_intent_bytes` + a daemon-passed
        // `RecordIntentDemoConfig` when demo mode is enabled. Today
        // the JIT-ready entry never carries canonical bits (the
        // proposal-side wiring has not been migrated), so we pass
        // `None` and the handoff behaves byte-for-byte identical to
        // pre-Agent-I.
        let summary = match create_signing_handoff(
            &entry.plan,
            &entry.preflight,
            &self.signing_store,
            self.blockhash_provider.as_ref(),
            self.audit.as_ref(),
            self.signing_lease_seconds,
            None,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                let label = signing_handoff_error_label(&e);
                let msg = format!("{e}");
                self.audit
                    .append_solend_event(
                        AUDIT_EVENT_HANDOFF_FAILED,
                        approval_request_id.to_string(),
                        Some(session_id.to_string()),
                        claw_state_store::audit::AuditSeverity::Warning,
                        serde_json::json!({
                            "approval_request_id": approval_request_id,
                            "intent_id":           entry.intent_id,
                            "session_wallet":      entry.session_wallet.to_string(),
                            "verified_slot":       entry.verified_slot,
                            "error_type":          label,
                            "message":             msg,
                        }),
                    )
                    .await;
                return SolendJitPrepareResult::HandoffCreateFailed {
                    error_type: label.to_string(),
                    message: msg,
                };
            }
        };

        // Audit the success.
        self.audit
            .append_solend_event(
                AUDIT_EVENT_HANDOFF_CREATED,
                approval_request_id.to_string(),
                Some(session_id.to_string()),
                claw_state_store::audit::AuditSeverity::Info,
                serde_json::json!({
                    "approval_request_id":     approval_request_id,
                    "signing_request_id":      summary.signing_request_id,
                    "intent_id":               summary.intent_id,
                    "session_wallet":          summary.session_wallet.to_string(),
                    "verified_slot":           summary.verified_slot,
                    "last_valid_block_height": summary.last_valid_block_height,
                    "obligation_signer_backend_partial":
                        summary.obligation_signer_backend_partial,
                }),
            )
            .await;

        SolendJitPrepareResult::Ready {
            approval_request_id,
            signing_request_id: summary.signing_request_id,
            session_id: session_id.clone(),
            wallet: expected_wallet,
            last_valid_block_height: summary.last_valid_block_height,
            verified_slot: summary.verified_slot,
            expires_at_unix_ms: summary.expires_at.timestamp_millis(),
        }
    }
}

#[async_trait]
impl SolendJitPrepareHandler for GatewaySolendJitPrepareHandler {
    fn prepare(
        &self,
        session_id: &SessionId,
        approval_request_id: Uuid,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = SolendJitPrepareResult> + Send + '_>,
    > {
        let this = self.clone();
        let sid = session_id.clone();
        Box::pin(async move { this.run(sid, approval_request_id).await })
    }
}

#[cfg(test)]
mod tests {
    //! Behavioural tests for the validation chain. The Ready /
    //! HandoffCreateFailed paths require live RPC for a real blockhash
    //! and are exercised by the wire-shape tests in
    //! `routes/solend_jit_signing.rs` plus end-to-end integration
    //! tests; here we cover the four pure-data validation branches:
    //!
    //!   - approval missing → `NotFound`
    //!   - cross-session probe → `NotFound` (indistinguishable)
    //!   - approval not yet decided → `NotApproved`
    //!   - approved but no JIT-ready entry → `JitReadyMissing`
    //!   - approved + JIT-ready but bound wallet ≠ captured wallet →
    //!     `WalletMismatch`

    use super::*;
    use chrono::{Duration, Utc};
    use solana_sdk::pubkey::Pubkey;

    use claw_types::approval::{ApprovalRequest, ApprovalWorkflow};
    use claw_types::policy::PolicyVerdict;
    use claw_types::transaction::SimulationResult;

    use crate::external_wallet::ExternalWalletStore;
    use crate::integrations::solend_jit_ready::{SolendJitReady, SolendJitReadyStore};
    use crate::integrations::solend_signing::SolendSigningStore;
    use crate::integrations::solend_submit::NullSolendAuditSink;
    use crate::integrations::solend_tx_plan::{
        SolendDepositAccountsSummary, SolendDepositTxPlan,
    };
    use crate::integrations::solend_preflight::SolendPreflightOutcome;
    use crate::lending::{ProposedAction, ProtocolTag, UnderlyingAmount};

    /// Stub blockhash provider: never called by the validation
    /// branches under test. Returning Err means a test that DOES
    /// reach `create_signing_handoff` would fail loudly — useful
    /// regression guard if a future change accidentally bypasses
    /// validation gates.
    struct UnreachableBlockhash;
    #[async_trait]
    impl RecentBlockhashProvider for UnreachableBlockhash {
        async fn latest_blockhash(
            &self,
        ) -> Result<
            (solana_sdk::hash::Hash, u64),
            crate::integrations::solend_signing::BlockhashError,
        > {
            panic!("blockhash provider should not be reached on validation-failure paths");
        }
    }

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

    fn minimal_plan(wallet: Pubkey) -> SolendDepositTxPlan {
        let usdc_mint = Pubkey::new_unique();
        SolendDepositTxPlan {
            request_id: Uuid::nil(),
            intent_id: Uuid::nil(),
            session_id: nil_session(),
            session_wallet: wallet,
            verified_slot: 0,
            action: ProposedAction::Deposit {
                protocol: ProtocolTag::Solend,
                reserve_mint: usdc_mint,
                amount: UnderlyingAmount::new(1_000),
            },
            setup_instructions: vec![],
            refresh_instructions: vec![],
            deposit_instructions: vec![],
            transient_obligation_pubkey: None,
            obligation_signer_required: false,
            accounts_summary: SolendDepositAccountsSummary {
                reserve_pubkey: Pubkey::new_unique(),
                reserve_mint: usdc_mint,
                obligation_pubkey: Pubkey::new_unique(),
                source_liquidity_ata: Pubkey::new_unique(),
                user_collateral_ata: Pubkey::new_unique(),
                source_ata_exists: true,
                collateral_ata_exists: true,
                obligation_exists: true,
            },
            planning_rent_exempt_lamports: 0,
        }
    }

    fn passed_preflight() -> SolendPreflightOutcome {
        SolendPreflightOutcome::Passed {
            simulation_slot: Some(100),
            units_consumed: Some(20_000),
            verified_slot: 0,
            logs: vec![],
            rent_updated_for_preflight: false,
            planning_rent_lamports: None,
            fresh_rent_lamports: None,
            sig_verify: false,
            replace_recent_blockhash: true,
        }
    }

    fn jit_ready_for(session: &SessionId, wallet: Pubkey) -> SolendJitReady {
        let now = Utc::now();
        SolendJitReady {
            intent_id: Uuid::new_v4(),
            session_id: session.clone(),
            session_wallet: wallet,
            plan: minimal_plan(wallet),
            preflight: passed_preflight(),
            verified_slot: 0,
            created_at: now,
            expires_at: now + Duration::seconds(120),
        }
    }

    fn make_handler(
        approval_store: ApprovalStore,
        external_wallet: ExternalWalletStore,
        jit_ready_store: SolendJitReadyStore,
    ) -> GatewaySolendJitPrepareHandler {
        let ew_ref: Arc<dyn SessionBoundWallet> = Arc::new(external_wallet);
        GatewaySolendJitPrepareHandler::new(
            approval_store,
            ew_ref,
            jit_ready_store,
            SolendSigningStore::new(),
            Arc::new(UnreachableBlockhash),
            Arc::new(NullSolendAuditSink),
            120,
        )
    }

    #[tokio::test]
    async fn prepare_returns_not_found_for_unknown_approval_request_id() {
        let handler = make_handler(
            ApprovalStore::new(),
            ExternalWalletStore::new(),
            SolendJitReadyStore::new(),
        );
        let result = handler.run(nil_session(), Uuid::new_v4()).await;
        assert_eq!(result, SolendJitPrepareResult::NotFound);
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
            SolendJitReadyStore::new(),
        );
        // probe_session != owner_session — must collapse to NotFound,
        // not WalletMismatch / NotApproved / etc.
        let result = handler.run(probe_session, request_id).await;
        assert_eq!(result, SolendJitPrepareResult::NotFound);
    }

    #[tokio::test]
    async fn prepare_returns_not_approved_when_workflow_still_pending() {
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
            SolendJitReadyStore::new(),
        );
        let result = handler.run(session, request_id).await;
        match result {
            SolendJitPrepareResult::NotApproved { state } => {
                assert_eq!(state, "pending");
            }
            other => panic!("expected NotApproved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_returns_jit_ready_missing_when_approved_but_no_entry() {
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
            SolendJitReadyStore::new(),
        );
        let result = handler.run(session, request_id).await;
        assert_eq!(result, SolendJitPrepareResult::JitReadyMissing);
    }

    #[tokio::test]
    async fn prepare_returns_wallet_mismatch_when_bound_wallet_differs() {
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let captured_wallet = Pubkey::new_unique();
        let other_wallet = Pubkey::new_unique();
        assert_ne!(captured_wallet, other_wallet);

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );

        let jit_ready_store = SolendJitReadyStore::new();
        jit_ready_store.put(request_id, jit_ready_for(&session, captured_wallet));

        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &other_wallet.to_string());

        let handler = make_handler(approval_store, external_wallet, jit_ready_store);
        let result = handler.run(session, request_id).await;
        match result {
            SolendJitPrepareResult::WalletMismatch { expected, bound } => {
                assert_eq!(expected, captured_wallet.to_string());
                assert_eq!(bound, Some(other_wallet.to_string()));
            }
            other => panic!("expected WalletMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_returns_wallet_mismatch_when_no_wallet_bound() {
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let captured_wallet = Pubkey::new_unique();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );

        let jit_ready_store = SolendJitReadyStore::new();
        jit_ready_store.put(request_id, jit_ready_for(&session, captured_wallet));

        // External wallet store has no binding for this session.
        let handler = make_handler(
            approval_store,
            ExternalWalletStore::new(),
            jit_ready_store,
        );
        let result = handler.run(session, request_id).await;
        match result {
            SolendJitPrepareResult::WalletMismatch { expected, bound } => {
                assert_eq!(expected, captured_wallet.to_string());
                assert_eq!(bound, None);
            }
            other => panic!("expected WalletMismatch with bound=None, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn jit_ready_entry_is_NOT_consumed_by_validation_failure_paths() {
        // Ensures we can re-attempt prepare after fixing the wallet
        // binding without re-approving — the JIT-ready entry must
        // survive a WalletMismatch outcome.
        let session = nil_session();
        let request_id = Uuid::new_v4();
        let captured_wallet = Pubkey::new_unique();
        let other_wallet = Pubkey::new_unique();

        let approval_store = ApprovalStore::new();
        approval_store.register(
            dummy_request(&session, request_id),
            approved_workflow_for(&session, request_id),
        );

        let jit_ready_store = SolendJitReadyStore::new();
        jit_ready_store.put(request_id, jit_ready_for(&session, captured_wallet));

        let external_wallet = ExternalWalletStore::new();
        external_wallet.bind_wallet(&session, &other_wallet.to_string());

        let handler = make_handler(
            approval_store,
            external_wallet,
            jit_ready_store.clone(),
        );

        // First call → WalletMismatch.
        let r1 = handler.run(session.clone(), request_id).await;
        assert!(matches!(r1, SolendJitPrepareResult::WalletMismatch { .. }));

        // JIT-ready entry must still be present so the operator can
        // rebind and try again.
        assert!(jit_ready_store.get(request_id).is_some());

        // Second call (with same mismatched binding) → still
        // WalletMismatch, not JitReadyMissing.
        let r2 = handler.run(session, request_id).await;
        assert!(matches!(r2, SolendJitPrepareResult::WalletMismatch { .. }));
    }
}

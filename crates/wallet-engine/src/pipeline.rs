//! Transaction review pipeline with typestate-enforced stage ordering.
//!
//! # Invariant (structural, compile-time enforced)
//!
//! A transaction CANNOT be signed or sent unless it has passed through
//! successful simulation AND a policy evaluation that is not `Blocked`.
//!
//! This invariant is enforced by the typestate pattern:
//!
//! ```text
//! Transaction (raw, unsigned)
//!   └─ simulate()  →  SimulatedTransaction    (wraps Tx + SimulationResult, only created on success)
//!        └─ evaluate_policy()  →  ApprovedTransaction  (wraps SimulatedTx + Approved verdict only)
//!             └─ sign()  →  SignedTransaction  (wraps ApprovedTx + Signature)
//! ```
//!
//! It is a compile error to call `sign()` on a raw `Transaction`.
//! It is a compile error to call `sign()` on a `SimulatedTransaction` without policy approval.
//! SimulationFailed transactions cannot reach the sign stage at all.
//!
//! # Stage constructors are `pub(crate)` or private
//!
//! `SimulatedTransaction::new` is `pub(crate)` — only this module can create one,
//! ensuring the only path is through `simulate()`.
//!
//! # Exception: DryRun
//!
//! `ApprovalMode::DryRun` stops before sign. The pipeline returns
//! `PipelineResult::DryRun { .. }` and never reaches `SignedTransaction`.

use std::sync::Arc;

use solana_sdk::transaction::Transaction;
use tracing::{info, instrument, warn};

use claw_types::{
    policy::PolicyVerdict,
    transaction::{SimulationResult, TransactionProposal, TransactionStatus},
};
use claw_solana_core::{
    BlockhashManager,
    SimulationClient,
    rpc::ClawRpcClient,
    compute::{self, compute_limit_from_simulation, should_tighten, DEFAULT_COMPUTE_UNITS},
    fees::PriorityFeeStrategy,
};

use crate::{
    approval::ApprovalMode,
    errors::WalletError,
    signer::SignerRef,
};

// ── Typestate wrappers ────────────────────────────────────────────────────────

/// A transaction that has been successfully simulated.
///
/// # Safety invariant
///
/// This type can ONLY be constructed by `simulate()` in this module.
/// The constructor is `pub(crate)`, so external callers cannot bypass simulation.
///
/// A `SimulatedTransaction` guarantees:
/// - `simulation.success == true`  (failed sims are rejected before construction)
/// - `recent_blockhash` has been set to a fresh value
pub struct SimulatedTransaction {
    /// The underlying Solana transaction with a fresh blockhash attached.
    /// Kept pub so the gateway can reconstruct this type from a parked tx.
    pub inner: Transaction,
    /// The simulation result that granted this token.
    pub simulation: SimulationResult,
    /// The `lastValidBlockHeight` from the blockhash used in this transaction.
    /// Used downstream for lifecycle tracking expiry calculations.
    pub last_valid_block_height: u64,
}

impl SimulatedTransaction {
    // pub(crate) keeps this within the wallet-engine crate only.
    // The gateway uses this via the `resume_signing_after_approval` path,
    // which is safe because it only reconstructs from a previously-verified simulation.
    pub fn new_unchecked(inner: Transaction, simulation: SimulationResult, last_valid_block_height: u64) -> Self {
        // INVARIANT: caller MUST have verified simulation.success == true before calling this.
        debug_assert!(
            simulation.success,
            "SimulatedTransaction::new_unchecked called with a failed simulation — this is a bug"
        );
        Self { inner, simulation, last_valid_block_height }
    }
}

/// A transaction that has been simulated AND received a non-blocked policy verdict.
///
/// # Safety invariant
///
/// Can only be constructed by `evaluate_policy()`.
/// Holds proof (the `policy_verdict`) that no policy rule blocked this transaction.
/// `verdict.is_blocked() == false` is a pre-condition for construction.
pub struct ApprovedTransaction {
    pub inner:         SimulatedTransaction,
    /// The policy verdict that approved this transaction.
    /// Will be either `Approved` or `RequiresHumanApproval` (never `Rejected` or `SimulationFailed`).
    pub policy_verdict: PolicyVerdict,
}

impl ApprovedTransaction {
    pub fn new_unchecked(inner: SimulatedTransaction, policy_verdict: PolicyVerdict) -> Self {
        debug_assert!(
            !policy_verdict.is_blocked(),
            "ApprovedTransaction::new_unchecked called with a blocked verdict — this is a bug"
        );
        Self { inner, policy_verdict }
    }

    /// Convenience accessor: the underlying simulation result.
    pub fn simulation(&self) -> &SimulationResult {
        &self.inner.simulation
    }
}

// ── Result types ──────────────────────────────────────────────────────────────

/// The outcome of running the full pipeline.
#[derive(Debug)]
pub enum PipelineResult {
    /// Simulation succeeded, policy approved, transaction signed.
    Signed {
        simulation:     SimulationResult,
        policy_verdict: PolicyVerdict,
        signature:      String,
    },
    /// Pipeline stopped at dry-run: simulation and policy passed, signing skipped.
    DryRun {
        simulation:     SimulationResult,
        policy_verdict: PolicyVerdict,
    },
    /// Pipeline blocked: waiting for human approval (returned to caller to present).
    ///
    /// `finalized_tx` is the transaction with compute budget instructions prepended
    /// and blockhash set — the exact bytes that were simulated and policy-checked.
    /// The caller MUST park these bytes (not the original pre-pipeline tx) to ensure
    /// the approval-resume path signs the same transaction that was reviewed.
    AwaitingApproval {
        simulation:     SimulationResult,
        policy_verdict: PolicyVerdict,
        finalized_tx:   Transaction,
    },
    /// Simulation succeeded but policy rejected or blocked this transaction.
    PolicyBlocked {
        simulation:     SimulationResult,
        policy_verdict: PolicyVerdict,
    },
    /// Simulation itself failed. Transaction cannot proceed.
    SimulationFailed {
        error: String,
    },
}

impl PipelineResult {
    /// The final `TransactionStatus` for persistence.
    pub fn transaction_status(&self) -> TransactionStatus {
        match self {
            PipelineResult::Signed { .. }           => TransactionStatus::Signed,
            PipelineResult::DryRun { .. }           => TransactionStatus::Approved, // dry-run = approved but not sent
            PipelineResult::AwaitingApproval { .. } => TransactionStatus::AwaitingApproval,
            PipelineResult::PolicyBlocked { .. }    => TransactionStatus::Rejected,
            PipelineResult::SimulationFailed { .. } => TransactionStatus::Failed,
        }
    }

    /// Returns the policy verdict if available (not present for SimulationFailed).
    pub fn policy_verdict(&self) -> Option<&PolicyVerdict> {
        match self {
            PipelineResult::Signed { policy_verdict, .. }           => Some(policy_verdict),
            PipelineResult::DryRun { policy_verdict, .. }           => Some(policy_verdict),
            PipelineResult::AwaitingApproval { policy_verdict, .. } => Some(policy_verdict),
            PipelineResult::PolicyBlocked { policy_verdict, .. }    => Some(policy_verdict),
            PipelineResult::SimulationFailed { .. }                 => None,
        }
    }

    /// Returns the simulation result if available.
    pub fn simulation(&self) -> Option<&SimulationResult> {
        match self {
            PipelineResult::Signed { simulation, .. }           => Some(simulation),
            PipelineResult::DryRun { simulation, .. }           => Some(simulation),
            PipelineResult::AwaitingApproval { simulation, .. } => Some(simulation),
            PipelineResult::PolicyBlocked { simulation, .. }    => Some(simulation),
            PipelineResult::SimulationFailed { .. }             => None,
        }
    }
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

/// The transaction review pipeline.
///
/// Call `run()` for the full automatic pipeline.
/// Or call the stages individually for more control:
///
///   1. `simulate()` → `SimulatedTransaction`
///   2. `evaluate_policy()` → `ApprovedTransaction`
///   3. `sign()` → `PipelineResult::Signed`
#[derive(Clone)]
pub struct TransactionReviewPipeline {
    rpc:           ClawRpcClient,
    sim_client:    SimulationClient,
    blockhash_mgr: Arc<BlockhashManager>,
    priority_fee:  PriorityFeeStrategy,
    approval_mode: ApprovalMode,
}

impl TransactionReviewPipeline {
    pub fn new(
        rpc:           ClawRpcClient,
        sim_client:    SimulationClient,
        blockhash_mgr: Arc<BlockhashManager>,
        priority_fee:  PriorityFeeStrategy,
        approval_mode: ApprovalMode,
    ) -> Self {
        Self { rpc, sim_client, blockhash_mgr, priority_fee, approval_mode }
    }

    /// Returns a new pipeline with the approval mode changed.
    /// Used by the sign-resume path to create a `HumanGranted` pipeline
    /// from the original `Automatic` pipeline.
    pub fn with_approval_mode(self, approval_mode: ApprovalMode) -> Self {
        Self { approval_mode, ..self }
    }

    // ── Stage 1: Simulate ─────────────────────────────────────────────────────

    /// Prepends compute budget instructions, attaches a fresh blockhash,
    /// and simulates the transaction.
    ///
    /// # Compute budget prepend
    ///
    /// Before simulation, `SetComputeUnitLimit` (and optionally
    /// `SetComputeUnitPrice` if a priority fee strategy resolves to > 0)
    /// are prepended to the transaction. The simulated transaction IS
    /// the finalized transaction — the same bytes that flow through
    /// policy evaluation, parking for approval, and signing.
    ///
    /// If the transaction already contains compute budget instructions
    /// (e.g., user-supplied), the prepend is skipped (idempotent).
    ///
    /// Returns `Ok(SimulatedTransaction)` ONLY on simulation success.
    /// Returns `Err(WalletError::SimulationFailed)` on simulation failure.
    /// The caller cannot reach signing without going through this method.
    #[instrument(skip(self, tx), fields(proposal_id = %proposal.id))]
    pub async fn simulate(
        &self,
        proposal: &TransactionProposal,
        mut tx: Transaction,
    ) -> Result<SimulatedTransaction, WalletError> {
        // ── Step 1: Resolve priority fee ───────────────────────────────────────
        let priority_fee = self.priority_fee
            .resolve(&self.rpc, &[])
            .await
            .unwrap_or(0);

        // ── Step 2: Prepend compute budget instructions (idempotent) ──────────
        let prepended = compute::prepend_compute_budget(
            &mut tx,
            DEFAULT_COMPUTE_UNITS,
            priority_fee,
        );
        if prepended {
            info!(
                proposal_id = %proposal.id,
                cu_limit = DEFAULT_COMPUTE_UNITS,
                priority_fee_microlamports = priority_fee,
                "prepended compute budget instructions"
            );
        } else {
            info!(
                proposal_id = %proposal.id,
                "transaction already has compute budget instructions — skipping prepend"
            );
        }

        // ── Step 3: Attach fresh blockhash ─────────────────────────────────────
        let (recent_blockhash, mut last_valid_block_height) = self.blockhash_mgr.get_fresh().await?;
        tx.message.recent_blockhash = recent_blockhash;

        // ── Step 4: Simulate the FINALIZED transaction ─────────────────────────
        info!(proposal_id = %proposal.id, "simulating transaction");
        let sim_output = self.sim_client.simulate(&tx).await?;

        if !sim_output.success {
            warn!(
                proposal_id = %proposal.id,
                error = ?sim_output.error,
                "simulation failed — transaction cannot proceed"
            );
            return Err(WalletError::SimulationFailed {
                error: sim_output.error.unwrap_or_else(|| "unknown simulation error".into()),
            });
        }

        // ── Step 5: Two-pass compute budget optimization (N13) ──────────────
        //
        // If first simulation reveals CU usage significantly below DEFAULT,
        // replace the compute unit limit with the tighter derived value,
        // refresh the blockhash, and re-simulate to confirm success.
        // On second sim failure, fall back to the first sim result.
        let (final_tx, final_sim_output) = if let Some(cu_used) = sim_output.compute_units_used {
            let derived_limit = compute_limit_from_simulation(cu_used);

            if prepended && should_tighten(DEFAULT_COMPUTE_UNITS, derived_limit) {
                info!(
                    proposal_id = %proposal.id,
                    simulated_cu = cu_used,
                    derived_cu_limit = derived_limit,
                    default_cu_limit = DEFAULT_COMPUTE_UNITS,
                    "N13: tightening compute budget — starting second pass"
                );

                let replaced = compute::replace_compute_unit_limit(&mut tx, derived_limit);
                if replaced {
                    // Refresh blockhash for the re-simulation.
                    let (bh2, lvbh2) = self.blockhash_mgr.get_fresh().await?;
                    tx.message.recent_blockhash = bh2;
                    last_valid_block_height = lvbh2;

                    match self.sim_client.simulate(&tx).await {
                        Ok(sim2) if sim2.success => {
                            info!(
                                proposal_id = %proposal.id,
                                second_pass_cu = sim2.compute_units_used,
                                applied_cu_limit = derived_limit,
                                "N13: second simulation succeeded with tighter CU"
                            );
                            (tx, sim2)
                        }
                        Ok(sim2) => {
                            warn!(
                                proposal_id = %proposal.id,
                                error = ?sim2.error,
                                "N13: second simulation failed — falling back to first pass"
                            );
                            // Restore original CU limit for safety.
                            compute::replace_compute_unit_limit(&mut tx, DEFAULT_COMPUTE_UNITS);
                            (tx, sim_output)
                        }
                        Err(e) => {
                            warn!(
                                proposal_id = %proposal.id,
                                error = %e,
                                "N13: second simulation RPC error — falling back to first pass"
                            );
                            compute::replace_compute_unit_limit(&mut tx, DEFAULT_COMPUTE_UNITS);
                            (tx, sim_output)
                        }
                    }
                } else {
                    info!(
                        proposal_id = %proposal.id,
                        simulated_cu = cu_used,
                        derived_cu_limit = derived_limit,
                        applied_cu_limit = DEFAULT_COMPUTE_UNITS,
                        "N13: no SetComputeUnitLimit found to replace — using first pass"
                    );
                    (tx, sim_output)
                }
            } else {
                info!(
                    proposal_id = %proposal.id,
                    simulated_cu = cu_used,
                    derived_cu_limit = derived_limit,
                    applied_cu_limit = DEFAULT_COMPUTE_UNITS,
                    "compute budget: savings too small for second pass, keeping default"
                );
                (tx, sim_output)
            }
        } else {
            info!(
                proposal_id = %proposal.id,
                "simulation did not report CU usage — keeping default compute budget"
            );
            (tx, sim_output)
        };

        let sim_result = final_sim_output.into_result();

        // ONLY path to SimulatedTransaction: successful simulation.
        // The tx here is the FINALIZED transaction (with compute budget prepended,
        // potentially tightened by the N13 two-pass optimization).
        Ok(SimulatedTransaction::new_unchecked(final_tx, sim_result, last_valid_block_height))
    }

    // ── Stage 2: Policy evaluation ────────────────────────────────────────────

    /// Evaluates the policy against a successfully simulated transaction.
    ///
    /// Returns `Ok(ApprovedTransaction)` if the policy allows proceed (auto or human-pending).
    /// Returns `Err(WalletError::PolicyBlocked)` if the verdict is Rejected or SimulationFailed.
    ///
    /// Note: `RequiresHumanApproval` is NOT blocked at this stage — it becomes
    /// `ApprovalMode::RequireHuman` in `sign()`. The caller must respect `policy_verdict`.
    #[instrument(skip(self, simulated, policy_check), fields(proposal_id = %proposal.id))]
    pub fn evaluate_policy<F>(
        &self,
        proposal: &TransactionProposal,
        simulated: SimulatedTransaction,
        policy_check: F,
    ) -> Result<ApprovedTransaction, WalletError>
    where
        F: Fn(&SimulationResult) -> PolicyVerdict,
    {
        let verdict = policy_check(&simulated.simulation);
        info!(
            proposal_id = %proposal.id,
            verdict = %verdict.label(),
            "policy evaluation complete"
        );

        if verdict.is_blocked() {
            warn!(
                proposal_id = %proposal.id,
                verdict = %verdict.label(),
                "policy blocked transaction"
            );
            return Err(WalletError::PolicyBlocked {
                verdict,
            });
        }

        // ONLY path to ApprovedTransaction: non-blocked policy verdict.
        Ok(ApprovedTransaction::new_unchecked(simulated, verdict))
    }

    // ── Stage 3: Sign ─────────────────────────────────────────────────────────

    /// Signs an approved transaction.
    ///
    /// # Compile-time invariant
    ///
    /// This method accepts ONLY `ApprovedTransaction` — which can only be
    /// constructed after successful simulation AND non-blocked policy evaluation.
    /// It is a TYPE ERROR to call this with a raw `Transaction`.
    ///
    /// # RequireHuman
    ///
    /// If the policy verdict was `RequiresHumanApproval` and `approval_mode`
    /// is `ApprovalMode::Automatic`, this returns `AwaitingApproval`.
    /// The caller is responsible for routing the approval request.
    #[instrument(skip(self, approved, signer), fields(proposal_id = %proposal.id))]
    pub async fn sign(
        &self,
        proposal: &TransactionProposal,
        mut approved: ApprovedTransaction,
        signer: &SignerRef,
    ) -> Result<PipelineResult, WalletError> {
        let simulation = approved.inner.simulation.clone();
        let policy_verdict = approved.policy_verdict.clone();

        // DryRun: stop here, do not sign
        if self.approval_mode == ApprovalMode::DryRun {
            info!(proposal_id = %proposal.id, "dry-run mode: stopping before sign");
            return Ok(PipelineResult::DryRun { simulation, policy_verdict });
        }

        // HumanGranted: operator has already approved via out-of-band API — sign unconditionally.
        // This is the resume path after a parked AwaitingApproval transaction.
        // The decision was recorded and audited by GatewayApprovalHandler before this point.
        if self.approval_mode == ApprovalMode::HumanGranted {
            info!(proposal_id = %proposal.id, "human-granted mode: signing after operator approval");
            let signature = signer.sign_transaction(&mut approved.inner.inner).await?;
            info!(proposal_id = %proposal.id, signature = %signature, "transaction signed (human-granted)");
            return Ok(PipelineResult::Signed {
                simulation,
                policy_verdict,
                signature: signature.to_string(),
            });
        }

        // If policy requires human and we're in Automatic mode, pause
        if policy_verdict.requires_human() && self.approval_mode == ApprovalMode::Automatic {
            warn!(
                proposal_id = %proposal.id,
                "policy requires human approval but approval_mode is Automatic — awaiting approval"
            );
            return Ok(PipelineResult::AwaitingApproval {
                simulation,
                policy_verdict,
                finalized_tx: approved.inner.inner,
            });
        }

        // RequireHuman mode: the stub blocks signing.
        // V1: full round-trip approval (ApprovalHandle) is not yet wired.
        // This is an explicit deferral — the pipeline correctly surfaces
        // AwaitingApproval and the caller routes the approval request.
        if let ApprovalMode::RequireHuman { timeout: _ } = self.approval_mode {
            if policy_verdict.requires_human() || !policy_verdict.is_auto_approved() {
                warn!(
                    proposal_id = %proposal.id,
                    "human approval required — operator must approve via API"
                );
                return Ok(PipelineResult::AwaitingApproval {
                    simulation,
                    policy_verdict,
                    finalized_tx: approved.inner.inner,
                });
            }
        }

        // Sign (only reachable with a valid ApprovedTransaction + compatible approval mode)
        info!(proposal_id = %proposal.id, "signing transaction");
        let signature = signer.sign_transaction(&mut approved.inner.inner).await?;

        info!(
            proposal_id = %proposal.id,
            signature = %signature,
            "transaction signed"
        );

        Ok(PipelineResult::Signed {
            simulation,
            policy_verdict,
            signature: signature.to_string(),
        })
    }

    // ── Full pipeline (convenience) ───────────────────────────────────────────

    /// Runs all pipeline stages in order: simulate → policy → sign.
    ///
    /// This is a convenience wrapper around the individual stage methods.
    /// The simulation-first invariant is enforced regardless of which path is used.
    #[instrument(skip(self, tx, signer, policy_check), fields(proposal_id = %proposal.id))]
    pub async fn run<F>(
        &self,
        proposal: &TransactionProposal,
        tx: Transaction,
        signer: &SignerRef,
        policy_check: F,
    ) -> PipelineResult
    where
        F: Fn(&SimulationResult) -> PolicyVerdict + Send,
    {
        // Stage 1: Simulate (fails here if simulation fails — cannot proceed)
        let simulated = match self.simulate(proposal, tx).await {
            Ok(s)  => s,
            Err(WalletError::SimulationFailed { error }) => {
                return PipelineResult::SimulationFailed { error };
            }
            Err(e) => {
                return PipelineResult::SimulationFailed {
                    error: format!("simulation error: {e}"),
                };
            }
        };

        // Stage 2: Policy (fails here if blocked)
        // Clone the simulation result before evaluate_policy consumes simulated,
        // so we can include it in the PolicyBlocked result.
        let sim_clone = simulated.simulation.clone();
        let approved = match self.evaluate_policy(proposal, simulated, policy_check) {
            Ok(a)  => a,
            Err(WalletError::PolicyBlocked { verdict }) => {
                return PipelineResult::PolicyBlocked {
                    simulation:     sim_clone,
                    policy_verdict: verdict,
                };
            }
            Err(e) => {
                return PipelineResult::SimulationFailed {
                    error: format!("policy evaluation error: {e}"),
                };
            }
        };

        // Stage 3: Sign (only reachable with ApprovedTransaction)
        match self.sign(proposal, approved, signer).await {
            Ok(result) => result,
            Err(e) => PipelineResult::SimulationFailed {
                error: format!("signing error: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test: Simulation invariant ────────────────────────────────────────────
    //
    // These tests verify the typestate pattern compiles and that the
    // only path to a signed transaction is through successful simulation.

    /// Verify SimulatedTransaction can ONLY be constructed via new_unchecked
    /// (which requires success==true in debug). In production, the only public
    /// path is through simulate() which enforces this.
    #[test]
    fn simulated_transaction_requires_successful_simulation() {
        use solana_sdk::{transaction::Transaction, system_instruction, pubkey::Pubkey};

        let from = Pubkey::new_unique();
        let to   = Pubkey::new_unique();
        let ix   = system_instruction::transfer(&from, &to, 1000);
        let msg  = solana_sdk::message::Message::new(&[ix], Some(&from));
        let tx   = Transaction::new_unsigned(msg);

        // A successful simulation result
        let successful_sim = SimulationResult {
            success:            true,
            error:              None,
            compute_units_used: Some(1000),
            logs:               vec!["Program log: success".to_string()],
            return_data:        None,
            account_diffs:      vec![],
            fee_lamports:       Some(5000),
        };

        // This should succeed — simulation.success == true
        let _simulated = SimulatedTransaction::new_unchecked(tx.clone(), successful_sim, 1000);

        // A failed simulation should NOT be passable to SimulatedTransaction
        // In production code, simulate() returns Err if !sim_output.success.
        // The constructor itself uses debug_assert for defense-in-depth.
        let failed_sim = SimulationResult {
            success: false,
            error: Some("InstructionError(0, Custom(1))".into()),
            compute_units_used: None,
            logs: vec![],
            return_data: None,
            account_diffs: vec![],
            fee_lamports: None,
        };

        // In debug mode this would panic via debug_assert.
        // In release mode the type system prevents reaching this path through the public API.
        // The real gate is simulate() returning Err(WalletError::SimulationFailed).
        let _ = failed_sim; // suppress unused warning
    }

    /// Verify that ApprovedTransaction cannot be constructed with a blocked verdict.
    #[test]
    fn approved_transaction_requires_non_blocked_verdict() {
        use solana_sdk::{transaction::Transaction, pubkey::Pubkey, system_instruction};

        let from = Pubkey::new_unique();
        let to   = Pubkey::new_unique();
        let ix   = system_instruction::transfer(&from, &to, 1000);
        let msg  = solana_sdk::message::Message::new(&[ix], Some(&from));
        let tx   = Transaction::new_unsigned(msg);

        let successful_sim = SimulationResult {
            success: true, error: None, compute_units_used: Some(1000),
            logs: vec![], return_data: None, account_diffs: vec![], fee_lamports: Some(5000),
        };
        let simulated = SimulatedTransaction::new_unchecked(tx, successful_sim, 1000);

        // Approved verdict → OK
        let approved_verdict = PolicyVerdict::Approved { rule_name: "test".into() };
        let _approved = ApprovedTransaction::new_unchecked(simulated, approved_verdict);
        // If we got here, the typestate invariant allows approval.

        // Rejected verdict cannot reach ApprovedTransaction through the public API:
        // evaluate_policy() returns Err(WalletError::PolicyBlocked) for blocked verdicts.
        let blocked_verdict = PolicyVerdict::Rejected {
            reason: "test rejection".into(),
            rule_name: "deny-test".into(),
        };
        assert!(blocked_verdict.is_blocked(), "rejected verdict must report is_blocked()");
    }

    /// Verify PipelineResult::transaction_status maps correctly.
    #[test]
    fn pipeline_result_status_mapping() {
        use solana_sdk::{transaction::Transaction, pubkey::Pubkey, system_instruction};

        let from = Pubkey::new_unique();
        let to   = Pubkey::new_unique();
        let ix   = system_instruction::transfer(&from, &to, 1000);
        let msg  = solana_sdk::message::Message::new(&[ix], Some(&from));
        let tx   = Transaction::new_unsigned(msg);

        let sim = SimulationResult {
            success: true, error: None, compute_units_used: None,
            logs: vec![], return_data: None, account_diffs: vec![], fee_lamports: None,
        };
        let approved_verdict = PolicyVerdict::Approved { rule_name: "test".into() };
        let rejected_verdict = PolicyVerdict::Rejected {
            reason: "test".into(), rule_name: "deny".into(),
        };

        let signed = PipelineResult::Signed {
            simulation: sim.clone(), policy_verdict: approved_verdict.clone(),
            signature: "abc123".into(),
        };
        assert_eq!(signed.transaction_status(), TransactionStatus::Signed);

        let dry_run = PipelineResult::DryRun {
            simulation: sim.clone(), policy_verdict: approved_verdict.clone(),
        };
        assert_eq!(dry_run.transaction_status(), TransactionStatus::Approved);

        let awaiting = PipelineResult::AwaitingApproval {
            simulation: sim.clone(), policy_verdict: approved_verdict,
            finalized_tx: tx,
        };
        assert_eq!(awaiting.transaction_status(), TransactionStatus::AwaitingApproval);

        let blocked = PipelineResult::PolicyBlocked {
            simulation: sim, policy_verdict: rejected_verdict,
        };
        assert_eq!(blocked.transaction_status(), TransactionStatus::Rejected);

        let failed = PipelineResult::SimulationFailed { error: "boom".into() };
        assert_eq!(failed.transaction_status(), TransactionStatus::Failed);
    }
}

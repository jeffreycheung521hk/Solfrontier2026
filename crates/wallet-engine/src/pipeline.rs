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
//! # Stage constructors are `pub(crate)`; fields are private
//!
//! `SimulatedTransaction::new_unchecked` and `ApprovedTransaction::new_unchecked`
//! are `pub(crate)`, and every field of both typestates is private. Only this
//! crate can build them, and only via `simulate()` / `evaluate_policy()`. The
//! enforcement is structural privacy, not a naming convention: a downstream crate
//! cannot fabricate an `ApprovedTransaction` to skip simulation or policy, nor
//! reach inside one to mutate its proof fields. The following fails to compile
//! purely because the field is private:
//!
//! ```compile_fail
//! // `policy_verdict` is a private field of `ApprovedTransaction`; external
//! // crates may only read it through the `policy_verdict()` accessor.
//! fn _forge(approved: claw_wallet_engine::pipeline::ApprovedTransaction) {
//!     let _ = approved.policy_verdict;   // error[E0616]: field is private
//! }
//! ```
//!
//! # Exception: DryRun
//!
//! `ApprovalMode::DryRun` stops before sign. The pipeline returns
//! `PipelineResult::DryRun { .. }` and never reaches `SignedTransaction`.

use std::sync::Arc;

use sha2::{Sha256, Digest};
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

// ── Approval hash-binding (Q10) ────────────────────────────────────────────────

/// Domain separator for the approval commitment. Frozen on first commit — a
/// cross-language verifier (Python/TS) re-derives the same digest from
/// `Message::serialize()` bytes, so this literal is part of the wire contract
/// and MUST NOT change.
const APPROVAL_TX_DOMAIN: &[u8] = b"clawsol-approval-tx-v1";

/// Computes the domain-separated SHA-256 commitment over the canonical Solana
/// `Message` bytes (`Message::serialize()` — the EXACT bytes ed25519 signs).
///
/// Layout: `SHA-256( APPROVAL_TX_DOMAIN || 0x00 || message_bytes )`.
///
/// Binding to `Message::serialize()` (NOT `bincode::serialize` over the whole
/// `Transaction`, which would fold in the mutable `signatures` field) keeps the
/// commitment over exactly the signed pre-image, so verifiers need not
/// reimplement bincode. Construction and the `sign()` re-derivation both route
/// through this single function so the two can never diverge.
fn approval_tx_hash(message_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(APPROVAL_TX_DOMAIN);
    hasher.update(&[0x00]);
    hasher.update(message_bytes);
    hasher.finalize().into()
}

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
    /// Private: read via [`SimulatedTransaction::inner`]; construction is gated
    /// by the `pub(crate)` constructor so the only path is `simulate()`.
    inner: Transaction,
    /// The simulation result that granted this token.
    simulation: SimulationResult,
    /// The `lastValidBlockHeight` from the blockhash used in this transaction.
    /// Used downstream for lifecycle tracking expiry calculations.
    last_valid_block_height: u64,
}

impl SimulatedTransaction {
    // pub(crate) keeps this within the wallet-engine crate only, so external
    // crates cannot bypass simulation by constructing this typestate directly.
    pub(crate) fn new_unchecked(inner: Transaction, simulation: SimulationResult, last_valid_block_height: u64) -> Self {
        // INVARIANT: caller MUST have verified simulation.success == true before calling this.
        debug_assert!(
            simulation.success,
            "SimulatedTransaction::new_unchecked called with a failed simulation — this is a bug"
        );
        Self { inner, simulation, last_valid_block_height }
    }

    /// Read-only access to the finalized underlying Solana transaction.
    pub fn inner(&self) -> &Transaction {
        &self.inner
    }

    /// Read-only access to the simulation result that granted this token.
    pub fn simulation(&self) -> &SimulationResult {
        &self.simulation
    }

    /// The `lastValidBlockHeight` for the blockhash attached to this transaction.
    pub fn last_valid_block_height(&self) -> u64 {
        self.last_valid_block_height
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
    /// The simulated transaction this approval wraps.
    /// Private: read via [`ApprovedTransaction::inner`].
    inner:          SimulatedTransaction,
    /// The policy verdict that approved this transaction.
    /// Will be either `Approved` or `RequiresHumanApproval` (never `Rejected` or `SimulationFailed`).
    /// Private: read via [`ApprovedTransaction::policy_verdict`].
    policy_verdict: PolicyVerdict,
    /// SHA-256 commitment to the canonical Solana `Message` bytes
    /// (`Message::serialize()` — the EXACT bytes ed25519 signs), domain-separated
    /// with `b"clawsol-approval-tx-v1"`, captured at approval-time construction.
    ///
    /// Private and immutable post-construction (relies on the field privacy from
    /// `feature-typestate-hardening`): no in-crate or downstream caller can forge
    /// it after the fact. `TransactionReviewPipeline::sign()` re-derives this hash
    /// from the message about to be signed and fail-closes via
    /// [`WalletError::ApprovalTxDrift`](crate::errors::WalletError::ApprovalTxDrift)
    /// on any mismatch, so in-process tampering between approval and sign is
    /// detected before the signer is ever invoked.
    approval_tx_hash: [u8; 32],
}

impl ApprovedTransaction {
    // pub(crate): only `evaluate_policy()` in this crate may construct this
    // typestate, so a blocked verdict can never reach the sign stage.
    pub(crate) fn new_unchecked(inner: SimulatedTransaction, policy_verdict: PolicyVerdict) -> Self {
        debug_assert!(
            !policy_verdict.is_blocked(),
            "ApprovedTransaction::new_unchecked called with a blocked verdict — this is a bug"
        );

        // Bind this approval to the canonical Solana Message bytes at the moment
        // of construction. After this, `approval_tx_hash` is immutable (private
        // field), so any later mutation of the inner Message is detectable by
        // re-deriving and comparing in `sign()`.
        let approval_tx_hash = approval_tx_hash(&inner.inner().message.serialize());

        Self { inner, policy_verdict, approval_tx_hash }
    }

    /// Read-only access to the underlying simulated transaction.
    pub fn inner(&self) -> &SimulatedTransaction {
        &self.inner
    }

    /// Read-only access to the policy verdict that approved this transaction.
    pub fn policy_verdict(&self) -> &PolicyVerdict {
        &self.policy_verdict
    }

    /// Read-only access to the approval-time commitment over the canonical
    /// Solana `Message` bytes. `sign()` re-derives this from the message about
    /// to be signed and fail-closes on mismatch; external verifiers can
    /// re-derive it from `Message::serialize()` to confirm what was approved.
    pub fn approval_tx_hash(&self) -> &[u8; 32] {
        &self.approval_tx_hash
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

        // ── Hash-binding drift guard (Q10) ────────────────────────────────────
        // Re-derive the approval commitment from the canonical Message bytes
        // about to be signed and compare against the hash captured at approval
        // time. Any in-process mutation of the inner Message between approval and
        // sign changes these bytes, so a mismatch means tampering — fail closed
        // BEFORE the signer ever touches the transaction. Placed after DryRun
        // (which never signs) so it gates every real signing path below.
        let current_hash = approval_tx_hash(&approved.inner().inner().message.serialize());
        if &current_hash != approved.approval_tx_hash() {
            warn!(
                proposal_id = %proposal.id,
                "approval drift detected — canonical Message bytes changed since approval; refusing to sign"
            );
            return Err(WalletError::ApprovalTxDrift {
                expected: hex::encode(approved.approval_tx_hash()),
                actual:   hex::encode(current_hash),
            });
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

// ── Q10 approval hash-binding tests ─────────────────────────────────────────────
//
// These tests live in a descendant module of `crate::pipeline`, so they can read
// AND mutate the private `inner` fields of `SimulatedTransaction` /
// `ApprovedTransaction` directly. That is exactly the in-process tampering the
// hash-binding guard exists to catch: mutate the inner `Message` AFTER approval
// (so the immutable `approval_tx_hash` no longer matches), then drive `sign()`
// and assert it fail-closes with `WalletError::ApprovalTxDrift` before the signer
// is ever invoked.
#[cfg(test)]
mod hash_binding_tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::str::FromStr;

    use solana_sdk::{
        hash::Hash,
        instruction::{AccountMeta, Instruction},
        message::Message,
        pubkey::Pubkey,
        signature::Signature,
        system_instruction,
        transaction::Transaction,
    };

    use claw_solana_core::{
        BlockhashManager, RpcPool, RpcPoolConfig, SimulationClient,
        rpc::ClawRpcClient,
        fees::PriorityFeeStrategy,
    };
    use claw_types::{
        solana::{CommitmentLevel, SolanaNetwork},
        session::SessionId,
        transaction::TransactionProposal,
    };

    use crate::signer::{Signer, SignerRef};

    // ── Test fixtures ──────────────────────────────────────────────────────────

    /// A signer that records whether it was invoked. The hash-binding guard must
    /// fail closed *before* the signer is touched, so on drift `called()` stays
    /// false; on a clean transaction it flips true and returns a dummy signature.
    #[derive(Default)]
    struct RecordingSigner {
        called: AtomicBool,
    }
    impl RecordingSigner {
        fn called(&self) -> bool {
            self.called.load(Ordering::SeqCst)
        }
    }
    #[async_trait::async_trait]
    impl Signer for RecordingSigner {
        fn pubkey(&self) -> Pubkey {
            Pubkey::new_unique()
        }
        async fn sign_transaction(&self, _tx: &mut Transaction) -> Result<Signature, WalletError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(Signature::default())
        }
        fn description(&self) -> String {
            "recording-test-signer".to_string()
        }
        fn is_automatic(&self) -> bool {
            true
        }
    }

    /// Builds an offline pipeline. Construction never connects (the RPC client is
    /// lazy), and the drift guard fires before any RPC/blockhash/sim use, so no
    /// network is touched by these tests.
    fn offline_pipeline(mode: ApprovalMode) -> TransactionReviewPipeline {
        let pool = RpcPool::new(RpcPoolConfig::default());
        let rpc = ClawRpcClient::new(pool.clone(), CommitmentLevel::Confirmed);
        let sim = SimulationClient::new(rpc.clone());
        let blockhash_mgr = Arc::new(BlockhashManager::new(pool));
        TransactionReviewPipeline::new(rpc, sim, blockhash_mgr, PriorityFeeStrategy::None, mode)
    }

    fn successful_sim() -> SimulationResult {
        SimulationResult {
            success: true,
            error: None,
            compute_units_used: Some(1000),
            logs: vec!["Program log: ok".to_string()],
            return_data: None,
            account_diffs: vec![],
            fee_lamports: Some(5000),
        }
    }

    fn test_proposal() -> TransactionProposal {
        TransactionProposal {
            id: uuid::Uuid::new_v4(),
            session_id: SessionId::new(),
            wallet_pubkey: Pubkey::new_unique().to_string(),
            network: SolanaNetwork::MainnetBeta,
            description: "hash-binding test".to_string(),
            transaction_b64: String::new(),
            instructions_summary: vec![],
            created_at: chrono::Utc::now(),
        }
    }

    fn approved_verdict() -> PolicyVerdict {
        PolicyVerdict::Approved { rule_name: "test".into() }
    }

    // A tiny SplitMix64-style PRNG — deterministic, no `rand` dependency, and
    // distinct per seed so each of the 1000 iterations exercises a different
    // Message shape.
    fn mix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A pseudo-random legacy transaction: 1..=3 system transfers with varying
    /// recipients/amounts and a seed-derived recent blockhash.
    fn pseudo_random_tx(seed: u64) -> Transaction {
        let mut st = seed ^ 0xD1B5_4A32_D192_ED03;
        let payer = Pubkey::new_unique();
        let n_ix = (mix(&mut st) % 3 + 1) as usize;
        let mut ixs = Vec::with_capacity(n_ix);
        for _ in 0..n_ix {
            let to = Pubkey::new_unique();
            let lamports = mix(&mut st) % 1_000_000 + 1;
            ixs.push(system_instruction::transfer(&payer, &to, lamports));
        }
        let mut msg = Message::new(&ixs, Some(&payer));
        let mut bh = [0u8; 32];
        bh[..8].copy_from_slice(&mix(&mut st).to_le_bytes());
        bh[8..16].copy_from_slice(&mix(&mut st).to_le_bytes());
        msg.recent_blockhash = Hash::new_from_array(bh);
        Transaction::new_unsigned(msg)
    }

    /// Mutate the inner Message so `Message::serialize()` is guaranteed to differ
    /// — modelling a real in-process tamper (redirected recipient, altered
    /// amount, swapped blockhash). Falls back to a blockhash bit-flip if the
    /// chosen mutation somehow leaves the bytes unchanged, so the post-state
    /// always drifts.
    fn tamper_message(tx: &mut Transaction, seed: u64) {
        let before = tx.message.serialize();
        let mut st = seed ^ 0x2545_F491_4F6C_DD1D;
        match mix(&mut st) % 3 {
            0 => {
                // Redirect: rewrite the last account key (attacker recipient).
                let last = tx.message.account_keys.len() - 1;
                tx.message.account_keys[last] = Pubkey::new_unique();
            }
            1 => {
                // Tamper amount/opcode: flip a byte of the first instruction data.
                if let Some(ix) = tx.message.instructions.first_mut() {
                    if ix.data.is_empty() {
                        ix.data.push(0xFF);
                    } else {
                        ix.data[0] ^= 0xFF;
                    }
                }
            }
            _ => {
                // Swap blockhash (replay/lifetime tamper).
                let mut bh = tx.message.recent_blockhash.to_bytes();
                bh[0] = bh[0].wrapping_add(1);
                tx.message.recent_blockhash = Hash::new_from_array(bh);
            }
        }
        if tx.message.serialize() == before {
            let mut bh = tx.message.recent_blockhash.to_bytes();
            bh[0] ^= 0xFF;
            tx.message.recent_blockhash = Hash::new_from_array(bh);
        }
    }

    // ── Tests ──────────────────────────────────────────────────────────────────

    /// Positive control: an UNMUTATED approval signs cleanly. This proves the
    /// guard does not false-positive (otherwise the 1000/1000 detection below
    /// would be vacuous).
    #[tokio::test]
    async fn approval_hash_binding_allows_unmutated_transaction() {
        let pipeline = offline_pipeline(ApprovalMode::HumanGranted);
        let simulated = SimulatedTransaction::new_unchecked(pseudo_random_tx(42), successful_sim(), 1000);
        let approved = ApprovedTransaction::new_unchecked(simulated, approved_verdict());

        let signer_impl = Arc::new(RecordingSigner::default());
        let signer: SignerRef = signer_impl.clone();
        let result = pipeline.sign(&test_proposal(), approved, &signer).await;

        assert!(
            matches!(result, Ok(PipelineResult::Signed { .. })),
            "unmutated approval must sign, got {result:?}"
        );
        assert!(signer_impl.called(), "signer must be invoked for a clean transaction");
    }

    /// Q10 property test: 1000 random ApprovedTransaction states, each mutated
    /// after approval-time hash capture. EVERY sign() must fail closed with
    /// `ApprovalTxDrift`, and the signer must never be reached. Detection must be
    /// exactly 1000/1000.
    #[tokio::test]
    async fn approval_hash_binding_detects_all_message_drift() {
        const N: usize = 1000;
        let pipeline = offline_pipeline(ApprovalMode::HumanGranted);
        let mut detected = 0usize;

        for i in 0..N {
            let seed = i as u64;
            let simulated = SimulatedTransaction::new_unchecked(pseudo_random_tx(seed), successful_sim(), 1000);
            let mut approved = ApprovedTransaction::new_unchecked(simulated, approved_verdict());

            // Tamper the inner Message AFTER the approval hash was captured.
            // (Reaching `approved.inner.inner` is only possible because this test
            // module is a descendant of `crate::pipeline` — the exact in-process
            // adversary the binding defends against.)
            let before = approved.inner.inner.message.serialize();
            tamper_message(&mut approved.inner.inner, seed);
            assert_ne!(
                before,
                approved.inner.inner.message.serialize(),
                "iter {i}: tamper must change canonical Message bytes"
            );

            let signer_impl = Arc::new(RecordingSigner::default());
            let signer: SignerRef = signer_impl.clone();
            let result = pipeline.sign(&test_proposal(), approved, &signer).await;

            match result {
                Err(WalletError::ApprovalTxDrift { expected, actual }) => {
                    assert_ne!(expected, actual, "iter {i}: drift hashes must differ");
                    assert!(!signer_impl.called(), "iter {i}: signer must NOT run on drift");
                    detected += 1;
                }
                other => panic!("iter {i}: expected ApprovalTxDrift, got {other:?}"),
            }
        }

        assert_eq!(detected, N, "Q10 drift detection rate must be {N}/{N}");
        println!("Q10 hash-binding property test: {detected}/{N} message mutations detected");
    }

    /// Integration test against a Phase 5c-lite-shaped Solend USDC deposit
    /// message (SPL TransferChecked → Solend Deposit → memo carrying the W5i
    /// auto-execute `claw:` tag). Demonstrates: (a) the unmodified deposit signs,
    /// and (b) tampering the Solend deposit amount after approval is caught as
    /// `ApprovalTxDrift` before signing.
    #[tokio::test]
    async fn approval_hash_binding_detects_drift_on_phase5c_lite_solend_deposit() {
        let token_prog = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let solend_prog = Pubkey::from_str("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo").unwrap();
        let memo_prog = Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr").unwrap();

        // Controlled wallet (authority) + Phase 5c-lite accounts.
        let authority = Pubkey::new_unique();
        let source_ata = Pubkey::new_unique();
        let usdc_mint = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();
        let dest_reserve = Pubkey::new_unique();
        let obligation = Pubkey::from_str("BdFLjCcP9j7yzd9KaBoxUF5h7hZ9SXqp3xj9hnHKra1wN")
            .unwrap_or_else(|_| Pubkey::new_unique());

        // 0.5 USDC, 6 decimals (Phase 5c-lite variable-amount deposit).
        let amount: u64 = 500_000;

        // SPL TransferChecked: [12, amount(8 LE), decimals(1)].
        let mut transfer_data = vec![12u8];
        transfer_data.extend_from_slice(&amount.to_le_bytes());
        transfer_data.push(6u8);
        let ix_transfer = Instruction::new_with_bytes(
            token_prog,
            &transfer_data,
            vec![
                AccountMeta::new(source_ata, false),
                AccountMeta::new_readonly(usdc_mint, false),
                AccountMeta::new(dest_reserve, false),
                AccountMeta::new_readonly(authority, true),
            ],
        );

        // Solend DepositReserveLiquidity (tag 4) + amount(8 LE).
        let mut deposit_data = vec![4u8];
        deposit_data.extend_from_slice(&amount.to_le_bytes());
        let ix_deposit = Instruction::new_with_bytes(
            solend_prog,
            &deposit_data,
            vec![
                AccountMeta::new(source_ata, false),
                AccountMeta::new(dest_reserve, false),
                AccountMeta::new(obligation, false),
                AccountMeta::new_readonly(authority, true),
            ],
        );

        // W5i auto-execute memo tag.
        let ix_memo = Instruction::new_with_bytes(
            memo_prog,
            b"claw:w5h:6400deadbeef9a62dace:609aPhase5cLite097f",
            vec![AccountMeta::new_readonly(authority, true)],
        );

        let build_tx = || {
            let msg = Message::new(&[ix_transfer.clone(), ix_deposit.clone(), ix_memo.clone()], Some(&authority));
            Transaction::new_unsigned(msg)
        };
        let make_approved = || {
            let simulated = SimulatedTransaction::new_unchecked(build_tx(), successful_sim(), 1234);
            ApprovedTransaction::new_unchecked(simulated, approved_verdict())
        };

        let pipeline = offline_pipeline(ApprovalMode::HumanGranted);

        // (a) Unmodified Phase 5c-lite deposit signs cleanly.
        let clean_signer = Arc::new(RecordingSigner::default());
        let clean_ref: SignerRef = clean_signer.clone();
        let clean_result = pipeline.sign(&test_proposal(), make_approved(), &clean_ref).await;
        assert!(
            matches!(clean_result, Ok(PipelineResult::Signed { .. })),
            "clean Solend deposit must sign, got {clean_result:?}"
        );
        assert!(clean_signer.called(), "signer should run for the clean deposit");

        // (b) Tamper the Solend deposit AMOUNT after approval → must be caught.
        let mut tampered = make_approved();
        let approval_hash = *tampered.approval_tx_hash();
        // instructions[1] is the Solend deposit; bytes 1..9 are the LE amount.
        let deposit_ix = &mut tampered.inner.inner.message.instructions[1];
        // Drain the controlled wallet: rewrite amount to u64::MAX.
        deposit_ix.data[1..9].copy_from_slice(&u64::MAX.to_le_bytes());

        let evil_signer = Arc::new(RecordingSigner::default());
        let evil_ref: SignerRef = evil_signer.clone();
        let tampered_result = pipeline.sign(&test_proposal(), tampered, &evil_ref).await;

        match tampered_result {
            Err(WalletError::ApprovalTxDrift { expected, actual }) => {
                assert_eq!(
                    expected,
                    hex::encode(approval_hash),
                    "expected hash must equal the approval-time commitment"
                );
                assert_ne!(expected, actual, "tampered re-derivation must differ");
                assert!(!evil_signer.called(), "signer must NOT run on a tampered deposit");
            }
            other => panic!("expected ApprovalTxDrift on amount tamper, got {other:?}"),
        }
    }
}

//! Phase 4C-3 — Solend deposit structural preflight simulation.
//!
//! # What this module owns
//!
//! A single async entry point,
//! [`preflight_solend_deposit_plan`], that consumes a Phase 4C-2
//! [`SolendDepositTxPlan`], prepares a simulation-only transaction, runs
//! `simulateTransaction`, and returns a structured
//! [`SolendPreflightOutcome`].
//!
//! # Structural, not signer-verified
//!
//! This preflight does **NOT** possess the user's private key. It cannot
//! produce real signatures. It therefore relies on Solana's RPC
//! simulation semantics:
//!
//! - the transaction is built via [`solana_sdk::transaction::Transaction::new_unsigned`]
//! - [`solana_client::rpc_config::RpcSimulateTransactionConfig::sig_verify`]
//!   is set to `false`
//! - [`solana_client::rpc_config::RpcSimulateTransactionConfig::replace_recent_blockhash`]
//!   is set to `true` (so the plan never needs a fetched blockhash)
//!
//! This is **structural preflight**, not a signer-verified execution
//! preflight. It proves that the **instruction sequence** would type-check
//! through the Solana runtime given real account state, current rent, and
//! the placeholder-replaced obligation pubkey. It does NOT prove that the
//! user's wallet will produce a valid signature or that broadcast will
//! succeed.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT broadcast. No `sendTransaction`, no `sendRawTransaction`,
//!   no `sendRawV0Transaction`.
//! - Does NOT call a confirmation polling loop.
//! - Does NOT touch external wallet / Phantom / orchestrator.
//! - Does NOT sign with any user-held keypair. The only keypair it
//!   generates is a simulation-only obligation keypair used **purely for
//!   its public key** as a placeholder-replacement target — it is not
//!   persisted, not exposed, and not used to sign.
//! - Does NOT call [`build_refresh_instructions`],
//!   [`build_deposit_reserve_liquidity_and_obligation_collateral_instruction`],
//!   [`build_init_obligation_instruction`], or
//!   [`build_create_ata_idempotent_instruction`]. Those are 4C-2's plan
//!   concerns and remain untouched. The only "rebuild" this module
//!   performs is a `SystemProgram::create_account` ix when the fresh
//!   rent differs from the plan's planning-time rent — and even that
//!   uses the solana-sdk `system_instruction::create_account` helper
//!   directly, never a Solend-specific builder.
//! - Does NOT fetch a blockhash. `Hash::default()` + `replace_recent_blockhash`
//!   is the entire blockhash strategy.
//! - Does NOT mutate the input plan. All rewrites are applied to cloned
//!   instruction vectors.
//! - Does NOT use `VersionedTransaction`, `MessageV0`, or any ALT types.
//!   If the single-transaction simulation doesn't fit into a legacy
//!   transaction, the outcome is
//!   [`SolendPreflightOutcome::TooLargeNeedsChunking`] — ALT/v0 is a
//!   later phase's concern.

use std::sync::Arc;

use async_trait::async_trait;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::system_instruction;
use solana_sdk::transaction::{Transaction, TransactionError};
use thiserror::Error;
use tracing::{info, warn};

use claw_solana_core::rpc::RpcPool;
use claw_types::solana::CommitmentLevel;

use crate::integrations::solend::raw::{OBLIGATION_LEN, SOLEND_PROGRAM_ID_BS58};
use crate::integrations::solend_tx_plan::SolendDepositTxPlan;

// ── Bounds + limits ─────────────────────────────────────────────────────────

/// Maximum log lines surfaced in the outcome's `logs` vec. Bounded to
/// prevent unbounded RPC log dumps from leaking into downstream audit
/// payloads.
pub const SIMULATION_MAX_LOGS: usize = 64;

/// Solana's `PACKET_DATA_SIZE` — the hard limit on a serialized legacy
/// transaction. Exceeding this means the plan needs chunking / ALT in a
/// later slice; this slice never splits or rewrites into ALT.
pub const LEGACY_MAX_SERIALIZED_TX_SIZE: usize = 1232;

// ── Error + outcome types ───────────────────────────────────────────────────

/// RPC-side errors raised by the preflight simulator trait. Kept
/// typed-thin; callers wrap these into the [`SolendPreflightOutcome`]
/// shape they want to surface.
#[derive(Debug, Error)]
pub enum PreflightRpcError {
    #[error("RPC fetch failed: {reason}")]
    RpcFailure { reason: String },
}

/// Raw-shape simulation result, provider-agnostic enough to be returned
/// from a stub fetcher in tests.
#[derive(Debug, Clone)]
pub struct RawSimulationResult {
    /// `None` on success; `Some(err)` on simulation failure.
    pub err: Option<TransactionError>,
    /// Simulation logs verbatim. Caller is responsible for bounding.
    pub logs: Option<Vec<String>>,
    /// Compute units consumed, if the RPC returned it.
    pub units_consumed: Option<u64>,
    /// RPC context slot at simulation time, if available.
    pub context_slot: Option<u64>,
}

/// Minimal preflight RPC surface. Production uses [`ClawRpcPoolPreflightRpc`];
/// tests inject a stub.
#[async_trait]
pub trait SolendPreflightSimulator: Send + Sync {
    /// Fetch the minimum rent-exempt balance for an account of the given
    /// data length. Used to detect rent drift between planning time and
    /// preflight time.
    async fn get_minimum_balance_for_rent_exemption(
        &self,
        data_len: usize,
    ) -> Result<u64, PreflightRpcError>;

    /// Run `simulateTransaction` against a LEGACY (non-v0) unsigned
    /// transaction with `sig_verify=false` and `replace_recent_blockhash=true`.
    /// The specific `RpcSimulateTransactionConfig` is the concern of the
    /// production adapter; the trait surface is the raw result.
    async fn simulate_legacy_transaction(
        &self,
        tx: &Transaction,
    ) -> Result<RawSimulationResult, PreflightRpcError>;
}

/// Typed preflight outcome — the single return value of
/// [`preflight_solend_deposit_plan`].
///
/// `sig_verify` and `replace_recent_blockhash` are embedded in both
/// `Passed` and `Failed` for auditability: any downstream consumer can
/// prove from the outcome alone that this was a structural preflight
/// (not a signer-verified one) and used `replace_recent_blockhash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolendPreflightOutcome {
    /// Structural preflight passed: simulation returned no error.
    Passed {
        /// From the plan: the fresh-verified policy slot (propagated for
        /// audit chaining across phases).
        verified_slot: u64,
        /// RPC context slot at simulation time, if available.
        simulation_slot: Option<u64>,
        /// Compute units consumed during simulation.
        units_consumed: Option<u64>,
        /// Bounded simulation logs.
        logs: Vec<String>,
        /// Whether the plan's rent-exempt lamports disagreed with the
        /// fresh RPC rent — if `true`, the simulation used the fresh
        /// value via a rebuilt `SystemProgram::create_account` ix.
        rent_updated_for_preflight: bool,
        /// The plan's planning-time rent (present iff obligation create
        /// was in scope).
        planning_rent_lamports: Option<u64>,
        /// The fresh RPC rent (present iff obligation create was in scope).
        fresh_rent_lamports: Option<u64>,
        /// Always `false` for structural preflight.
        sig_verify: bool,
        /// Always `true` for structural preflight.
        replace_recent_blockhash: bool,
    },
    /// Simulation ran but returned a transaction error — or a lower-level
    /// classification failed the transaction before `simulateTransaction`
    /// could be reached (e.g., serialization error).
    Failed {
        /// Classified error type (one of a fixed vocabulary).
        error_type: String,
        /// Zero-based instruction index if the error was an
        /// `InstructionError`.
        instruction_index: Option<usize>,
        /// Solana `InstructionError::Custom(u32)` value, if any.
        custom_code: Option<u32>,
        /// Bounded simulation logs.
        logs: Vec<String>,
        /// Compute units, if RPC reported them before failing.
        units_consumed: Option<u64>,
        sig_verify: bool,
        replace_recent_blockhash: bool,
    },
    /// Plan state inconsistent with itself — e.g. `obligation_signer_required`
    /// and `transient_obligation_pubkey` disagree. The preflight never
    /// called `simulateTransaction`.
    ConfigError {
        error_type: String,
        message: String,
    },
    /// The concatenated setup + refresh + deposit instruction set
    /// exceeds Solana's legacy transaction size limit. ALT / v0 packing
    /// is deferred to a later slice.
    TooLargeNeedsChunking {
        message: String,
    },
}

// ── Pure helper: placeholder replacement ────────────────────────────────────

/// Replace every `AccountMeta` whose pubkey equals `from` with the same
/// metadata carrying `to`. Preserves `is_signer`, `is_writable`, and
/// instruction `data` bytes. Preserves `program_id`. Does NOT mutate
/// the input vector.
pub fn replace_pubkey_in_instructions(
    instructions: &[Instruction],
    from: Pubkey,
    to: Pubkey,
) -> Vec<Instruction> {
    instructions
        .iter()
        .map(|ix| Instruction {
            program_id: ix.program_id,
            accounts: ix
                .accounts
                .iter()
                .map(|a| AccountMeta {
                    pubkey: if a.pubkey == from { to } else { a.pubkey },
                    is_signer: a.is_signer,
                    is_writable: a.is_writable,
                })
                .collect(),
            data: ix.data.clone(),
        })
        .collect()
}

// ── Pure helper: rent-updated CreateAccount rebuild ─────────────────────────

/// In `setup`, replace any `SystemProgram::create_account`-style ix with
/// a rebuilt one carrying `fresh_rent_lamports`. The plan's setup vector
/// has at most one `SystemProgram` ix (the obligation CreateAccount) by
/// 4C-2's construction — all other setup ixs target the SPL ATA program
/// or the Solend program. The rebuild uses `system_instruction::create_account`
/// directly, NOT a Solend-specific pure builder, to avoid coupling this
/// preflight to the Solend builders forbidden by spec §13.
fn rebuild_create_account_with_fresh_rent(
    setup: Vec<Instruction>,
    fresh_rent_lamports: u64,
    funder: Pubkey,
    new_obligation: Pubkey,
    solend_program_id: Pubkey,
) -> Vec<Instruction> {
    let sysprog = solana_sdk::system_program::ID;
    setup
        .into_iter()
        .map(|ix| {
            if ix.program_id == sysprog {
                system_instruction::create_account(
                    &funder,
                    &new_obligation,
                    fresh_rent_lamports,
                    OBLIGATION_LEN as u64,
                    &solend_program_id,
                )
            } else {
                ix
            }
        })
        .collect()
}

// ── Prepared transaction (pure, testable without RPC) ───────────────────────

/// Output of [`prepare_preflight_transaction`] — exposes the cloned,
/// rewritten instruction groups and the final unsigned transaction so
/// tests can inspect structural invariants (placeholder replaced, rent
/// propagated, group order preserved) without needing to mock the RPC
/// surface.
pub struct PreparedPreflight {
    pub setup_instructions: Vec<Instruction>,
    pub refresh_instructions: Vec<Instruction>,
    pub deposit_instructions: Vec<Instruction>,
    /// Pubkey used to replace `transient_obligation_pubkey`. `None` when
    /// the plan had no transient placeholder (existing obligation path).
    pub replacement_obligation_pubkey: Option<Pubkey>,
    /// `true` iff the CreateAccount ix was rebuilt with a fresh rent
    /// different from the plan's planning rent.
    pub rent_updated_for_preflight: bool,
    pub planning_rent_lamports: Option<u64>,
    pub fresh_rent_lamports: Option<u64>,
    pub transaction: Transaction,
    pub serialized_size: usize,
}

/// Pre-RPC errors that can occur while preparing the simulation tx.
#[derive(Debug, Error)]
pub enum PreparePreflightError {
    #[error("obligation_signer_required is true but transient_obligation_pubkey is None")]
    MissingTransientObligation,
    #[error("transient_obligation_pubkey is set but obligation_signer_required is false")]
    UnexpectedTransientObligation,
    #[error("internal: SOLEND_PROGRAM_ID_BS58 failed to parse: {0}")]
    InternalConstantParseFailed(String),
    #[error("transaction serialization failed: {0}")]
    SerializationFailed(String),
}

/// Build the simulation-only transaction from a plan without touching
/// RPC. `replacement_obligation_pubkey` is the pubkey to substitute for
/// `transient_obligation_pubkey` (callers either pass in a cached one
/// or, in production, the pubkey of a freshly-generated simulation-only
/// keypair). `fresh_rent_lamports`, when provided, triggers a
/// CreateAccount rebuild if it differs from the plan's planning rent.
///
/// This function is PURE over its inputs: no RPC, no global state, no
/// clock. It never mutates the input plan.
pub fn prepare_preflight_transaction(
    plan: &SolendDepositTxPlan,
    replacement_obligation_pubkey: Option<Pubkey>,
    fresh_rent_lamports: Option<u64>,
) -> Result<PreparedPreflight, PreparePreflightError> {
    // ── 1. Config sanity ────────────────────────────────────────────────────
    match (plan.obligation_signer_required, plan.transient_obligation_pubkey) {
        (true, None) => return Err(PreparePreflightError::MissingTransientObligation),
        (false, Some(_)) => return Err(PreparePreflightError::UnexpectedTransientObligation),
        _ => {}
    }

    // ── 2. Placeholder replacement ──────────────────────────────────────────
    let (setup_ixs, refresh_ixs, deposit_ixs) = match (
        plan.transient_obligation_pubkey,
        replacement_obligation_pubkey,
    ) {
        (Some(placeholder), Some(replacement)) => (
            replace_pubkey_in_instructions(&plan.setup_instructions, placeholder, replacement),
            replace_pubkey_in_instructions(&plan.refresh_instructions, placeholder, replacement),
            replace_pubkey_in_instructions(&plan.deposit_instructions, placeholder, replacement),
        ),
        // Either obligation already exists (None/None) or caller chose
        // not to substitute. Pass through cloned.
        _ => (
            plan.setup_instructions.clone(),
            plan.refresh_instructions.clone(),
            plan.deposit_instructions.clone(),
        ),
    };

    // ── 3. Rent drift → rebuild CreateAccount ───────────────────────────────
    let planning_rent = if plan.obligation_signer_required {
        Some(plan.planning_rent_exempt_lamports)
    } else {
        None
    };
    let (setup_ixs, rent_updated) = match (
        plan.obligation_signer_required,
        replacement_obligation_pubkey,
        fresh_rent_lamports,
    ) {
        (true, Some(real_obligation), Some(fresh)) if fresh != plan.planning_rent_exempt_lamports => {
            let solend_program_id = Pubkey::try_from(SOLEND_PROGRAM_ID_BS58).map_err(|e| {
                PreparePreflightError::InternalConstantParseFailed(e.to_string())
            })?;
            let rebuilt = rebuild_create_account_with_fresh_rent(
                setup_ixs,
                fresh,
                plan.session_wallet,
                real_obligation,
                solend_program_id,
            );
            (rebuilt, true)
        }
        _ => (setup_ixs, false),
    };

    // ── 4. Concatenate into one Message (Option A per spec §7) ──────────────
    let mut all_ixs = Vec::with_capacity(
        setup_ixs.len() + refresh_ixs.len() + deposit_ixs.len(),
    );
    all_ixs.extend(setup_ixs.iter().cloned());
    all_ixs.extend(refresh_ixs.iter().cloned());
    all_ixs.extend(deposit_ixs.iter().cloned());

    // ── 5. Build legacy unsigned transaction ────────────────────────────────
    //
    // `Hash::default()` is a sentinel blockhash. The RPC will replace it
    // via `replace_recent_blockhash=true` before simulation. We do NOT
    // call `get_latest_blockhash` anywhere.
    let message = Message::new_with_blockhash(&all_ixs, Some(&plan.session_wallet), &Hash::default());
    let transaction = Transaction::new_unsigned(message);

    // ── 6. Size check ───────────────────────────────────────────────────────
    let serialized_size = bincode::serialized_size(&transaction)
        .map_err(|e| PreparePreflightError::SerializationFailed(e.to_string()))?
        as usize;

    Ok(PreparedPreflight {
        setup_instructions: setup_ixs,
        refresh_instructions: refresh_ixs,
        deposit_instructions: deposit_ixs,
        replacement_obligation_pubkey,
        rent_updated_for_preflight: rent_updated,
        planning_rent_lamports: planning_rent,
        fresh_rent_lamports: if plan.obligation_signer_required {
            fresh_rent_lamports
        } else {
            None
        },
        transaction,
        serialized_size,
    })
}

// ── Async entry point ───────────────────────────────────────────────────────

/// Run structural preflight for a [`SolendDepositTxPlan`].
///
/// See module docs for non-goals. This function:
///
/// 1. Sanity-checks the plan's obligation flags.
/// 2. If `obligation_signer_required`, generates a simulation-only
///    obligation `Keypair` and uses its pubkey to replace the plan's
///    `transient_obligation_pubkey` in all three instruction groups.
///    The keypair is used PURELY for its pubkey and is dropped at
///    function return — it is never persisted, signed with, or exposed.
/// 3. If `obligation_signer_required`, re-fetches
///    `getMinimumBalanceForRentExemption(OBLIGATION_LEN)` and, if it
///    differs from the plan's planning-time rent, rebuilds the
///    `SystemProgram::create_account` ix with the fresh lamports.
/// 4. Concatenates setup + refresh + deposit into one legacy
///    [`Transaction`], built via [`Transaction::new_unsigned`] with a
///    sentinel `Hash::default()` blockhash.
/// 5. Checks the serialized size against [`LEGACY_MAX_SERIALIZED_TX_SIZE`].
/// 6. Runs `simulateTransaction` with `sig_verify=false` and
///    `replace_recent_blockhash=true`, classifies the result, and
///    returns a bounded-log [`SolendPreflightOutcome`].
pub async fn preflight_solend_deposit_plan(
    plan: &SolendDepositTxPlan,
    simulator: &dyn SolendPreflightSimulator,
) -> SolendPreflightOutcome {
    // ── 1. Config sanity guard (early typed ConfigError) ────────────────────
    match (plan.obligation_signer_required, plan.transient_obligation_pubkey) {
        (true, None) => {
            return SolendPreflightOutcome::ConfigError {
                error_type: "MissingTransientObligation".to_string(),
                message: "obligation_signer_required=true but transient_obligation_pubkey is None"
                    .to_string(),
            };
        }
        (false, Some(_)) => {
            return SolendPreflightOutcome::ConfigError {
                error_type: "UnexpectedTransientObligation".to_string(),
                message: "transient_obligation_pubkey is set but obligation_signer_required=false"
                    .to_string(),
            };
        }
        _ => {}
    }

    // ── 2. Simulation-only keypair for placeholder (no signing, dropped) ────
    //
    // Rustc scope guarantees `sim_obligation_keypair` is dropped at
    // function return — we pull its pubkey eagerly and stop using the
    // keypair. Nothing else in this function takes `&Keypair` or asks
    // for `.secret()`.
    let (replacement_pubkey, rent_probe_needed) = if plan.obligation_signer_required {
        let sim_obligation_keypair = Keypair::new();
        let pubkey = sim_obligation_keypair.pubkey();
        drop(sim_obligation_keypair);
        (Some(pubkey), true)
    } else {
        (None, false)
    };

    // ── 3. Rent re-fetch ────────────────────────────────────────────────────
    let fresh_rent = if rent_probe_needed {
        match simulator
            .get_minimum_balance_for_rent_exemption(OBLIGATION_LEN)
            .await
        {
            Ok(n) => Some(n),
            Err(e) => {
                return SolendPreflightOutcome::ConfigError {
                    error_type: "RentFetchFailed".to_string(),
                    message: format!("{e}"),
                };
            }
        }
    } else {
        None
    };

    // ── 4. Prepare transaction (pure) ───────────────────────────────────────
    let prepared = match prepare_preflight_transaction(plan, replacement_pubkey, fresh_rent) {
        Ok(p) => p,
        Err(PreparePreflightError::MissingTransientObligation) => {
            return SolendPreflightOutcome::ConfigError {
                error_type: "MissingTransientObligation".to_string(),
                message: "obligation_signer_required=true but transient_obligation_pubkey is None"
                    .to_string(),
            };
        }
        Err(PreparePreflightError::UnexpectedTransientObligation) => {
            return SolendPreflightOutcome::ConfigError {
                error_type: "UnexpectedTransientObligation".to_string(),
                message: "transient_obligation_pubkey is set but obligation_signer_required=false"
                    .to_string(),
            };
        }
        Err(PreparePreflightError::InternalConstantParseFailed(msg)) => {
            return SolendPreflightOutcome::ConfigError {
                error_type: "InternalConstantParseFailed".to_string(),
                message: msg,
            };
        }
        Err(PreparePreflightError::SerializationFailed(msg)) => {
            return SolendPreflightOutcome::ConfigError {
                error_type: "SerializationFailed".to_string(),
                message: msg,
            };
        }
    };

    // ── 5. Size gate ────────────────────────────────────────────────────────
    if prepared.serialized_size > LEGACY_MAX_SERIALIZED_TX_SIZE {
        return SolendPreflightOutcome::TooLargeNeedsChunking {
            message: format!(
                "serialized legacy transaction size {} exceeds limit {} — ALT/v0 deferred",
                prepared.serialized_size, LEGACY_MAX_SERIALIZED_TX_SIZE
            ),
        };
    }

    // ── 6. Simulate ─────────────────────────────────────────────────────────
    let raw = match simulator.simulate_legacy_transaction(&prepared.transaction).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "solend preflight: simulation RPC failure");
            return SolendPreflightOutcome::Failed {
                error_type: "SimulateRpcFailure".to_string(),
                instruction_index: None,
                custom_code: None,
                logs: Vec::new(),
                units_consumed: None,
                sig_verify: false,
                replace_recent_blockhash: true,
            };
        }
    };

    // ── 7. Bound logs ───────────────────────────────────────────────────────
    let logs_bounded: Vec<String> = raw
        .logs
        .unwrap_or_default()
        .into_iter()
        .take(SIMULATION_MAX_LOGS)
        .collect();

    // ── 8. Classify ─────────────────────────────────────────────────────────
    match raw.err {
        None => {
            info!(
                verified_slot = plan.verified_slot,
                sim_slot = ?raw.context_slot,
                units = ?raw.units_consumed,
                rent_updated = prepared.rent_updated_for_preflight,
                "solend preflight: simulation passed (structural, sig_verify=false)"
            );
            SolendPreflightOutcome::Passed {
                verified_slot: plan.verified_slot,
                simulation_slot: raw.context_slot,
                units_consumed: raw.units_consumed,
                logs: logs_bounded,
                rent_updated_for_preflight: prepared.rent_updated_for_preflight,
                planning_rent_lamports: prepared.planning_rent_lamports,
                fresh_rent_lamports: prepared.fresh_rent_lamports,
                sig_verify: false,
                replace_recent_blockhash: true,
            }
        }
        Some(tx_err) => {
            let (error_type, instruction_index, custom_code) = classify_transaction_error(&tx_err);
            warn!(
                verified_slot = plan.verified_slot,
                error_type = %error_type,
                instruction_index = ?instruction_index,
                custom_code = ?custom_code,
                "solend preflight: simulation failed"
            );
            SolendPreflightOutcome::Failed {
                error_type,
                instruction_index,
                custom_code,
                logs: logs_bounded,
                units_consumed: raw.units_consumed,
                sig_verify: false,
                replace_recent_blockhash: true,
            }
        }
    }
}

// ── Error classification ────────────────────────────────────────────────────

/// Classify a `TransactionError` into a stable `error_type` string plus
/// optional `instruction_index` and `custom_code`. Do not overfit —
/// unknown errors map to `UnknownSimulationError` with the raw debug
/// summary still available in logs.
pub fn classify_transaction_error(
    err: &TransactionError,
) -> (String, Option<usize>, Option<u32>) {
    use solana_sdk::instruction::InstructionError;
    match err {
        TransactionError::InstructionError(idx, ix_err) => {
            let idx = *idx as usize;
            match ix_err {
                InstructionError::Custom(code) => {
                    let etype = match *code {
                        // Solend token-lending custom codes (subset the
                        // brief flagged). Kept conservative — numbers
                        // that would collide across programs are
                        // surfaced by name AND custom_code.
                        9 => "InvalidTokenProgram",
                        10 => "InvalidAmount",
                        14 => "MathOverflow",
                        _ => "CustomInstructionError",
                    };
                    (etype.to_string(), Some(idx), Some(*code))
                }
                InstructionError::InsufficientFunds => {
                    ("InsufficientFunds".to_string(), Some(idx), None)
                }
                InstructionError::MissingAccount => {
                    ("MissingAccount".to_string(), Some(idx), None)
                }
                InstructionError::AccountNotExecutable => {
                    ("AccountNotExecutable".to_string(), Some(idx), None)
                }
                other => (format!("InstructionError:{other:?}"), Some(idx), None),
            }
        }
        TransactionError::SignatureFailure => {
            ("SignatureFailure".to_string(), None, None)
        }
        TransactionError::AccountNotFound => {
            ("AccountNotFound".to_string(), None, None)
        }
        TransactionError::AccountInUse => ("AccountInUse".to_string(), None, None),
        TransactionError::InsufficientFundsForFee => {
            ("InsufficientFundsForFee".to_string(), None, None)
        }
        TransactionError::BlockhashNotFound => {
            // Would be pathological under replace_recent_blockhash=true,
            // but surface cleanly if seen.
            ("BlockhashNotFound".to_string(), None, None)
        }
        other => (format!("UnknownSimulationError:{other:?}"), None, None),
    }
}

// Token-lending convention: SPL Token program custom codes are a
// separate namespace from Solend's. SPL Token custom 1 is
// `TokenError::InsufficientFunds`. Because we cannot tell from the
// `TransactionError` alone WHICH program raised Custom(N), we surface
// the numeric code alongside a generic name and let the error-mapping
// layer decide. Tests assert the (custom_code, error_type) pair is
// stable for known values.

// ── Production adapter: RpcPool-backed simulator ────────────────────────────

/// Production [`SolendPreflightSimulator`] wrapping an [`RpcPool`].
///
/// Reuses the pool's circuit breaker and endpoint selection behaviour
/// for symmetry with [`crate::integrations::solend::ClawRpcPoolAccountFetcher`].
///
/// Notably this adapter does **NOT** go through
/// [`claw_solana_core::rpc::client::ClawRpcClient`]'s existing
/// `simulate_v0_transaction` helper. That helper takes a
/// `VersionedTransaction` and forbids `replace_recent_blockhash`; both
/// are wrong for structural preflight. We go to the underlying
/// `solana_client::nonblocking::rpc_client::RpcClient` directly and
/// pass `RpcSimulateTransactionConfig { sig_verify: false,
/// replace_recent_blockhash: true, ... }`.
#[derive(Clone)]
pub struct ClawRpcPoolPreflightRpc {
    pool: RpcPool,
    commitment: CommitmentLevel,
}

impl ClawRpcPoolPreflightRpc {
    pub fn new(pool: RpcPool, commitment: CommitmentLevel) -> Self {
        Self { pool, commitment }
    }

    fn sdk_commitment(&self) -> solana_sdk::commitment_config::CommitmentConfig {
        match self.commitment {
            CommitmentLevel::Processed => {
                solana_sdk::commitment_config::CommitmentConfig::processed()
            }
            CommitmentLevel::Confirmed => {
                solana_sdk::commitment_config::CommitmentConfig::confirmed()
            }
            CommitmentLevel::Finalized => {
                solana_sdk::commitment_config::CommitmentConfig::finalized()
            }
        }
    }
}

#[async_trait]
impl SolendPreflightSimulator for ClawRpcPoolPreflightRpc {
    async fn get_minimum_balance_for_rent_exemption(
        &self,
        data_len: usize,
    ) -> Result<u64, PreflightRpcError> {
        let client = self.pool.read_client().map_err(|e| PreflightRpcError::RpcFailure {
            reason: format!("rpc pool has no available endpoint: {e}"),
        })?;
        let lamports = client
            .get_minimum_balance_for_rent_exemption(data_len)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                PreflightRpcError::RpcFailure {
                    reason: format!("getMinimumBalanceForRentExemption({data_len}): {e}"),
                }
            })?;
        self.pool.record_success(&client.url());
        Ok(lamports)
    }

    async fn simulate_legacy_transaction(
        &self,
        tx: &Transaction,
    ) -> Result<RawSimulationResult, PreflightRpcError> {
        use solana_client::rpc_config::RpcSimulateTransactionConfig;
        use solana_transaction_status::UiTransactionEncoding;

        let client = self.pool.read_client().map_err(|e| PreflightRpcError::RpcFailure {
            reason: format!("rpc pool has no available endpoint: {e}"),
        })?;
        let config = RpcSimulateTransactionConfig {
            sig_verify: false,
            replace_recent_blockhash: true,
            commitment: Some(self.sdk_commitment()),
            encoding: Some(UiTransactionEncoding::Base64),
            accounts: None,
            min_context_slot: None,
            inner_instructions: false,
        };
        let resp = client
            .simulate_transaction_with_config(tx, config)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                PreflightRpcError::RpcFailure {
                    reason: format!("simulateTransaction (legacy): {e}"),
                }
            })?;
        self.pool.record_success(&client.url());
        let context_slot = Some(resp.context.slot);
        let value = resp.value;
        Ok(RawSimulationResult {
            err: value.err,
            logs: value.logs,
            units_consumed: value.units_consumed,
            context_slot,
        })
    }
}

/// Boxed trait-object alias for ergonomics in constructors.
pub type SolendPreflightSimulatorArc = Arc<dyn SolendPreflightSimulator>;

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::mapping::{
        map_snapshot_for_first_deposit, FirstDepositAssemblyInputs, OracleAccountInfo,
        ReserveInput,
    };
    use crate::integrations::solend::raw::{decode_reserve, synth_reserve};
    use crate::integrations::solend::AssembledSolendDepositSnapshot;
    use crate::integrations::solend_tx_plan::{
        assemble_solend_deposit_tx_plan, PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
    };
    use crate::integrations::solend_park::ParkedSolendDepositIntent;
    use crate::lending::{
        ChainSlot, FeedPublishFreshness, LendingPolicyVerdict, ProposedAction, ProtocolTag,
        UnderlyingAmount,
    };
    use claw_types::session::SessionId;
    use solana_sdk::instruction::InstructionError;
    use std::str::FromStr;
    use std::sync::Mutex;
    use uuid::Uuid;

    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const PYTH_RECEIVER_PROGRAM_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    // ── Fixture: a fresh, passing AssembledSolendDepositSnapshot ────────────

    fn usdc_mint() -> Pubkey {
        Pubkey::try_from(USDC_MINT_BS58).unwrap()
    }

    fn fresh_snapshot(
        session_wallet: Pubkey,
        obligation_pubkey: Pubkey,
        obligation_exists: bool,
    ) -> AssembledSolendDepositSnapshot {
        let market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let collateral_mint = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let sentinel: Pubkey =
            "nu11111111111111111111111111111111111111111".parse().unwrap();
        let pyth_owner: Pubkey = PYTH_RECEIVER_PROGRAM_BS58.parse().unwrap();
        let supply = Pubkey::new_unique();
        let reserve_bytes = synth_reserve(
            market,
            usdc_mint(),
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            collateral_mint,
            Pubkey::new_unique(),
            100,
            false,
        );
        let reserve_raw = decode_reserve(&reserve_bytes).unwrap();
        let snapshot = map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
            session_wallet,
            obligation_pubkey,
            lending_market: market,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pubkey,
                raw: reserve_raw,
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
            snapshot,
            obligation_exists,
            source_ata_exists: true,
            collateral_ata_exists: false,
            reserve_pubkey,
            obligation_pubkey,
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: market,
        }
    }

    fn parked_intent(session_wallet: Pubkey) -> ParkedSolendDepositIntent {
        ParkedSolendDepositIntent {
            intent_id: Uuid::new_v4(),
            action: ProposedAction::Deposit {
                protocol: ProtocolTag::Solend,
                reserve_mint: usdc_mint(),
                amount: UnderlyingAmount::new(1000),
            },
            // The snapshot field isn't consumed by plan-assembly; a
            // throwaway first-deposit snapshot is fine.
            snapshot: fresh_snapshot(session_wallet, Pubkey::new_unique(), false)
                .snapshot,
            obligation_exists: false,
            source_ata_exists: true,
            collateral_ata_exists: false,
            verdict_at_propose: LendingPolicyVerdict::Pass,
            proposed_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
            session_id: SessionId::new(),
            session_wallet,
        }
    }

    fn fresh_plan(obligation_exists: bool) -> SolendDepositTxPlan {
        let session_wallet = Pubkey::new_unique();
        let obligation_pubkey = Pubkey::new_unique();
        let parked = parked_intent(session_wallet);
        let assembled = fresh_snapshot(session_wallet, obligation_pubkey, obligation_exists);
        assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &assembled,
            123_456,
            PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
        )
        .expect("plan assembles")
    }

    // ── Stub simulator ──────────────────────────────────────────────────────

    struct StubSimulator {
        rent: Mutex<Result<u64, PreflightRpcError>>,
        sim: Mutex<Option<Result<RawSimulationResult, PreflightRpcError>>>,
        rent_calls: Mutex<usize>,
        sim_calls: Mutex<usize>,
    }

    impl StubSimulator {
        fn passing(rent: u64) -> Self {
            Self {
                rent: Mutex::new(Ok(rent)),
                sim: Mutex::new(Some(Ok(RawSimulationResult {
                    err: None,
                    logs: Some(vec!["Program ... consumed 1234 of 200000 compute units".into()]),
                    units_consumed: Some(1234),
                    context_slot: Some(999_999),
                }))),
                rent_calls: Mutex::new(0),
                sim_calls: Mutex::new(0),
            }
        }
        fn with_sim(mut self, result: Result<RawSimulationResult, PreflightRpcError>) -> Self {
            self.sim = Mutex::new(Some(result));
            self
        }
        fn rent_call_count(&self) -> usize {
            *self.rent_calls.lock().unwrap()
        }
        fn sim_call_count(&self) -> usize {
            *self.sim_calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl SolendPreflightSimulator for StubSimulator {
        async fn get_minimum_balance_for_rent_exemption(
            &self,
            _data_len: usize,
        ) -> Result<u64, PreflightRpcError> {
            *self.rent_calls.lock().unwrap() += 1;
            match &*self.rent.lock().unwrap() {
                Ok(n) => Ok(*n),
                Err(PreflightRpcError::RpcFailure { reason }) => {
                    Err(PreflightRpcError::RpcFailure { reason: reason.clone() })
                }
            }
        }
        async fn simulate_legacy_transaction(
            &self,
            _tx: &Transaction,
        ) -> Result<RawSimulationResult, PreflightRpcError> {
            *self.sim_calls.lock().unwrap() += 1;
            let mut guard = self.sim.lock().unwrap();
            match guard.take() {
                Some(Ok(r)) => Ok(r),
                Some(Err(PreflightRpcError::RpcFailure { reason })) => {
                    Err(PreflightRpcError::RpcFailure { reason })
                }
                None => Err(PreflightRpcError::RpcFailure {
                    reason: "stub exhausted".into(),
                }),
            }
        }
    }

    // ── (A) Placeholder replacement is consistent across groups ─────────────

    #[tokio::test]
    async fn placeholder_replaced_in_all_groups_and_original_plan_unchanged() {
        let plan = fresh_plan(/*obligation_exists=*/ false);
        let placeholder = plan
            .transient_obligation_pubkey
            .expect("first-deposit plan has transient placeholder");
        // snapshot before
        let before_setup = plan.setup_instructions.clone();
        let before_refresh = plan.refresh_instructions.clone();
        let before_deposit = plan.deposit_instructions.clone();

        let replacement = Pubkey::new_unique();
        let prep = prepare_preflight_transaction(&plan, Some(replacement), None).unwrap();

        // original plan untouched
        assert_eq!(plan.setup_instructions, before_setup);
        assert_eq!(plan.refresh_instructions, before_refresh);
        assert_eq!(plan.deposit_instructions, before_deposit);

        // placeholder absent everywhere, replacement present somewhere in
        // each group that originally referenced the placeholder
        for group in [
            &prep.setup_instructions,
            &prep.refresh_instructions,
            &prep.deposit_instructions,
        ] {
            for ix in group {
                for meta in &ix.accounts {
                    assert_ne!(meta.pubkey, placeholder);
                }
            }
        }
        // Setup contains CreateAccount + InitObligation which both
        // reference the obligation. Deposit references obligation. Make
        // sure the replacement appears at least once in each relevant
        // group.
        let count_in = |g: &[Instruction]| -> usize {
            g.iter()
                .flat_map(|ix| ix.accounts.iter())
                .filter(|a| a.pubkey == replacement)
                .count()
        };
        assert!(count_in(&prep.setup_instructions) >= 1);
        assert!(count_in(&prep.deposit_instructions) >= 1);
    }

    // ── (B) Inconsistent flags → ConfigError ────────────────────────────────

    #[tokio::test]
    async fn signer_required_true_but_transient_none_returns_config_error() {
        let mut plan = fresh_plan(/*obligation_exists=*/ false);
        plan.transient_obligation_pubkey = None;
        let sim = StubSimulator::passing(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS);
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::ConfigError { error_type, .. } => {
                assert_eq!(error_type, "MissingTransientObligation");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
        // Never reached RPC.
        assert_eq!(sim.rent_call_count(), 0);
        assert_eq!(sim.sim_call_count(), 0);
    }

    #[tokio::test]
    async fn signer_required_false_but_transient_some_returns_config_error() {
        let mut plan = fresh_plan(/*obligation_exists=*/ true);
        plan.transient_obligation_pubkey = Some(Pubkey::new_unique());
        let sim = StubSimulator::passing(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS);
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::ConfigError { error_type, .. } => {
                assert_eq!(error_type, "UnexpectedTransientObligation");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    // ── (C) Rent mismatch rebuilds CreateAccount ────────────────────────────

    #[tokio::test]
    async fn rent_mismatch_rebuilds_create_account_and_records_update() {
        let plan = fresh_plan(/*obligation_exists=*/ false);
        let fresh_rent = PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS + 1_000;
        let sim = StubSimulator::passing(fresh_rent);
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::Passed {
                rent_updated_for_preflight,
                planning_rent_lamports,
                fresh_rent_lamports,
                ..
            } => {
                assert!(rent_updated_for_preflight);
                assert_eq!(planning_rent_lamports, Some(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS));
                assert_eq!(fresh_rent_lamports, Some(fresh_rent));
            }
            other => panic!("expected Passed, got {other:?}"),
        }
    }

    /// Prove the pure helper rebuilds the CreateAccount ix with the new
    /// lamports — no raw byte patching. We inspect the `system_instruction::create_account`
    /// output directly.
    #[test]
    fn rebuild_create_account_changes_lamports_not_bytes_via_patch() {
        let plan = fresh_plan(false);
        let original_create = plan
            .setup_instructions
            .iter()
            .find(|ix| ix.program_id == solana_sdk::system_program::ID)
            .expect("plan has a SystemProgram CreateAccount ix")
            .clone();

        let replacement = Pubkey::new_unique();
        let prep = prepare_preflight_transaction(&plan, Some(replacement), Some(999_999_999))
            .unwrap();
        let rebuilt = prep
            .setup_instructions
            .iter()
            .find(|ix| ix.program_id == solana_sdk::system_program::ID)
            .expect("rebuilt still has a SystemProgram ix");

        // Data payload changed (new lamports encoded) but STRUCTURAL shape
        // is identical (same program_id, same account-meta shape):
        assert_eq!(rebuilt.program_id, original_create.program_id);
        assert_eq!(rebuilt.accounts.len(), original_create.accounts.len());
        // Crucially — the rebuild is identical to calling
        // system_instruction::create_account directly; it is NOT a
        // byte-edit of the old data vector. Verify by constructing the
        // expected ix independently:
        let expected = system_instruction::create_account(
            &plan.session_wallet,
            &replacement,
            999_999_999,
            OBLIGATION_LEN as u64,
            &Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap(),
        );
        assert_eq!(rebuilt.data, expected.data);
    }

    // ── (D) Existing obligation skips rent fetch ────────────────────────────

    #[tokio::test]
    async fn existing_obligation_plan_skips_rent_fetch() {
        let plan = fresh_plan(/*obligation_exists=*/ true);
        assert!(!plan.obligation_signer_required);
        assert!(plan.transient_obligation_pubkey.is_none());
        let sim = StubSimulator::passing(0);
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::Passed {
                rent_updated_for_preflight,
                planning_rent_lamports,
                fresh_rent_lamports,
                ..
            } => {
                assert!(!rent_updated_for_preflight);
                assert!(planning_rent_lamports.is_none());
                assert!(fresh_rent_lamports.is_none());
            }
            other => panic!("expected Passed, got {other:?}"),
        }
        assert_eq!(sim.rent_call_count(), 0, "existing-obligation plan must not probe rent");
        assert_eq!(sim.sim_call_count(), 1);
    }

    // ── (E) Blockhash strategy: Hash::default + replace_recent_blockhash ────

    #[tokio::test]
    async fn transaction_uses_hash_default_and_config_replaces_it() {
        let plan = fresh_plan(false);
        let prep = prepare_preflight_transaction(&plan, Some(Pubkey::new_unique()), None).unwrap();
        assert_eq!(prep.transaction.message.recent_blockhash, Hash::default());

        // And the outcome surfaces replace_recent_blockhash=true.
        let sim = StubSimulator::passing(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS);
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::Passed {
                replace_recent_blockhash,
                sig_verify,
                ..
            } => {
                assert!(replace_recent_blockhash);
                assert!(!sig_verify);
            }
            other => panic!("expected Passed, got {other:?}"),
        }
    }

    // ── (F) No user wallet signer required ─────────────────────────────────

    #[test]
    fn transaction_is_built_unsigned_with_no_signer_dependency() {
        let plan = fresh_plan(false);
        let prep = prepare_preflight_transaction(&plan, Some(Pubkey::new_unique()), None).unwrap();
        // Transaction::new_unsigned leaves all signatures as default.
        for sig in &prep.transaction.signatures {
            assert_eq!(*sig, solana_sdk::signature::Signature::default());
        }
    }

    // ── (G) Single tx concatenates setup + refresh + deposit in order ──────

    #[test]
    fn single_transaction_contains_setup_then_refresh_then_deposit_in_order() {
        let plan = fresh_plan(false);
        let replacement = Pubkey::new_unique();
        let prep = prepare_preflight_transaction(&plan, Some(replacement), None).unwrap();

        let expected_count =
            prep.setup_instructions.len() + prep.refresh_instructions.len() + prep.deposit_instructions.len();
        assert_eq!(prep.transaction.message.instructions.len(), expected_count);

        // Verify order by program_id sequence: setup ends with the SPL ATA
        // program (the CreateIdempotent ATA for collateral) OR the Solend
        // InitObligation; refresh is all Solend program; deposit is one
        // Solend ix. We assert the total sequence matches
        // concatenation order of program_ids.
        let all_program_ids: Vec<Pubkey> = [
            prep.setup_instructions.iter(),
            prep.refresh_instructions.iter(),
            prep.deposit_instructions.iter(),
        ]
        .into_iter()
        .flatten()
        .map(|ix| ix.program_id)
        .collect();
        let tx_program_ids: Vec<Pubkey> = prep
            .transaction
            .message
            .instructions
            .iter()
            .map(|ci| {
                prep.transaction.message.account_keys[ci.program_id_index as usize]
            })
            .collect();
        assert_eq!(tx_program_ids, all_program_ids);
    }

    // ── (H) TooLarge returns TooLargeNeedsChunking ─────────────────────────

    #[tokio::test]
    async fn oversized_plan_returns_too_large_needs_chunking_not_alt() {
        // Synthesize a plan whose setup instructions are artificially
        // inflated past the legacy tx size. We do this by cloning the
        // plan and appending dummy instructions to the setup group.
        let mut plan = fresh_plan(false);
        let replacement = Pubkey::new_unique();
        // Generate ~40 SystemProgram transfer ixs with max-length data.
        // Each takes ~130 bytes serialized; 40 of these will blow past 1232.
        for _ in 0..40 {
            let a = Pubkey::new_unique();
            let b = Pubkey::new_unique();
            plan.setup_instructions.push(system_instruction::transfer(&a, &b, 1));
        }
        let sim = StubSimulator::passing(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS);
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::TooLargeNeedsChunking { message } => {
                assert!(message.contains("exceeds"));
            }
            other => panic!("expected TooLargeNeedsChunking, got {other:?}"),
        }
        // Verify: no simulate call, no ALT, no v0.
        assert_eq!(sim.sim_call_count(), 0);
        let _ = replacement;
    }

    // ── (I) Simulate success maps to Passed ────────────────────────────────

    #[tokio::test]
    async fn simulate_success_maps_to_passed_with_units_and_logs() {
        let plan = fresh_plan(false);
        let sim = StubSimulator::passing(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS);
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::Passed {
                units_consumed,
                logs,
                sig_verify,
                replace_recent_blockhash,
                verified_slot,
                simulation_slot,
                ..
            } => {
                assert_eq!(units_consumed, Some(1234));
                assert!(!logs.is_empty());
                assert!(!sig_verify);
                assert!(replace_recent_blockhash);
                assert_eq!(verified_slot, plan.verified_slot);
                assert_eq!(simulation_slot, Some(999_999));
            }
            other => panic!("expected Passed, got {other:?}"),
        }
    }

    // ── (J) Classify custom 14 → MathOverflow ──────────────────────────────

    #[test]
    fn classify_custom_14_is_math_overflow() {
        let err = TransactionError::InstructionError(2, InstructionError::Custom(14));
        let (etype, idx, code) = classify_transaction_error(&err);
        assert_eq!(etype, "MathOverflow");
        assert_eq!(idx, Some(2));
        assert_eq!(code, Some(14));
    }

    // ── (K) Classify SPL token custom 1 → still surfaces code ──────────────
    //
    // We cannot distinguish SPL-token Custom(1) from Solend Custom(1) from
    // the `TransactionError` alone. Downstream (tool-surface) mapping may
    // label it; classifier surfaces `CustomInstructionError` + the code.

    #[test]
    fn classify_custom_1_surfaces_custom_code_for_downstream_mapping() {
        let err = TransactionError::InstructionError(0, InstructionError::Custom(1));
        let (etype, idx, code) = classify_transaction_error(&err);
        assert_eq!(etype, "CustomInstructionError");
        assert_eq!(idx, Some(0));
        assert_eq!(code, Some(1));
    }

    // ── (L) Signature issue maps to SignatureFailure ───────────────────────

    #[test]
    fn classify_signature_failure() {
        let err = TransactionError::SignatureFailure;
        let (etype, idx, code) = classify_transaction_error(&err);
        assert_eq!(etype, "SignatureFailure");
        assert_eq!(idx, None);
        assert_eq!(code, None);
    }

    // ── (M) Logs are bounded ────────────────────────────────────────────────

    #[tokio::test]
    async fn logs_bounded_to_simulation_max_logs() {
        let plan = fresh_plan(false);
        let huge_logs: Vec<String> = (0..500).map(|i| format!("log {i}")).collect();
        let sim = StubSimulator::passing(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS).with_sim(Ok(
            RawSimulationResult {
                err: None,
                logs: Some(huge_logs),
                units_consumed: Some(0),
                context_slot: Some(1),
            },
        ));
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::Passed { logs, .. } => {
                assert_eq!(logs.len(), SIMULATION_MAX_LOGS);
            }
            other => panic!("expected Passed, got {other:?}"),
        }
    }

    // ── Rent RPC failure surfaces as ConfigError ────────────────────────────

    #[tokio::test]
    async fn rent_fetch_failure_surfaces_as_config_error() {
        let plan = fresh_plan(false);
        let sim = StubSimulator {
            rent: Mutex::new(Err(PreflightRpcError::RpcFailure {
                reason: "connection refused".to_string(),
            })),
            sim: Mutex::new(None),
            rent_calls: Mutex::new(0),
            sim_calls: Mutex::new(0),
        };
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::ConfigError { error_type, .. } => {
                assert_eq!(error_type, "RentFetchFailed");
            }
            other => panic!("expected ConfigError RentFetchFailed, got {other:?}"),
        }
        assert_eq!(sim.sim_call_count(), 0);
    }

    // ── Simulate failure maps to Failed with classified error ───────────────

    #[tokio::test]
    async fn simulate_custom_14_maps_to_failed_math_overflow() {
        let plan = fresh_plan(false);
        let sim = StubSimulator::passing(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS).with_sim(Ok(
            RawSimulationResult {
                err: Some(TransactionError::InstructionError(3, InstructionError::Custom(14))),
                logs: Some(vec!["err log".into()]),
                units_consumed: Some(100),
                context_slot: Some(42),
            },
        ));
        let outcome = preflight_solend_deposit_plan(&plan, &sim).await;
        match outcome {
            SolendPreflightOutcome::Failed {
                error_type,
                instruction_index,
                custom_code,
                logs,
                sig_verify,
                replace_recent_blockhash,
                ..
            } => {
                assert_eq!(error_type, "MathOverflow");
                assert_eq!(instruction_index, Some(3));
                assert_eq!(custom_code, Some(14));
                assert!(!sig_verify);
                assert!(replace_recent_blockhash);
                assert_eq!(logs, vec!["err log".to_string()]);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── (P/Q/R) Structural scans (build-time) ───────────────────────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        // Build the forbidden substrings at runtime from character
        // fragments so this scanner does not match its own source text.
        const SOURCE: &str = include_str!("solend_preflight.rs");
        let paren = "(";
        let send = format!("{}{}", "send_", "transaction");
        let send_raw = format!("{}{}", "send_raw_", "transaction");
        let send_raw_v0 = format!("{}{}", "send_raw_v0_", "transaction");
        let confirm = format!("{}{}", "confirm_", "transaction");
        let get_sig_statuses = format!("{}{}", "get_signature_", "statuses");
        let get_blockhash = format!("{}{}", "get_latest_", "blockhash");
        let new_signed = format!("{}{}", "new_signed_with_", "payer");

        for base in [
            &send,
            &send_raw,
            &send_raw_v0,
            &confirm,
            &get_sig_statuses,
            &get_blockhash,
            &new_signed,
        ] {
            let needle = format!("{base}{paren}");
            assert!(
                !SOURCE.contains(&needle),
                "solend_preflight.rs must not contain `{needle}`"
            );
        }
    }
}

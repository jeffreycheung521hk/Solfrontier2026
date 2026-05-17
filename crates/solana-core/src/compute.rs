//! Compute budget instruction helpers.
//!
//! Solana transactions include `ComputeBudgetProgram` instructions to set
//! the compute unit limit and priority fee. These must be the first
//! instructions in the transaction for proper handling.
//!
//! # Instruction order
//!
//! Compute budget instructions are prepended in this order:
//! 1. `SetComputeUnitLimit`
//! 2. `SetComputeUnitPrice` (if priority fee > 0)
//! 3. Original instructions...
//!
//! # Idempotency
//!
//! `prepend_compute_budget()` checks whether the transaction already contains
//! compute budget instructions. If it does, no instructions are added.
//! This prevents double-prepend in retry scenarios or user-supplied budgets.

use solana_sdk::{
    compute_budget::{self, ComputeBudgetInstruction},
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    transaction::Transaction,
};

/// The default compute unit limit if no estimate is available.
/// Set conservatively; will be overridden by simulation estimates.
pub const DEFAULT_COMPUTE_UNITS: u32 = 200_000;

/// Headroom multiplier applied to simulation-based CU estimates.
/// 1.1 = 10% headroom above the simulated CU usage.
pub const CU_HEADROOM_FACTOR: f64 = 1.1;

/// Builds a `SetComputeUnitLimit` instruction.
pub fn set_compute_unit_limit(units: u32) -> Instruction {
    ComputeBudgetInstruction::set_compute_unit_limit(units)
}

/// Builds a `SetComputeUnitPrice` instruction.
/// `microlamports` is the price per compute unit.
pub fn set_compute_unit_price(microlamports: u64) -> Instruction {
    ComputeBudgetInstruction::set_compute_unit_price(microlamports)
}

/// Given a simulation's compute units used, return a suitable limit with headroom.
pub fn compute_limit_from_simulation(simulated_units: u64) -> u32 {
    let with_headroom = (simulated_units as f64 * CU_HEADROOM_FACTOR) as u64;
    // Clamp between 1000 (minimum useful) and 1_400_000 (max per tx)
    with_headroom.clamp(1_000, 1_400_000) as u32
}

/// Minimum CU savings (vs DEFAULT) to justify a second simulation pass.
/// If the derived limit is within this fraction of DEFAULT, skip re-simulation.
pub const TWO_PASS_THRESHOLD: f64 = 0.10;

/// Replaces the `SetComputeUnitLimit` instruction in a transaction with
/// a new limit value. Returns `true` if a replacement was made, `false`
/// if no `SetComputeUnitLimit` instruction was found.
///
/// The transaction must be unsigned. The message is rebuilt with the
/// new instruction data while preserving all other instructions.
pub fn replace_compute_unit_limit(tx: &mut Transaction, new_limit: u32) -> bool {
    debug_assert!(
        tx.signatures.is_empty()
            || tx.signatures.iter().all(|s| *s == solana_sdk::signature::Signature::default()),
        "replace_compute_unit_limit called on a signed transaction — this is a bug"
    );

    let cb_id: Pubkey = compute_budget::id();

    // Decompile all instructions, replacing the SetComputeUnitLimit one.
    let mut found = false;
    let payer = tx.message.account_keys.first().copied();
    let new_limit_ix = set_compute_unit_limit(new_limit);

    let new_instructions: Vec<Instruction> = tx
        .message
        .instructions
        .iter()
        .map(|compiled_ix| {
            let program_id = tx.message.account_keys[compiled_ix.program_id_index as usize];
            if program_id == cb_id && is_set_compute_unit_limit_data(&compiled_ix.data) {
                found = true;
                return new_limit_ix.clone();
            }
            // Decompile unchanged instruction.
            let accounts = compiled_ix
                .accounts
                .iter()
                .map(|&idx| {
                    let pubkey = tx.message.account_keys[idx as usize];
                    let is_signer = tx.message.is_signer(idx as usize);
                    let is_writable = tx.message.is_maybe_writable(idx as usize, None);
                    solana_sdk::instruction::AccountMeta {
                        pubkey,
                        is_signer,
                        is_writable,
                    }
                })
                .collect();
            Instruction {
                program_id,
                accounts,
                data: compiled_ix.data.clone(),
            }
        })
        .collect();

    if found {
        let blockhash = tx.message.recent_blockhash;
        tx.message = Message::new(&new_instructions, payer.as_ref());
        tx.message.recent_blockhash = blockhash;
    }

    found
}

/// Checks whether a compute budget instruction's data encodes a
/// `SetComputeUnitLimit`. The discriminator byte is 0x02.
fn is_set_compute_unit_limit_data(data: &[u8]) -> bool {
    // ComputeBudgetInstruction::SetComputeUnitLimit is encoded as:
    //   [0x02] [u32 little-endian]  = 5 bytes total
    data.len() == 5 && data[0] == 0x02
}

/// Returns `true` if the derived CU limit is meaningfully tighter than the
/// current limit — i.e., the savings justify a second simulation pass.
pub fn should_tighten(current_limit: u32, derived_limit: u32) -> bool {
    let savings = current_limit.saturating_sub(derived_limit) as f64 / current_limit as f64;
    savings > TWO_PASS_THRESHOLD
}

/// Returns `true` if the transaction already contains any Compute Budget
/// program instructions (`SetComputeUnitLimit` or `SetComputeUnitPrice`).
///
/// Used to prevent double-prepend.
pub fn has_compute_budget_instructions(tx: &Transaction) -> bool {
    let cb_id: Pubkey = compute_budget::id();
    tx.message
        .instructions
        .iter()
        .any(|ix| {
            tx.message.account_keys.get(ix.program_id_index as usize) == Some(&cb_id)
        })
}

/// Prepends compute budget instructions to a transaction.
///
/// If the transaction already contains compute budget instructions,
/// this is a no-op (returns `false`). Otherwise, prepends
/// `SetComputeUnitLimit` and optionally `SetComputeUnitPrice`
/// (only if `priority_fee_microlamports > 0`), then returns `true`.
///
/// # Arguments
///
/// * `tx` — The unsigned transaction to modify. Must not be signed yet.
/// * `compute_unit_limit` — The CU limit to set.
/// * `priority_fee_microlamports` — The priority fee per CU. 0 means no price instruction.
///
/// # Panics
///
/// Debug-asserts that no signatures are present (unsigned tx only).
pub fn prepend_compute_budget(
    tx: &mut Transaction,
    compute_unit_limit: u32,
    priority_fee_microlamports: u64,
) -> bool {
    debug_assert!(
        tx.signatures.is_empty() || tx.signatures.iter().all(|s| *s == solana_sdk::signature::Signature::default()),
        "prepend_compute_budget called on a signed transaction — this is a bug"
    );

    if has_compute_budget_instructions(tx) {
        return false;
    }

    // Build the new instruction list: compute budget first, then originals.
    let payer = tx.message.account_keys.first().copied();
    let mut new_instructions = Vec::new();

    // 1. SetComputeUnitLimit (always)
    new_instructions.push(set_compute_unit_limit(compute_unit_limit));

    // 2. SetComputeUnitPrice (only if non-zero)
    if priority_fee_microlamports > 0 {
        new_instructions.push(set_compute_unit_price(priority_fee_microlamports));
    }

    // 3. Original instructions (preserve order)
    let original_instructions: Vec<Instruction> = tx
        .message
        .instructions
        .iter()
        .map(|compiled_ix| {
            let program_id = tx.message.account_keys[compiled_ix.program_id_index as usize];
            let accounts = compiled_ix
                .accounts
                .iter()
                .map(|&idx| {
                    let pubkey = tx.message.account_keys[idx as usize];
                    let is_signer = tx.message.is_signer(idx as usize);
                    let is_writable = tx.message.is_maybe_writable(idx as usize, None);
                    solana_sdk::instruction::AccountMeta {
                        pubkey,
                        is_signer,
                        is_writable,
                    }
                })
                .collect();
            Instruction {
                program_id,
                accounts,
                data: compiled_ix.data.clone(),
            }
        })
        .collect();

    new_instructions.extend(original_instructions);

    // Rebuild the transaction message with prepended instructions.
    tx.message = Message::new(&new_instructions, payer.as_ref());

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::system_instruction;

    fn make_test_tx() -> Transaction {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let ix = system_instruction::transfer(&from, &to, 1000);
        let msg = Message::new(&[ix], Some(&from));
        Transaction::new_unsigned(msg)
    }

    #[test]
    fn prepend_adds_compute_budget_instructions() {
        let mut tx = make_test_tx();
        let original_ix_count = tx.message.instructions.len();
        assert_eq!(original_ix_count, 1, "should start with 1 instruction");

        let added = prepend_compute_budget(&mut tx, 200_000, 1000);
        assert!(added, "should return true when instructions are added");

        // Should now have: SetComputeUnitLimit + SetComputeUnitPrice + original transfer
        assert_eq!(tx.message.instructions.len(), 3);

        // First two instructions should be Compute Budget program
        let cb_id = compute_budget::id();
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[0].program_id_index as usize],
            cb_id,
            "first instruction must be compute budget"
        );
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[1].program_id_index as usize],
            cb_id,
            "second instruction must be compute budget"
        );
    }

    #[test]
    fn prepend_skips_price_when_zero() {
        let mut tx = make_test_tx();
        let added = prepend_compute_budget(&mut tx, 200_000, 0);
        assert!(added);
        // Should have: SetComputeUnitLimit + original transfer (no price instruction)
        assert_eq!(tx.message.instructions.len(), 2);
    }

    #[test]
    fn prepend_is_idempotent() {
        let mut tx = make_test_tx();

        let first = prepend_compute_budget(&mut tx, 200_000, 1000);
        assert!(first, "first prepend should succeed");
        let ix_count_after_first = tx.message.instructions.len();

        let second = prepend_compute_budget(&mut tx, 200_000, 1000);
        assert!(!second, "second prepend should be no-op");
        assert_eq!(
            tx.message.instructions.len(),
            ix_count_after_first,
            "instruction count must not change on duplicate prepend"
        );
    }

    #[test]
    fn has_compute_budget_detects_existing() {
        let mut tx = make_test_tx();
        assert!(!has_compute_budget_instructions(&tx));

        prepend_compute_budget(&mut tx, 200_000, 0);
        assert!(has_compute_budget_instructions(&tx));
    }

    #[test]
    fn original_instruction_order_preserved() {
        let from = Pubkey::new_unique();
        let to1 = Pubkey::new_unique();
        let to2 = Pubkey::new_unique();
        let ix1 = system_instruction::transfer(&from, &to1, 1000);
        let ix2 = system_instruction::transfer(&from, &to2, 2000);
        let msg = Message::new(&[ix1, ix2], Some(&from));
        let mut tx = Transaction::new_unsigned(msg);

        prepend_compute_budget(&mut tx, 200_000, 500);

        // 4 instructions: limit + price + transfer1 + transfer2
        assert_eq!(tx.message.instructions.len(), 4);

        // The last two should be system program (transfers)
        let system_id = solana_sdk::system_program::id();
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[2].program_id_index as usize],
            system_id,
            "third instruction must be original transfer"
        );
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[3].program_id_index as usize],
            system_id,
            "fourth instruction must be original transfer"
        );
    }

    #[test]
    fn compute_limit_from_simulation_applies_headroom() {
        // 100k CU → 110k with 10% headroom
        assert_eq!(compute_limit_from_simulation(100_000), 110_000);
        // Minimum clamp
        assert_eq!(compute_limit_from_simulation(500), 1_000);
        // Maximum clamp
        assert_eq!(compute_limit_from_simulation(2_000_000), 1_400_000);
    }

    // ── N13: replace_compute_unit_limit tests ──────────────────────────────

    #[test]
    fn replace_updates_compute_unit_limit() {
        let mut tx = make_test_tx();
        prepend_compute_budget(&mut tx, 200_000, 1000);
        assert_eq!(tx.message.instructions.len(), 3);

        let replaced = replace_compute_unit_limit(&mut tx, 50_000);
        assert!(replaced, "should find and replace the SetComputeUnitLimit");

        // Still 3 instructions (limit + price + transfer).
        assert_eq!(tx.message.instructions.len(), 3);

        // Verify the new limit data: discriminator 0x02 + u32 LE.
        let limit_ix = &tx.message.instructions[0];
        let cb_id = compute_budget::id();
        assert_eq!(
            tx.message.account_keys[limit_ix.program_id_index as usize],
            cb_id,
        );
        assert_eq!(limit_ix.data.len(), 5);
        assert_eq!(limit_ix.data[0], 0x02);
        let encoded_limit = u32::from_le_bytes(limit_ix.data[1..5].try_into().unwrap());
        assert_eq!(encoded_limit, 50_000);
    }

    #[test]
    fn replace_returns_false_when_no_compute_budget() {
        let mut tx = make_test_tx();
        assert!(!replace_compute_unit_limit(&mut tx, 50_000));
        // Transaction should be unchanged.
        assert_eq!(tx.message.instructions.len(), 1);
    }

    #[test]
    fn replace_preserves_price_and_original_instructions() {
        let from = Pubkey::new_unique();
        let to1 = Pubkey::new_unique();
        let to2 = Pubkey::new_unique();
        let ix1 = system_instruction::transfer(&from, &to1, 1000);
        let ix2 = system_instruction::transfer(&from, &to2, 2000);
        let msg = Message::new(&[ix1, ix2], Some(&from));
        let mut tx = Transaction::new_unsigned(msg);

        prepend_compute_budget(&mut tx, 200_000, 500);
        assert_eq!(tx.message.instructions.len(), 4); // limit + price + 2 transfers

        replace_compute_unit_limit(&mut tx, 80_000);
        assert_eq!(tx.message.instructions.len(), 4);

        // Price instruction (index 1) should still be compute budget.
        let cb_id = compute_budget::id();
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[1].program_id_index as usize],
            cb_id,
        );

        // Last two should still be system transfers.
        let system_id = solana_sdk::system_program::id();
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[2].program_id_index as usize],
            system_id,
        );
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[3].program_id_index as usize],
            system_id,
        );
    }

    #[test]
    fn should_tighten_thresholds() {
        // 200k → 50k = 75% savings → tighten
        assert!(should_tighten(200_000, 50_000));
        // 200k → 110k = 45% savings → tighten
        assert!(should_tighten(200_000, 110_000));
        // 200k → 185k = 7.5% savings → don't tighten (below 10%)
        assert!(!should_tighten(200_000, 185_000));
        // 200k → 200k = 0% → don't tighten
        assert!(!should_tighten(200_000, 200_000));
        // Edge: derived > current → don't tighten (saturating_sub → 0)
        assert!(!should_tighten(100_000, 200_000));
    }

    // ── N13: Full two-pass flow integration test ────────────────────────────

    /// Simulates the full N13 two-pass flow at the compute.rs level:
    /// prepend(DEFAULT) → "simulate" → derive CU → should_tighten? → replace → verify.
    #[test]
    fn n13_two_pass_full_flow() {
        let mut tx = make_test_tx();

        // Pass 1: prepend with DEFAULT_COMPUTE_UNITS
        assert!(prepend_compute_budget(&mut tx, DEFAULT_COMPUTE_UNITS, 500));
        let ix_count = tx.message.instructions.len(); // 3: limit + price + transfer

        // "First simulation" reports 50k CU used
        let simulated_cu: u64 = 50_000;
        let derived_limit = compute_limit_from_simulation(simulated_cu);
        assert_eq!(derived_limit, 55_000); // 50k * 1.1

        // Decision: should we tighten?
        assert!(should_tighten(DEFAULT_COMPUTE_UNITS, derived_limit));

        // Pass 2: replace with tighter limit
        assert!(replace_compute_unit_limit(&mut tx, derived_limit));

        // Instruction count unchanged
        assert_eq!(tx.message.instructions.len(), ix_count);

        // Verify the limit instruction now encodes 55_000
        let limit_ix = &tx.message.instructions[0];
        assert_eq!(limit_ix.data[0], 0x02); // SetComputeUnitLimit discriminator
        let encoded = u32::from_le_bytes(limit_ix.data[1..5].try_into().unwrap());
        assert_eq!(encoded, 55_000);

        // Price instruction still present
        let cb_id = compute_budget::id();
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[1].program_id_index as usize],
            cb_id,
        );

        // Original transfer still present
        let system_id = solana_sdk::system_program::id();
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[2].program_id_index as usize],
            system_id,
        );
    }

    /// When CU usage is close to DEFAULT, skip the second pass entirely.
    #[test]
    fn n13_skip_when_savings_too_small() {
        // Simulation reports 185k CU → derived = 203_500 → exceeds DEFAULT
        // should_tighten returns false
        let simulated_cu: u64 = 185_000;
        let derived = compute_limit_from_simulation(simulated_cu);
        assert_eq!(derived, 203_500); // 185k * 1.1
        assert!(!should_tighten(DEFAULT_COMPUTE_UNITS, derived));
    }

    /// Fallback: if second sim would fail, caller restores DEFAULT.
    /// This test verifies replace can restore the original value.
    #[test]
    fn n13_fallback_restores_default() {
        let mut tx = make_test_tx();
        prepend_compute_budget(&mut tx, DEFAULT_COMPUTE_UNITS, 0);

        // Tighten to 55k
        replace_compute_unit_limit(&mut tx, 55_000);
        let limit_ix = &tx.message.instructions[0];
        assert_eq!(u32::from_le_bytes(limit_ix.data[1..5].try_into().unwrap()), 55_000);

        // "Second sim failed" — restore DEFAULT
        replace_compute_unit_limit(&mut tx, DEFAULT_COMPUTE_UNITS);
        let limit_ix = &tx.message.instructions[0];
        assert_eq!(
            u32::from_le_bytes(limit_ix.data[1..5].try_into().unwrap()),
            DEFAULT_COMPUTE_UNITS,
        );
    }
}

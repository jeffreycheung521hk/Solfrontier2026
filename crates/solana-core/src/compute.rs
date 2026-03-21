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
                    let is_writable = tx.message.is_writable(idx as usize);
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
}

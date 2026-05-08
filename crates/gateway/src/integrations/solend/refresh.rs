//! Solend refresh-instruction builder — Slice 3A precondition.
//!
//! Spec anchor: `docs/lending_policy_vocabulary.md` Part 6B §66
//! ("standalone refresh transaction as precondition" — option (b),
//! adopted in Slice 2C closeout).
//!
//! ## Why this module exists
//!
//! Solend's `MaxOracleStalenessMs` and `RequireFreshState` rules both
//! HardBlock on `protocol_native_stale = Stale`, which is the default
//! state of a Solend obligation / reserve on a bare `getAccountInfo`
//! read. Refreshing requires submitting a Solend `RefreshReserve` (and
//! optionally `RefreshObligation`) instruction in a separate transaction,
//! awaiting its finalization, then re-fetching the accounts and
//! re-assembling the snapshot.
//!
//! This module provides the **pure builder** for those instructions. It
//! does NOT execute, sign, broadcast, or hold any RPC client. The caller
//! (outer wiring, future Slice 3B / 3C) owns transaction assembly,
//! signing, submission, and post-finalization re-fetch.
//!
//! ## Boundary discipline
//!
//! - This builder is INTERNAL to `integrations::solend`. The evaluator
//!   and the `lending` module are blind to refresh instructions.
//! - The builder does NOT take a `LendingSnapshot`. It takes
//!   pre-decoded Solend-side metadata (reserve pubkey + oracle pubkeys,
//!   obligation pubkey + referenced reserve pubkeys). This breaks the
//!   chicken-and-egg: refresh instructions must be buildable BEFORE a
//!   fresh policy snapshot exists.
//! - The builder does NOT mutate snapshot stale markers anywhere. Stale
//!   state remains stale until the caller actually re-fetches the
//!   refreshed account bytes.
//!
//! ## Source for instruction layout
//!
//! `solendprotocol/solana-program-library`,
//! `token-lending/program/src/instruction.rs`:
//!
//! - `LendingInstruction::RefreshReserve` — variant tag = 3, single-byte
//!   ix data `[3]`, accounts:
//!     1. reserve (writable)
//!     2. pyth oracle (readonly)
//!     3. switchboard oracle (readonly)
//!     4. clock sysvar (readonly)
//! - `LendingInstruction::RefreshObligation` — variant tag = 7,
//!   single-byte ix data `[7]`, accounts:
//!     1. obligation (writable)
//!     2..N. each reserve referenced by obligation deposits + borrows
//!         (each readonly), in deposit-then-borrow order.
//!
//!   **Phase 6I-H correction:** the deployed mainnet `RefreshObligation`
//!   handler does NOT take a clock sysvar account. It reads the clock
//!   via `Clock::get()?` syscall and consumes accounts as
//!   `[obligation, ...deposit_reserves, ...borrow_reserves]`. The earlier
//!   layout in this comment (which placed clock at slot 1) was incorrect
//!   and produced a live-mainnet failure on 2026-05-08
//!   (tx `4YnQFUTm…`): the clock account at slot 1 was interpreted as
//!   collateral 0's deposit reserve, which is not Solend-owned, and
//!   `process_refresh_obligation` returned
//!   `LendingError::InvalidAccountOwner` (custom 5). The same divergence
//!   is what `withdraw.rs` already documents for the withdraw ix —
//!   both `RefreshObligation` and the combined withdraw ix on the
//!   `mainnet` branch read `Clock::get()?` and do NOT take it as an
//!   account; only `RefreshReserve` keeps the clock account slot.
//!
//! Solend's program passes oracle slots even when they are the null
//! sentinel; the program itself decides whether to read them. Callers
//! pass the on-chain pubkeys verbatim (sentinel or real).

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;

/// Tag byte for `LendingInstruction::RefreshReserve` (=3 in the Solend
/// instruction enum at the source cited above).
const SOLEND_IX_TAG_REFRESH_RESERVE: u8 = 3;

/// Tag byte for `LendingInstruction::RefreshObligation` (=7).
const SOLEND_IX_TAG_REFRESH_OBLIGATION: u8 = 7;

/// One reserve's refresh-input payload. Caller derives this from the
/// `SolendReserveRaw` they decoded for that reserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveRefreshInput {
    pub reserve_pubkey: Pubkey,
    /// Pyth oracle pubkey for this reserve. May be the Solend null
    /// sentinel (`raw::SOLEND_NULL_ORACLE_SENTINEL_BS58`) — Solend's
    /// RefreshReserve still expects the slot account in the instruction
    /// account list; the program reads it conditionally.
    pub pyth_oracle: Pubkey,
    pub switchboard_oracle: Pubkey,
}

/// Obligation refresh payload. `None` means the caller is on the
/// first-deposit path: there is no obligation account on chain yet,
/// so no `RefreshObligation` instruction is emitted. Reserves still
/// need refreshing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObligationRefreshInput {
    pub obligation_pubkey: Pubkey,
    /// Reserves the obligation references — deposits first, then
    /// borrows, in the order the obligation stores them. Solend's
    /// `RefreshObligation` walks this list to recompute the obligation's
    /// derived values from the (just-refreshed) reserves.
    pub referenced_reserves_in_order: Vec<Pubkey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshPlanInputs {
    pub solend_program_id: Pubkey,
    /// Reserves to refresh. Order is preserved in the output instruction
    /// list. Caller is responsible for including every reserve that any
    /// downstream policy step (or the deposit ix itself) will require to
    /// be fresh.
    pub reserves: Vec<ReserveRefreshInput>,
    /// `Some` for an existing-obligation deposit; `None` for a
    /// first-deposit path.
    pub obligation: Option<ObligationRefreshInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshPlan {
    /// Refresh instructions in dispatch order: every `RefreshReserve`
    /// first, then (if any) `RefreshObligation` last. This ordering is
    /// load-bearing: Solend's `RefreshObligation` walks the just-refreshed
    /// reserves and would otherwise read stale values.
    pub instructions: Vec<Instruction>,
    /// Echo of which reserves the plan refreshes; for audit and tests.
    pub reserves_refreshed: Vec<Pubkey>,
    /// Echo of which obligation the plan refreshes (if any).
    pub obligation_refreshed: Option<Pubkey>,
}

/// Pure builder. No RPC. No `LendingSnapshot` dependency. Stale markers
/// on any snapshot the caller may hold are NOT mutated.
///
/// Caller's responsibility AFTER this returns:
///   1. Assemble [`RefreshPlan::instructions`] into a transaction.
///   2. Sign and submit through the existing Phantom / daemon path.
///   3. Await finalization.
///   4. Re-fetch the obligation and the reserves.
///   5. Re-decode through `raw::decode_*`.
///   6. Re-build a fresh `LendingSnapshot` via
///      [`crate::integrations::solend::mapping::map_snapshot`] (or
///      [`crate::integrations::solend::mapping::map_snapshot_for_first_deposit`]).
///   7. Run the full three-layer gate sequence and the evaluator AGAINST
///      THE NEW SNAPSHOT. The evaluator never sees these refresh
///      instructions and never knows refresh happened.
pub fn build_refresh_instructions(inputs: RefreshPlanInputs) -> RefreshPlan {
    let RefreshPlanInputs {
        solend_program_id,
        reserves,
        obligation,
    } = inputs;

    let mut instructions: Vec<Instruction> = Vec::with_capacity(reserves.len() + 1);
    let mut reserves_refreshed: Vec<Pubkey> = Vec::with_capacity(reserves.len());

    for r in &reserves {
        instructions.push(Instruction {
            program_id: solend_program_id,
            accounts: vec![
                AccountMeta::new(r.reserve_pubkey, false),
                AccountMeta::new_readonly(r.pyth_oracle, false),
                AccountMeta::new_readonly(r.switchboard_oracle, false),
                AccountMeta::new_readonly(sysvar::clock::id(), false),
            ],
            data: vec![SOLEND_IX_TAG_REFRESH_RESERVE],
        });
        reserves_refreshed.push(r.reserve_pubkey);
    }

    let obligation_refreshed = obligation.as_ref().map(|o| o.obligation_pubkey);
    if let Some(o) = obligation {
        // Phase 6I-H — the deployed mainnet `RefreshObligation` handler
        // reads the clock via `Clock::get()?` syscall and does NOT take
        // a clock account in its account list. Account layout is:
        //
        //   [0] obligation (writable)
        //   [1..1 + N] referenced reserves in obligation order
        //              (deposits first, then borrows; each readonly)
        //
        // Including a clock account here shifts the reserve list by one
        // slot, causing `process_refresh_obligation` to interpret the
        // clock sysvar as `obligation.deposits[0].deposit_reserve` and
        // reject it with `LendingError::InvalidAccountOwner` (the
        // sysvar is not owned by the lending program). See the failed
        // live tx `4YnQFUTm…` for the original symptom.
        let mut accounts: Vec<AccountMeta> =
            Vec::with_capacity(1 + o.referenced_reserves_in_order.len());
        accounts.push(AccountMeta::new(o.obligation_pubkey, false));
        for r in &o.referenced_reserves_in_order {
            accounts.push(AccountMeta::new_readonly(*r, false));
        }
        instructions.push(Instruction {
            program_id: solend_program_id,
            accounts,
            data: vec![SOLEND_IX_TAG_REFRESH_OBLIGATION],
        });
    }

    RefreshPlan {
        instructions,
        reserves_refreshed,
        obligation_refreshed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::raw::SOLEND_NULL_ORACLE_SENTINEL_BS58;
    use std::str::FromStr;

    fn solend_program() -> Pubkey {
        Pubkey::from_str("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo").unwrap()
    }

    #[test]
    fn first_deposit_refresh_plan_emits_only_reserve_ixs_no_obligation() {
        // First-deposit path: obligation account does not exist yet, so
        // RefreshObligation is omitted. Only RefreshReserve(s) emitted.
        let pyth = Pubkey::new_unique();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();
        let reserve_pk = Pubkey::new_unique();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![ReserveRefreshInput {
                reserve_pubkey: reserve_pk,
                pyth_oracle: pyth,
                switchboard_oracle: sentinel,
            }],
            obligation: None,
        });
        assert_eq!(plan.instructions.len(), 1);
        assert_eq!(plan.reserves_refreshed, vec![reserve_pk]);
        assert!(plan.obligation_refreshed.is_none());
        let ix = &plan.instructions[0];
        assert_eq!(ix.program_id, solend_program());
        assert_eq!(ix.data, vec![SOLEND_IX_TAG_REFRESH_RESERVE]);
        // Account ordering matches the cited Solend layout.
        assert_eq!(ix.accounts.len(), 4);
        assert_eq!(ix.accounts[0].pubkey, reserve_pk);
        assert!(ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[1].pubkey, pyth);
        assert!(!ix.accounts[1].is_writable);
        assert_eq!(ix.accounts[2].pubkey, sentinel);
        assert!(!ix.accounts[2].is_writable);
        assert_eq!(ix.accounts[3].pubkey, sysvar::clock::id());
    }

    #[test]
    fn existing_obligation_refresh_plan_emits_reserve_then_obligation_ixs() {
        let pyth_a = Pubkey::new_unique();
        let pyth_b = Pubkey::new_unique();
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().unwrap();
        let reserve_a = Pubkey::new_unique();
        let reserve_b = Pubkey::new_unique();
        let obligation = Pubkey::new_unique();

        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![
                ReserveRefreshInput {
                    reserve_pubkey: reserve_a,
                    pyth_oracle: pyth_a,
                    switchboard_oracle: sentinel,
                },
                ReserveRefreshInput {
                    reserve_pubkey: reserve_b,
                    pyth_oracle: sentinel,
                    switchboard_oracle: pyth_b, // arbitrary; testing pubkey passthrough
                },
            ],
            obligation: Some(ObligationRefreshInput {
                obligation_pubkey: obligation,
                referenced_reserves_in_order: vec![reserve_a, reserve_b],
            }),
        });
        assert_eq!(plan.instructions.len(), 3);
        assert_eq!(plan.reserves_refreshed, vec![reserve_a, reserve_b]);
        assert_eq!(plan.obligation_refreshed, Some(obligation));

        // Refresh-reserve ixs come first.
        assert_eq!(plan.instructions[0].data, vec![SOLEND_IX_TAG_REFRESH_RESERVE]);
        assert_eq!(plan.instructions[0].accounts[0].pubkey, reserve_a);
        assert_eq!(plan.instructions[1].data, vec![SOLEND_IX_TAG_REFRESH_RESERVE]);
        assert_eq!(plan.instructions[1].accounts[0].pubkey, reserve_b);

        // Refresh-obligation ix comes last.
        // Phase 6I-H: account list is [obligation, ...reserves]. The
        // clock account that this test previously asserted at slot 1
        // was the bug — see module doc-comment.
        let obl_ix = &plan.instructions[2];
        assert_eq!(obl_ix.data, vec![SOLEND_IX_TAG_REFRESH_OBLIGATION]);
        assert_eq!(obl_ix.accounts.len(), 1 + 2); // obligation + 2 reserves (NO clock)
        assert_eq!(obl_ix.accounts[0].pubkey, obligation);
        assert!(obl_ix.accounts[0].is_writable);
        // Reserves start at slot 1 — multi-reserve order is preserved.
        assert_eq!(obl_ix.accounts[1].pubkey, reserve_a);
        assert!(!obl_ix.accounts[1].is_writable);
        assert_eq!(obl_ix.accounts[2].pubkey, reserve_b);
        assert!(!obl_ix.accounts[2].is_writable);
    }

    #[test]
    fn empty_plan_with_no_reserves_and_no_obligation_emits_nothing() {
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![],
            obligation: None,
        });
        assert!(plan.instructions.is_empty());
        assert!(plan.reserves_refreshed.is_empty());
        assert!(plan.obligation_refreshed.is_none());
    }

    #[test]
    fn obligation_only_with_no_reserves_still_emits_refresh_obligation() {
        // Edge case: caller asks to refresh just the obligation against
        // a snapshot of already-fresh reserves. Phase 6I-H: account list
        // is just [obligation] when no reserves are referenced — the
        // clock account is no longer present.
        let obligation = Pubkey::new_unique();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![],
            obligation: Some(ObligationRefreshInput {
                obligation_pubkey: obligation,
                referenced_reserves_in_order: vec![],
            }),
        });
        assert_eq!(plan.instructions.len(), 1);
        assert_eq!(plan.instructions[0].data, vec![SOLEND_IX_TAG_REFRESH_OBLIGATION]);
        assert_eq!(plan.instructions[0].accounts.len(), 1); // obligation only
        assert_eq!(plan.instructions[0].accounts[0].pubkey, obligation);
        assert_eq!(plan.obligation_refreshed, Some(obligation));
    }

    // ── Phase 6I-H regression tests ──────────────────────────────────────

    /// RefreshObligation account list length is exactly
    /// `1 + referenced_reserves.len()`. Locks against any reintroduction
    /// of the clock-as-account bug or any other slot insertion.
    #[test]
    fn refresh_obligation_account_list_length_is_one_plus_reserves() {
        let obligation = Pubkey::new_unique();
        let r0 = Pubkey::new_unique();
        let r1 = Pubkey::new_unique();
        let r2 = Pubkey::new_unique();

        for (label, reserves, expected_len) in [
            ("0 reserves", vec![], 1),
            ("1 reserve", vec![r0], 2),
            ("2 reserves", vec![r0, r1], 3),
            ("3 reserves", vec![r0, r1, r2], 4),
        ] {
            let plan = build_refresh_instructions(RefreshPlanInputs {
                solend_program_id: solend_program(),
                reserves: vec![],
                obligation: Some(ObligationRefreshInput {
                    obligation_pubkey: obligation,
                    referenced_reserves_in_order: reserves,
                }),
            });
            let obl_ix = plan
                .instructions
                .iter()
                .find(|i| i.data == vec![SOLEND_IX_TAG_REFRESH_OBLIGATION])
                .expect("RefreshObligation ix present");
            assert_eq!(
                obl_ix.accounts.len(),
                expected_len,
                "[{label}] account list length"
            );
        }
    }

    /// Slot 0 is the obligation; slot 1 is the FIRST referenced reserve
    /// (NOT the clock sysvar).
    #[test]
    fn refresh_obligation_slot_zero_is_obligation_slot_one_is_first_reserve() {
        let obligation = Pubkey::new_unique();
        let first_reserve = Pubkey::new_unique();
        let second_reserve = Pubkey::new_unique();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![],
            obligation: Some(ObligationRefreshInput {
                obligation_pubkey: obligation,
                referenced_reserves_in_order: vec![first_reserve, second_reserve],
            }),
        });
        let obl_ix = &plan.instructions[0];
        assert_eq!(obl_ix.accounts[0].pubkey, obligation, "slot 0 = obligation");
        assert!(obl_ix.accounts[0].is_writable);
        assert_eq!(obl_ix.accounts[1].pubkey, first_reserve, "slot 1 = first reserve (no clock)");
        assert!(!obl_ix.accounts[1].is_writable);
        assert_eq!(obl_ix.accounts[2].pubkey, second_reserve, "slot 2 = second reserve");
    }

    /// Phase 6I-H regression guard. `sysvar::clock::id()` MUST NOT
    /// appear anywhere in the RefreshObligation account list. The
    /// deployed Solend program reads the clock via `Clock::get()?`
    /// syscall and rejects the sysvar account if supplied.
    #[test]
    fn refresh_obligation_must_not_contain_clock_sysvar() {
        let obligation = Pubkey::new_unique();
        let r = Pubkey::new_unique();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![],
            obligation: Some(ObligationRefreshInput {
                obligation_pubkey: obligation,
                referenced_reserves_in_order: vec![r],
            }),
        });
        let obl_ix = &plan.instructions[0];
        for (i, a) in obl_ix.accounts.iter().enumerate() {
            assert_ne!(
                a.pubkey,
                sysvar::clock::id(),
                "clock sysvar must not appear in RefreshObligation accounts (slot={i})"
            );
        }
    }

    /// HcKrv5Jo / Solend Main Pool USDC fixture: the on-chain obligation
    /// has a single deposit on the USDC reserve. After the fix, the
    /// reserve must land at account[1] of the RefreshObligation ix
    /// (NOT account[2] as the buggy [obligation, clock, reserve...]
    /// layout produced).
    #[test]
    fn hckrv5jo_usdc_reserve_is_at_slot_one_not_slot_two() {
        let obligation = Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV")
            .unwrap();
        let usdc_reserve = Pubkey::from_str("BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw")
            .unwrap();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![],
            obligation: Some(ObligationRefreshInput {
                obligation_pubkey: obligation,
                // Single deposit on the USDC reserve, no borrows — same
                // shape Phase 6H reports for HcKrv5Jo.
                referenced_reserves_in_order: vec![usdc_reserve],
            }),
        });
        let obl_ix = &plan.instructions[0];
        assert_eq!(obl_ix.accounts.len(), 2, "[obligation, usdc_reserve] only");
        assert_eq!(obl_ix.accounts[0].pubkey, obligation);
        assert_eq!(
            obl_ix.accounts[1].pubkey, usdc_reserve,
            "USDC reserve must be at slot 1 (the first slot the Solend program iterates as collateral 0)"
        );
    }

    /// Multi-reserve obligation: order is preserved verbatim, with no
    /// gaps and no reordering. A future change that sorts or
    /// deduplicates inside the builder would break this.
    #[test]
    fn refresh_obligation_preserves_multi_reserve_order_exactly() {
        let obligation = Pubkey::new_unique();
        let reserves: Vec<Pubkey> = (0..5).map(|_| Pubkey::new_unique()).collect();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![],
            obligation: Some(ObligationRefreshInput {
                obligation_pubkey: obligation,
                referenced_reserves_in_order: reserves.clone(),
            }),
        });
        let obl_ix = &plan.instructions[0];
        assert_eq!(obl_ix.accounts.len(), 1 + 5);
        assert_eq!(obl_ix.accounts[0].pubkey, obligation);
        for (i, expected) in reserves.iter().enumerate() {
            assert_eq!(
                obl_ix.accounts[1 + i].pubkey,
                *expected,
                "reserve order mismatch at index {i}"
            );
        }
    }

    /// RefreshReserve still includes the clock sysvar at slot 3.
    /// Phase 6I-H must NOT regress the RefreshReserve shape — the live
    /// failure happened at RefreshObligation only; RefreshReserve in
    /// the same failed tx succeeded with the existing layout.
    #[test]
    fn refresh_reserve_still_includes_clock_sysvar_unchanged() {
        let pyth = Pubkey::new_unique();
        let switchboard = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let plan = build_refresh_instructions(RefreshPlanInputs {
            solend_program_id: solend_program(),
            reserves: vec![ReserveRefreshInput {
                reserve_pubkey: reserve,
                pyth_oracle: pyth,
                switchboard_oracle: switchboard,
            }],
            obligation: None,
        });
        let res_ix = plan
            .instructions
            .iter()
            .find(|i| i.data == vec![SOLEND_IX_TAG_REFRESH_RESERVE])
            .expect("RefreshReserve ix present");
        assert_eq!(res_ix.accounts.len(), 4, "RefreshReserve still has 4 accounts");
        assert_eq!(res_ix.accounts[0].pubkey, reserve);
        assert!(res_ix.accounts[0].is_writable);
        assert_eq!(res_ix.accounts[1].pubkey, pyth);
        assert_eq!(res_ix.accounts[2].pubkey, switchboard);
        assert_eq!(
            res_ix.accounts[3].pubkey,
            sysvar::clock::id(),
            "RefreshReserve slot 3 must remain the clock sysvar (Phase 6I-H scope is RefreshObligation only)"
        );
    }

    #[test]
    fn refresh_builder_does_not_depend_on_lending_snapshot_types() {
        // Compile-time evidence: the builder's signature and types do
        // not mention any `crate::lending::*` symbol. This test compiles
        // iff that property holds (no `use crate::lending::*` in this
        // file's scope makes the assertion structural).
        fn _accept(_: RefreshPlanInputs) -> RefreshPlan {
            build_refresh_instructions(RefreshPlanInputs {
                solend_program_id: Pubkey::new_unique(),
                reserves: vec![],
                obligation: None,
            })
        }
        let _ = _accept as fn(RefreshPlanInputs) -> RefreshPlan;
    }
}

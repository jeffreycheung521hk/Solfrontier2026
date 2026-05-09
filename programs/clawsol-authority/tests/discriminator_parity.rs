//! Cross-crate discriminator parity.
//!
//! Asserts that the on-chain `Stage2ActionType::to_u8` values
//! (programs/clawsol-authority/src/state.rs) match the substrate-side
//! `WatchRuleActionType::to_u8` values (crates/types/src/stage2_watch_rule.rs).
//!
//! If either side is renumbered without the other, this test fails
//! loudly — keeping the on-chain `allowed_action_type` byte and the
//! off-chain canonical-rule `action_type` in lockstep.

use claw_types::stage2_watch_rule::WatchRuleActionType;
use clawsol_authority::state::Stage2ActionType;

#[test]
fn solend_withdraw_all_delegated_discriminators_agree() {
    assert_eq!(
        Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
        WatchRuleActionType::SolendWithdrawAllDelegated.to_u8(),
    );
}

#[test]
fn jupiter_buy_sol_with_usdc_discriminators_agree() {
    assert_eq!(
        Stage2ActionType::JupiterBuySolWithUsdc.to_u8(),
        WatchRuleActionType::JupiterBuySolWithUsdc.to_u8(),
    );
}

#[test]
fn rejected_bytes_agree() {
    // Bytes the substrate type rejects must also be bytes the on-chain
    // type rejects, and vice versa. We exhaust the relevant set
    // (zero-default plus a sample of out-of-range bytes).
    for v in [0u8, 3, 4, 100, 255] {
        assert_eq!(
            WatchRuleActionType::from_u8(v).map(|t| t.to_u8()),
            Stage2ActionType::from_u8(v).map(|t| t.to_u8()),
            "discriminator mapping diverges at byte {v}",
        );
    }
}

#[test]
fn full_byte_range_agrees() {
    // Belt-and-braces: walk the entire u8 space and assert that for
    // every byte value, both sides agree on accept/reject and on the
    // resulting discriminator byte.
    for v in 0u8..=u8::MAX {
        let auth = Stage2ActionType::from_u8(v).map(|t| t.to_u8());
        let types = WatchRuleActionType::from_u8(v).map(|t| t.to_u8());
        assert_eq!(auth, types, "byte {v} disagrees: auth={auth:?} types={types:?}");
    }
}

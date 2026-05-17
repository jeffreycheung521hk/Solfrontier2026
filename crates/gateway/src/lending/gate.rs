//! Minimal System Invariant Gate surface.
//!
//! Spec anchor: `docs/lending_policy_vocabulary.md` §51.
//!
//! In the full policy pipeline the gate sits between the assembler
//! (which returns a complete [`LendingSnapshot`]) and the evaluator
//! (which consumes it). Slice 1 implements only the checks the
//! read-model tests need to observe:
//!
//! - Owner check: `snapshot.obligation.owner == expected_session_wallet`
//! - Protocol-tag check: `snapshot.protocol_tag == expected_protocol`
//!
//! Freshness / age / oracle checks belong to the rule evaluator and are
//! intentionally absent from this gate per §51.2.
//!
//! This is a pure function. It does not read RPC, does not read a clock,
//! does not depend on ambient state.

use solana_sdk::pubkey::Pubkey;

use super::snapshot::{LendingSnapshot, ProtocolTag};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SystemInvariantError {
    #[error("owner mismatch: snapshot obligation owner {snapshot_owner} != session wallet {session_wallet}")]
    OwnerMismatch {
        snapshot_owner: Pubkey,
        session_wallet: Pubkey,
    },
    #[error("protocol-tag mismatch: snapshot {snapshot:?} != expected {expected:?}")]
    ProtocolTagMismatch {
        snapshot: ProtocolTag,
        expected: ProtocolTag,
    },
}

/// Verify the structural preconditions Part 3B §24 requires before rule
/// evaluation. Pure over `(snapshot, expected_session_wallet, expected_protocol)`.
pub fn check_system_invariants(
    snapshot: &LendingSnapshot,
    expected_session_wallet: &Pubkey,
    expected_protocol: ProtocolTag,
) -> Result<(), SystemInvariantError> {
    if snapshot.protocol_tag != expected_protocol {
        return Err(SystemInvariantError::ProtocolTagMismatch {
            snapshot: snapshot.protocol_tag,
            expected: expected_protocol,
        });
    }
    if snapshot.obligation.owner != *expected_session_wallet {
        return Err(SystemInvariantError::OwnerMismatch {
            snapshot_owner: snapshot.obligation.owner,
            session_wallet: *expected_session_wallet,
        });
    }
    Ok(())
}

//! Authorization PDA on-chain record.
//!
//! Stage 2 P1 — substrate only. This module pins the exact wire shape
//! the on-chain program writes into the PDA's data buffer and exposes
//! the action-type discriminator the program uses to fail-closed on
//! unknown actions.
//!
//! # What this PDA stores
//!
//! The user-signed bound contract that authorizes a delegated daemon
//! to dispatch ONE specific watch-rule action within ONE specific
//! amount cap before ONE specific expiry slot — and nothing else.
//! Concretely, the [`AuthorizationRecord`] commits to:
//!
//! - the user's identity (signs creation, must match for revoke),
//! - the executor's identity (the only signer the eventual
//!   `execute_action` ix accepts),
//! - the delegated wallet that owns the actual position
//!   (per Stage 2 spec § 5.1 the obligation owner is this wallet,
//!   never the user's main wallet),
//! - the canonical hash of the full [`WatchRule`] (so the rule body —
//!   conditions, action payload, oracle params — is committed without
//!   being stored on chain),
//! - the discriminator of the rule's action type (fast-path filter
//!   so `execute_action` can reject mismatches before doing
//!   anything else),
//! - the input cap, the destination, and the expiry,
//! - replay-protection flags (`revoked`, `completed`) and bookkeeping
//!   fields (`execution_nonce`, `last_execution_slot`).
//!
//! [`WatchRule`]: claw_types::stage2_watch_rule::WatchRule
//!
//! # What this PDA does NOT store
//!
//! - the conditions vector,
//! - the action payload (input/output mints, slippage, target obligation,
//!   reserve pubkey, …),
//! - any oracle freshness parameters.
//!
//! All of those live in the off-chain [`WatchRule`] and are committed
//! via `canonical_rule_hash`. Storing them again here would duplicate
//! truth (and inflate space cost) without adding security: the hash is
//! sufficient to prove the daemon is dispatching against the rule the
//! user signed.
//!
//! # Borsh wire shape
//!
//! Borsh serializes structs in declared field order, integers
//! little-endian, fixed `[u8; N]` arrays without a length prefix
//! (https://borsh.io). The fields below are listed in the order the
//! on-chain program writes them; reordering is a wire break.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

/// Stage 2 schema version pinned by this build of the program.
///
/// Mirrors `claw_types::stage2_watch_rule::STAGE2_WATCH_RULE_SCHEMA_VERSION`.
/// The two values MUST agree — a daemon producing a `WatchRule` at
/// schema_version N MUST submit the matching authorization at the
/// same version, and the program rejects any other value to keep the
/// canonical-rule-hash domain isolated from older / future shapes.
pub const STAGE2_AUTHORITY_SCHEMA_VERSION: u8 = 1;

/// Discriminator for the rule action this authorization is allowed
/// to execute. `u8` on the wire to keep the PDA fixed-size; values
/// are mirrored in `claw_types::stage2_watch_rule::WatchRuleActionType`
/// (see the `discriminator_parity` test in `tests/`).
///
/// `0` is intentionally NOT used so that a default-zeroed buffer
/// cannot be interpreted as a valid action type — see the comment on
/// `WatchRuleActionType::to_u8` in the substrate types crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage2ActionType {
    SolendWithdrawAllDelegated = 1,
    JupiterBuySolWithUsdc = 2,
}

impl Stage2ActionType {
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::SolendWithdrawAllDelegated),
            2 => Some(Self::JupiterBuySolWithUsdc),
            _ => None,
        }
    }
}

/// On-chain Authorization record (Stage 2 P1 skeleton).
///
/// Replay-protection fields per Stage 2 spec § 3.1:
///   - `revoked`        — set true by `revoke_authorization`.
///   - `completed`      — reserved for the eventual `execute_action`
///                        processor (P1 leaves this `false`).
///   - `execution_nonce` — monotonically incremented on each
///                        successful execute (P1 leaves this `0`).
///   - `last_execution_slot` — set by execute (P1 leaves this `0`).
///
/// The struct is fixed-size — no `Vec` / `Option` / `String` — so its
/// Borsh serialization length is a compile-time sum and every byte
/// position is anchored by [`AuthorizationRecord::LEN`].
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationRecord {
    pub schema_version: u8,
    pub rule_id: [u8; 16],
    pub user: Pubkey,
    pub executor: Pubkey,
    pub delegated_wallet: Pubkey,
    pub canonical_rule_hash: [u8; 32],
    pub allowed_action_type: u8,
    pub max_input_amount_raw: u64,
    pub used_amount_raw: u64,
    pub destination: Pubkey,
    pub expires_at_slot: u64,
    pub revoked: bool,
    pub completed: bool,
    pub execution_nonce: u64,
    pub last_execution_slot: u64,
    pub bump: u8,
}

impl AuthorizationRecord {
    /// Borsh-serialized length in bytes. Fixed because the struct has
    /// no variable-length fields.
    pub const LEN: usize = 1   // schema_version
        + 16  // rule_id
        + 32  // user
        + 32  // executor
        + 32  // delegated_wallet
        + 32  // canonical_rule_hash
        + 1   // allowed_action_type
        + 8   // max_input_amount_raw
        + 8   // used_amount_raw
        + 32  // destination
        + 8   // expires_at_slot
        + 1   // revoked  (Borsh bool = 1 byte)
        + 1   // completed
        + 8   // execution_nonce
        + 8   // last_execution_slot
        + 1; // bump
}

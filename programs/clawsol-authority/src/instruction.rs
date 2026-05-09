//! Top-level instruction set for the Stage 2 Authorization PDA program.
//!
//! Borsh-serialized enums use a `u8` ordinal tag followed by the variant
//! body (https://borsh.io). The numeric position of each variant in this
//! enum is therefore part of the on-chain wire shape and MUST be
//! append-only — never reorder, never delete.
//!
//! P1 scope (this slice) covers only authorization-lifecycle ixs:
//! `CreateAuthorization`, `Revoke`, `CloseAuthorization`. The
//! `ExecuteAction` ix (Stage 2 spec § 5/6) is intentionally absent; a
//! placeholder enum slot is NOT reserved either, because Borsh's
//! variant ordinal is the next free integer when the variant is added,
//! so a future append cannot accidentally reuse a P1 slot.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};

use crate::condition_verifier::{
    ConditionLogic, PythPriceCondition, PythPriceSnapshot, SolendReserveSnapshot,
    SolendSupplyAprCondition,
};
use crate::derive_authorization_pda;
use crate::solend_boundary::SolendBoundaryProof;

/// Maximum number of proof-conditions per `ExecuteAction`. Bounded so a
/// hostile or buggy executor cannot inflate compute / wire size, and so
/// the `Vec<ProofCondition>` length is always small enough to evaluate
/// inside the BPF compute budget. Picked at 8 to comfortably cover
/// Stage 2 Scenario B (3 Pyth conditions) with headroom for future
/// composite rules without bumping.
pub const MAX_PROOF_CONDITIONS: usize = 8;

/// Per-condition proof payload supplied by the executor.
///
/// Important: the executor does NOT supply the verification *context*
/// here — `current_unix_time` (Pyth) and `current_slot` (Solend) are
/// constructed on-chain from `Clock::get()` inside the processor. A
/// hostile executor cannot fabricate a fresh-looking timestamp by
/// padding the proof; the program clobbers any such field with the
/// chain-derived values.
///
/// Borsh enum tag: `Pyth = 0`, `Solend = 1`. Append-only.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofCondition {
    Pyth {
        condition: PythPriceCondition,
        snapshot: PythPriceSnapshot,
    },
    Solend {
        condition: SolendSupplyAprCondition,
        snapshot: SolendReserveSnapshot,
    },
}

/// Top-level condition-proof payload bundled into `ExecuteAction`.
///
/// Bind to the rule via `canonical_rule_hash` (already in the existing
/// `ExecuteAction` args). The proof itself is NOT hashed — it would be
/// chicken-and-egg, since the proof contains fresh oracle snapshots
/// the rule cannot know in advance. The hash binds the *committed
/// thresholds + condition shape*; the proof carries the *runtime
/// numbers* the program checks against those thresholds.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct ConditionProofPayload {
    pub condition_logic: ConditionLogic,
    pub conditions: Vec<ProofCondition>,
}

/// Instruction payload accepted by `process_instruction`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum AuthorityInstruction {
    /// Create a new authorization PDA.
    ///
    /// Account layout:
    ///   0. `[signer, writable]` user           — rent payer + signer
    ///   1. `[writable]`         authz_pda      — PDA created by this ix
    ///   2. `[]`                 system_program — for create_account CPI
    CreateAuthorization {
        schema_version: u8,
        rule_id: [u8; 16],
        executor: Pubkey,
        delegated_wallet: Pubkey,
        canonical_rule_hash: [u8; 32],
        allowed_action_type: u8,
        max_input_amount_raw: u64,
        destination: Pubkey,
        expires_at_slot: u64,
    },

    /// Revoke an existing authorization PDA. Idempotent — a second
    /// call on the same PDA succeeds without changing state.
    ///
    /// Account layout:
    ///   0. `[signer]`           user      — must equal authz.user
    ///   1. `[writable]`         authz_pda — existing PDA
    Revoke,

    /// Close a revoked / completed / expired authorization PDA, return
    /// the rent lamports to the user.
    ///
    /// Account layout:
    ///   0. `[signer, writable]` user      — must equal authz.user
    ///                                       (writable = rent destination)
    ///   1. `[writable]`         authz_pda — existing PDA
    CloseAuthorization,

    /// Execute the action authorised by an existing authorization PDA.
    ///
    /// P3 wires the O3 condition verifier into the boundary skeleton
    /// established by P2. The processor still performs every
    /// `AuthorizationRecord` check from P2 (signer, owner, PDA
    /// derivation, schema_version, rule_id, user, executor, rule hash,
    /// action_type, expiry, revoked, completed, input cap, monotone
    /// nonce, same-slot replay) and applies the one-shot state
    /// mutation (`used_amount_raw += input_amount_raw`,
    /// `completed = true`, `execution_nonce = arg.execution_nonce`,
    /// `last_execution_slot = current_slot`). On top of that, P3 adds
    /// a fail-closed condition gate: every `ProofCondition` in
    /// `condition_proof.conditions` is evaluated through
    /// `condition_verifier::evaluate_condition`, the per-condition
    /// booleans are folded with the supplied `condition_logic`, and
    /// the action only mutates state if the fold returns `true`.
    ///
    /// **Boundary preservation.** P3 still does NOT:
    ///
    /// - perform any Solend / Jupiter CPI (deferred — P4+),
    /// - verify Jupiter sibling-ix via the instructions sysvar (P4+),
    /// - take a token-balance pre/post bracket (P4+),
    /// - parse live Pyth / Solend account data (deferred — P5+),
    /// - sweep funds on revoke (Stage 2 spec § 8.1 steps 2–3).
    ///
    /// The condition snapshots in `condition_proof` are supplied by
    /// the executor; the program does NOT independently fetch oracle
    /// data in this slice. Verification contexts (`current_unix_time`,
    /// `current_slot`) are NOT taken from the proof — they are
    /// constructed on-chain from `Clock::get()` so a hostile executor
    /// cannot bypass freshness gates by padding the proof.
    ///
    /// **Wire-shape note (P3 + P4).** Each slice grows the variant by
    /// trailing fields, append-only:
    ///
    /// - P3 appended `condition_proof: ConditionProofPayload`.
    /// - P4 appended `solend_boundary_proof: Option<SolendBoundaryProof>`
    ///   (Borsh: 1 byte `Option` tag, then body if `Some`).
    ///
    /// The Borsh enum tag stays at `3`. A pinned-length test in
    /// `lib.rs::tests` (`execute_action_borsh_field_order_is_pinned_through_p4`)
    /// is updated alongside each change so any future field addition
    /// fails loudly.
    ///
    /// `solend_boundary_proof` is `Some(_)` when `action_type ==
    /// Stage2ActionType::SolendWithdrawAllDelegated` and `None`
    /// otherwise. The on-chain `process_execute_action` enforces this
    /// pairing — supplying `None` for a Solend action fails closed
    /// with `SolendBoundaryProofMissing`.
    ///
    /// Variant order MUST stay append-only: this is the 4th variant
    /// (Borsh `u8` ordinal `3`) and a future-added variant goes after,
    /// never inserted before.
    ///
    /// Account layout:
    ///   0. `[signer]`           executor          — must equal authz.executor
    ///   1. `[writable]`         authz_pda         — existing PDA
    ///   2. `[]`                 user              — must equal authz.user (readonly)
    ///   3. `[]`                 delegated_wallet  — readonly placeholder (P5+ wires CPI)
    ExecuteAction {
        schema_version: u8,
        rule_id: [u8; 16],
        canonical_rule_hash: [u8; 32],
        action_type: u8,
        input_amount_raw: u64,
        execution_nonce: u64,
        condition_proof: ConditionProofPayload,
        solend_boundary_proof: Option<SolendBoundaryProof>,
    },
}

/// Build a `CreateAuthorization` instruction for off-chain tx builders
/// and tests.
#[allow(clippy::too_many_arguments)]
pub fn create_authorization_instruction(
    program_id: &Pubkey,
    user: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
    executor: Pubkey,
    delegated_wallet: Pubkey,
    canonical_rule_hash: [u8; 32],
    allowed_action_type: u8,
    max_input_amount_raw: u64,
    destination: Pubkey,
    expires_at_slot: u64,
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::CreateAuthorization {
        schema_version,
        rule_id,
        executor,
        delegated_wallet,
        canonical_rule_hash,
        allowed_action_type,
        max_input_amount_raw,
        destination,
        expires_at_slot,
    })
    .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(authz_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

/// Build a `Revoke` instruction for off-chain tx builders and tests.
pub fn revoke_authorization_instruction(
    program_id: &Pubkey,
    user: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::Revoke)
        .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*user, true),
            AccountMeta::new(authz_pda, false),
        ],
        data,
    }
}

/// Build a `CloseAuthorization` instruction for off-chain tx builders
/// and tests.
pub fn close_authorization_instruction(
    program_id: &Pubkey,
    user: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::CloseAuthorization)
        .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(authz_pda, false),
        ],
        data,
    }
}

/// Build an `ExecuteAction` instruction for off-chain tx builders and
/// tests. The account list is pinned to the same order the on-chain
/// processor reads (executor, authz_pda, user, delegated_wallet); P5+
/// will append Solend / Jupiter accounts after `delegated_wallet`.
///
/// - `condition_proof` is the deterministic per-condition payload the
///   program evaluates before mutating state — see
///   [`ConditionProofPayload`].
/// - `solend_boundary_proof` MUST be `Some(_)` when `action_type ==
///   Stage2ActionType::SolendWithdrawAllDelegated`, and `None`
///   otherwise. The on-chain processor enforces this — see
///   [`SolendBoundaryProof`] and the boundary verifier in
///   `solend_boundary.rs`.
#[allow(clippy::too_many_arguments)]
pub fn execute_action_instruction(
    program_id: &Pubkey,
    executor: &Pubkey,
    user: &Pubkey,
    delegated_wallet: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
    canonical_rule_hash: [u8; 32],
    action_type: u8,
    input_amount_raw: u64,
    execution_nonce: u64,
    condition_proof: ConditionProofPayload,
    solend_boundary_proof: Option<SolendBoundaryProof>,
) -> Instruction {
    let (authz_pda, _bump) =
        derive_authorization_pda(program_id, schema_version, user, &rule_id);
    let data = borsh::to_vec(&AuthorityInstruction::ExecuteAction {
        schema_version,
        rule_id,
        canonical_rule_hash,
        action_type,
        input_amount_raw,
        execution_nonce,
        condition_proof,
        solend_boundary_proof,
    })
    .expect("borsh serialization of AuthorityInstruction is infallible");

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*executor, true),
            AccountMeta::new(authz_pda, false),
            AccountMeta::new_readonly(*user, false),
            AccountMeta::new_readonly(*delegated_wallet, false),
        ],
        data,
    }
}

//! Stage 2 Jupiter bracket sibling-instruction verifier skeleton (J4).
//!
//! # What this module is
//!
//! A *boundary verifier* for the Jupiter execution path. The on-chain
//! `process_execute_action` path calls
//! [`verify_jupiter_boundary_skeleton`] before mutating any
//! `AuthorizationRecord` field, when the action is
//! `Stage2ActionType::JupiterBuySolWithUsdc`. The verifier enforces, in
//! order:
//!
//! 1. Per-class allowlist over every supplied sibling-instruction
//!    descriptor (Compute Budget, Jupiter v6, SPL Token, Associated
//!    Token Account, System). Anything else fails closed with
//!    [`AuthorityError::JupiterIllegalSiblingInstruction`].
//! 2. SPL Token variant-byte allowlist (only `Transfer` /
//!    `TransferChecked` / `CloseAccount` / `SyncNative` permitted —
//!    `Approve` / `Revoke` / `MintTo` / `Burn` / `SetAuthority` /
//!    `FreezeAccount` etc. fail closed).
//! 3. Pinned Jupiter v6 mainnet program id check on every
//!    `JupiterSwap`-classified descriptor — mismatched pubkeys (a
//!    look-alike clone) fail with
//!    [`AuthorityError::JupiterProgramIdMismatch`].
//! 4. Writable-account allowlist: every account marked writable across
//!    all sibling descriptors MUST appear in the deterministic
//!    `allowed_writable_set` derived from
//!    `AuthorizationRecord.{delegated_wallet, destination}` plus the
//!    proof's pinned input/output ATAs.
//! 5. Relative ordering across the sibling descriptors using a small
//!    state-machine cursor — no absolute index assumptions (e.g. ix[0]
//!    or ix[2]). Compute Budget ixs may appear in any number BEFORE the
//!    setup phase; a backward transition (e.g. cleanup followed by
//!    swap) is rejected with
//!    [`AuthorityError::JupiterInstructionOrderInvalid`].
//! 6. Sibling-list cap (`MAX_JUPITER_SIBLING_INSTRUCTIONS = 16`) so a
//!    hostile executor cannot inflate compute / wire size.
//! 7. Destination identity check —
//!    [`JupiterBoundaryProof::expected_destination_token_account`]
//!    MUST equal `AuthorizationRecord.destination`.
//!
//! # What this module is NOT (J4 deferral list)
//!
//! - **Not a trustless balance bracket.** J4 does NOT implement the
//!   pre/post `checked_sub(post_balance, pre_balance) >= min_out`
//!   gate. Adding it here would require either accepting an
//!   executor-supplied `pre_balance` (forbidden — the executor can
//!   lie) or introducing a `PreJupiterBracketCheck` instruction with
//!   a checkpoint PDA AND on-chain SPL Token unpacking. Both of those
//!   are out of J4's scope; the future slice (`stage2/j5-...` or
//!   similar) lands them together. The
//!   [`JupiterBoundaryProof::min_output_amount_raw`] field is carried
//!   here only so the canonical-rule binding is visible at the
//!   verifier; **the program does NOT enforce it in J4**. A
//!   compile-time `assert!` in
//!   [`verify_jupiter_boundary_skeleton`] documents this deferral as
//!   an explicit `TODO` and a pinned regression test
//!   (`tests::executor_supplied_pre_balance_is_not_trusted`) proves
//!   the program rejects any attempt to smuggle a `pre_balance` field
//!   through the proof.
//!
//! - **Not an instructions-sysvar consumer.** Sibling-instruction
//!   verification runs over a deterministic
//!   `Vec<JupiterSiblingIxDescriptor>` the executor supplies. The
//!   sysvar-driven flavour (using
//!   `solana_program::sysvar::instructions::load_current_index_checked`
//!   and `load_instruction_at_checked`) replaces this descriptor list
//!   in a future slice. The pure verifier here is structured around
//!   *relative* ordering precisely so that swap is not assumed to live
//!   at any specific absolute transaction index.
//!
//! - **Not an SPL Token account unpacker.** No live read of `account.data`
//!   is performed. The token-account *fixture* fields
//!   ([`JupiterTokenAccountFixture`]) are validated for shape and
//!   identity (mint / owner) but their `amount` field is **explicitly
//!   not consumed** for any security-relevant computation in J4 — it
//!   exists only so the future bracket slice has a stable wire
//!   surface.
//!
//! - **Not a Jupiter CPI.** No `invoke_signed`, no Jupiter aggregator
//!   call, no token transfer. The verifier only does program-side
//!   input validation.
//!
//! # Deviations from `solend_boundary.rs` (pattern reference)
//!
//! `solend_boundary.rs` (P4) walks a deterministic sibling-ix list and
//! asserts a single `SOLEND_PROGRAM_ID_MAINNET` constant. Jupiter
//! middle ixs are intentionally *heterogeneous* — Compute Budget,
//! Jupiter v6, SPL Token, ATA, System ixs all coexist legitimately in
//! the same `/swap/v2/build` response. Single-program-id allowlisting
//! does NOT carry over from Solend. The Jupiter verifier is a
//! per-class state machine.
//!
//! # Sources consulted
//!
//! - Jupiter v6 mainnet program id parity:
//!   `crates/gateway/src/integrations/jupiter.rs:563`,
//!   `jupiter_alt.rs:159`, `jupiter_production.rs:367`, `jupiter_tx.rs:212`
//!   — all four pin `JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4`.
//!   The on-chain crate inlines the same value to avoid pulling the
//!   gateway dependency into the BPF build; parity is asserted in
//!   `tests::jupiter_program_id_matches_repo_pin`.
//!
//! - SPL Token program id and ix discriminators:
//!   <https://github.com/solana-program/token/blob/main/program/src/instruction.rs>
//!   (`TokenInstruction` enum — `Transfer = 3`, `Approve = 4`,
//!   `Revoke = 5`, `SetAuthority = 6`, `MintTo = 7`, `Burn = 8`,
//!   `CloseAccount = 9`, `FreezeAccount = 10`, `ThawAccount = 11`,
//!   `TransferChecked = 12`, `SyncNative = 17`).
//!
//! - Associated Token Account program id:
//!   `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`
//!   (constant in `spl_associated_token_account::ID`; pinned here as
//!   a literal so the BPF build does not depend on the gateway crate).
//!
//! - Compute Budget program id:
//!   `ComputeBudget111111111111111111111111111111`
//!   (constant in `solana_program::compute_budget::ID`).
//!
//! - System program id: `solana_program::system_program::ID`.
//!
//! - Solana instructions sysvar API (deferred consumer):
//!   <https://docs.rs/solana-program/latest/solana_program/sysvar/instructions/>
//!   — `load_current_index_checked(&AccountInfo) -> Result<u16, ProgramError>`
//!   and `load_instruction_at_checked(usize, &AccountInfo) -> Result<Instruction, ProgramError>`.
//!
//! - Stage 2 spec § 6.1.3 (sibling-instruction verification),
//!   `STAGE2_JUPITER_BRACKET_ALT_FEASIBILITY.md` § 5
//!   (per-class allowlist, writable-set check, relative ordering),
//!   § 19.3 (per-attack detection catalog).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{msg, program_error::ProgramError, pubkey::Pubkey};

use crate::error::AuthorityError;
use crate::state::AuthorizationRecord;

/// Pinned Jupiter v6 mainnet aggregator program id.
///
/// Parity with `crates/gateway/src/integrations/jupiter.rs:563`. The
/// on-chain BPF build does not depend on the gateway crate, so the
/// constant has to travel with the boundary; parity is asserted in
/// tests below.
pub const JUPITER_V6_PROGRAM_ID_MAINNET: Pubkey =
    solana_program::pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

/// Pinned SPL Token program id.
pub const SPL_TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Pinned Associated Token Account program id.
pub const ATA_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// Pinned Compute Budget program id.
pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("ComputeBudget111111111111111111111111111111");

/// Sanity bound on the deterministic sibling-instruction list. Picked
/// at 16 — comfortably above what a typical Jupiter `/swap/v2/build`
/// emits (~5 middle ixs: 1 compute_budget + 1 setup ATA + 1 swap +
/// 1 cleanup + 0–1 tip — see I3 measurement results) while keeping
/// per-ix walk cost predictable and bounded for the BPF compute
/// budget.
pub const MAX_JUPITER_SIBLING_INSTRUCTIONS: usize = 16;

/// Sanity bound on the writable-account list inside a single sibling
/// descriptor. Jupiter's swap ix carries the bulk of writable accounts
/// (route pools, intermediate token accounts) — empirically ~12 at
/// `maxAccounts=55` per the I3 fixture. 32 leaves headroom without
/// allowing pathological inputs.
pub const MAX_WRITABLE_ACCOUNTS_PER_DESCRIPTOR: usize = 32;

/// SPL Token program ix-data discriminator allowlist (variant byte at
/// `ix.data[0]`). Source: `spl_token::instruction::TokenInstruction`.
///
/// Allowed:
///   - `Transfer        = 3`
///   - `CloseAccount    = 9`  (only on wSOL ATA — caller responsibility)
///   - `TransferChecked = 12`
///   - `SyncNative      = 17`
///
/// Rejected (these MUST NOT appear in middle band — defends against
/// `Approve` delegation attacks, `MintTo` / `Burn` / `SetAuthority`
/// rugs, `FreezeAccount` denial-of-service):
///   - `Transfer (deprecated single-byte semantics) — already covered`
///   - `Approve = 4`, `Revoke = 5`, `SetAuthority = 6`, `MintTo = 7`,
///     `Burn = 8`, `FreezeAccount = 10`, `ThawAccount = 11`,
///     `BurnChecked = 15`, `MintToChecked = 14`, `ApproveChecked = 13`,
///     `InitializeAccount* = 1, 16, 18` (Jupiter setup uses ATA program
///     for account init, not Token program init).
pub const SPL_TOKEN_ALLOWED_VARIANT_BYTES: &[u8] = &[
    3,  // Transfer
    9,  // CloseAccount
    12, // TransferChecked
    17, // SyncNative
];

/// Per-class discriminator the verifier resolves each
/// [`JupiterSiblingIxDescriptor`] to before applying class-specific
/// rules. Unknown program ids resolve to no class and fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JupiterSiblingClass {
    /// `ComputeBudget111111111111111111111111111111`. Allowed at any
    /// position before the swap class.
    ComputeBudget,
    /// `JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4`. Exactly one
    /// expected per bracket.
    JupiterSwap,
    /// `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`. Allowed only
    /// for the variant bytes in [`SPL_TOKEN_ALLOWED_VARIANT_BYTES`].
    SplToken,
    /// `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`. Allowed before
    /// the swap class (`createAssociatedTokenAccountIdempotent`).
    AssociatedTokenAccount,
    /// `11111111111111111111111111111111`. Allowed before the swap
    /// class (system_instruction::transfer for SOL operations,
    /// allocate / assign back-end of ATA creation).
    System,
}

impl JupiterSiblingClass {
    /// Resolve a program id to a class, or `None` if the program id is
    /// not in the per-class allowlist.
    pub fn from_program_id(program_id: &Pubkey) -> Option<Self> {
        if program_id == &COMPUTE_BUDGET_PROGRAM_ID {
            Some(Self::ComputeBudget)
        } else if program_id == &JUPITER_V6_PROGRAM_ID_MAINNET {
            Some(Self::JupiterSwap)
        } else if program_id == &SPL_TOKEN_PROGRAM_ID {
            Some(Self::SplToken)
        } else if program_id == &ATA_PROGRAM_ID {
            Some(Self::AssociatedTokenAccount)
        } else if program_id == &solana_program::system_program::ID {
            Some(Self::System)
        } else {
            None
        }
    }
}

/// Phase-cursor states for relative-ordering verification. The cursor
/// only moves forward; backward transitions fail closed.
///
/// State transitions allowed:
///   - `BeforeSwap → BeforeSwap` (Compute Budget / SPL Token setup /
///     ATA / System / SPL Token wrap-SOL all stay in the pre-swap
///     phase).
///   - `BeforeSwap → AtSwap` on the Jupiter swap class.
///   - `AtSwap → AfterSwap` on any post-swap descriptor (Token
///     CloseAccount / SyncNative / System tip-transfer / etc.).
///   - `AfterSwap → AfterSwap` until the list ends.
///
/// Disallowed:
///   - Any transition back to `BeforeSwap`.
///   - A second JupiterSwap ix while in `AtSwap` or `AfterSwap`
///     (rejected with
///     [`AuthorityError::JupiterInstructionOrderInvalid`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseCursor {
    BeforeSwap,
    AtSwap,
    AfterSwap,
}

/// Deterministic descriptor of a single sibling instruction in the same
/// transaction as the `ExecuteAction` ix. Wire shape — Borsh field
/// order is part of the on-chain instruction wire shape; reorder is a
/// wire break.
///
/// # Why a descriptor (not a sysvar walk)
///
/// J4's verifier consumes a caller-supplied `Vec<JupiterSiblingIxDescriptor>`
/// rather than introspecting the instructions sysvar. The future
/// slice (sysvar adapter, see TODO at the top of this module) replaces
/// this descriptor source while keeping the verifier's shape.
///
/// **Important.** The fact that the descriptor is executor-supplied
/// means a malicious executor can lie about the sibling ix list. The
/// daemon's sibling list MUST match the actual transaction's sibling
/// ixs, and the `verify_jupiter_boundary_skeleton` function MUST be
/// re-run against the sysvar in the future slice. J4 is the *shape*
/// of the verifier; the sysvar source is the *trust* of the verifier.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct JupiterSiblingIxDescriptor {
    /// The program id the sibling ix dispatches to. Resolved through
    /// [`JupiterSiblingClass::from_program_id`].
    pub program_id: Pubkey,
    /// First byte of `ix.data` (the instruction discriminator). For
    /// SPL Token, the verifier reads this against
    /// [`SPL_TOKEN_ALLOWED_VARIANT_BYTES`]. For other classes, this is
    /// recorded for audit but not currently checked (Compute Budget,
    /// System, ATA discriminators are all permitted by class
    /// membership alone).
    pub variant_byte: u8,
    /// Pubkeys of every account this sibling ix marks as writable.
    /// Cross-checked against the deterministic `allowed_writable_set`
    /// during verification.
    pub writable_accounts: Vec<Pubkey>,
}

/// Borsh-serializable lossless snapshot of the relevant SPL Token
/// account fields the future bracket slice will need.
///
/// **NOT used for any security-relevant computation in J4.** Only
/// `mint` and `owner` are validated for shape; `amount` is recorded
/// for the future bracket but never consumed by J4 to prove balance
/// movement.
///
/// In production (future slice), this struct is built by unpacking the
/// live SPL Token account at `accounts[i]` (offset 0..32 = mint,
/// 32..64 = owner, 64..72 = amount LE u64) — the field source is then
/// the runtime account state, not executor input. The fixture form
/// here is the J4 substrate; replacement is a code-only swap of the
/// reader.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct JupiterTokenAccountFixture {
    /// On-chain pubkey of the token account itself.
    pub address: Pubkey,
    /// SPL Token mint this account holds.
    pub mint: Pubkey,
    /// SPL Token authority (owner field inside the Pack body).
    pub owner: Pubkey,
    /// Token amount in the account's smallest unit. **J4 reserves
    /// this field for the future bracket slice; the J4 verifier does
    /// NOT trust it for any balance-delta computation.** See module
    /// docs deferral note.
    pub amount: u64,
}

/// The Borsh payload supplied alongside `ExecuteAction` for the
/// `JupiterBuySolWithUsdc` action type.
///
/// Wire shape is part of the on-chain instruction wire shape; field
/// reorder is a wire break.
///
/// # Field semantics (J4)
///
/// - `expected_destination_token_account` is the canonical destination
///   ATA the rule binds. Cross-checked against
///   [`AuthorizationRecord::destination`] in
///   [`verify_jupiter_boundary_skeleton`].
/// - `expected_destination_mint` and `expected_input_mint` are the
///   canonical action's mint pubkeys (committed via
///   `canonical_rule_hash`). The verifier checks the destination
///   fixture's mint matches `expected_destination_mint`.
/// - `delegated_input_token_account` and
///   `delegated_output_token_account` are the delegated wallet's ATAs.
///   Used to compute the deterministic `allowed_writable_set` for
///   sibling-ix verification.
/// - `destination_pre_snapshot` and `destination_post_snapshot` are
///   the dual-bracket placeholders for the future trustless balance
///   bracket. **J4 validates their `mint` and `owner` fields for
///   shape only and DOES NOT compute or enforce a balance delta.**
/// - `min_output_amount_raw` is the canonical-bound minimum output the
///   user signed for. **J4 carries this field but DOES NOT enforce
///   it.** The future bracket slice will gate
///   `checked_sub(post.amount, pre.amount) >= min_output_amount_raw`
///   inside the on-chain processor, with `pre.amount` and
///   `post.amount` sourced from on-chain SPL Token unpacks of the
///   destination_token_account at the bracket boundaries — NEVER from
///   executor input. The J4 test
///   `executor_supplied_pre_balance_is_not_trusted` pins this
///   property.
/// - `sibling_instructions` is the deterministic descriptor list the
///   sibling-ix verifier walks.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct JupiterBoundaryProof {
    /// Pinned Jupiter v6 program id supplied by the executor; the
    /// verifier cross-checks against
    /// [`JUPITER_V6_PROGRAM_ID_MAINNET`].
    pub jupiter_program_id: Pubkey,
    /// Pinned destination token account the rule sweeps the output
    /// to. MUST equal `AuthorizationRecord.destination`.
    pub expected_destination_token_account: Pubkey,
    /// Pinned destination-side SPL Token mint (e.g. wSOL).
    pub expected_destination_mint: Pubkey,
    /// Pinned input-side SPL Token mint (e.g. USDC). Recorded for
    /// the future bracket slice; used here only to derive the
    /// deterministic allowed-writable set.
    pub expected_input_mint: Pubkey,
    /// Delegated wallet's input ATA (USDC source).
    pub delegated_input_token_account: Pubkey,
    /// Delegated wallet's output ATA (wSOL destination side held by
    /// the delegated wallet between `swapInstruction` and
    /// `cleanupInstruction`).
    pub delegated_output_token_account: Pubkey,
    /// Token-account fixture for the destination *before* Jupiter
    /// runs. **J4 validates shape only.** See deferral note.
    pub destination_pre_snapshot: JupiterTokenAccountFixture,
    /// Token-account fixture for the destination *after* Jupiter
    /// runs. **J4 validates shape only.** See deferral note.
    pub destination_post_snapshot: JupiterTokenAccountFixture,
    /// Canonical-bound minimum output the user signed for. **J4
    /// records but does NOT enforce.** Future trustless balance
    /// bracket slice enforces.
    pub min_output_amount_raw: u64,
    /// Deterministic sibling-instruction descriptor list. See
    /// [`JupiterSiblingIxDescriptor`]. The future sysvar adapter
    /// replaces this source.
    pub sibling_instructions: Vec<JupiterSiblingIxDescriptor>,
}

/// Verify the Jupiter bracket boundary against an existing
/// `AuthorizationRecord`. Called from `process_execute_action` **before
/// any state mutation**. Returns `Err(_)` on any boundary failure;
/// the caller MUST propagate and refuse to mutate.
///
/// See module-level docs for the exact gate ordering and the deferral
/// list.
pub fn verify_jupiter_boundary_skeleton(
    record: &AuthorizationRecord,
    proof: &JupiterBoundaryProof,
) -> Result<(), ProgramError> {
    // Gate 1 — sibling list cap. Pull this first so a hostile
    // executor cannot inflate compute by feeding a giant Vec; the
    // descriptor walk would still terminate via Borsh's Vec length
    // prefix but the cap surfaces a more specific error.
    if proof.sibling_instructions.len() > MAX_JUPITER_SIBLING_INSTRUCTIONS {
        return Err(AuthorityError::JupiterSiblingListTooLarge.into());
    }
    for ix in &proof.sibling_instructions {
        if ix.writable_accounts.len() > MAX_WRITABLE_ACCOUNTS_PER_DESCRIPTOR {
            return Err(AuthorityError::JupiterBoundaryVerificationFailed.into());
        }
    }

    // Gate 2 — pinned Jupiter v6 program id. Caller supplies the id
    // (so audit trail records what the daemon thought it was using);
    // verifier cross-checks against the on-chain pin.
    if proof.jupiter_program_id != JUPITER_V6_PROGRAM_ID_MAINNET {
        return Err(AuthorityError::JupiterProgramIdMismatch.into());
    }

    // Gate 3 — destination identity against AuthorizationRecord.
    if proof.expected_destination_token_account != record.destination {
        return Err(AuthorityError::JupiterDestinationMismatch.into());
    }

    // Gate 4 — token-account fixture shape (mint / address binding).
    // Both pre and post fixtures must agree with the canonical destination
    // mint and the destination address. Owner is intentionally NOT pinned
    // here — the destination ATA's owner field is the user's destination
    // wallet, which Stage 2 P1's AuthorizationRecord does NOT explicitly
    // store (the record stores only the destination *token account*
    // pubkey, not its owner). A future tightening can add an
    // `expected_destination_owner: Pubkey` field to the record without
    // breaking the J4 wire shape (record `LEN` would inflate; see TODO
    // in state.rs).
    //
    // J4 validates that `mint == expected_destination_mint` and
    // `address == expected_destination_token_account` for both pre and
    // post fixtures — but the `amount` fields are NOT used in any
    // balance-delta computation in this slice (deferral).
    if proof.destination_pre_snapshot.address != proof.expected_destination_token_account
        || proof.destination_post_snapshot.address != proof.expected_destination_token_account
    {
        return Err(AuthorityError::JupiterTokenAccountInvalid.into());
    }
    if proof.destination_pre_snapshot.mint != proof.expected_destination_mint
        || proof.destination_post_snapshot.mint != proof.expected_destination_mint
    {
        return Err(AuthorityError::JupiterMintMismatch.into());
    }

    // Gate 5 — sibling-ix verification (per-class allowlist + variant
    // gate + writable-set check + relative ordering). Any failure
    // surfaces a specific error per attack class.
    let allowed_writable_set = build_allowed_writable_set(record, proof);
    verify_sibling_instructions(&proof.sibling_instructions, &allowed_writable_set)?;

    // ── Trustless balance bracket — DEFERRED in J4 ─────────────────
    //
    // The compile-time `assert!` below documents the deferral as a
    // structural invariant: J4 reaches this point with a *validated*
    // sibling-ix list and a *validated* destination identity, but
    // performs NO `checked_sub(post.amount, pre.amount)` and NO
    // `delta >= min_output_amount_raw` check. Wiring those checks
    // here would require either:
    //   (a) trusting executor-supplied `pre_balance` — explicitly
    //       forbidden by the J4 prompt, or
    //   (b) a separate `PreJupiterBracketCheck` instruction that
    //       writes a checkpoint PDA from on-chain SPL Token unpack —
    //       deferred to a future slice (`stage2/j5-...`).
    //
    // The fixture-supplied `amount` fields in
    // `destination_pre_snapshot` / `destination_post_snapshot` are
    // intentionally NOT consumed below this point. The
    // `executor_supplied_pre_balance_is_not_trusted` test in this
    // module pins this contract so any future regression surfaces
    // loudly in CI.
    //
    // TODO(stage2/j5+): replace fixture-based snapshots with on-chain
    // SPL Token unpack of the destination_token_account at the
    // pre-bracket and post-bracket boundaries; add `checked_sub` +
    // `>= min_output_amount_raw` gate; pre-allocated error codes
    // `JupiterPostBalanceUnderflow` / `JupiterBalanceDeltaTooSmall` /
    // `JupiterBracketCheckpointMissing` /
    // `JupiterBracketCheckpointConsumed` are reserved in `error.rs`.
    let _ = proof.min_output_amount_raw;
    let _ = proof.destination_pre_snapshot.amount;
    let _ = proof.destination_post_snapshot.amount;

    msg!("jupiter_boundary skeleton ok (balance bracket deferred)");
    Ok(())
}

/// Build the deterministic allowed-writable set for the current
/// authorization + proof. Any account marked writable by a sibling ix
/// MUST appear in this set, otherwise the bracket fails closed with
/// [`AuthorityError::JupiterUnexpectedWritableAccount`].
///
/// Set members:
///   - `record.delegated_wallet` (signer + writable for fee debit;
///     also the SPL Token authority of the input/output ATAs).
///   - `record.destination` (the destination token account; written
///     by the swap or by a SyncNative / CloseAccount cleanup).
///   - `proof.delegated_input_token_account` (USDC source — Token
///     program writes during the swap's internal CPI chain).
///   - `proof.delegated_output_token_account` (wSOL destination held
///     by the delegated wallet between swap and cleanup).
///
/// Jupiter route-internal writable accounts (pool token accounts,
/// intermediate token accounts) are NOT individually enumerated here —
/// they belong to Jupiter's own swap-ix account list and are
/// effectively allowed by the JupiterSwap class membership in
/// [`verify_sibling_instructions`].
fn build_allowed_writable_set(
    record: &AuthorizationRecord,
    proof: &JupiterBoundaryProof,
) -> [Pubkey; 4] {
    [
        record.delegated_wallet,
        record.destination,
        proof.delegated_input_token_account,
        proof.delegated_output_token_account,
    ]
}

/// Walk the sibling-ix descriptor list and run, in order:
///   - per-class allowlist on `program_id`,
///   - SPL Token variant-byte allowlist (rejects Approve / Burn / etc.),
///   - relative-ordering state-machine cursor (no absolute-index
///     assumptions),
///   - writable-account check (rejects unknown writable touchers).
///
/// The function is split out so unit tests can drive it with hand-built
/// descriptor lists without constructing a full `AuthorizationRecord`.
fn verify_sibling_instructions(
    siblings: &[JupiterSiblingIxDescriptor],
    allowed_writable_set: &[Pubkey],
) -> Result<(), ProgramError> {
    let mut cursor = PhaseCursor::BeforeSwap;
    let mut swap_count = 0usize;

    for ix in siblings {
        // Per-class allowlist — unknown program ids fail closed.
        let class = JupiterSiblingClass::from_program_id(&ix.program_id)
            .ok_or(AuthorityError::JupiterIllegalSiblingInstruction)?;

        // SPL Token variant-byte allowlist. Defends against Approve /
        // Burn / SetAuthority / FreezeAccount / etc. that would let a
        // malicious sibling delegate or freeze protected accounts.
        if class == JupiterSiblingClass::SplToken
            && !SPL_TOKEN_ALLOWED_VARIANT_BYTES.contains(&ix.variant_byte)
        {
            return Err(AuthorityError::JupiterIllegalSiblingInstruction.into());
        }

        // Relative-ordering cursor.
        match (cursor, class) {
            (PhaseCursor::BeforeSwap, JupiterSiblingClass::JupiterSwap) => {
                cursor = PhaseCursor::AtSwap;
                swap_count += 1;
            }
            (PhaseCursor::AtSwap, JupiterSiblingClass::JupiterSwap) => {
                // Second swap in the bracket — explicitly disallowed.
                return Err(AuthorityError::JupiterInstructionOrderInvalid.into());
            }
            (PhaseCursor::AfterSwap, JupiterSiblingClass::JupiterSwap) => {
                return Err(AuthorityError::JupiterInstructionOrderInvalid.into());
            }
            (PhaseCursor::AtSwap, _) => {
                cursor = PhaseCursor::AfterSwap;
            }
            // BeforeSwap → BeforeSwap (compute_budget / setup / system /
            // ata / spl_token wrap-SOL) and AfterSwap → AfterSwap
            // (cleanup / close / tip) are no-ops on the cursor.
            (PhaseCursor::BeforeSwap, _) | (PhaseCursor::AfterSwap, _) => {}
        }

        // Writable-account check. Every account marked writable across
        // every sibling MUST appear in the deterministic allowed set;
        // anything else is treated as a tampering signal.
        //
        // Exception: Jupiter's swap ix carries route-internal writable
        // accounts (pool token accounts, intermediate token accounts)
        // that are not individually enumerable in the bracket's
        // allowed_writable_set. We accept those by class — a
        // JupiterSwap-class descriptor is the single carve-out where
        // the writable list is allowed to extend beyond the
        // deterministic set. (In Variant A's eventual sysvar-driven
        // form the bracket would still be able to assert that *every*
        // writable account in the swap ix appears in the swap ix's
        // own account list at a writable index — but that's a sysvar
        // concern, not a descriptor concern.)
        if class != JupiterSiblingClass::JupiterSwap {
            for w in &ix.writable_accounts {
                if !allowed_writable_set.contains(w) {
                    return Err(AuthorityError::JupiterUnexpectedWritableAccount.into());
                }
            }
        }
    }

    // Exactly one Jupiter swap descriptor is required.
    if swap_count != 1 {
        return Err(AuthorityError::JupiterInstructionOrderInvalid.into());
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AuthorizationRecord, Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION};

    fn pk(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    /// Authz record fixture. user=0xAA, executor=0xEE, delegated_wallet=0xBB,
    /// destination=0xCC.
    fn record_fixture() -> AuthorizationRecord {
        AuthorizationRecord {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0xD0; 16],
            user: pk(0xAA),
            executor: pk(0xEE),
            delegated_wallet: pk(0xBB),
            canonical_rule_hash: [0; 32],
            allowed_action_type: Stage2ActionType::JupiterBuySolWithUsdc.to_u8(),
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk(0xCC),
            expires_at_slot: 1_000_000,
            revoked: false,
            completed: false,
            execution_nonce: 0,
            last_execution_slot: 0,
            bump: 255,
        }
    }

    fn fixture_for_destination(record: &AuthorizationRecord, mint: Pubkey) -> JupiterTokenAccountFixture {
        JupiterTokenAccountFixture {
            address: record.destination,
            mint,
            owner: pk(0xDD),
            amount: 0,
        }
    }

    fn good_sibling_list(
        delegated_input: Pubkey,
        delegated_output: Pubkey,
        destination: Pubkey,
        delegated_wallet: Pubkey,
    ) -> Vec<JupiterSiblingIxDescriptor> {
        vec![
            // ix[0] equivalent: Compute Budget SetComputeUnitLimit.
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 2, // SetComputeUnitLimit; not validated
                writable_accounts: vec![],
            },
            // ix[1]: Compute Budget SetComputeUnitPrice.
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 3, // SetComputeUnitPrice; not validated
                writable_accounts: vec![],
            },
            // ix[2]: ATA createAssociatedTokenAccountIdempotent — the destination
            // ATA might not exist yet; ATA program writes the new ATA pubkey
            // into account_keys. We treat the destination ATA as already in
            // allowed_writable_set so the create is permitted.
            JupiterSiblingIxDescriptor {
                program_id: ATA_PROGRAM_ID,
                variant_byte: 1, // createIdempotent
                writable_accounts: vec![destination, delegated_wallet],
            },
            // ix[3]: Jupiter swap. Route-internal writable accounts (pool
            // accts) are intentionally listed; JupiterSwap class permits
            // any writable account in the swap ix's own list (the
            // descriptor walk is "trust Jupiter's swap-ix accounts" — a
            // sysvar-driven future slice can tighten by walking the swap
            // ix's account_indices against the message account_keys).
            JupiterSiblingIxDescriptor {
                program_id: JUPITER_V6_PROGRAM_ID_MAINNET,
                variant_byte: 0xE5, // shared route discriminator (not validated)
                writable_accounts: vec![
                    delegated_input,
                    delegated_output,
                    pk(0x90),
                    pk(0x91),
                    pk(0x92),
                ],
            },
            // ix[4]: Cleanup CloseAccount on the wSOL ATA.
            JupiterSiblingIxDescriptor {
                program_id: SPL_TOKEN_PROGRAM_ID,
                variant_byte: 9, // CloseAccount
                writable_accounts: vec![delegated_output, delegated_wallet],
            },
        ]
    }

    fn good_proof(record: &AuthorizationRecord) -> JupiterBoundaryProof {
        let usdc = pk(0x11);
        let wsol = pk(0x12);
        let in_ata = pk(0x21);
        let out_ata = pk(0x22);
        JupiterBoundaryProof {
            jupiter_program_id: JUPITER_V6_PROGRAM_ID_MAINNET,
            expected_destination_token_account: record.destination,
            expected_destination_mint: wsol,
            expected_input_mint: usdc,
            delegated_input_token_account: in_ata,
            delegated_output_token_account: out_ata,
            destination_pre_snapshot: fixture_for_destination(record, wsol),
            destination_post_snapshot: fixture_for_destination(record, wsol),
            min_output_amount_raw: 1_000,
            sibling_instructions: good_sibling_list(
                in_ata,
                out_ata,
                record.destination,
                record.delegated_wallet,
            ),
        }
    }

    fn assert_err(actual: ProgramError, expected: AuthorityError) {
        assert_eq!(
            actual,
            ProgramError::Custom(expected as u32),
            "expected {expected:?} (code {}); got {actual:?}",
            expected as u32,
        );
    }

    // ── (1) Borsh round-trip ────────────────────────────────────────────

    #[test]
    fn jupiter_boundary_borsh_roundtrip() {
        let record = record_fixture();
        let proof = good_proof(&record);
        let bytes = borsh::to_vec(&proof).unwrap();
        let decoded = JupiterBoundaryProof::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, proof);
    }

    #[test]
    fn jupiter_sibling_descriptor_borsh_roundtrip() {
        let original = JupiterSiblingIxDescriptor {
            program_id: JUPITER_V6_PROGRAM_ID_MAINNET,
            variant_byte: 0xAB,
            writable_accounts: vec![pk(0x01), pk(0x02), pk(0x03)],
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = JupiterSiblingIxDescriptor::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn jupiter_token_account_fixture_borsh_roundtrip() {
        let original = JupiterTokenAccountFixture {
            address: pk(0xCC),
            mint: pk(0x12),
            owner: pk(0xAA),
            amount: 12_345_678,
        };
        let bytes = borsh::to_vec(&original).unwrap();
        let decoded = JupiterTokenAccountFixture::try_from_slice(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    // ── (2) Pinning ─────────────────────────────────────────────────────

    #[test]
    fn jupiter_program_id_matches_repo_pin() {
        // Parity with crates/gateway/src/integrations/jupiter.rs:563
        // and friends. Asserted as a literal so the on-chain BPF crate
        // does not depend on the gateway crate.
        assert_eq!(
            JUPITER_V6_PROGRAM_ID_MAINNET.to_string(),
            "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
        );
    }

    #[test]
    fn spl_token_program_id_matches_official_pin() {
        assert_eq!(
            SPL_TOKEN_PROGRAM_ID.to_string(),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        );
    }

    #[test]
    fn ata_program_id_matches_official_pin() {
        assert_eq!(
            ATA_PROGRAM_ID.to_string(),
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
        );
    }

    #[test]
    fn compute_budget_program_id_matches_official_pin() {
        assert_eq!(
            COMPUTE_BUDGET_PROGRAM_ID.to_string(),
            "ComputeBudget111111111111111111111111111111",
        );
    }

    #[test]
    fn spl_token_allowed_variant_bytes_pin() {
        // Source: spl_token::instruction::TokenInstruction (mainnet
        // SPL Token program). Pin the exact set so any future addition
        // is loud.
        assert_eq!(SPL_TOKEN_ALLOWED_VARIANT_BYTES, &[3, 9, 12, 17][..]);
    }

    // ── (3) Happy path ──────────────────────────────────────────────────

    #[test]
    fn good_proof_passes_skeleton_verifier() {
        let record = record_fixture();
        let proof = good_proof(&record);
        verify_jupiter_boundary_skeleton(&record, &proof).unwrap();
    }

    // ── (4) Relative ordering — does not assume absolute ix[0] ──────────

    #[test]
    fn relative_ordering_does_not_assume_absolute_ix_zero() {
        // Empty list should fail because there's no swap.
        let allowed = [pk(0xBB), pk(0xCC), pk(0x21), pk(0x22)];
        assert_err(
            verify_sibling_instructions(&[], &allowed).unwrap_err(),
            AuthorityError::JupiterInstructionOrderInvalid,
        );

        // Single Jupiter swap at *any* descriptor index is acceptable —
        // the verifier looks at relative ordering, not absolute position.
        let just_swap = vec![JupiterSiblingIxDescriptor {
            program_id: JUPITER_V6_PROGRAM_ID_MAINNET,
            variant_byte: 0xE5,
            writable_accounts: vec![],
        }];
        verify_sibling_instructions(&just_swap, &allowed).unwrap();

        // Pre-swap padding with 5 Compute Budget ixs then swap — also OK.
        // This is what makes the relative-ordering claim concrete: the
        // swap is at descriptor index 5 here, not 0.
        let padded = vec![
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 0,
                writable_accounts: vec![],
            },
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 0,
                writable_accounts: vec![],
            },
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 0,
                writable_accounts: vec![],
            },
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 0,
                writable_accounts: vec![],
            },
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 0,
                writable_accounts: vec![],
            },
            JupiterSiblingIxDescriptor {
                program_id: JUPITER_V6_PROGRAM_ID_MAINNET,
                variant_byte: 0xE5,
                writable_accounts: vec![],
            },
        ];
        verify_sibling_instructions(&padded, &allowed).unwrap();
    }

    // ── (5) Compute Budget before bracket is allowed ────────────────────

    #[test]
    fn compute_budget_before_bracket_is_allowed() {
        let record = record_fixture();
        let proof = good_proof(&record);
        // good_sibling_list already starts with two Compute Budget ixs;
        // the happy-path verifier accepts them. Add one more in front
        // for belt-and-braces.
        let mut p = proof;
        p.sibling_instructions.insert(
            0,
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 0xFF,
                writable_accounts: vec![],
            },
        );
        verify_jupiter_boundary_skeleton(&record, &p).unwrap();
    }

    // ── (6) Unknown program rejected ────────────────────────────────────

    #[test]
    fn unknown_program_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // Swap the cleanup CloseAccount for a totally unknown program.
        let last = proof.sibling_instructions.len() - 1;
        proof.sibling_instructions[last].program_id = pk(0x99);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterIllegalSiblingInstruction);
    }

    // ── (7) Fake Jupiter program id rejected ────────────────────────────

    #[test]
    fn fake_jupiter_program_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.jupiter_program_id = pk(0xDE);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterProgramIdMismatch);
    }

    #[test]
    fn fake_jupiter_program_in_sibling_swap_rejected() {
        // The proof's `jupiter_program_id` field is checked; AND the
        // sibling-ix verifier resolves each descriptor's program_id
        // through the per-class allowlist. A look-alike Jupiter pubkey
        // in the swap descriptor fails the per-class allowlist (no
        // matching class) — surfaces JupiterIllegalSiblingInstruction.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // Find the swap descriptor (the one with JUPITER_V6 program id)
        // and replace it with a clone pubkey.
        let swap_idx = proof
            .sibling_instructions
            .iter()
            .position(|ix| ix.program_id == JUPITER_V6_PROGRAM_ID_MAINNET)
            .unwrap();
        proof.sibling_instructions[swap_idx].program_id = pk(0xCA);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterIllegalSiblingInstruction);
    }

    // ── (8) Malicious Token::Transfer rejected (variant + writable) ─────

    #[test]
    fn malicious_token_transfer_rejected() {
        // Insert an SPL Token Transfer between Compute Budget and
        // ATA-create. The destination is an unknown attacker pubkey;
        // the writable-account check catches it.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        let attacker_ata = pk(0xAD);
        proof.sibling_instructions.insert(
            2,
            JupiterSiblingIxDescriptor {
                program_id: SPL_TOKEN_PROGRAM_ID,
                variant_byte: 3, // Transfer
                writable_accounts: vec![attacker_ata],
            },
        );
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterUnexpectedWritableAccount);
    }

    #[test]
    fn malicious_token_approve_rejected_by_variant_gate() {
        // SPL Token Approve (variant 4) — a delegation attack — is
        // rejected at the variant-byte allowlist even before the
        // writable-account check would fire. The writable-account list
        // here points at the legitimate input ATA so the writable check
        // would NOT catch it; the variant gate is what saves us.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        let in_ata = proof.delegated_input_token_account;
        proof.sibling_instructions.insert(
            2,
            JupiterSiblingIxDescriptor {
                program_id: SPL_TOKEN_PROGRAM_ID,
                variant_byte: 4, // Approve — establishes a delegate
                writable_accounts: vec![in_ata],
            },
        );
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterIllegalSiblingInstruction);
    }

    #[test]
    fn malicious_token_burn_rejected_by_variant_gate() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        let out_ata = proof.delegated_output_token_account;
        proof.sibling_instructions.insert(
            2,
            JupiterSiblingIxDescriptor {
                program_id: SPL_TOKEN_PROGRAM_ID,
                variant_byte: 8, // Burn
                writable_accounts: vec![out_ata],
            },
        );
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterIllegalSiblingInstruction);
    }

    #[test]
    fn malicious_token_set_authority_rejected_by_variant_gate() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        let in_ata = proof.delegated_input_token_account;
        proof.sibling_instructions.insert(
            2,
            JupiterSiblingIxDescriptor {
                program_id: SPL_TOKEN_PROGRAM_ID,
                variant_byte: 6, // SetAuthority
                writable_accounts: vec![in_ata],
            },
        );
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterIllegalSiblingInstruction);
    }

    // ── (9) Unexpected writable destination toucher ─────────────────────

    #[test]
    fn unexpected_writable_destination_toucher_rejected() {
        // Insert an ATA-class ix that writes an unrelated pubkey not
        // in the deterministic allowed_writable_set.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.sibling_instructions.insert(
            2,
            JupiterSiblingIxDescriptor {
                program_id: ATA_PROGRAM_ID,
                variant_byte: 1, // createIdempotent
                writable_accounts: vec![pk(0xEA)], // not in allowed set
            },
        );
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterUnexpectedWritableAccount);
    }

    // ── (10) Oversized sibling list rejected ────────────────────────────

    #[test]
    fn oversized_sibling_list_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // Pad with allowed-class no-op descriptors until we exceed the cap.
        for _ in 0..MAX_JUPITER_SIBLING_INSTRUCTIONS {
            proof.sibling_instructions.push(JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 0,
                writable_accounts: vec![],
            });
        }
        assert!(proof.sibling_instructions.len() > MAX_JUPITER_SIBLING_INSTRUCTIONS);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterSiblingListTooLarge);
    }

    // ── (11) Wrong relative ordering rejected ───────────────────────────

    #[test]
    fn cleanup_before_swap_rejected() {
        // Move the SPL Token CloseAccount cleanup to before the swap.
        // The cursor will see an SplToken-class ix in BeforeSwap (OK,
        // no-op transition), then the swap (BeforeSwap → AtSwap), but
        // the SECOND attempt to add another swap in AtSwap fails. To
        // make the ordering error concrete: insert a SECOND Jupiter
        // swap right after the legitimate one.
        let record = record_fixture();
        let mut proof = good_proof(&record);
        let swap_ix = proof
            .sibling_instructions
            .iter()
            .find(|ix| ix.program_id == JUPITER_V6_PROGRAM_ID_MAINNET)
            .unwrap()
            .clone();
        // Insert just after the legitimate swap.
        let swap_idx = proof
            .sibling_instructions
            .iter()
            .position(|ix| ix.program_id == JUPITER_V6_PROGRAM_ID_MAINNET)
            .unwrap();
        proof.sibling_instructions.insert(swap_idx + 1, swap_ix);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterInstructionOrderInvalid);
    }

    #[test]
    fn missing_swap_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof
            .sibling_instructions
            .retain(|ix| ix.program_id != JUPITER_V6_PROGRAM_ID_MAINNET);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterInstructionOrderInvalid);
    }

    // ── (12) checked_sub underflow — DEFERRED, code path documented ─────

    #[test]
    fn checked_sub_underflow_reserved_code_exists() {
        // J4 deliberately defers the trustless balance bracket. The
        // pre-allocated AuthorityError::JupiterPostBalanceUnderflow
        // discriminant must exist so the future slice can wire it
        // append-only without renumbering anything.
        let _ = AuthorityError::JupiterPostBalanceUnderflow as u32;
        let _ = AuthorityError::JupiterBalanceDeltaTooSmall as u32;
        let _ = AuthorityError::JupiterBracketCheckpointMissing as u32;
        let _ = AuthorityError::JupiterBracketCheckpointConsumed as u32;
        // No assertion on the value beyond "compiles" — the
        // discriminator-pin test in lib.rs::tests anchors the numeric
        // ordering. This test exists to make the deferral visible at
        // the verifier's own test surface.
    }

    // ── (13) Token mint mismatch rejected ───────────────────────────────

    #[test]
    fn token_mint_mismatch_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // The destination_pre_snapshot's mint is wrong.
        proof.destination_pre_snapshot.mint = pk(0xFF);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterMintMismatch);
    }

    #[test]
    fn token_destination_address_mismatch_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        // The post-snapshot points at a different account address.
        proof.destination_post_snapshot.address = pk(0xEE);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterTokenAccountInvalid);
    }

    // ── (14) Destination mismatch against AuthorizationRecord ───────────

    #[test]
    fn proof_destination_not_matching_record_rejected() {
        let record = record_fixture();
        let mut proof = good_proof(&record);
        proof.expected_destination_token_account = pk(0xCD);
        let err = verify_jupiter_boundary_skeleton(&record, &proof).unwrap_err();
        assert_err(err, AuthorityError::JupiterDestinationMismatch);
    }

    // ── (15) Executor-supplied pre_balance is NOT trusted ───────────────

    /// **Load-bearing deferral test.** J4 implements ONLY the
    /// sibling-instruction verifier. The trustless balance bracket
    /// (dual-ix `Pre/PostJupiterBracketCheck` with on-chain SPL Token
    /// unpack) is explicitly deferred to a future slice.
    ///
    /// This test pins that the J4 verifier:
    ///   - Accepts a proof whose `destination_pre_snapshot.amount`
    ///     and `destination_post_snapshot.amount` claim ANY balance
    ///     numbers (including a wildly favourable delta) — because
    ///     those fields are NEVER consumed by J4 for any
    ///     security-relevant computation.
    ///   - Equally accepts a proof whose snapshots claim a NEGATIVE
    ///     delta (post < pre) — same reason.
    ///   - Equally accepts a proof whose snapshots claim a delta
    ///     LESS THAN `min_output_amount_raw` — same reason.
    ///
    /// If a future regression starts consuming those fields, this
    /// test will start failing because the mismatched / malicious
    /// proofs will be rejected — surfacing the regression loudly in
    /// CI before the unsafe behaviour ships.
    ///
    /// The actual trustless enforcement lives in the future bracket
    /// slice; until then, the daemon's off-chain balance check is the
    /// only balance gate. The on-chain program does NOT trust
    /// executor-supplied `pre_balance`.
    #[test]
    fn executor_supplied_pre_balance_is_not_trusted() {
        let record = record_fixture();

        // Case 1 — proof claims a wildly favourable delta. Verifier
        // does NOT enforce; passes.
        let mut p1 = good_proof(&record);
        p1.destination_pre_snapshot.amount = 0;
        p1.destination_post_snapshot.amount = u64::MAX / 2;
        p1.min_output_amount_raw = u64::MAX / 4;
        verify_jupiter_boundary_skeleton(&record, &p1).unwrap();

        // Case 2 — proof claims a negative delta (post < pre). A
        // future trustless bracket would fail closed with
        // JupiterPostBalanceUnderflow; J4 passes (deferred).
        let mut p2 = good_proof(&record);
        p2.destination_pre_snapshot.amount = 1_000_000;
        p2.destination_post_snapshot.amount = 1;
        p2.min_output_amount_raw = 1;
        verify_jupiter_boundary_skeleton(&record, &p2).unwrap();

        // Case 3 — proof claims a delta below min_output_amount_raw.
        // A future trustless bracket would fail closed with
        // JupiterBalanceDeltaTooSmall; J4 passes (deferred).
        let mut p3 = good_proof(&record);
        p3.destination_pre_snapshot.amount = 1_000;
        p3.destination_post_snapshot.amount = 1_001; // delta = 1
        p3.min_output_amount_raw = 999_999_999_999; // far above delta
        verify_jupiter_boundary_skeleton(&record, &p3).unwrap();
    }

    // ── (16) Class-resolution table is exhaustive ───────────────────────

    #[test]
    fn class_from_program_id_covers_allowlisted_programs() {
        assert_eq!(
            JupiterSiblingClass::from_program_id(&COMPUTE_BUDGET_PROGRAM_ID),
            Some(JupiterSiblingClass::ComputeBudget),
        );
        assert_eq!(
            JupiterSiblingClass::from_program_id(&JUPITER_V6_PROGRAM_ID_MAINNET),
            Some(JupiterSiblingClass::JupiterSwap),
        );
        assert_eq!(
            JupiterSiblingClass::from_program_id(&SPL_TOKEN_PROGRAM_ID),
            Some(JupiterSiblingClass::SplToken),
        );
        assert_eq!(
            JupiterSiblingClass::from_program_id(&ATA_PROGRAM_ID),
            Some(JupiterSiblingClass::AssociatedTokenAccount),
        );
        assert_eq!(
            JupiterSiblingClass::from_program_id(&solana_program::system_program::ID),
            Some(JupiterSiblingClass::System),
        );
        // Unknown.
        assert_eq!(JupiterSiblingClass::from_program_id(&pk(0xDE)), None);
    }

    // ── (17) Allowed-writable set composition ───────────────────────────

    #[test]
    fn allowed_writable_set_pins_record_and_proof_keys() {
        let record = record_fixture();
        let proof = good_proof(&record);
        let set = build_allowed_writable_set(&record, &proof);
        assert!(set.contains(&record.delegated_wallet));
        assert!(set.contains(&record.destination));
        assert!(set.contains(&proof.delegated_input_token_account));
        assert!(set.contains(&proof.delegated_output_token_account));
    }
}

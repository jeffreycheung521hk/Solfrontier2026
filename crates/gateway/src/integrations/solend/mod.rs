//! Solend protocol-side read-model assembly.
//!
//! Spec anchors: `docs/lending_policy_vocabulary.md` Parts 5A §40 – §41
//! (placement and seam), Part 6A §55 (protocol choice), Part 6B §62 – §65
//! (implementation spec). Slice 1 scope is read-model foundation only.
//!
//! # What this module owns
//! - Solend-specific raw account decoders (`raw`).
//! - Solend raw → normalized [`crate::lending::LendingSnapshot`] mapping
//!   (`mapping`).
//!
//! # What this module does NOT own
//! - Refresh transaction builder (future slice).
//! - Deposit transaction builder (future slice).
//! - ATA handling, Phantom / daemon wiring, signing path (future slices).
//! - Full evaluator, rule config (Part 5B `evaluate(…)` surface — future).
//!
//! # Anti-corruption boundary (Part 6B §63)
//! - Raw Solend types (`SolendObligationRaw`, `SolendReserveRaw`,
//!   `SolendOracleRaw`) are INTERNAL to this subtree. They must NOT appear
//!   in the public signature of any function called from outside
//!   `crate::integrations::solend`.
//! - Evaluator-facing output is `crate::lending::LendingSnapshot` plus the
//!   newtypes / enums re-exported from `crate::lending`.
//! - The `raw` → normalized mapping in `mapping.rs` is the single site
//!   where Solend vocabulary terminates and protocol-agnostic vocabulary
//!   begins. Grep for `Solend` outside this subtree should find only
//!   `ProtocolTag::Solend` variant uses and string literals.
//!
//! # Forbidden imports
//! - `crate::approval_store` / `crate::approval_routing`
//! - `crate::pending_signing` / any park_store / daemon-runtime state
//! - `crate::external_wallet::submit_signed_transaction` or related
//!   signer/completion machinery
//! - `crate::daemon` runtime types
//! - `crate::orchestrator` internals that carry pending / signing state
//! - Any future `crate::lending::evaluate` / rule-engine entrypoint
//!
//! These restrictions match Part 5A §40.4 and Part 6B §62.3 verbatim.

pub mod assembler;
pub mod ata;
pub mod deposit;
pub mod init_obligation;
pub mod mapping;
pub mod oracle_decoder;
pub mod raw;
pub mod refresh;

// Phase 5H-A / 6I-F — Solend withdraw ix builder.
//
// Phase 5H-A kept this under `#[cfg(test)]` so production builds excluded
// it entirely. Phase 6I-F flips the gate: the withdraw JIT-prepare
// handler builds a `WithdrawObligationCollateralAndRedeemReserveCollateral`
// ix at user Sign-click time. Visibility stays `pub(crate)` so only the
// crate-internal plan assembler / JIT prepare handler can call it; no
// downstream crate can construct a withdraw ix directly. The one
// permitted path remains "chat tool → parked intent → resume re-check →
// JIT prepare → assembled plan".
pub(crate) mod withdraw;

pub use assembler::{
    AssembledSolendDepositSnapshot, AtaKind, ClawRpcPoolAccountFetcher, FetchError,
    SolanaAccountFetcher, SolendAssemblyError, SolendSnapshotAssembler,
    PYTH_SOLANA_RECEIVER_PROGRAM_BS58, USDC_MINT_BS58, USDC_RESERVE_BS58,
};
pub use ata::{build_create_ata_idempotent_instruction, derive_associated_token_address};
pub use deposit::{
    build_deposit_reserve_liquidity_and_obligation_collateral_instruction,
    DepositBuildError, DepositInstructionInputs,
};
pub use init_obligation::{
    build_create_obligation_account_instruction, build_init_obligation_instruction,
    CreateObligationAccountInputs, InitObligationInputs,
};
pub use mapping::{
    map_snapshot, map_snapshot_for_first_deposit, FirstDepositAssemblyInputs, MappingError,
    OracleAccountInfo, SolendAssemblyInputs,
};
pub use oracle_decoder::extract_publish_freshness;
pub use refresh::{
    build_refresh_instructions, ObligationRefreshInput, RefreshPlan, RefreshPlanInputs,
    ReserveRefreshInput,
};

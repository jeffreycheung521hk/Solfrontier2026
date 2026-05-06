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

// Phase 5H-A — un-wired Solend withdraw ix builder.
//
// `withdraw` is intentionally NOT `pub mod`. It is declared only
// under `#[cfg(test)]` so its structural unit tests run under
// `cargo test -p claw-gateway --lib solend`. Production builds
// exclude this file entirely; no symbol from it is reachable from
// any tool, runtime, park, signing, or submit code path.
//
// Phase 5H-C added a sibling `integrations/solend_withdraw_tx_plan.rs`
// (also `#[cfg(test)]`) that depends on this builder. To let that
// sibling import from here without exporting outside the crate, the
// visibility is `pub(crate)` — still gated by `#[cfg(test)]`, so the
// production binary remains byte-identical to the un-wired posture.
//
// Phase 5H-D will flip the `#[cfg(test)]` gates together when the
// `solend_withdraw_usdc` tool is wired.
#[cfg(test)]
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

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

pub mod mapping;
pub mod oracle_decoder;
pub mod raw;

pub use mapping::{MappingError, map_snapshot, OracleAccountInfo, SolendAssemblyInputs};
pub use oracle_decoder::extract_publish_freshness;

//! Solend adapter — first adapter for the Bounded Intent Execution
//! Loop.
//!
//! See [`docs/ARCHITECTURE.md`](../../../../docs/ARCHITECTURE.md) §9
//! and [`docs/ROADMAP.md`](../../../../docs/ROADMAP.md) Phase 4 /
//! Phase 6.
//!
//! # Status
//!
//! Scaffold. The deposit instruction builder lands in Phase 4;
//! withdraw-all lands in Phase 6.
//!
//! # Boundary
//!
//! - Inputs: a typed deposit request (`amount_raw`, reserve pubkey,
//!   controlled wallet pubkey) — validated against the pinned demo
//!   shape before construction.
//! - Output: an unsigned `Transaction` ready for the executor to
//!   sign + broadcast.
//! - This crate does NOT hold a keypair, does NOT sign, does NOT
//!   broadcast.
//!
//! # Pinned demo shape (initial)
//!
//! - Action: `SolendDeposit`
//! - Amount: `250_000` raw (0.25 USDC)
//! - Reserve: Solend Main Pool USDC
//!   (`BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw`)
//! - Lending market: `4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY`
//! - USDC mint: `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`
//! - cToken mint: `993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk`
//!
//! Anything outside this shape is rejected at the adapter boundary.
//!
//! # Processor-parity discipline
//!
//! The hackathon prototype's Phase 6I-H/I/J/K sequence proved that
//! the Solend SDK's `instruction.rs` helpers disagree with the
//! deployed mainnet `processor.rs` on several account-meta fields
//! (notably `lending_market` writability under the rate-limiter).
//! This adapter's instruction layout must be derived from
//! `processor.rs`, **not** the SDK helpers. A processor-parity
//! table in the test suite cross-checks every account slot
//! (writable/signer flags) against a known-good live tx of the
//! same instruction.

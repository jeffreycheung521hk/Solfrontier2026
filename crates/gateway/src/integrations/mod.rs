//! External protocol integrations.
//!
//! Each submodule implements a client for an external protocol that
//! Claw's control plane can route approved transactions through.

pub mod jupiter;
pub mod jupiter_alt;
pub mod jupiter_jit;
pub mod jupiter_park;
pub mod jupiter_production;
pub mod jupiter_tx;
pub mod solend;
pub mod solend_confirmation;
pub mod solend_jit_ready;
pub mod solend_lifecycle;
pub mod solend_park;
pub mod solend_preflight;
pub mod solend_signing;
pub mod solend_submit;
pub mod solend_tx_plan;

// Phase 5H-C — un-wired Solend WITHDRAW transaction PLAN assembler.
//
// `solend_withdraw_tx_plan` is intentionally NOT `pub mod`. It is
// declared only under `#[cfg(test)]` so its deterministic tests run
// under `cargo test -p claw-gateway --lib solend`. Production builds
// exclude this file entirely; no symbol from it is reachable from any
// tool, runtime, park, signing, submit, or chat code path.
//
// Mirrors the test-only-module posture introduced in Phase 5H-A for
// `integrations/solend/withdraw.rs`. Phase 5H-D will flip both gates
// together when the `solend_withdraw_usdc` tool is wired.
#[cfg(test)]
mod solend_withdraw_tx_plan;

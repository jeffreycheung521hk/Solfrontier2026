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

// Phase 5H-C / 6I-F — Solend WITHDRAW transaction PLAN assembler.
//
// Phase 5H-C kept this `#[cfg(test)] mod` so production builds excluded
// the file entirely. Phase 6I-F flips the gate to `pub mod` because the
// withdraw JIT-prepare handler (Phase 6I-F gateway runtime) calls
// `assemble_solend_withdraw_tx_plan` at user Sign-click time. Public
// surface stays narrow: only the assembler entry point and its typed
// plan / accounts-summary / error are usable from outside this module.
pub mod solend_withdraw_tx_plan;

// Phase 6I-D — Solend withdraw-all park store. Holds parked intents
// keyed by `approval_request_id` for the chat tool's awaiting_approval
// path. Production-callable; carries no transaction bytes / blockhashes
// / signer handles. The withdraw EXECUTION substrate
// (`solend::withdraw` + `solend_withdraw_tx_plan`) remains
// `#[cfg(test)]`-gated; the resume / JIT signing / submit pipeline is
// deferred to a follow-up slice.
pub mod solend_withdraw_park;

//! Gateway runtime wiring modules.
//!
//! Per [`DEBT.md` D-2](../../../DEBT.md), `daemon.rs` is the "wiring +
//! main loop only" surface. Integration-specific dependency wiring
//! (construct RPC adapters, build the per-tool config, compose the tool
//! struct, and register it into the `ToolRegistry`) belongs here — not
//! inside `daemon.rs` — so that adding the next integration-specific
//! piece does not further bloat the daemon.
//!
//! Each submodule exposes one small function that `daemon.rs` calls in
//! sequence. Submodules must be:
//! - pure wiring: no new background tasks, no new routes, no new
//!   signer / broadcast / blockhash-fetch paths
//! - wire one tool or one integration each
//! - accept the dependencies the daemon already owns (RPC pool,
//!   external wallet store, approval store, park stores, ...) and
//!   return whatever registry / handle shape the caller needs
//!
//! Adding a new integration? Add a new `<name>_wiring.rs` here and one
//! call line in `daemon.rs`. Do NOT inline the block into `daemon.rs`.

pub mod chat_wiring;
pub mod copilot_tools_wiring;
pub mod solend_jit_prepare_wiring;
pub mod solend_submit_wiring;
pub mod solend_wiring;
pub mod solend_withdraw_jit_prepare_wiring;
pub mod stage2_chat_execute_wiring;
pub mod stage2_w5h_funding_confirm_wiring;
pub mod stage2_w5h_intent_finalize_wiring;
pub mod stage2_w5h_order_status_wiring;
pub mod tool_output_explainer;

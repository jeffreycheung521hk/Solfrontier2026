//! Gateway-private tool implementations.
//!
//! These tools require access to gateway-level state (pipeline, approval store,
//! event bus) and cannot live in `claw-tool-system` without creating circular
//! crate dependencies. They are injected into the `ToolRegistry` at startup.

pub mod jupiter_swap;
pub mod signing;
pub mod solend_deposit;

pub use jupiter_swap::SubmitJupiterSwapTool;
pub use signing::SubmitForSigningTool;
pub use signing::resume_after_approval;
pub use solend_deposit::SubmitSolendDepositTool;

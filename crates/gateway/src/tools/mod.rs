//! Gateway-private tool implementations.
//!
//! These tools require access to gateway-level state (pipeline, approval store,
//! event bus) and cannot live in `claw-tool-system` without creating circular
//! crate dependencies. They are injected into the `ToolRegistry` at startup.

pub mod signing;

pub use signing::SubmitForSigningTool;
pub use signing::resume_after_approval;

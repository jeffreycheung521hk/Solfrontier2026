//! `claw-tool-system` — tool registry, dispatch, and built-in tools.
//!
//! # Responsibilities
//! - Define the `Tool` async trait
//! - Hold the `ToolRegistry` (keyed by name)
//! - Dispatch tool calls with timeout, cancellation, and trace recording
//! - Validate tool inputs against JSON Schema
//! - Enforce capability checks per tool
//!
//! # What does NOT belong here
//! - Agent orchestration logic
//! - LLM calls
//! - Policy evaluation
//! - Wallet signing

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod dispatch;
pub mod errors;
pub mod permissions;
pub mod registry;
pub mod tool;
pub mod tools;

pub use dispatch::ToolDispatcher;
pub use errors::ToolError;
pub use registry::ToolRegistry;
pub use tool::Tool;

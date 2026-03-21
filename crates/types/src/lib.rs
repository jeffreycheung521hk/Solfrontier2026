//! `claw-types` — the shared domain vocabulary of ClawSolana.
//!
//! # Design contract
//!
//! This crate must remain:
//! - Dependency-free from other `claw-*` crates (no circular deps)
//! - Focused: domain types only — no business logic, no I/O, no async
//! - Stable: changes here ripple everywhere; think before adding
//!
//! Every significant semantic boundary in the system is encoded here as
//! a named type. "Stringly-typed" patterns are explicitly prohibited.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod agent;
pub mod alert;
pub mod approval;
pub mod errors;
pub mod events;
pub mod messages;
pub mod policy;
pub mod session;
pub mod solana;
pub mod tool;
pub mod transaction;
pub mod wallet;

// Re-export the most commonly used identifiers at the crate root
// so callers can do `use claw_types::{SessionId, AgentRole, ...}`.
pub use agent::{AgentCommand, AgentResponse, AgentRole};
pub use alert::{Alert, AlertSeverity};
pub use approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
pub use errors::ClawError;
pub use events::GatewayEvent;
pub use messages::{InboundMessage, MessageContent, OutboundContent, OutboundMessage};
pub use policy::{PolicyRule, PolicyVerdict};
pub use session::{SessionId, SessionState, SessionSummary};
pub use solana::{CommitmentLevel, SolanaEvent, SolanaNetwork};
pub use tool::{ToolInput, ToolOutput, ToolSpec, ToolTrace};
pub use transaction::{TransactionProposal, TransactionRecord, TransactionStatus};
pub use wallet::{SignerType, WalletRef};

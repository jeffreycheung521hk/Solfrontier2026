//! Protocol-specific read-only tool implementations.
//!
//! These tools provide agents with intelligence about DeFi protocols
//! on Solana. All tools in this module are strictly READ-ONLY — they
//! fetch and decode on-chain state but do not build, sign, or send
//! any transactions.
//!
//! # Security note
//!
//! All data returned by these tools is treated as untrusted external data
//! by the tool/LLM boundary (`push_tool_result` in the agent loop).
//! The agent must not use protocol data to derive signing parameters
//! without a separate policy evaluation step.

pub mod orca;

pub use orca::{OrcaGetWhirlpoolTool, OrcaGetQuoteTool, OrcaPoolAllowlistTool};

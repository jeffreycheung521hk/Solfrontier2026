//! `claw-solana-core` — Solana infrastructure layer.
//!
//! # Responsibilities
//! - Wrap the Solana RPC client with retry logic and multi-endpoint failover
//! - Manage a pool of `RpcClient`s with health-check-based routing
//! - Maintain a refreshing pool of recent blockhashes
//! - Provide `simulateTransaction` with structured result parsing
//! - Normalize websocket subscription events into typed `SolanaEvent`s
//! - Decode raw account data for known program types
//! - Compute budget and priority fee helpers
//!
//! # What does NOT belong here
//! - Agent logic
//! - Policy evaluation
//! - Transaction signing (that's `claw-wallet-engine`)
//! - Persistence (that's `claw-state-store`)
//! - The "build a transfer" business logic (that's in `claw-tool-system`)

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod blockhash;
pub mod compute;
pub mod decode;
pub mod errors;
pub mod fees;
pub mod rpc;
pub mod simulation;
pub mod subscriptions;

pub use blockhash::BlockhashManager;
pub use errors::SolanaError;
pub use rpc::{RpcPool, RpcPoolConfig};
pub use simulation::{SimulationClient, SimulationOutput};

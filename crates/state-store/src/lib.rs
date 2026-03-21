//! `claw-state-store` — SQLite-backed persistence.
//!
//! # Responsibilities
//! - Own the SQLite connection pool (via `sqlx`)
//! - Run schema migrations on startup
//! - Expose typed repositories for each domain entity
//! - Ensure the audit log is append-only (no updates, no deletes)
//!
//! # What does NOT belong here
//! - Business logic — repositories are thin data-access wrappers
//! - Policy evaluation — that's `claw-risk-engine`
//! - Event bus interactions — write to the DB, emit the event elsewhere
//!
//! # SQLite configuration
//! - WAL mode enabled for concurrent reads
//! - `PRAGMA journal_mode=WAL` and `PRAGMA synchronous=NORMAL` applied at open
//! - Integrity check run on startup
//! - Single writer enforced by the connection pool configuration

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audit;
pub mod db;
pub mod errors;
pub mod pending_state;
pub mod sessions;
pub mod spend;
pub mod tool_traces;
pub mod transactions;
pub mod wallet_challenges;
pub mod wallets;

pub use audit::AuditRepository;
pub use db::{Database, DatabaseConfig};
pub use errors::StoreError;
pub use pending_state::PendingStateRepository;
pub use sessions::SessionRepository;
pub use spend::SpendRepository;
pub use tool_traces::ToolTraceRepository;
pub use transactions::TransactionRepository;
pub use wallet_challenges::WalletChallengeRepository;
pub use wallets::WalletRepository;

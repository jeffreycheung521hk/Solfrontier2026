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
#![allow(missing_docs)]

pub mod approval_workflow;
pub mod audit;
pub mod db;
pub mod errors;
pub mod pending_jupiter;
pub mod pending_state;
pub mod session_policy;
pub mod sessions;
pub mod spend;
pub mod stage2_w5h_funding;
pub mod stage2_watch_rules;
pub mod tool_traces;
pub mod tracking;
pub mod transactions;
pub mod wallet_bindings;
pub mod wallet_challenges;
pub mod wallets;

pub use approval_workflow::ApprovalWorkflowRepository;
pub use audit::AuditRepository;
pub use db::{Database, DatabaseConfig};
pub use errors::StoreError;
pub use pending_jupiter::{PendingJupiterRepository, StoredJupiterIntent};
pub use pending_state::PendingStateRepository;
pub use session_policy::SessionPolicyRepository;
pub use sessions::SessionRepository;
pub use spend::SpendRepository;
pub use stage2_w5h_funding::{
    NewW5hFundingIntent, Stage2W5hFundingIntentRepository, W5hFundingIntent,
    W5hIntentStatus,
};
pub use stage2_watch_rules::{
    Stage2WatchRuleRepository, StoredWatchRule, WatchRuleStatus,
};
pub use tool_traces::ToolTraceRepository;
pub use transactions::TransactionRepository;
pub use tracking::TransactionTrackingRepository;
pub use wallet_bindings::WalletBindingRepository;
pub use wallet_challenges::WalletChallengeRepository;
pub use wallets::WalletRepository;

//! `claw-wallet-engine` — wallet abstraction and transaction signing pipeline.
//!
//! # Responsibilities
//! - Define the `Signer` trait, the single abstraction over all signing backends
//! - Implement `LocalKeypairSigner` for private keys loaded from config
//! - Manage key material securely (`SecretKeystore`)
//! - Orchestrate the `TransactionReviewPipeline`:
//!     build → compute budget → priority fee → blockhash → simulate → summarize → approve → sign
//!
//! # Security notes
//! - Key material is held in `SecretKeypair`, which implements `Zeroize` and
//!   has its `Debug` impl redacted to prevent accidental log leakage.
//! - Signing happens inside the `Signer` impl — the caller only receives a
//!   `Signature`, never the raw secret bytes.
//! - `DRY_RUN` mode is the default; mainnet signing requires explicit config.
//!
//! # What does NOT belong here
//! - Policy evaluation (that's `claw-risk-engine`)
//! - RPC calls beyond signing (that's `claw-solana-core`)
//! - Agent orchestration

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod approval;
pub mod errors;
pub mod keystore;
pub mod local;
pub mod pipeline;
pub mod signer;

pub use approval::{ApprovalHandle, ApprovalMode};
pub use errors::WalletError;
pub use keystore::SecretKeystore;
pub use local::LocalKeypairSigner;
pub use pipeline::TransactionReviewPipeline;
pub use signer::{Signer, SignerRef};

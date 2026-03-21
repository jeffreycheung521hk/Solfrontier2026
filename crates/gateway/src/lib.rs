//! `claw-gateway` — the control plane supervisor.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod approval_store;
pub mod completion_metadata;
pub mod config;
pub mod daemon;
pub mod durable_pending;
pub mod errors;
pub mod event_bus;
pub mod external_wallet;
pub mod orchestrator;
pub mod pending_signing;
pub mod session_mgr;
pub mod supervisor;
pub mod tools;
pub mod wallet_challenge;

pub use approval_store::ApprovalStore;
pub use config::{ClawConfig, RpcConfig};
pub use daemon::GatewayDaemon;
pub use errors::GatewayError;
pub use event_bus::EventBus;
pub use external_wallet::{
    ExternalWalletStore, SubmitError, VerifyError,
    submit_signed_transaction, verify_signed_tx,
};
pub use pending_signing::PendingSigningStore;
pub use session_mgr::SessionManager;

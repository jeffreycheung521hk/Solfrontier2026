//! `claw-gateway` — the control plane supervisor.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod approval_audit;
pub mod approval_routing;
pub mod approval_store;
pub mod integrations;
pub mod completion_metadata;
pub mod config;
pub mod daemon;
pub mod durable_pending;
pub mod errors;
pub mod event_bus;
pub mod external_wallet;
pub mod lending;
pub mod lifecycle_persister;
pub mod orchestrator;
pub mod pending_signing;
pub mod policy_alerting;
pub mod runtime;
pub mod session_mgr;
pub mod session_policy;
pub mod supervisor;
pub mod tools;
pub mod wallet_challenge;
pub mod wallet_policy;

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
pub use session_policy::SessionPolicyStore;

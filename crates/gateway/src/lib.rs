//! `claw-gateway` — the control plane supervisor.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod approval_audit;
pub mod approval_routing;
pub mod approval_store;
// Stage 1 Tail — canonical-intent expiry gate. Standalone helper +
// tests; production wiring (calling into approval-decision /
// signing-prepare / submit-verify paths) is pending Prompt I.
pub mod canonical_intent_gate;
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
// Stage 1 Tail Agent I — env-driven demo-mode gate that controls
// whether the Solend deposit JIT handoff prepends a `record_intent`
// instruction. Default off; production behaviour is unchanged unless
// `CLAW_RECORD_INTENT_DEMO_ENABLED=1` AND
// `CLAW_RECORD_INTENT_PROGRAM_ID=<base58 pubkey>` are both set.
pub mod record_intent_demo;
pub mod runtime;
pub mod session_mgr;
// Stage 2 W2 — watcher scheduler substrate over the W1 watch-rule
// DB. Lifecycle + tick + force-tick + health surface ONLY. No live
// RPC, no signing, no broadcast, no Solend/Jupiter tx construction.
// Real evaluator + simulator implementations land in W3+.
pub mod stage2_watcher;
// Stage 2 W3 — watcher condition evaluator substrate. Snapshot
// provider + rule evaluator + batched Stage2ConditionEvaluator
// adapter with per-tick dedupe cache. Evaluation only — no signing,
// no broadcast, no transaction construction, no Solend/Jupiter CPI.
// Live providers must be added behind explicit constructors and must
// not auto-enable from ambient environment variables.
pub mod stage2_evaluator;
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

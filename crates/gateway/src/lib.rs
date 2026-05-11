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
// Stage 2 W4 — Solend demo executor glue. Selects condition_met rules,
// leases via the state-store CAS guard + same-process in-flight set,
// builds a strongly typed Stage2ExecuteActionRequest from the rule and
// the bound DemoSolendExecutionFixture (MAINNET_BETA_DEMO_USDC_TUPLE),
// dispatches through an injected Stage2ExecutionClient, writes back
// completed / failed. W4-lite default: MockExecutionClient only — no
// live RPC, no signing, no broadcast, no transaction construction.
pub mod stage2_executor;
// Stage 2 W5d — chat/demo APR conditional-deposit bridge. Deterministic
// parser for one specific demo grammar + B-O1-on-chain-APR evaluator
// + a `W5dAprFetcher` trait the chat handler depends on. No
// `sendTransaction` site; no keypair; no LLM call. Live RPC is the
// `LiveW5dAprFetcher` impl; tests use a mock.
pub mod stage2_demo_apr_bridge;
// Stage 2 W5g — chat-card controlled-wallet Solend deposit execution.
// Production-side orchestrator + injectable sender trait. The route
// is the only place outside the W5c env-gated test harness that
// invokes a live Solend deposit; every gate is fail-closed by
// default. No Phantom popup, no user-main-wallet signer, no
// clawsol-authority ExecuteAction, no AuthorizationRecord PDA.
pub mod stage2_chat_execute;
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

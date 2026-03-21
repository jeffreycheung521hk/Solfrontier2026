//! `GatewayDaemon` — the top-level wiring layer for the ClawSolana daemon.
//!
//! `GatewayDaemon::run()` is the single entry point called from `clawd/main.rs`.
//! It owns the full startup sequence, runs all background tasks, and handles
//! graceful shutdown on SIGINT/SIGTERM.
//!
//! # Startup sequence
//!
//! 1. Open database, run migrations
//! 2. Build RPC pool from config
//! 3. Start BlockhashManager background refresh
//! 4. Build tool registry + dispatcher
//! 5. Build LLM client
//! 6. Build Agent + AgentRouter
//! 7. Build EventBus + SessionManager
//! 8. Build risk engine PolicySet
//! 9. Load wallets from config into SecretKeystore (N4)
//! 10. Build pipeline + SignatureOrchestrator + inject SubmitForSigningTool (N5/STEP5)
//! 11. Start HTTP API server (with MessageHandler + SessionOps wired in)
//! 12. Await SIGINT/SIGTERM → graceful shutdown
//!
//! # What belongs here
//! Startup wiring, task supervision, shutdown orchestration, adapter impls.
//!
//! # What does NOT belong here
//! Business logic, Solana protocol knowledge, LLM calls, agent execution logic.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::json;
use tracing::{info, warn};

use std::future::Future;
use std::pin::Pin;

use claw_agent_runtime::{
    llm::{anthropic::AnthropicClient, LlmClientRef},
    Agent, AgentRouter,
};
use claw_api::{
    AppState, ApprovalHandler, ApprovalHandlerRef, AuthToken,
    EventSubscriber, EventSubscriberRef,
    MessageHandler, MessageHandlerRef, SessionManagerRef, SessionOps,
    WalletSignatureHandler, WalletSignatureHandlerRef, WalletSignatureOutcome,
    PendingWalletSignatureInfo,
};
use claw_observability::HealthRegistry;
use claw_risk_engine::PolicySet;
use claw_solana_core::{
    blockhash::BlockhashManager,
    fees::PriorityFeeStrategy,
    rpc::{ClawRpcClient, EndpointConfig, RpcPool, RpcPoolConfig},
    SimulationClient,
};
use claw_state_store::{
    audit::{AuditRepository, AuditSeverity},
    db::{Database, DatabaseConfig},
    spend::SpendRepository,
    tool_traces::ToolTraceRepository,
};
use claw_tool_system::{
    dispatch::ToolDispatcher,
    permissions::CapabilitySet,
    tools::default_registry,
    ToolRegistry,
};
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::{ApprovalLifecycleEvent, EventHeader, GatewayEvent},
    session::SessionId,
    solana::SolanaNetwork,
    wallet::SignerType,
};
use claw_wallet_engine::{
    approval::ApprovalMode,
    keystore::SecretKeystore,
    local::LocalKeypairSigner,
    pipeline::TransactionReviewPipeline,
    signer::SignerRef,
};

use crate::{
    approval_store::ApprovalStore,
    completion_metadata::CompletionMetadataStore,
    config::{ClawConfig, WalletConfig},
    durable_pending::DurablePendingState,
    errors::GatewayError,
    event_bus::EventBus,
    external_wallet::ExternalWalletStore,
    orchestrator::SignatureOrchestrator,
    orchestrator::local_adapter::LocalWalletAdapter,
    orchestrator::external_adapter::ExternalWalletAdapter,
    orchestrator::types::{SignatureOutcome, SignatureRequestId},
    pending_signing::PendingSigningStore,
    session_mgr::SessionManager,
    supervisor::spawn_supervised,
    tools::SubmitForSigningTool,
};

/// The runtime handle for a running daemon.
/// Held alive for the duration of `run()`. Drop triggers cleanup.
#[allow(dead_code)]
struct DaemonRuntime {
    db:              Database,
    event_bus:       EventBus,
    session_mgr:     SessionManager,
    agent_router:    Arc<AgentRouter>,
    policy_set:      Arc<PolicySet>,
    health_registry: HealthRegistry,
    approval_store:  ApprovalStore,
    api_handle:      tokio::task::JoinHandle<()>,
}

/// Main daemon entry point — owns startup, lifetime, and shutdown.
pub struct GatewayDaemon;

impl GatewayDaemon {
    /// Start the daemon, block until shutdown signal, clean up gracefully.
    pub async fn run(config: ClawConfig) -> Result<(), GatewayError> {
        info!("GatewayDaemon starting");

        // ── 1. Database ────────────────────────────────────────────────────
        let db = Database::open(&DatabaseConfig {
            path: config.daemon.db_path.clone(),
            max_connections: 5,
        })
        .await
        .map_err(|e| GatewayError::Startup(format!("database: {e}")))?;

        info!(path = %config.daemon.db_path, "database ready");

        // ── 2. RPC pool ────────────────────────────────────────────────────
        let mut endpoints = vec![EndpointConfig {
            url:               config.rpc.primary_url.clone(),
            label:             "primary".to_string(),
            is_write_endpoint: true,
        }];
        for (i, url) in config.rpc.fallback_urls.iter().enumerate() {
            endpoints.push(EndpointConfig {
                url:               url.clone(),
                label:             format!("fallback-{i}"),
                is_write_endpoint: false,
            });
        }

        let rpc_pool = RpcPool::new(RpcPoolConfig {
            endpoints,
            failure_threshold: 3,
            recovery_interval: Duration::from_secs(30),
            request_timeout:   Duration::from_millis(config.rpc.timeout_ms),
        });
        let rpc_client = ClawRpcClient::new(
            rpc_pool.clone(),
            claw_types::solana::CommitmentLevel::Confirmed,
        );
        info!(url = %config.rpc.primary_url, "RPC pool ready");

        // ── 3. BlockhashManager ────────────────────────────────────────────
        let blockhash_mgr = Arc::new(BlockhashManager::new(rpc_pool.clone()));
        {
            let mgr = blockhash_mgr.clone();
            spawn_supervised("blockhash-manager", move || {
                let mgr = mgr.clone();
                async move { mgr.run_refresh_loop(Duration::from_secs(20)).await }
            });
        }

        // ── 4. Tool registry (base — pipeline tool injected below) ────────
        let sim_client = SimulationClient::new(rpc_client.clone());
        let registry   = default_registry(rpc_client.clone(), sim_client.clone());

        info!(
            tool_count = registry.names().len(),
            "base tool registry ready"
        );

        // ── 5. LLM client ─────────────────────────────────────────────────
        let llm: LlmClientRef = Arc::new(
            AnthropicClient::new(config.llm.api_key.clone())
                .with_model(config.llm.model.clone()),
        );

        // ── 6. Agent + AgentRouter ────────────────────────────────────────
        let agent        = Arc::new(Agent::new(llm.clone()));
        let agent_router = Arc::new(AgentRouter::new());

        // ── 7. Event bus + SessionManager ─────────────────────────────────
        let event_bus   = EventBus::new();
        let session_mgr = SessionManager::new(event_bus.clone());

        // ── 8. Risk policy ────────────────────────────────────────────────
        let policy_set = Arc::new(build_policy_set(&config));
        info!("risk policy engine ready");

        // ── 9. Wallet loading (N4) ────────────────────────────────────────
        let keystore = SecretKeystore::new();
        let mut loaded_wallets: Vec<(String, String)> = Vec::new(); // (label, pubkey)

        for wallet_cfg in &config.wallets {
            match load_wallet_into_keystore(&keystore, wallet_cfg) {
                Ok(pubkey_str) => {
                    info!(
                        label = %wallet_cfg.label,
                        pubkey = %pubkey_str,
                        signer_type = %wallet_cfg.signer_type,
                        "wallet loaded"
                    );
                    loaded_wallets.push((wallet_cfg.label.clone(), pubkey_str));
                }
                Err(e) => {
                    warn!(
                        label = %wallet_cfg.label,
                        error = %e,
                        "failed to load wallet — signing will be unavailable for this wallet"
                    );
                }
            }
        }

        if config.wallets.is_empty() {
            info!("no wallets configured — signing tools will be unavailable");
        } else {
            info!(
                configured = config.wallets.len(),
                loaded = loaded_wallets.len(),
                "wallet loading complete"
            );
        }

        // ── 10. Pipeline + SignatureOrchestrator + SubmitForSigningTool (STEP 5) ──
        let approval_store    = ApprovalStore::new();
        let pending_signing   = PendingSigningStore::new();
        let external_wallet   = ExternalWalletStore::new();
        let completion_meta   = CompletionMetadataStore::new();
        let audit_repo        = AuditRepository::new(db.pool().clone());
        let spend_repo        = claw_state_store::SpendRepository::new(db.pool().clone());
        let tool_traces_repo  = ToolTraceRepository::new(db.pool().clone());
        let pending_state_repo = claw_state_store::PendingStateRepository::new(db.pool().clone());
        let durable_state      = DurablePendingState::new(pending_state_repo);

        // Build the main SignatureOrchestrator — shared between tool, handler, and reaper.
        let main_orchestrator = if let Some((_, pubkey_str)) = loaded_wallets.first() {
            let pubkey = pubkey_str.parse::<solana_sdk::pubkey::Pubkey>()
                .map_err(|e| GatewayError::Startup(format!("invalid wallet pubkey: {e}")))?;

            let signer: SignerRef = Arc::new(
                LocalKeypairSigner::new(pubkey, keystore.clone())
            );

            // Build adapter chain: external first (more specific), then local.
            let local_adapter = Arc::new(LocalWalletAdapter::new(signer.clone(), keystore.clone()));
            let external_adapter = Arc::new(ExternalWalletAdapter::new(
                Arc::new(external_wallet.clone()),
            ));
            SignatureOrchestrator::new(vec![external_adapter, local_adapter])
        } else {
            // No wallets — orchestrator with no adapters (fail-closed).
            SignatureOrchestrator::new(vec![])
        };

        let registry = if let Some((label, pubkey_str)) = loaded_wallets.first() {
            let pubkey = pubkey_str.parse::<solana_sdk::pubkey::Pubkey>()
                .map_err(|e| GatewayError::Startup(format!("invalid wallet pubkey: {e}")))?;

            let signer: SignerRef = Arc::new(
                LocalKeypairSigner::new(pubkey, keystore.clone())
            );

            let pipeline = TransactionReviewPipeline::new(
                rpc_client.clone(),
                sim_client,
                blockhash_mgr.clone(),
                PriorityFeeStrategy::None,
                ApprovalMode::Automatic,
            );

            let submit_tool = Arc::new(SubmitForSigningTool::new(
                pipeline,
                main_orchestrator.clone(),
                pending_signing.clone(),
                approval_store.clone(),
                event_bus.clone(),
                audit_repo.clone(),
                spend_repo.clone(),
                policy_set.clone(),
                config.network.network,
                external_wallet.clone(),
                completion_meta.clone(),
            ).with_durable(durable_state.clone()));

            info!(
                label = %label,
                "submit_for_signing tool active (wallet: {})",
                pubkey_str
            );

            registry.with_tool(submit_tool)
        } else {
            info!("no wallets loaded — submit_for_signing tool not available");
            registry
        };

        info!(
            tool_count = registry.names().len(),
            "final tool registry ready"
        );

        // ── 10b. Startup recovery (P1-1) ─────────────────────────────────
        // Recover pending state from the previous daemon lifetime.
        // Must happen AFTER orchestrator is built (for signature recovery)
        // and BEFORE the API server starts accepting requests.
        {
            let recovery_report = durable_state.recover(
                Duration::from_secs(120), // same TTL as reaper
                &approval_store,
                &pending_signing,
                &main_orchestrator,
                &completion_meta,
            ).await;

            if recovery_report.any_recovered() || recovery_report.any_corrupt() {
                info!(
                    recovered_approvals  = recovery_report.recovered_approvals,
                    recovered_signatures = recovery_report.recovered_signatures,
                    recovered_meta       = recovery_report.recovered_meta,
                    expired_approvals    = recovery_report.expired_approvals,
                    expired_signatures   = recovery_report.expired_signatures,
                    corrupt_approvals    = recovery_report.corrupt_approvals,
                    corrupt_signatures   = recovery_report.corrupt_signatures,
                    "pending state recovery complete"
                );
            }

            // Spawn resume tasks for recovered approval requests.
            // Each recovered approval has a fresh oneshot in PendingSigningStore.
            // We need to spawn a resume_after_approval task for each.
            for entry in pending_signing.all_parked_ids() {
                let decision_rx = match pending_signing.take_decision_rx(&entry) {
                    Some(rx) => rx,
                    None => continue,
                };
                let proposal = match pending_signing.get_proposal(&entry) {
                    Some(p) => p,
                    None => continue,
                };
                let wallet_pubkey = proposal.wallet_pubkey.clone();

                let orchestrator_c = main_orchestrator.clone();
                let pending_c      = pending_signing.clone();
                let audit_c        = audit_repo.clone();
                let spend_c        = spend_repo.clone();
                let event_bus_c    = event_bus.clone();
                let session_c      = proposal.session_id.clone();
                let cm_c           = completion_meta.clone();
                let dur_c          = Some(durable_state.clone());

                tokio::spawn(async move {
                    crate::tools::resume_after_approval(
                        decision_rx, entry, proposal, orchestrator_c,
                        pending_c, audit_c, spend_c, event_bus_c,
                        session_c, wallet_pubkey, cm_c, dur_c,
                    ).await;
                });
            }
        }

        // ── 10c. Startup lazy purge of terminal rows (P1-1 housekeeping) ──
        // Remove terminal rows older than retention period to prevent unbounded growth.
        // Recent terminal rows are preserved for post-mortem / audit.
        let terminal_retention_ms = config.daemon.terminal_retention_days as i64 * 24 * 60 * 60 * 1000;
        {
            let pending_state_repo_purge = claw_state_store::PendingStateRepository::new(db.pool().clone());
            match pending_state_repo_purge.purge_terminal(terminal_retention_ms).await {
                Ok(count) if count > 0 => {
                    info!(
                        purged = count,
                        retention_days = config.daemon.terminal_retention_days,
                        "purged old terminal pending-state rows on startup"
                    );
                }
                Ok(_) => {} // nothing to purge
                Err(e) => {
                    warn!(error = %e, "failed to purge terminal rows on startup — continuing");
                }
            }
        }

        // ── 11. Health registry ───────────────────────────────────────────
        let health_registry = HealthRegistry::new();

        // ── 12. API server ────────────────────────────────────────────────
        let api_token = load_api_token();

        let session_ref = SessionManagerRef::new(Arc::new(GatewaySessionAdapter {
            session_mgr:  session_mgr.clone(),
            agent_router: agent_router.clone(),
        }));

        let message_handler_ref = MessageHandlerRef::new(Arc::new(GatewayMessageHandler {
            agent:        agent.clone(),
            agent_router: agent_router.clone(),
            registry,
            audit:        audit_repo.clone(),
            tool_traces:  tool_traces_repo,
        }));

        let approval_handler_ref = ApprovalHandlerRef::new(Arc::new(GatewayApprovalHandler {
            store:           approval_store.clone(),
            pending_signing: pending_signing.clone(),
            event_bus:       event_bus.clone(),
            audit:           audit_repo.clone(),
            durable:         durable_state.clone(),
        }));

        let event_subscriber_ref = EventSubscriberRef::new(Arc::new(GatewayEventSubscriber {
            event_bus: event_bus.clone(),
        }));

        let wallet_sig_handler_ref = WalletSignatureHandlerRef::new(Arc::new(GatewayWalletSignatureHandler {
            orchestrator:    main_orchestrator.clone(),
            external_wallet: external_wallet.clone(),
            completion_meta: completion_meta.clone(),
            event_bus:       event_bus.clone(),
            audit:           audit_repo.clone(),
            spend:           spend_repo.clone(),
            durable:         durable_state.clone(),
        }));

        let app_state = AppState {
            session_mgr:       session_ref,
            message_handler:   message_handler_ref,
            approval:          approval_handler_ref,
            events:            event_subscriber_ref,
            wallet_signatures: wallet_sig_handler_ref,
            auth_token:        AuthToken::new(api_token),
        };

        let api_handle = claw_api::start(
            &config.api.bind_addr,
            config.api.port,
            app_state,
            health_registry.clone(),
        )
        .await
        .map_err(|e| GatewayError::Startup(format!("API server: {e}")))?;

        info!(
            bind = %config.api.bind_addr,
            port = config.api.port,
            "API server started"
        );

        // ── 13. Signature request expiry reaper (P0-2) ──────────────────
        //
        // Runs every 30 seconds. Expires pending signature requests older
        // than 120 seconds (2 minutes — Solana blockhash validity window).
        // Emits WalletSignatureExpired event and audit entry for each expired request.
        // The orchestrator's expire() atomically removes the entry.
        {
            let reaper_orchestrator = main_orchestrator.clone();
            let reaper_event_bus = event_bus.clone();
            let reaper_audit = audit_repo.clone();
            let reaper_completion_meta = completion_meta.clone();
            let reaper_durable = durable_state.clone();
            spawn_supervised("signature-reaper", move || {
                let orchestrator = reaper_orchestrator.clone();
                let event_bus = reaper_event_bus.clone();
                let audit = reaper_audit.clone();
                let completion_meta = reaper_completion_meta.clone();
                let durable = reaper_durable.clone();
                async move {
                    let ttl = Duration::from_secs(120);
                    let mut interval = tokio::time::interval(Duration::from_secs(30));
                    loop {
                        interval.tick().await;
                        let candidates = orchestrator.expired_candidates(ttl);
                        for request_id in candidates {
                            match orchestrator.expire(&request_id) {
                                Ok(expired_request) => {
                                    // P1-1 hardening: mark terminal BEFORE in-memory cleanup.
                                    durable.mark_signature_terminal(&request_id, "expired", Some("TTL exceeded")).await;
                                    durable.mark_completion_meta_terminal(request_id.0, "expired").await;

                                    // Clean up completion metadata.
                                    completion_meta.remove(&request_id.0);

                                    let session_id = expired_request.session_id.clone();
                                    let session_str = session_id.to_string();

                                    let _ = audit.append(
                                        Some(&session_str),
                                        &request_id.0.to_string(),
                                        "wallet_signature_expired",
                                        "reaper",
                                        &serde_json::json!({
                                            "request_id":     request_id.0,
                                            "transaction_id": expired_request.transaction_id,
                                            "expected_signer": expired_request.expected_signer.to_string(),
                                        }),
                                        AuditSeverity::Warning,
                                    ).await;

                                    event_bus.publish(GatewayEvent::WalletSignatureExpired(
                                        claw_types::events::WalletSignatureEvent {
                                            header: claw_types::events::EventHeader::new(Some(session_id.clone())),
                                            session_id: session_id.clone(),
                                            request_id: request_id.0,
                                            transaction_id: expired_request.transaction_id,
                                            wallet_pubkey: expired_request.expected_signer.to_string(),
                                            error: Some("signature request expired (TTL exceeded)".to_string()),
                                        },
                                    ));

                                    info!(
                                        request_id = %request_id,
                                        transaction_id = %expired_request.transaction_id,
                                        "expired pending signature request"
                                    );
                                }
                                Err(_) => {
                                    // Already consumed by complete() between
                                    // expired_candidates() and expire() — safe to ignore.
                                }
                            }
                        }
                    }
                }
            });
        }
        info!("signature reaper started (TTL=120s, interval=30s)");

        // ── 13b. Periodic terminal-row purge (P1-1 housekeeping) ─────────
        //
        // Low-frequency background task that removes terminal pending-state
        // rows older than the configured retention period. Supplements the
        // one-time startup purge. Failures are logged and do not crash the
        // daemon. Never touches pending rows.
        {
            let purge_interval_secs = config.daemon.terminal_purge_interval_hours * 3600;
            let purge_retention_ms  = terminal_retention_ms;
            let purge_repo = claw_state_store::PendingStateRepository::new(db.pool().clone());
            spawn_supervised("terminal-purge", move || {
                let repo = purge_repo.clone();
                async move {
                    let mut interval = tokio::time::interval(
                        Duration::from_secs(purge_interval_secs)
                    );
                    // First tick fires immediately — skip it since startup purge already ran.
                    interval.tick().await;
                    loop {
                        interval.tick().await;
                        match repo.purge_terminal(purge_retention_ms).await {
                            Ok(count) if count > 0 => {
                                info!(
                                    purged = count,
                                    "periodic terminal-row purge complete"
                                );
                            }
                            Ok(_) => {} // nothing to purge
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "periodic terminal-row purge failed — will retry next interval"
                                );
                            }
                        }
                    }
                }
            });
            info!(
                interval_hours = config.daemon.terminal_purge_interval_hours,
                retention_days = config.daemon.terminal_retention_days,
                "terminal-row purge task started"
            );
        }

        // ── 14. Hold runtime alive until shutdown ─────────────────────────
        let _runtime = DaemonRuntime {
            db,
            event_bus,
            session_mgr,
            agent_router,
            policy_set,
            health_registry,
            approval_store,
            api_handle,
        };

        info!("daemon ready — press Ctrl-C or send SIGTERM to stop");
        Self::await_shutdown_signal().await;
        info!("shutdown signal received — stopping");

        Ok(())
    }

    async fn await_shutdown_signal() {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "SIGTERM unavailable; using SIGINT only");
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { info!("SIGINT"); }
                _ = sigterm.recv()          => { info!("SIGTERM"); }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

// ── Wallet loader ───────────────────────────────────────────────────────────

fn load_wallet_into_keystore(
    keystore:    &SecretKeystore,
    wallet_cfg:  &WalletConfig,
) -> Result<String, String> {
    match wallet_cfg.signer_type {
        SignerType::LocalKeypair => {
            if let Some(ref path) = wallet_cfg.keypair_path {
                let json = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read keypair file '{}': {}", path, e))?;
                let pubkey = keystore.load_from_json_bytes(&json)
                    .map_err(|e| format!("invalid keypair in '{}': {}", path, e))?;
                Ok(pubkey.to_string())
            } else if let Some(ref b58) = wallet_cfg.keypair_base58 {
                let pubkey = keystore.load_from_base58(b58)
                    .map_err(|e| format!("invalid base58 keypair for '{}': {}", wallet_cfg.label, e))?;
                Ok(pubkey.to_string())
            } else {
                Err(format!(
                    "wallet '{}' is LocalKeypair but neither keypair_path nor keypair_base58 is set",
                    wallet_cfg.label
                ))
            }
        }
        SignerType::Ledger => {
            Err(format!(
                "wallet '{}': Ledger signer is not yet supported in V1 — deferred",
                wallet_cfg.label
            ))
        }
        SignerType::External => {
            Err(format!(
                "wallet '{}': External signer is not yet supported in V1 — deferred",
                wallet_cfg.label
            ))
        }
        SignerType::ReadOnly => {
            Err(format!(
                "wallet '{}' is ReadOnly — no key material to load (this is expected)",
                wallet_cfg.label
            ))
        }
    }
}

// ── Policy builder ─────────────────────────────────────────────────────────

fn build_policy_set(config: &ClawConfig) -> PolicySet {
    if config.policy.mainnet_safe_defaults
        && config.network.network == SolanaNetwork::MainnetBeta
    {
        info!("mainnet-safe policy: all transactions require human approval");
        PolicySet::mainnet_safe_default()
    } else {
        info!("permissive policy: auto-approve (devnet/testnet)");
        PolicySet::permissive_default()
    }
}

// ── API token bootstrap ────────────────────────────────────────────────────

fn load_api_token() -> String {
    match std::env::var("CLAW_API_TOKEN") {
        Ok(token) => {
            let preview = &token[..token.len().min(8)];
            info!(
                token_preview = preview,
                source = "CLAW_API_TOKEN env var",
                "API token loaded"
            );
            token
        }
        Err(_) => {
            let token   = uuid::Uuid::new_v4().to_string();
            let preview = &token[..token.len().min(8)];
            info!(
                token_preview = preview,
                source = "generated (ephemeral)",
                "API token generated — set CLAW_API_TOKEN for a persistent token"
            );
            eprintln!(
                "[clawd] Ephemeral API token (set CLAW_API_TOKEN to suppress): {}",
                token
            );
            token
        }
    }
}

// ── SessionOps adapter ─────────────────────────────────────────────────────

struct GatewaySessionAdapter {
    session_mgr:  SessionManager,
    agent_router: Arc<AgentRouter>,
}

impl SessionOps for GatewaySessionAdapter {
    fn open(&self, role: AgentRole, channel: String) -> SessionId {
        let id = self.session_mgr.open(role, channel.clone());
        let router = self.agent_router.clone();
        let id2    = id.clone();
        tokio::spawn(async move {
            router.open_session(id2, role, channel).await;
        });
        id
    }

    fn close(&self, id: &SessionId, reason: &str) {
        self.session_mgr.close(id, reason);
        let router = self.agent_router.clone();
        let id2    = id.clone();
        tokio::spawn(async move {
            router.close_session(&id2).await;
        });
    }

    fn active_count(&self) -> usize {
        self.session_mgr.active_count()
    }

    fn is_active(&self, id: &SessionId) -> bool {
        self.session_mgr.is_active(id)
    }
}

// ── MessageHandler adapter ─────────────────────────────────────────────────

struct GatewayMessageHandler {
    agent:        Arc<Agent>,
    agent_router: Arc<AgentRouter>,
    registry:     ToolRegistry,
    audit:        AuditRepository,
    tool_traces:  ToolTraceRepository,
}

impl MessageHandler for GatewayMessageHandler {
    fn handle<'a>(
        &'a self,
        session_id: &'a SessionId,
        command: AgentCommand,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>> {
        Box::pin(self.handle_inner(session_id, command))
    }
}

impl GatewayMessageHandler {
    async fn handle_inner(
        &self,
        session_id: &SessionId,
        command: AgentCommand,
    ) -> Result<AgentResponse, String> {
        let session_id_str = session_id.to_string();
        let correlation_id = command.id.to_string();

        let role = self.agent_router.session_role(session_id).await
            .unwrap_or(AgentRole::Research);
        let caps       = CapabilitySet::for_role(role);
        let dispatcher = ToolDispatcher::with_capabilities(self.registry.clone(), caps);

        let _ = self.audit.append(
            Some(&session_id_str),
            &correlation_id,
            "agent_task_started",
            "agent",
            &serde_json::json!({
                "command_id": &correlation_id,
                "agent_role": role.to_string(),
                "text_preview": &command.text.chars().take(200).collect::<String>(),
            }),
            AuditSeverity::Info,
        ).await;

        let result = self.agent_router
            .dispatch(session_id, command, &self.agent, dispatcher, &self.tool_traces)
            .await;

        match &result {
            Ok(response) => {
                let _ = self.audit.append(
                    Some(&session_id_str),
                    &correlation_id,
                    "agent_task_completed",
                    "agent",
                    &serde_json::json!({
                        "command_id": response.command_id,
                        "status":     response.status,
                        "tool_calls": response.tool_calls.len(),
                    }),
                    AuditSeverity::Info,
                ).await;
            }
            Err(ref e) => {
                let _ = self.audit.append(
                    Some(&session_id_str),
                    &correlation_id,
                    "agent_task_failed",
                    "agent",
                    &serde_json::json!({ "error": e.to_string() }),
                    AuditSeverity::Error,
                ).await;
            }
        }

        result.map_err(|e| e.to_string())
    }
}

// ── ApprovalHandler adapter ────────────────────────────────────────────────

struct GatewayApprovalHandler {
    store:           ApprovalStore,
    pending_signing: PendingSigningStore,
    event_bus:       EventBus,
    audit:           AuditRepository,
    durable:         DurablePendingState,
}

impl ApprovalHandler for GatewayApprovalHandler {
    fn pending_for_session(&self, session_id: &SessionId) -> Vec<ApprovalRequest> {
        self.store.pending_for_session(session_id)
    }

    fn decide(
        &self,
        decision: ApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>> {
        Box::pin(self.decide_inner(decision))
    }
}

impl GatewayApprovalHandler {
    async fn decide_inner(
        &self,
        decision: ApprovalDecision,
    ) -> (ApprovalOutcome, Option<ApprovalRequest>) {
        let (outcome, maybe_request) = self.store.decide(&decision);

        if let Some(ref request) = maybe_request {
            let session_str = request.session_id.to_string();
            let request_id  = decision.request_id.to_string();

            let (audit_event, audit_severity) = if decision.approved {
                ("human_approved", AuditSeverity::Info)
            } else {
                ("human_rejected", AuditSeverity::Warning)
            };

            let _ = self.audit.append(
                Some(&session_str),
                &request_id,
                audit_event,
                "operator",
                &serde_json::json!({
                    "request_id":     decision.request_id,
                    "transaction_id": request.transaction_id,
                    "approved":       decision.approved,
                    "note":           decision.note,
                }),
                audit_severity,
            ).await;

            let signaled = self.pending_signing.signal(decision.request_id, decision.approved);
            if !signaled {
                info!(
                    request_id = %decision.request_id,
                    "no parked signing task for this approval request (non-signing flow or already resolved)"
                );
            }

            let event = GatewayEvent::ApprovalReceived(ApprovalLifecycleEvent {
                header:         EventHeader::new(Some(request.session_id.clone())),
                session_id:     request.session_id.clone(),
                request_id:     request.id,
                transaction_id: request.transaction_id,
                approved:       Some(decision.approved),
            });
            self.event_bus.publish(event);
        }

        (outcome, maybe_request)
    }
}

// ── WalletSignatureHandler adapter ─────────────────────────────────────
//
// Handles signed transactions submitted by external wallets.
// Uses SignatureOrchestrator.complete() for verification (Execution Plane).
// Audit, spend, and events remain caller-side (Control Plane).

struct GatewayWalletSignatureHandler {
    orchestrator:     SignatureOrchestrator,
    external_wallet:  ExternalWalletStore,
    completion_meta:  CompletionMetadataStore,
    event_bus:        EventBus,
    audit:            AuditRepository,
    spend:            SpendRepository,
    durable:          DurablePendingState,
}

impl WalletSignatureHandler for GatewayWalletSignatureHandler {
    fn submit_signed_tx(
        &self,
        session_id: &SessionId,
        request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>> {
        let session_id = session_id.clone();
        Box::pin(async move {
            self.submit_signed_tx_inner(&session_id, request_id, signed_tx_b64).await
        })
    }

    fn pending_for_session(&self, session_id: &SessionId) -> Vec<PendingWalletSignatureInfo> {
        // Use the orchestrator's pending_for_session for execution-plane pending state.
        self.orchestrator
            .pending_for_session(session_id)
            .into_iter()
            .map(|summary| {
                let unsigned_tx_b64 = base64::engine::general_purpose::STANDARD.encode(&summary.finalized_tx_bytes);
                PendingWalletSignatureInfo {
                    request_id:      summary.request_id.0,
                    transaction_id:  summary.transaction_id,
                    description:     summary.description,
                    expected_signer: summary.expected_signer,
                    unsigned_tx_b64,
                }
            })
            .collect()
    }

    fn bind_wallet(&self, session_id: &SessionId, pubkey: &str) {
        self.external_wallet.bind_wallet(session_id, pubkey);
        info!(
            session = %session_id,
            pubkey = %pubkey,
            "external wallet bound to session"
        );
    }

    fn wallets_for_session(&self, session_id: &SessionId) -> Vec<String> {
        self.external_wallet.wallets_for_session(session_id)
    }
}

impl GatewayWalletSignatureHandler {
    /// Thin facade over `SignatureOrchestrator.complete()`.
    ///
    /// Decodes the incoming base64 payload, calls orchestrator.complete() for
    /// verification (Execution Plane), then records audit/spend/events
    /// (Control Plane) based on the outcome.
    async fn submit_signed_tx_inner(
        &self,
        session_id: &SessionId,
        request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> WalletSignatureOutcome {
        let session_str = session_id.to_string();

        // Decode the submitted signed transaction from base64.
        let signed_tx_bytes = match base64::engine::general_purpose::STANDARD.decode(&signed_tx_b64) {
            Ok(b) => b,
            Err(e) => {
                return WalletSignatureOutcome {
                    request_id,
                    accepted: false,
                    signature: None,
                    error: Some(format!("invalid base64: {e}")),
                };
            }
        };

        // Convert the UUID to a SignatureRequestId for the orchestrator.
        let sig_request_id = SignatureRequestId::from(request_id);

        // Call orchestrator.complete() — atomic consume-on-attempt.
        // The orchestrator handles verification via the ExternalWalletAdapter.
        match self.orchestrator.complete(&sig_request_id, &signed_tx_bytes).await {
            Ok(SignatureOutcome::Signed {
                signature, transaction_id, expected_signer,
                request_id: _, signed_tx_bytes: _, adapter_type: _,
            }) => {
                // Accepted — record spend, audit, events.
                // transaction_id and expected_signer come from the consumed SignatureRequest.
                // fee_lamports comes from the caller-side CompletionMetadataStore.
                let fee_lamports = self.completion_meta.take(&request_id)
                    .and_then(|m| m.fee_lamports);

                // P1-1 hardening: mark terminal BEFORE any further processing.
                self.durable.mark_signature_terminal(&sig_request_id, "consumed", Some("signature accepted")).await;
                self.durable.mark_completion_meta_terminal(request_id, "consumed").await;

                if let Some(fee) = fee_lamports {
                    let _ = self.spend.record_spend(
                        &expected_signer,
                        &session_str,
                        &transaction_id.to_string(),
                        fee,
                    ).await;
                }

                let _ = self.audit.append(
                    Some(&session_str),
                    &request_id.to_string(),
                    "wallet_signature_received",
                    "external_wallet",
                    &json!({
                        "request_id":     request_id,
                        "transaction_id": transaction_id,
                        "wallet_pubkey":  expected_signer,
                        "signature":      signature,
                    }),
                    AuditSeverity::Info,
                ).await;

                self.event_bus.publish(GatewayEvent::WalletSignatureReceived(
                    claw_types::events::WalletSignatureEvent {
                        header:         claw_types::events::EventHeader::new(Some(session_id.clone())),
                        session_id:     session_id.clone(),
                        request_id,
                        transaction_id,
                        wallet_pubkey:  expected_signer.clone(),
                        error:          None,
                    },
                ));

                self.event_bus.publish(GatewayEvent::TransactionSigned(
                    claw_types::events::TransactionLifecycleEvent {
                        header:         claw_types::events::EventHeader::new(Some(session_id.clone())),
                        session_id:     session_id.clone(),
                        transaction_id,
                        wallet_pubkey:  expected_signer.clone(),
                        status:         claw_types::transaction::TransactionStatus::Signed,
                        signature:      Some(signature.clone()),
                    },
                ));

                info!(
                    session = %session_str,
                    request_id = %request_id,
                    signature = %signature,
                    "external wallet signature accepted via orchestrator"
                );

                WalletSignatureOutcome {
                    request_id,
                    accepted: true,
                    signature: Some(signature),
                    error: None,
                }
            }

            Ok(SignatureOutcome::Pending { .. }) => {
                warn!(
                    session = %session_str,
                    request_id = %request_id,
                    "unexpected Pending outcome from orchestrator.complete()"
                );
                WalletSignatureOutcome {
                    request_id,
                    accepted: false,
                    signature: None,
                    error: Some("internal error: unexpected pending state".to_string()),
                }
            }

            Err(e) => {
                let reason = e.to_string();
                warn!(
                    session = %session_str,
                    request_id = %request_id,
                    "wallet signature rejected by orchestrator: {reason}"
                );

                // P1-1 hardening: mark terminal BEFORE in-memory cleanup.
                self.durable.mark_signature_terminal(&sig_request_id, "rejected", Some(&reason)).await;
                self.durable.mark_completion_meta_terminal(request_id, "rejected").await;

                // Clean up completion metadata on rejection.
                self.completion_meta.remove(&request_id);

                // The orchestrator already consumed the pending entry (consume-on-attempt).
                // We use the request_id for audit correlation — transaction_id and wallet_pubkey
                // are not available after consumption on the error path.
                let _ = self.audit.append(
                    Some(&session_str),
                    &request_id.to_string(),
                    "wallet_signature_rejected",
                    "external_wallet",
                    &json!({
                        "request_id": request_id,
                        "reason":     reason,
                    }),
                    AuditSeverity::Warning,
                ).await;

                self.event_bus.publish(GatewayEvent::WalletSignatureRejected(
                    claw_types::events::WalletSignatureEvent {
                        header:         claw_types::events::EventHeader::new(Some(session_id.clone())),
                        session_id:     session_id.clone(),
                        request_id,
                        transaction_id: uuid::Uuid::nil(),
                        wallet_pubkey:  String::new(),
                        error:          Some(reason.clone()),
                    },
                ));

                WalletSignatureOutcome {
                    request_id,
                    accepted: false,
                    signature: None,
                    error: Some(reason),
                }
            }
        }
    }
}

// ── EventSubscriber adapter ────────────────────────────────────────────────

struct GatewayEventSubscriber {
    event_bus: EventBus,
}

impl EventSubscriber for GatewayEventSubscriber {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<GatewayEvent> {
        self.event_bus.subscribe()
    }
}

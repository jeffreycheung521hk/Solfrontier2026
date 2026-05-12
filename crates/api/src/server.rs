//! HTTP API server for the ClawSolana daemon.
//!
//! Binds to `127.0.0.1:<port>` by default (never 0.0.0.0 in production).
//! All routes except `/health` require a valid Bearer token.
//!
//! # Routes
//!
//! | Method | Path                        | Auth | Description                      |
//! |--------|-----------------------------|------|----------------------------------|
//! | GET    | /health                     | No   | System health aggregation        |
//! | GET    | /sessions                   | Yes  | List active sessions             |
//! | POST   | /sessions                   | Yes  | Open a new session               |
//! | DELETE | /sessions/:id               | Yes  | Close a session                  |
//! | POST   | /sessions/:id/messages      | Yes  | Submit a command to the agent    |
//! | POST   | /sessions/:id/approve       | Yes  | Submit operator approval/rejection |
//! | GET    | /sessions/:id/approvals     | Yes  | List pending approval requests   |
//! | GET    | /sessions/:id/events        | Yes  | SSE stream of session events     |
//! | POST   | /sessions/:id/propose-transfer | Yes | Direct SOL transfer proposal  |
//! | GET    | /metrics                    | Yes  | Operational metrics (JSON)       |
//!
//! Future routes (V2): /tools, /subscriptions, /wallets
//!
//! **What belongs here:** router wiring, middleware stacking, server lifecycle.
//! **What does NOT belong here:** business logic, Solana code, agent execution.

use std::net::SocketAddr;

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{delete, get, post},
    Router,
};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing::info;

use claw_observability::HealthRegistry;

use crate::{
    auth::require_bearer_token,
    errors::ApiError,
    routes::{
        approve::{list_approvals, submit_approval},
        audit::list_audit,
        chat::post_chat,
        debug_seed::seed_demo,
        events::session_events,
        health::health_handler,
        messages::send_message,
        metrics::metrics_handler,
        get_approval::get_approval,
        pending_approvals::list_pending_approvals,
        policy_rules::list_policy_rules,
        propose::propose_transfer,
        sessions::{close_session, list_sessions, open_session},
        solend_jit_signing::prepare_solend_signing,
        solend_signatures::{get_solend_signature, submit_solend_signature},
        solend_withdraw_jit_signing::prepare_solend_withdraw_signing,
        stage2_chat_execute::post_w5g_execute,
        stage2_w5h_funding_confirm::post_w5h_funding_confirm,
        wallets::list_wallets,
        wallet_challenges::{create_wallet_challenge, confirm_wallet_challenge},
        wallet_signatures::{bind_wallet, list_wallet_signatures, submit_wallet_signature},
    },
};

// Re-export for route handlers that need it
pub use crate::state::AppState;

/// Builds the full `axum::Router` for the daemon API.
///
/// Separated from `start()` so that integration tests can build the
/// router without actually binding a port.
pub fn create_router(state: AppState, health: HealthRegistry) -> Router {
    // Routes that do NOT require auth
    let public = Router::new()
        .route("/health", get(health_handler))
        .with_state(health);

    // Routes that DO require auth
    let mut protected = Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions", post(open_session))
        .route("/sessions/:id", delete(close_session))
        // Operator loop: submit a command to an active session's agent
        .route("/sessions/:id/messages", post(send_message))
        // Human approval loop
        .route("/sessions/:id/approve", post(submit_approval))
        .route("/sessions/:id/approvals", get(list_approvals))
        // External wallet signatures
        .route("/sessions/:id/wallet-signatures", post(submit_wallet_signature))
        .route("/sessions/:id/wallet-signatures", get(list_wallet_signatures))
        .route("/sessions/:id/bind-wallet", post(bind_wallet))
        // Solend-specific signing retrieval + submit (Phase 4C-6)
        .route(
            "/sessions/:id/solend-signatures/:request_id",
            get(get_solend_signature),
        )
        .route(
            "/sessions/:id/solend-signatures/:request_id",
            post(submit_solend_signature),
        )
        // Phase 6B Window 2 — JIT signing-handoff prepare. The frontend
        // calls this from the user's "Sign with Phantom" click handler.
        .route(
            "/sessions/:session_id/approvals/:approval_request_id/solend-signing/prepare",
            post(prepare_solend_signing),
        )
        // Phase 6I-F — Solend WITHDRAW JIT signing-handoff prepare. The
        // frontend calls this on the user's Sign-click for an approved
        // withdraw-all proposal. Reuses the same blockhash provider,
        // signing store, and Phase 6D / 6E submit + confirmation
        // pipeline as deposit; the route differs only in which parked
        // intent it consumes and which plan assembler it invokes.
        .route(
            "/sessions/:session_id/approvals/:approval_request_id/solend-withdraw-jit/prepare",
            post(prepare_solend_withdraw_signing),
        )
        // Phase 5D.2 — user-facing chat route. Body limit is enforced
        // at the framework layer via `DefaultBodyLimit::max(4096)`;
        // oversize bodies are rejected with 413 BEFORE the handler
        // runs.
        .route(
            "/sessions/:id/chat",
            post(post_chat).layer(DefaultBodyLimit::max(4096)),
        )
        // W5g — chat-card controlled-wallet Solend deposit execution.
        // The route is opt-in via the daemon's env gates; when the
        // executor isn't wired the handler returns 503. Body limit is
        // 2 KB — well above the maximum legitimate payload (a 32-char
        // rule_id + 64-char canonical hash + 64-char approval phrase +
        // JSON envelope is < 300 bytes).
        .route(
            "/sessions/:id/stage2/w5g/execute",
            post(post_w5g_execute).layer(DefaultBodyLimit::max(2048)),
        )
        // W5h-lite — funding-confirm route. After the user Phantom-signs
        // a TransferChecked of 0.25 USDC into the controlled wallet's
        // USDC ATA, the frontend POSTs the signature here so the
        // backend can authoritatively verify the token-balance delta
        // (read-only `getTransaction` with `maxSupportedTransactionVersion:0`).
        // The handler is opt-in via the daemon's W5h substrate wiring;
        // when not wired the route returns 503. Body limit is 2 KB —
        // the legitimate payload (rule_id + signature + 4 base58
        // pubkeys + JSON envelope) is < 500 bytes.
        .route(
            "/sessions/:id/stage2/w5h/funding/confirm",
            post(post_w5h_funding_confirm).layer(DefaultBodyLimit::max(2048)),
        )
        // Wallet ownership proof (challenge-response)
        .route("/sessions/:id/wallet-bind-challenge", post(create_wallet_challenge))
        .route("/sessions/:id/wallet-bind-confirm", post(confirm_wallet_challenge))
        // SSE event stream
        .route("/sessions/:id/events", get(session_events))
        // Operational metrics
        .route("/metrics", get(metrics_handler))
        // Direct transaction proposal (bypasses LLM agent)
        .route("/sessions/:id/propose-transfer", post(propose_transfer))
        // Showcase / dashboard read surfaces
        .route("/policy/rules", get(list_policy_rules))
        .route("/pending-approvals", get(list_pending_approvals))
        .route("/approvals/:id", get(get_approval))
        .route("/wallets", get(list_wallets))
        .route("/audit", get(list_audit))
        // Development-only — returns 503 unless CLAW_ENABLE_DEMO_SEED=1
        .route("/debug/seed-demo", post(seed_demo));

    // Rate limiting layer — applied after auth, before handlers.
    if let Some(ref limiter) = state.rate_limiter {
        protected = protected.route_layer(middleware::from_fn_with_state(
            limiter.clone(),
            crate::rate_limit::rate_limit_middleware,
        ));
    }

    let auth_state = crate::auth::AuthState {
        token: state.auth_token.clone(),
        registry: state.operator_registry.clone(),
    };
    let protected = protected
        .route_layer(middleware::from_fn_with_state(
            auth_state,
            require_bearer_token,
        ))
        .with_state(state);

    // CORS: Allow local bridge pages to call the daemon API.
    // Only permits localhost origins. In production, restrict further.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .merge(public)
        .merge(protected)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
}

/// Starts the HTTP server and returns a `JoinHandle`.
/// This function does not block — the server runs as a background task.
pub async fn start(
    bind_addr: &str,
    port: u16,
    state: AppState,
    health: HealthRegistry,
) -> Result<tokio::task::JoinHandle<()>, ApiError> {
    let addr: SocketAddr = format!("{}:{}", bind_addr, port)
        .parse()
        .map_err(|e| ApiError::Config(format!("invalid bind address: {e}")))?;

    let router = create_router(state, health);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| ApiError::Bind(e.to_string()))?;

    info!(addr = %addr, "API server listening");

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error = %e, "API server error");
        }
    });

    Ok(handle)
}

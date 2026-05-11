//! `claw-api` — local HTTP control surface for the ClawSolana daemon.
//!
//! Exposes a minimal REST API on `127.0.0.1:7070` by default.
//! Protected by a per-session bearer token except for `/health`.
//!
//! # Design constraints
//!
//! - This crate does NOT depend on `claw-gateway` directly. The gateway
//!   wires the API server with concrete implementations at startup.
//! - Route handlers receive only the minimal state they need (via `AppState`).
//! - No Solana code belongs here.
//! - No business logic belongs here — routes delegate to gateway components.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod auth;
pub mod errors;
pub mod rate_limit;
pub mod routes;
pub mod serde_str;
pub mod server;
pub mod state;

pub use errors::ApiError;
pub use server::{create_router, start};
pub use state::{
    AppState, ApprovalHandler, ApprovalHandlerRef,
    ChatExecuteHandler, ChatExecuteHandlerRef, ChatExecuteRequestDto,
    ChatExecuteResultDto, ChatExecuteRouteOutcome,
    ChatHandler, ChatHandlerRef, ChatResponse, ChatRouteOutcome,
    EventSubscriber, EventSubscriberRef,
    MessageHandler, MessageHandlerRef, SessionManagerRef, SessionOps,
    TransactionProposer, TransactionProposerRef, ProposeTransferResult,
    WalletChallengeHandler, WalletChallengeHandlerRef, WalletChallengeInfo,
    WalletSignatureHandler, WalletSignatureHandlerRef, WalletSignatureOutcome,
    PendingWalletSignatureInfo,
};
pub use auth::AuthToken;
pub use rate_limit::RateLimiter;

//! HTTP gateway (REST surface + chat-route dispatch).
//!
//! Apps-layer entry point for the Bounded Intent Execution Loop.
//! See [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md) for
//! the loop the gateway hosts.
//!
//! # Status
//!
//! Scaffold. The HTTP server, route handlers, and chat dispatcher
//! land alongside Phases 2–4 of the roadmap.
//!
//! # Routes (planned)
//!
//! - `POST /sessions` — session creation.
//! - `POST /sessions/:id/chat` — chat dispatcher (deterministic
//!   recogniser first; LLM fallback only for non-Intent messages).
//! - `POST /sessions/:id/funding/confirm` — backend funding
//!   verification (delegates to [`funding`](../../crates/funding)).
//! - `GET /sessions/:id/order/:rule_id_hex` — read-only order
//!   status (polled by the frontend after `budget_reserved`).
//!
//! # Out of scope (Phase 0)
//!
//! Everything below the doc layer. This binary currently prints a
//! pointer and exits.

fn main() {
    println!("solfrontier 2026 gateway — scaffold; see docs/ARCHITECTURE.md");
}

//! Typed Intent + canonical hashing.
//!
//! Phase 1 of the Bounded Intent Execution Loop. See
//! [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md) for the
//! loop and this crate's role inside it.
//!
//! # Status
//!
//! Scaffold. No public types yet; behaviour lands in Phase 1.
//!
//! # Boundary
//!
//! - Inputs: a structured request from the chat dispatcher
//!   (deterministic recognise; **never** LLM-derived).
//! - Outputs: a typed `Intent` carrying `rule_id` + canonical hash.
//!   The hash is the source of truth for everything downstream.
//! - This crate never touches RPC, never touches a keypair, never
//!   builds a transaction.
//!
//! # Parity requirement
//!
//! The canonical serializer that lands in Phase 1 must produce
//! byte-identical output to the corresponding TypeScript encoder in
//! the frontend. CI gates this with a shared fixture.

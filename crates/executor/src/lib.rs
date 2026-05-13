//! Controlled-wallet executor + adapter dispatcher.
//!
//! Phase 4 of the Bounded Intent Execution Loop. See
//! [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md) §9 and
//! [`docs/SECURITY_BOUNDARIES.md`](../../../docs/SECURITY_BOUNDARIES.md) §B2/§B5.
//!
//! # Status
//!
//! Scaffold. Signing + broadcasting + adapter dispatch land in
//! Phase 4.
//!
//! # Boundary
//!
//! - The controlled-wallet keypair is loaded at daemon startup from
//!   a path pointed to by an env var. The keypair never appears in
//!   source, never in an HTTP request body, never in logs.
//! - This crate is the **only** module in the workspace that holds
//!   keypair bytes at runtime.
//! - This crate does not accept an opaque "signed transaction" —
//!   it accepts a typed `ActionRequest` and delegates tx
//!   construction to the appropriate adapter, then signs the
//!   resulting message.
//!
//! # ActionRequest variants (planned)
//!
//! - `SolendDeposit` (Phase 4 — first adapter)
//! - `SolendWithdrawAll` (Phase 6)
//! - `JupiterSwap` (Phase 6 — second adapter)
//!
//! Each adapter validates its own inputs against the pinned demo
//! shape **before** the executor signs. The controlled wallet does
//! not sign blobs.
//!
//! # Out of scope
//!
//! - PDA-authority signing (Phase 7).
//! - HSM / KMS integration.
//! - Multi-wallet rotation.

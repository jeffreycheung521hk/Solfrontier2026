//! Jupiter adapter — next adapter for the Bounded Intent Execution
//! Loop, after Solend.
//!
//! See [`docs/ROADMAP.md`](../../../../docs/ROADMAP.md) Phase 6.
//!
//! # Status
//!
//! Scaffold. Implementation is deliberately deferred behind the
//! Solend adapter (Phase 4) because the Jupiter conditional bracket
//! is real work — not glue code:
//!
//! - dual bracket instructions;
//! - checkpoint PDA lifecycle;
//! - SPL Token account unpacking for delta enforcement;
//! - instructions-sysvar adapter for sibling-instruction
//!   verification;
//! - fresh transaction-size / CU measurement against the deployed
//!   Jupiter program.
//!
//! The hackathon sizing harness (J-prep / I3 / J4) produced
//! evidence that the conditional path is mapped; the live trustless
//! execution is the next milestone here.
//!
//! # Boundary (planned)
//!
//! - Inputs: a typed `JupiterSwap` request (input mint, output
//!   mint, amount, slippage cap, route hash).
//! - Output: a single transaction containing the swap bracket
//!   instructions, ready for the executor to sign + broadcast.
//! - The adapter MUST refuse any inputs that don't match a route
//!   hash the gateway pre-cleared at order-creation time. The
//!   route hash is part of the Intent's canonical fields.
//!
//! # Out of scope
//!
//! - Plain LLM-driven Jupiter swaps (the hackathon shipped this in
//!   the manual chat path; it's a different loop).
//! - Cross-chain swaps.

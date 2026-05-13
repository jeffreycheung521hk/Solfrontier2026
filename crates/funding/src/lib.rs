//! Funding-tx verifier (memo + token delta).
//!
//! Phase 2 of the Bounded Intent Execution Loop. See
//! [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md) §5 and
//! [`docs/SECURITY_BOUNDARIES.md`](../../../docs/SECURITY_BOUNDARIES.md) §B3
//! for the boundary and threat model.
//!
//! # Status
//!
//! Scaffold. The verifier itself lands in Phase 2.
//!
//! # Boundary
//!
//! - Inputs: a funding signature + the persisted `Intent` it claims
//!   to fund.
//! - Side effect: read-only `getTransaction` RPC call. No writes,
//!   no signing.
//! - Output: a typed result — `FundingConfirmed { signature, slot }`
//!   on success; `FundingPending` on RPC lag (null result);
//!   `FundingInvalid { code }` on any axis mismatch.
//! - A `FundingInvalid` result **must** block the
//!   `funding_pending → budget_reserved` transition.
//!
//! # Equality axes
//!
//! 1. Memo present, correct program id, exact payload bytes.
//! 2. `postTokenBalances − preTokenBalances` for the controlled
//!    USDC ATA equals the persisted `amount_raw`.
//! 3. Mint == USDC pin; owner == controlled wallet pubkey.
//! 4. `accountIndex → pubkey` resolved against the tx message keys
//!    (not positional).

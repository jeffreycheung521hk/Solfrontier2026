//! Shared integration-test support for the W5c conditional Solend
//! deposit path and downstream slices (W5d chat-conditional bridge,
//! and any future test that needs the same direct-Solend deposit
//! client / RPC plumbing / P5c invariants).
//!
//! Each test binary under `crates/gateway/tests/*.rs` declares
//! `mod common;` and then imports the items it needs via
//! `use common::w5c_deposit_support::*;` — Rust's standard idiom for
//! sharing code across separate test-binary compilations.
//!
//! See `w5c_deposit_support.rs` for the W5c origin notes — the items
//! here started life inside `tests/stage2_live_conditional_solend_deposit.rs`
//! (W5c) and were lifted verbatim (no behaviour change) so the W5d
//! demo bridge can reuse them without duplicating the live broadcast
//! path.

#![allow(dead_code)]

pub mod w5c_deposit_support;

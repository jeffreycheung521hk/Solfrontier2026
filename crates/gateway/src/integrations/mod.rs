//! External protocol integrations.
//!
//! Each submodule implements a client for an external protocol that
//! Claw's control plane can route approved transactions through.

pub mod jupiter;
pub mod jupiter_alt;
pub mod jupiter_jit;
pub mod jupiter_park;
pub mod jupiter_production;
pub mod jupiter_tx;
pub mod solend;
pub mod solend_confirmation;
pub mod solend_jit_ready;
pub mod solend_lifecycle;
pub mod solend_park;
pub mod solend_preflight;
pub mod solend_signing;
pub mod solend_submit;
pub mod solend_tx_plan;

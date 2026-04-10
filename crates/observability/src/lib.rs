//! `claw-observability` — structured logging, metrics, and health.
//!
//! # Responsibilities
//! - Initialize `tracing-subscriber` with environment-driven log levels
//! - Define span and field naming conventions (so all logs are consistent)
//! - Provide a `HealthCheck` trait and an `HealthRegistry` that aggregates
//!   component health for the `/health` API endpoint
//! - Provide lightweight counter/gauge/histogram types for V1 metrics
//! - Provide `CorrelationId` extraction/injection helpers
//!
//! # What does NOT belong here
//! - Business logic
//! - Database access
//! - Any Solana-specific code
//! - Alert routing (that's in `claw-gateway`)

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod correlation;
pub mod health;
pub mod init;
pub mod metrics;

pub use correlation::CorrelationId;
pub use health::{ComponentHealth, HealthCheck, HealthRegistry, HealthStatus};
pub use init::{init_tracing, init_tracing_with_filter, try_init_tracing, LogFormat};
pub use metrics::{Counter, Gauge, MetricsRegistry};

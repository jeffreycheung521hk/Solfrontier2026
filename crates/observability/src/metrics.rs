//! Lightweight metrics for V1.
//!
//! Uses atomic counters and gauges. For V2, swap this module's internals
//! for the `prometheus` crate while keeping the same public API.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

/// An atomic monotonically-increasing counter.
#[derive(Debug, Clone, Default)]
pub struct Counter(Arc<AtomicU64>);

impl Counter {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    pub fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// An atomic gauge (can go up or down).
#[derive(Debug, Clone, Default)]
pub struct Gauge(Arc<AtomicI64>);

impl Gauge {
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(0)))
    }

    pub fn set(&self, value: i64) {
        self.0.store(value, Ordering::Relaxed);
    }

    pub fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A simple registry holding named counters and gauges.
/// In V1 this is serialized to JSON for the `/metrics` endpoint.
/// In V2 replace with prometheus::Registry.
#[derive(Debug, Clone, Default)]
pub struct MetricsRegistry {
    // For V1, we define all metrics statically.
    pub sessions_opened:          Counter,
    pub sessions_closed:          Counter,
    pub sessions_active:          Gauge,
    pub agent_tasks_started:      Counter,
    pub agent_tasks_completed:    Counter,
    pub agent_tasks_failed:       Counter,
    pub tool_calls_total:         Counter,
    pub tool_calls_failed:        Counter,
    pub tool_calls_timed_out:     Counter,
    pub transactions_proposed:    Counter,
    pub transactions_sent:        Counter,
    pub transactions_confirmed:   Counter,
    pub transactions_failed:      Counter,
    pub policy_rejections:        Counter,
    pub rpc_calls_total:          Counter,
    pub rpc_calls_failed:         Counter,
    pub rpc_retries:              Counter,
    pub ws_subscriptions_active:  Gauge,
    pub alerts_emitted:           Counter,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

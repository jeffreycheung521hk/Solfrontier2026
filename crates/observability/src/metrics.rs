//! Lightweight metrics for V1.
//!
//! Uses atomic counters and gauges. For V2, swap this module's internals
//! for the `prometheus` crate while keeping the same public API.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

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
    pub rate_limit_hits:          Counter,

    /// Per-rule policy hit counters.
    /// Key: rule name (e.g. "denylist-block"), Value: hit count.
    /// Dynamic map — new rules automatically get a counter on first hit.
    /// When migrating to Prometheus/OTel, replace with a `CounterVec` label.
    pub policy_rule_hits:         CounterMap,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A dynamic map of named counters. Thread-safe, lock-free reads.
/// Designed to be forward-compatible with Prometheus `CounterVec` or
/// OpenTelemetry `Meter::u64_counter` with attribute labels.
#[derive(Debug, Clone, Default)]
pub struct CounterMap(Arc<DashMap<String, AtomicU64>>);

impl CounterMap {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    /// Increment the counter for the given key by 1.
    pub fn increment(&self, key: &str) {
        self.0
            .entry(key.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Get the current value for a key. Returns 0 if the key has never been incremented.
    pub fn get(&self, key: &str) -> u64 {
        self.0
            .get(key)
            .map(|v| v.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Returns a snapshot of all counters as (key, value) pairs.
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        self.0
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_map_increment_and_get() {
        let map = CounterMap::new();
        assert_eq!(map.get("rule-a"), 0);

        map.increment("rule-a");
        map.increment("rule-a");
        map.increment("rule-b");

        assert_eq!(map.get("rule-a"), 2);
        assert_eq!(map.get("rule-b"), 1);
        assert_eq!(map.get("rule-c"), 0);
    }

    #[test]
    fn counter_map_snapshot() {
        let map = CounterMap::new();
        map.increment("x");
        map.increment("x");
        map.increment("y");

        let mut snap = map.snapshot();
        snap.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(snap, vec![
            ("x".to_string(), 2),
            ("y".to_string(), 1),
        ]);
    }

    #[test]
    fn counter_map_clone_shares_state() {
        let map = CounterMap::new();
        let clone = map.clone();

        map.increment("shared");
        assert_eq!(clone.get("shared"), 1);
    }
}

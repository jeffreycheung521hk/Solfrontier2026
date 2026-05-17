//! Health check infrastructure.
//!
//! Each major component implements `HealthCheck`. The `HealthRegistry`
//! aggregates them and exposes a single system-wide `HealthStatus`.
//! This is served by the API crate on `/health`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The health status of a single component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

/// A single component's health report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    pub name:       String,
    pub status:     HealthStatus,
    pub message:    Option<String>,
    pub checked_at: DateTime<Utc>,
    pub latency_ms: Option<u64>,
}

/// Trait implemented by each major component that wants to participate
/// in the health aggregation.
#[async_trait]
pub trait HealthCheck: Send + Sync {
    /// The component name (shown in health responses).
    fn name(&self) -> &str;

    /// Perform an async health check and return the result.
    /// This method must not block for more than ~2 seconds.
    async fn check(&self) -> ComponentHealth;
}

/// Aggregates health checks from all registered components.
/// The overall system health is the minimum (worst) of all components.
#[derive(Clone, Default)]
pub struct HealthRegistry {
    checks: Arc<RwLock<Vec<Arc<dyn HealthCheck>>>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a component's health check.
    pub async fn register(&self, check: Arc<dyn HealthCheck>) {
        self.checks.write().await.push(check);
    }

    /// Run all registered health checks concurrently and return the results.
    pub async fn check_all(&self) -> SystemHealth {
        let checks = self.checks.read().await.clone();
        let mut results = HashMap::new();

        let futs: Vec<_> = checks.iter().map(|c| c.check()).collect();
        let component_results = futures::future::join_all(futs).await;

        let mut overall = HealthStatus::Healthy;
        for result in component_results {
            if result.status == HealthStatus::Unhealthy {
                overall = HealthStatus::Unhealthy;
            } else if result.status == HealthStatus::Degraded
                && overall == HealthStatus::Healthy
            {
                overall = HealthStatus::Degraded;
            }
            results.insert(result.name.clone(), result);
        }

        SystemHealth {
            status: overall,
            components: results,
            checked_at: Utc::now(),
        }
    }
}

/// The aggregated health of the entire system.
#[derive(Debug, Serialize, Deserialize)]
pub struct SystemHealth {
    pub status:     HealthStatus,
    pub components: HashMap<String, ComponentHealth>,
    pub checked_at: DateTime<Utc>,
}

//! RPC pool: multi-endpoint Solana RPC client with failover.
//!
//! # Design
//!
//! The `RpcPool` holds a list of configured `RpcEndpoint`s and routes requests
//! to healthy endpoints. The selection strategy is `PrimaryWithFallback` by
//! default: use the primary endpoint, fail over to replicas only when the
//! primary's circuit breaker is open.
//!
//! Write operations (sendTransaction) always try the primary first.
//! Read operations can be spread across replicas.
//!
//! # Circuit breaker
//!
//! Each endpoint tracks consecutive failures. After `failure_threshold`
//! failures, the circuit opens and the endpoint is skipped. A half-open probe
//! is issued every `recovery_interval` to re-test the endpoint.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use tracing::{debug, info, warn};

use claw_types::solana::CommitmentLevel;

use crate::errors::SolanaError;

pub mod client;
pub use client::ClawRpcClient;

/// Configuration for the RPC endpoint pool.
#[derive(Debug, Clone)]
pub struct RpcPoolConfig {
    /// Ordered list of endpoints. The first entry is the primary.
    pub endpoints: Vec<EndpointConfig>,
    /// Number of consecutive failures before an endpoint is circuit-broken.
    pub failure_threshold: u32,
    /// How long to wait before re-testing a broken endpoint.
    pub recovery_interval: Duration,
    /// Per-request timeout.
    pub request_timeout: Duration,
}

impl Default for RpcPoolConfig {
    fn default() -> Self {
        Self {
            endpoints: vec![EndpointConfig {
                url: "https://api.devnet.solana.com".to_string(),
                label: "devnet-public".to_string(),
                is_write_endpoint: true,
            }],
            failure_threshold: 3,
            recovery_interval: Duration::from_secs(30),
            request_timeout: Duration::from_secs(15),
        }
    }
}

/// A single RPC endpoint configuration.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub url: String,
    pub label: String,
    /// If true, this endpoint should be preferred for write operations
    /// (sendTransaction).
    pub is_write_endpoint: bool,
}

/// Internal state of a single endpoint's circuit breaker.
struct EndpointState {
    config: EndpointConfig,
    client: Arc<RpcClient>,
    consecutive_failures: u32,
    circuit_open_since: Option<Instant>,
}

impl EndpointState {
    fn new(config: EndpointConfig, timeout: Duration) -> Self {
        let client = Arc::new(RpcClient::new_with_timeout(
            config.url.clone(),
            timeout,
        ));
        Self {
            config,
            client,
            consecutive_failures: 0,
            circuit_open_since: None,
        }
    }

    fn is_available(&self, failure_threshold: u32, recovery_interval: Duration) -> bool {
        match self.circuit_open_since {
            None => true,
            Some(since) => {
                // Allow a probe after the recovery interval
                since.elapsed() >= recovery_interval
            }
        }
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.circuit_open_since = None;
    }

    fn record_failure(&mut self, failure_threshold: u32) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= failure_threshold {
            if self.circuit_open_since.is_none() {
                warn!(
                    endpoint = %self.config.label,
                    failures = self.consecutive_failures,
                    "circuit breaker opened for RPC endpoint"
                );
                self.circuit_open_since = Some(Instant::now());
            }
        }
    }
}

/// The RPC endpoint pool. Clone freely — the inner state is `Arc`-wrapped.
#[derive(Clone)]
pub struct RpcPool {
    config: Arc<RpcPoolConfig>,
    endpoints: Arc<RwLock<Vec<EndpointState>>>,
}

impl RpcPool {
    /// Creates a new pool from the given config.
    pub fn new(config: RpcPoolConfig) -> Self {
        let endpoints: Vec<EndpointState> = config
            .endpoints
            .iter()
            .cloned()
            .map(|ec| EndpointState::new(ec, config.request_timeout))
            .collect();

        info!(count = endpoints.len(), "RPC pool initialized");

        Self {
            endpoints: Arc::new(RwLock::new(endpoints)),
            config: Arc::new(config),
        }
    }

    /// Returns a client for read operations.
    /// Uses the first available endpoint (primary first).
    pub fn read_client(&self) -> Result<Arc<RpcClient>, SolanaError> {
        self.pick_any()
    }

    /// Returns a client for write operations (sendTransaction).
    /// Prefers `is_write_endpoint = true` endpoints.
    pub fn write_client(&self) -> Result<Arc<RpcClient>, SolanaError> {
        let endpoints = self.endpoints.read();
        // Try write-designated endpoints first
        for ep in endpoints.iter() {
            if ep.config.is_write_endpoint
                && ep.is_available(
                    self.config.failure_threshold,
                    self.config.recovery_interval,
                )
            {
                return Ok(ep.client.clone());
            }
        }
        // Fall back to any available
        drop(endpoints);
        self.pick_any()
    }

    fn pick_any(&self) -> Result<Arc<RpcClient>, SolanaError> {
        let endpoints = self.endpoints.read();
        for ep in endpoints.iter() {
            if ep.is_available(self.config.failure_threshold, self.config.recovery_interval) {
                return Ok(ep.client.clone());
            }
        }
        Err(SolanaError::PoolExhausted)
    }

    /// Records a successful call to an endpoint by URL.
    pub fn record_success(&self, url: &str) {
        let mut endpoints = self.endpoints.write();
        if let Some(ep) = endpoints.iter_mut().find(|e| e.config.url == url) {
            ep.record_success();
        }
    }

    /// Records a failed call to an endpoint by URL.
    pub fn record_failure(&self, url: &str) {
        let mut endpoints = self.endpoints.write();
        if let Some(ep) = endpoints.iter_mut().find(|e| e.config.url == url) {
            ep.record_failure(self.config.failure_threshold);
        }
    }
}

/// Map `CommitmentLevel` to the Solana SDK's `CommitmentConfig`.
pub fn to_sdk_commitment(level: CommitmentLevel) -> CommitmentConfig {
    match level {
        CommitmentLevel::Processed => CommitmentConfig::processed(),
        CommitmentLevel::Confirmed => CommitmentConfig::confirmed(),
        CommitmentLevel::Finalized => CommitmentConfig::finalized(),
    }
}

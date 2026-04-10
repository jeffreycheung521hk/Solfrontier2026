//! Per-token sliding-window rate limiter middleware.
//!
//! Each bearer token gets an independent counter. Timestamps of recent
//! requests are stored in a `VecDeque`; entries older than the window
//! are evicted on every check. If the count after eviction exceeds
//! `requests_per_minute + burst_size`, the request is rejected with
//! `429 Too Many Requests` and a `Retry-After` header.
//!
//! **What belongs here:** rate-limit logic, 429 formatting.
//! **What does NOT belong here:** auth, business logic, Solana code.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use dashmap::DashMap;
use serde_json::json;

use claw_observability::metrics::MetricsRegistry;

/// Shared rate limiter state, injected into the middleware via `State`.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<RateLimiterInner>,
}

struct RateLimiterInner {
    /// Per-token request timestamps.
    buckets: DashMap<u64, VecDeque<Instant>>,
    /// Sliding window duration (always 60 s for "per minute").
    window: Duration,
    /// Max requests allowed within the window (requests_per_minute + burst_size).
    limit: u32,
    /// Metrics handle for incrementing `rate_limit_hits`.
    metrics: Arc<MetricsRegistry>,
}

impl RateLimiter {
    /// Creates a new rate limiter.
    ///
    /// `requests_per_minute` + `burst_size` = effective ceiling within any
    /// 60-second sliding window.
    pub fn new(requests_per_minute: u32, burst_size: u32, metrics: Arc<MetricsRegistry>) -> Self {
        Self {
            inner: Arc::new(RateLimiterInner {
                buckets: DashMap::new(),
                window: Duration::from_secs(60),
                limit: requests_per_minute + burst_size,
                metrics,
            }),
        }
    }

    /// Check whether the token is allowed. Returns `Ok(())` if allowed,
    /// or `Err(retry_after_secs)` if the limit is exceeded.
    fn check(&self, token_hash: u64) -> Result<(), u64> {
        let now = Instant::now();
        let inner = &self.inner;
        let mut entry = inner.buckets.entry(token_hash).or_insert_with(VecDeque::new);
        let deque = entry.value_mut();

        // Evict timestamps outside the window.
        let cutoff = now - inner.window;
        while deque.front().map_or(false, |&t| t < cutoff) {
            deque.pop_front();
        }

        if deque.len() as u32 >= inner.limit {
            // Calculate retry-after from the oldest entry in the window.
            let oldest = deque.front().copied().unwrap_or(now);
            let retry_after = inner.window.saturating_sub(now.duration_since(oldest));
            inner.metrics.rate_limit_hits.increment();
            Err(retry_after.as_secs().max(1))
        } else {
            deque.push_back(now);
            Ok(())
        }
    }
}

/// Hash a token string to a u64 key for the DashMap.
/// We don't need cryptographic strength here — just fast distribution.
fn hash_token(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

/// Axum middleware function.
///
/// Extracts the bearer token from the `Authorization` header (or `?token=`
/// query param) and enforces the sliding-window limit. Must be applied
/// **after** auth so that only authenticated requests are counted.
pub async fn rate_limit_middleware(
    axum::extract::State(limiter): axum::extract::State<RateLimiter>,
    request: Request,
    next: Next,
) -> Response {
    // Extract token the same way auth does.
    let from_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    let from_query = request
        .uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|pair| pair.strip_prefix("token="))
                .map(|v| v.to_string())
        });

    let token = from_header.or(from_query).unwrap_or_default();
    let token_hash = hash_token(&token);

    match limiter.check(token_hash) {
        Ok(()) => next.run(request).await,
        Err(retry_after) => {
            let body = Json(json!({
                "error": "too_many_requests",
                "message": "rate limit exceeded",
                "retry_after": retry_after,
            }));
            let mut response = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
            response.headers_mut().insert(
                "Retry-After",
                retry_after.to_string().parse().unwrap(),
            );
            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_window_eviction() {
        let metrics = Arc::new(MetricsRegistry::new());
        let limiter = RateLimiter::new(3, 0, metrics.clone());

        let token_hash = hash_token("test-token");

        // First 3 requests should pass.
        assert!(limiter.check(token_hash).is_ok());
        assert!(limiter.check(token_hash).is_ok());
        assert!(limiter.check(token_hash).is_ok());

        // 4th should be rejected.
        assert!(limiter.check(token_hash).is_err());
        assert_eq!(metrics.rate_limit_hits.get(), 1);
    }

    #[test]
    fn different_tokens_independent() {
        let metrics = Arc::new(MetricsRegistry::new());
        let limiter = RateLimiter::new(1, 0, metrics);

        let h1 = hash_token("token-a");
        let h2 = hash_token("token-b");

        assert!(limiter.check(h1).is_ok());
        assert!(limiter.check(h1).is_err()); // token-a exhausted

        assert!(limiter.check(h2).is_ok()); // token-b still fresh
    }

    #[test]
    fn burst_extends_limit() {
        let metrics = Arc::new(MetricsRegistry::new());
        let limiter = RateLimiter::new(2, 3, metrics);
        let h = hash_token("burst-test");

        // Should allow 2 + 3 = 5 requests.
        for _ in 0..5 {
            assert!(limiter.check(h).is_ok());
        }
        assert!(limiter.check(h).is_err());
    }
}

/// Per-client-IP token bucket rate limiter.
///
/// Each client IP gets a token bucket that refills at a configured
/// rate. When the bucket is empty, requests are rejected with 429
/// Too Many Requests and a `Retry-After` header.
///
/// Stale buckets (no requests for 5 minutes) are garbage collected
/// every 60 seconds to bound memory usage.
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::types::RateLimitConfig;

/// Rate limiter state: a collection of per-IP token buckets.
pub struct RateLimiter {
    buckets: HashMap<IpAddr, TokenBucket>,
    last_gc: Instant,
}

/// A single token bucket for one client IP.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    last_used: Instant,
}

/// Result of a rate limit check.
pub enum RateLimitResult {
    /// Request is allowed.
    Allowed,
    /// Request is denied. Contains the number of seconds to wait.
    Denied { retry_after_secs: u64 },
}

/// How long a bucket can be idle before garbage collection.
const BUCKET_TTL: Duration = Duration::from_secs(300);

/// How often to run garbage collection.
const GC_INTERVAL: Duration = Duration::from_secs(60);

impl RateLimiter {
    /// Create a new rate limiter.
    pub fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            last_gc: Instant::now(),
        }
    }

    /// Check whether a request from `client_ip` is allowed.
    ///
    /// Refills the token bucket based on elapsed time, then tries
    /// to consume one token. If the bucket is empty, returns
    /// `Denied` with the time to wait for a token.
    pub fn check(&mut self, client_ip: IpAddr, config: &RateLimitConfig) -> RateLimitResult {
        let now = Instant::now();

        // Run GC periodically
        if now.duration_since(self.last_gc) >= GC_INTERVAL {
            self.gc(now);
        }

        let bucket = self
            .buckets
            .entry(client_ip)
            .or_insert_with(|| TokenBucket {
                tokens: config.burst as f64,
                last_refill: now,
                last_used: now,
            });

        // Refill tokens based on elapsed time
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * config.rps as f64).min(config.burst as f64);
        bucket.last_refill = now;
        bucket.last_used = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            RateLimitResult::Allowed
        } else {
            // How long until one token is available?
            let wait = (1.0 - bucket.tokens) / config.rps as f64;
            RateLimitResult::Denied {
                retry_after_secs: wait.ceil() as u64,
            }
        }
    }

    /// Remove buckets that haven't been used in `BUCKET_TTL`.
    fn gc(&mut self, now: Instant) {
        self.buckets
            .retain(|_, bucket| now.duration_since(bucket.last_used) < BUCKET_TTL);
        self.last_gc = now;
    }

    /// Number of tracked client IPs.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_10rps() -> RateLimitConfig {
        RateLimitConfig { rps: 10, burst: 20 }
    }

    #[test]
    fn under_limit_passes() {
        let mut limiter = RateLimiter::new();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        for _ in 0..20 {
            assert!(matches!(
                limiter.check(ip, &config_10rps()),
                RateLimitResult::Allowed
            ));
        }
    }

    #[test]
    fn over_limit_denied() {
        let mut limiter = RateLimiter::new();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        // Exhaust the burst
        for _ in 0..20 {
            limiter.check(ip, &config_10rps());
        }

        // Next request should be denied
        assert!(matches!(
            limiter.check(ip, &config_10rps()),
            RateLimitResult::Denied { .. }
        ));
    }

    #[test]
    fn denied_includes_retry_after() {
        let mut limiter = RateLimiter::new();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        for _ in 0..20 {
            limiter.check(ip, &config_10rps());
        }

        match limiter.check(ip, &config_10rps()) {
            RateLimitResult::Denied { retry_after_secs } => {
                assert!(retry_after_secs >= 1, "retry_after should be at least 1s");
            }
            RateLimitResult::Allowed => panic!("should be denied"),
        }
    }

    #[test]
    fn different_ips_independent() {
        let mut limiter = RateLimiter::new();
        let ip1: IpAddr = "192.168.1.1".parse().unwrap();
        let ip2: IpAddr = "192.168.1.2".parse().unwrap();
        let config = RateLimitConfig { rps: 1, burst: 1 };

        // ip1 exhausts its token
        limiter.check(ip1, &config);
        assert!(matches!(
            limiter.check(ip1, &config),
            RateLimitResult::Denied { .. }
        ));

        // ip2 should still be allowed
        assert!(matches!(
            limiter.check(ip2, &config),
            RateLimitResult::Allowed
        ));
    }

    #[test]
    fn gc_removes_stale_buckets() {
        let mut limiter = RateLimiter::new();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let config = config_10rps();

        limiter.check(ip, &config);
        assert_eq!(limiter.bucket_count(), 1);

        // Simulate time passing beyond TTL
        let future = Instant::now() + BUCKET_TTL + Duration::from_secs(1);
        limiter.gc(future);
        assert_eq!(limiter.bucket_count(), 0);
    }

    #[test]
    fn bucket_count_tracks_ips() {
        let mut limiter = RateLimiter::new();
        let config = config_10rps();

        for i in 1..=5u8 {
            let ip: IpAddr = format!("192.168.1.{i}").parse().unwrap();
            limiter.check(ip, &config);
        }

        assert_eq!(limiter.bucket_count(), 5);
    }
}

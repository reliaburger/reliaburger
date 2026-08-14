//! Ingress request metrics for the Wrapper proxy.
//!
//! Wrapper counts every request it handles and classifies each response by
//! status class (1xx–5xx), plus a live in-flight gauge. The counters are
//! plain atomics so the hot request path bumps them with no lock and no
//! allocation.
//!
//! Wrapper only *produces* these numbers. The Mayo metrics collector reads
//! the process-global instance via [`global_ingress_metrics`] and emits them
//! as `ingress_requests_total` (and friends); nothing in this module talks to
//! Mayo, so the two subsystems stay decoupled.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::StatusCode;

/// Process-global ingress metrics, initialised on first access.
///
/// A `OnceLock` is Rust's thread-safe lazy singleton: the first caller
/// constructs the `IngressMetrics`, every later caller gets the same
/// `&'static` reference. The proxy handler increments it; the Mayo collector
/// reads it. Sharing one instance means the collector sees the live totals
/// without Wrapper having to hand it a reference explicitly.
static GLOBAL: OnceLock<IngressMetrics> = OnceLock::new();

/// The process-wide ingress metrics accessor.
///
/// Returns the single shared [`IngressMetrics`]. The proxy calls this on every
/// request to record it; the Mayo collector calls it to read the counters.
pub fn global_ingress_metrics() -> &'static IngressMetrics {
    GLOBAL.get_or_init(IngressMetrics::new)
}

/// Atomic request counters for the ingress proxy.
///
/// Every field is an `AtomicU64` (or two, for the gauge) so several proxy
/// tasks on different runtime threads can update them concurrently without a
/// mutex. All operations use `Ordering::Relaxed`: the counters are
/// independent tallies, not a lock, so we don't need any cross-field ordering
/// guarantee — only that each individual add is atomic.
#[derive(Debug, Default)]
pub struct IngressMetrics {
    /// Total requests the proxy has started handling.
    total: AtomicU64,
    /// Responses with a 1xx status.
    status_1xx: AtomicU64,
    /// Responses with a 2xx status.
    status_2xx: AtomicU64,
    /// Responses with a 3xx status.
    status_3xx: AtomicU64,
    /// Responses with a 4xx status.
    status_4xx: AtomicU64,
    /// Responses with a 5xx status.
    status_5xx: AtomicU64,
    /// Requests currently being handled (a live gauge).
    in_flight: AtomicU64,
}

/// A point-in-time copy of the ingress counters.
///
/// The Mayo collector reads a [`MetricsSnapshot`] rather than the live atomics
/// so it works with a coherent set of values it can freely format and diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Total requests handled.
    pub total: u64,
    /// Responses with a 1xx status.
    pub status_1xx: u64,
    /// Responses with a 2xx status.
    pub status_2xx: u64,
    /// Responses with a 3xx status.
    pub status_3xx: u64,
    /// Responses with a 4xx status.
    pub status_4xx: u64,
    /// Responses with a 5xx status.
    pub status_5xx: u64,
    /// Requests currently in flight.
    pub in_flight: u64,
}

impl IngressMetrics {
    /// Create a fresh metrics set with every counter at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a request has started. Bumps the total counter.
    pub fn record_request(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a response by its status class. Increments exactly one of the
    /// per-class counters.
    pub fn record_response(&self, status: StatusCode) {
        let counter = match status.as_u16() / 100 {
            1 => &self.status_1xx,
            2 => &self.status_2xx,
            3 => &self.status_3xx,
            4 => &self.status_4xx,
            _ => &self.status_5xx,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark a request as entering the in-flight set.
    pub fn inc_in_flight(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark a request as leaving the in-flight set.
    ///
    /// Uses a saturating decrement so a stray underflow (never expected, since
    /// each increment is paired) can't wrap the gauge to `u64::MAX`.
    pub fn dec_in_flight(&self) {
        // `fetch_update` lets us apply `saturating_sub` atomically: the closure
        // re-runs if another thread changes the value in between, so the gauge
        // never underflows even under contention.
        let _ = self
            .in_flight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// Total requests handled so far.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Requests currently in flight.
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Take a coherent copy of all counters for a reader (e.g. Mayo).
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            total: self.total.load(Ordering::Relaxed),
            status_1xx: self.status_1xx.load(Ordering::Relaxed),
            status_2xx: self.status_2xx.load(Ordering::Relaxed),
            status_3xx: self.status_3xx.load(Ordering::Relaxed),
            status_4xx: self.status_4xx.load(Ordering::Relaxed),
            status_5xx: self.status_5xx.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_response_buckets_by_status_class() {
        let metrics = IngressMetrics::new();
        metrics.record_response(StatusCode::OK);
        metrics.record_response(StatusCode::CREATED);
        metrics.record_response(StatusCode::NOT_FOUND);
        metrics.record_response(StatusCode::INTERNAL_SERVER_ERROR);
        metrics.record_response(StatusCode::MOVED_PERMANENTLY);
        metrics.record_response(StatusCode::CONTINUE);

        let snap = metrics.snapshot();
        assert_eq!(snap.status_1xx, 1);
        assert_eq!(snap.status_2xx, 2);
        assert_eq!(snap.status_3xx, 1);
        assert_eq!(snap.status_4xx, 1);
        assert_eq!(snap.status_5xx, 1);
    }

    #[test]
    fn record_request_counts_total() {
        let metrics = IngressMetrics::new();
        metrics.record_request();
        metrics.record_request();
        metrics.record_request();
        assert_eq!(metrics.total(), 3);
    }

    #[test]
    fn in_flight_gauge_rises_and_falls() {
        let metrics = IngressMetrics::new();
        metrics.inc_in_flight();
        metrics.inc_in_flight();
        assert_eq!(metrics.in_flight(), 2);
        metrics.dec_in_flight();
        assert_eq!(metrics.in_flight(), 1);
    }

    #[test]
    fn dec_in_flight_saturates_at_zero() {
        let metrics = IngressMetrics::new();
        // Never incremented — a decrement must not wrap to u64::MAX.
        metrics.dec_in_flight();
        assert_eq!(metrics.in_flight(), 0);
    }

    #[test]
    fn global_accessor_returns_the_same_instance() {
        let a = global_ingress_metrics();
        let b = global_ingress_metrics();
        assert!(std::ptr::eq(a, b));
    }
}

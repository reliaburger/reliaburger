//! Rates sample accepted cumulative counts with a monotonic clock. A first
//! observation, counter reset or leadership change is unknown, never a spike.
use serde::Serialize;
use std::time::{Duration, Instant};

/// Accepted task rates over a bounded observation interval.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TaskRates {
    /// Unique successful tasks per second, excluding retries and duplicate reports.
    pub successes_per_second: Option<f64>,
    /// Unique failed terminal tasks per second.
    pub failures_per_second: Option<f64>,
    /// Seconds between accepted count samples.
    pub interval_seconds: Option<f64>,
    /// UTC timestamp of the observation.
    pub sampled_at_epoch_ms: u64,
}
/// Baseline lives only on the current leader; it is not cluster correctness state.
pub struct RateSample {
    counts: (u64, u64),
    observed: Instant,
    rates: TaskRates,
}
impl RateSample {
    /// First sample has no rate.
    pub fn new(counts: (u64, u64), now: Instant, epoch_ms: u64) -> Self {
        Self {
            counts,
            observed: now,
            rates: TaskRates {
                sampled_at_epoch_ms: epoch_ms,
                ..Default::default()
            },
        }
    }
    /// Return a recent derivative without averaging node quantiles or rates.
    pub fn update(&mut self, counts: (u64, u64), now: Instant, epoch_ms: u64) -> TaskRates {
        let elapsed = now.saturating_duration_since(self.observed);
        if elapsed < Duration::from_secs(1) {
            return self.rates.clone();
        }
        let valid = counts.0 >= self.counts.0
            && counts.1 >= self.counts.1
            && elapsed <= Duration::from_secs(10);
        self.rates = TaskRates {
            successes_per_second: valid
                .then(|| (counts.0 - self.counts.0) as f64 / elapsed.as_secs_f64()),
            failures_per_second: valid
                .then(|| (counts.1 - self.counts.1) as f64 / elapsed.as_secs_f64()),
            interval_seconds: valid.then_some(elapsed.as_secs_f64()),
            sampled_at_epoch_ms: epoch_ms,
        };
        self.counts = counts;
        self.observed = now;
        self.rates.clone()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rates_handle_reset_duplicate_and_stale_samples() {
        let now = Instant::now();
        let mut sample = RateSample::new((0, 0), now, 1);
        assert_eq!(
            sample
                .update((1000, 10), now + Duration::from_secs(2), 2)
                .successes_per_second,
            Some(500.0)
        );
        assert_eq!(
            sample
                .update((1000, 10), now + Duration::from_secs(4), 3)
                .successes_per_second,
            Some(0.0)
        );
        assert_eq!(
            sample
                .update((1, 0), now + Duration::from_secs(6), 4)
                .successes_per_second,
            None
        );
        assert_eq!(
            sample
                .update((10001, 0), now + Duration::from_secs(60), 5)
                .successes_per_second,
            None
        );
    }
}

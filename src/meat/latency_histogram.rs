//! A small, mergeable latency histogram for task arrays.
//!
//! Each node records every task's start lag and run time; the leader adds
//! the nodes' histograms together and reads percentiles off the sum. That
//! only works if merging is exact, so the buckets are fixed: log-linear,
//! four per power of two, in microseconds. A value lands in a bucket
//! whose width is at most a quarter of its lower bound, so a reported
//! percentile is within 25% of the truth (in practice about half that).
//! 128 buckets reach past an hour; longer values share the last bucket.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Number of buckets.
pub const BUCKETS: usize = 128;

/// Counts of durations in fixed log-linear buckets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<[u64; 2]>", into = "Vec<[u64; 2]>")]
pub struct LatencyHistogram {
    counts: Vec<u64>,
}

/// Why a serialised histogram was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistogramError {
    #[error("bucket {bucket} is out of range")]
    BucketOutOfRange { bucket: u64 },
    #[error("bucket {bucket} appears twice or out of order")]
    Unordered { bucket: u64 },
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            counts: vec![0; BUCKETS],
        }
    }
}

/// The bucket a value in microseconds falls into.
fn bucket_of(micros: u64) -> usize {
    if micros < 4 {
        return micros as usize;
    }
    let exponent = 63 - micros.leading_zeros() as usize; // floor(log2), at least 2
    let sub = ((micros >> (exponent - 2)) & 3) as usize;
    (4 * (exponent - 1) + sub).min(BUCKETS - 1)
}

/// The smallest value, in microseconds, that lands in `bucket`.
fn lower_bound(bucket: usize) -> u64 {
    if bucket < 4 {
        return bucket as u64;
    }
    let exponent = bucket / 4 + 1;
    let sub = (bucket % 4) as u64;
    (4 + sub) << (exponent - 2)
}

impl LatencyHistogram {
    /// An empty histogram.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one duration.
    pub fn record(&mut self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.counts[bucket_of(micros)] += 1;
    }

    /// Number of durations counted.
    pub fn count(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// Add every count from `other`. Merging is exact: recording into two
    /// histograms and merging gives the same result as recording into one.
    pub fn merge(&mut self, other: &LatencyHistogram) {
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine = mine.saturating_add(*theirs);
        }
    }

    /// The duration below which `percentile` percent of the counted values
    /// fall, as the midpoint of the bucket holding that rank. `None` when
    /// empty; `percentile` is clamped to 0..=100.
    pub fn percentile(&self, percentile: f64) -> Option<Duration> {
        let total = self.count();
        if total == 0 {
            return None;
        }
        let wanted = percentile.clamp(0.0, 100.0) / 100.0;
        // Nearest rank: the smallest rank covering the wanted fraction.
        let rank = ((wanted * total as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (bucket, count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                let low = lower_bound(bucket);
                let high = if bucket + 1 < BUCKETS {
                    lower_bound(bucket + 1)
                } else {
                    low * 5 / 4
                };
                return Some(Duration::from_micros(low + (high - low) / 2));
            }
        }
        None
    }
}

impl TryFrom<Vec<[u64; 2]>> for LatencyHistogram {
    type Error = HistogramError;

    fn try_from(pairs: Vec<[u64; 2]>) -> Result<Self, Self::Error> {
        let mut histogram = LatencyHistogram::default();
        let mut previous: Option<u64> = None;
        for [bucket, count] in pairs {
            if bucket >= BUCKETS as u64 {
                return Err(HistogramError::BucketOutOfRange { bucket });
            }
            if previous.is_some_and(|p| p >= bucket) {
                return Err(HistogramError::Unordered { bucket });
            }
            previous = Some(bucket);
            histogram.counts[bucket as usize] = count;
        }
        Ok(histogram)
    }
}

/// Serialised sparsely: only non-empty buckets, as `[bucket, count]`.
impl From<LatencyHistogram> for Vec<[u64; 2]> {
    fn from(histogram: LatencyHistogram) -> Self {
        histogram
            .counts
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(bucket, count)| [bucket as u64, *count])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn buckets_are_contiguous_and_ordered() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(3), 3);
        assert_eq!(bucket_of(4), 4);
        assert_eq!(bucket_of(7), 7);
        assert_eq!(bucket_of(8), 8);
        assert_eq!(bucket_of(9), 8);
        assert_eq!(bucket_of(10), 9);
        for bucket in 0..BUCKETS - 1 {
            assert_eq!(bucket_of(lower_bound(bucket)), bucket, "bucket {bucket}");
            assert_eq!(
                bucket_of(lower_bound(bucket + 1) - 1),
                bucket,
                "bucket {bucket}"
            );
        }
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn the_last_bucket_reaches_past_an_hour() {
        assert!(lower_bound(BUCKETS - 1) > 3_600_000_000);
    }

    #[test]
    fn an_empty_histogram_has_no_percentiles() {
        assert_eq!(LatencyHistogram::new().percentile(50.0), None);
    }

    #[test]
    fn percentiles_follow_the_recorded_values() {
        let mut histogram = LatencyHistogram::new();
        for millis in 1..=100 {
            histogram.record(Duration::from_millis(millis));
        }
        let within = |got: Duration, want: Duration| {
            let ratio = got.as_secs_f64() / want.as_secs_f64();
            (0.8..=1.25).contains(&ratio)
        };
        assert!(within(
            histogram.percentile(50.0).unwrap(),
            Duration::from_millis(50)
        ));
        assert!(within(
            histogram.percentile(99.0).unwrap(),
            Duration::from_millis(99)
        ));
        assert!(within(
            histogram.percentile(0.0).unwrap(),
            Duration::from_millis(1)
        ));
        assert_eq!(histogram.count(), 100);
    }

    #[test]
    fn merging_equals_recording_into_one() {
        let mut one = LatencyHistogram::new();
        let mut left = LatencyHistogram::new();
        let mut right = LatencyHistogram::new();
        for micros in (0..50_000u64).step_by(37) {
            one.record(Duration::from_micros(micros));
            if micros % 2 == 0 {
                left.record(Duration::from_micros(micros));
            } else {
                right.record(Duration::from_micros(micros));
            }
        }
        left.merge(&right);
        assert_eq!(left, one);
    }

    #[test]
    fn serialises_sparsely_and_round_trips() {
        let mut histogram = LatencyHistogram::new();
        histogram.record(Duration::from_micros(2));
        histogram.record(Duration::from_micros(2));
        histogram.record(Duration::from_micros(9));
        let json = serde_json::to_string(&histogram).unwrap();
        assert_eq!(json, "[[2,2],[8,1]]");
        let back: LatencyHistogram = serde_json::from_str(&json).unwrap();
        assert_eq!(back, histogram);
        assert!(serde_json::from_str::<LatencyHistogram>("[[128,1]]").is_err());
        assert!(serde_json::from_str::<LatencyHistogram>("[[5,1],[5,1]]").is_err());
        assert!(serde_json::from_str::<LatencyHistogram>("[[6,1],[5,1]]").is_err());
    }

    proptest! {
        #[test]
        fn a_single_value_is_reported_within_a_quarter(micros in 4u64..3_000_000_000) {
            let mut histogram = LatencyHistogram::new();
            histogram.record(Duration::from_micros(micros));
            let got = histogram.percentile(50.0).unwrap().as_micros() as f64;
            let ratio = got / micros as f64;
            prop_assert!((0.8..=1.25).contains(&ratio), "{micros} reported as {got}");
        }
    }
}

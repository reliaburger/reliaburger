//! Fixed-size, node-local executor phase distributions, independent of job count.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Snapshot of one executor phase; buckets are non-cumulative upper bounds.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PhaseSnapshot {
    /// Number of measured phase completions, including failed preparations.
    pub samples: u64,
    /// Sum of elapsed microseconds, not CPU time.
    pub total_microseconds: u64,
    /// Largest observed elapsed duration.
    pub maximum_microseconds: u64,
    /// Upper bounds 1, 2, 4, …, 16384 milliseconds and a final overflow bucket.
    pub buckets: [u64; 16],
}
/// Cumulative distributions for the lifetime of one bounded executor pool.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimingSnapshot {
    /// Waiting for an executor slot and its complete resource reservation.
    pub admission: PhaseSnapshot,
    /// Cold helper ownership and connection establishment; absent on warm commands.
    pub startup: PhaseSnapshot,
    /// Sending a command through observing its terminal event and captured output.
    pub command: PhaseSnapshot,
    /// Exact-sequence cleanup receipt and independently verified cgroup emptiness.
    pub cleanup: PhaseSnapshot,
}
#[derive(Default)]
struct Histogram {
    samples: AtomicU64,
    total: AtomicU64,
    maximum: AtomicU64,
    buckets: [AtomicU64; 16],
}
impl Histogram {
    fn observe(&self, duration: Duration) {
        let micros = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        let bucket = (0..15)
            .find(|index| duration <= Duration::from_millis(1 << index))
            .unwrap_or(15);
        self.total.fetch_add(micros, Ordering::Relaxed);
        self.maximum.fetch_max(micros, Ordering::Relaxed);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.samples.fetch_add(1, Ordering::Relaxed);
    }
    fn snapshot(&self) -> PhaseSnapshot {
        PhaseSnapshot {
            samples: self.samples.load(Ordering::Relaxed),
            total_microseconds: self.total.load(Ordering::Relaxed),
            maximum_microseconds: self.maximum.load(Ordering::Relaxed),
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
        }
    }
}
#[derive(Default)]
pub(super) struct Timings {
    admission: Histogram,
    startup: Histogram,
    command: Histogram,
    cleanup: Histogram,
}
pub(super) enum Phase {
    Admission,
    Startup,
    Command,
    Cleanup,
}
pub(super) struct Timer<'a> {
    histogram: &'a Histogram,
    started: Instant,
}
impl Drop for Timer<'_> {
    fn drop(&mut self) {
        self.histogram.observe(self.started.elapsed());
    }
}
impl Timings {
    pub(super) fn start(&self, phase: Phase) -> Timer<'_> {
        Timer {
            histogram: match phase {
                Phase::Admission => &self.admission,
                Phase::Startup => &self.startup,
                Phase::Command => &self.command,
                Phase::Cleanup => &self.cleanup,
            },
            started: Instant::now(),
        }
    }
    pub(super) fn snapshot(&self) -> TimingSnapshot {
        TimingSnapshot {
            admission: self.admission.snapshot(),
            startup: self.startup.snapshot(),
            command: self.command.snapshot(),
            cleanup: self.cleanup.snapshot(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phase_histograms_remain_fixed_size_and_preserve_overflow_samples() {
        let histogram = Histogram::default();
        for duration in [
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_secs(60),
        ] {
            histogram.observe(duration);
        }
        let row = histogram.snapshot();
        assert_eq!(row.samples, 4);
        assert_eq!(row.buckets[0], 2);
        assert_eq!(row.buckets[1], 1);
        assert_eq!(row.buckets[15], 1);
        assert_eq!(row.buckets.iter().sum::<u64>(), row.samples);
        assert_eq!(row.maximum_microseconds, 60_000_000);
    }
    #[test]
    fn finishing_a_phase_records_exactly_one_sample_in_that_phase() {
        let timings = Timings::default();
        drop(timings.start(Phase::Cleanup));
        let row = timings.snapshot();
        assert_eq!(row.cleanup.samples, 1);
        assert_eq!(row.command.samples, 0);
        assert_eq!(row.startup.samples, 0);
        for phase in [Phase::Admission, Phase::Startup, Phase::Command] {
            drop(timings.start(phase));
        }
        assert_eq!(timings.snapshot().admission.samples, 1);
    }
}

//! How long each turn of the node agent's loop takes.
//!
//! The agent loop (`BunAgent::run_loop`) runs one branch at a time, so a
//! caller waits for whatever turn is in progress. Every soak failure the loop
//! caused in 0.1.0 and 0.1.1 was a long turn. This meter makes turn length a
//! number: each finished turn lands in a histogram labelled by the branch
//! that ran it, and Bun exports the histogram through Mayo as
//! `bun_agent_loop_turn_seconds{branch}`.
//!
//! Tests also read the worst turn so far, including one still running, so a
//! starvation scenario can assert "no turn took a second" even when the turn
//! that broke the rule never finishes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::mayo::collector::CollectedMetric;
use crate::mayo::types::MetricKey;

/// The metric name the histogram is exported under.
pub const TURN_METRIC: &str = "bun_agent_loop_turn_seconds";

/// A turn at least this long is logged with its branch and what it handled.
pub const SLOW_TURN_LOG_THRESHOLD: Duration = Duration::from_millis(250);

/// The longest turn the starvation harness and the soak checker accept.
/// The report worker's deadline is 2 s, so a node breaks this well before it
/// goes stale.
pub const TURN_BUDGET: Duration = Duration::from_secs(1);

/// Upper bounds of the histogram buckets, in seconds. `+Inf` is implied.
const BUCKET_BOUNDS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Which branch of the loop's `select!` a turn ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopBranch {
    /// A report worker asked for a snapshot.
    Snapshot,
    /// A spawned stop finished its drain and signals.
    StopWait,
    /// A spawned workload identity signing finished.
    IdentitySigning,
    /// A deploy worker or health probe asked for an authoritative step.
    DeployOp,
    /// An `AgentCommand` from the API, `relish` or another subsystem.
    Command,
    /// The periodic health tick, whether due or forced by the starvation floor.
    HealthTick,
}

impl LoopBranch {
    const ALL: [LoopBranch; 6] = [
        LoopBranch::Snapshot,
        LoopBranch::StopWait,
        LoopBranch::IdentitySigning,
        LoopBranch::DeployOp,
        LoopBranch::Command,
        LoopBranch::HealthTick,
    ];

    /// The `branch` label value.
    pub fn label(self) -> &'static str {
        match self {
            LoopBranch::Snapshot => "snapshot",
            LoopBranch::StopWait => "stop_wait",
            LoopBranch::IdentitySigning => "identity_signing",
            LoopBranch::DeployOp => "deploy_op",
            LoopBranch::Command => "command",
            LoopBranch::HealthTick => "health_tick",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// One turn in progress: which branch, what it handled, when it began.
#[derive(Debug, Clone, Copy)]
pub struct Turn {
    branch: LoopBranch,
    /// The command or deploy-op variant, when the branch carries one.
    detail: Option<&'static str>,
    started: Instant,
}

impl Turn {
    fn describe(&self) -> String {
        match self.detail {
            Some(detail) => format!("{} ({detail})", self.branch.label()),
            None => self.branch.label().to_string(),
        }
    }
}

/// A finished (or still running) turn, as the harness sees it.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct TurnRecord {
    pub branch: LoopBranch,
    pub detail: Option<&'static str>,
    pub took: Duration,
}

#[cfg(test)]
impl std::fmt::Display for TurnRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} in {}", self.took, self.branch.label())?;
        if let Some(detail) = self.detail {
            write!(f, " ({detail})")?;
        }
        Ok(())
    }
}

/// Per-branch histogram: a count per bucket (not cumulative), plus the
/// running sum and count Prometheus histograms carry.
struct BranchHistogram {
    buckets: [AtomicU64; BUCKET_BOUNDS.len()],
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl BranchHistogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; BUCKET_BOUNDS.len()],
            sum_nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, took: Duration) {
        let seconds = took.as_secs_f64();
        if let Some(bucket) = BUCKET_BOUNDS.iter().position(|bound| seconds <= *bound) {
            self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        }
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Times the agent loop's turns. The loop owns one through an `Arc`; the
/// metrics collector holds another and reads it every interval.
pub struct LoopTurnMeter {
    branches: [BranchHistogram; LoopBranch::ALL.len()],
    /// Test-only: the longest finished turn and the turn in progress. A
    /// `std` mutex is fine here: it guards a copy and is never held across
    /// an `await`.
    #[cfg(test)]
    worst: std::sync::Mutex<Option<TurnRecord>>,
    #[cfg(test)]
    in_flight: std::sync::Mutex<Option<Turn>>,
}

impl Default for LoopTurnMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopTurnMeter {
    /// An empty meter.
    pub fn new() -> Self {
        Self {
            branches: [const { BranchHistogram::new() }; LoopBranch::ALL.len()],
            #[cfg(test)]
            worst: std::sync::Mutex::new(None),
            #[cfg(test)]
            in_flight: std::sync::Mutex::new(None),
        }
    }

    /// Start timing a turn of `branch`. `detail` names the command or deploy
    /// op it handles, for the slow-turn log.
    pub fn begin(&self, branch: LoopBranch, detail: Option<&'static str>) -> Turn {
        let turn = Turn {
            branch,
            detail,
            started: Instant::now(),
        };
        #[cfg(test)]
        if let Ok(mut in_flight) = self.in_flight.lock() {
            *in_flight = Some(turn);
        }
        turn
    }

    /// Record a finished turn, and log it when it ran long.
    pub fn finish(&self, turn: Turn) {
        let took = turn.started.elapsed();
        self.branches[turn.branch.index()].observe(took);
        if took >= SLOW_TURN_LOG_THRESHOLD {
            eprintln!(
                "bun: agent loop turn took {} ms in {}",
                took.as_millis(),
                turn.describe()
            );
        }
        #[cfg(test)]
        {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                *in_flight = None;
            }
            if let Ok(mut worst) = self.worst.lock()
                && worst.is_none_or(|record| record.took < took)
            {
                *worst = Some(TurnRecord {
                    branch: turn.branch,
                    detail: turn.detail,
                    took,
                });
            }
        }
    }

    /// The histogram as Mayo samples: cumulative `_bucket{branch,le}`, plus
    /// `_sum{branch}` and `_count{branch}`, for every branch that has run.
    pub fn samples(&self) -> Vec<CollectedMetric> {
        let mut samples = Vec::new();
        for branch in LoopBranch::ALL {
            let histogram = &self.branches[branch.index()];
            let count = histogram.count.load(Ordering::Relaxed);
            if count == 0 {
                continue;
            }
            let labels = |extra: Option<String>| {
                let mut labels =
                    BTreeMap::from([("branch".to_string(), branch.label().to_string())]);
                if let Some(bound) = extra {
                    labels.insert("le".to_string(), bound);
                }
                labels
            };
            let mut cumulative = 0;
            for (bound, bucket) in BUCKET_BOUNDS.iter().zip(&histogram.buckets) {
                cumulative += bucket.load(Ordering::Relaxed);
                samples.push(CollectedMetric {
                    key: MetricKey::with_labels(
                        format!("{TURN_METRIC}_bucket"),
                        labels(Some(bound.to_string())),
                    ),
                    value: cumulative as f64,
                });
            }
            samples.push(CollectedMetric {
                key: MetricKey::with_labels(
                    format!("{TURN_METRIC}_bucket"),
                    labels(Some("+Inf".to_string())),
                ),
                value: count as f64,
            });
            samples.push(CollectedMetric {
                key: MetricKey::with_labels(format!("{TURN_METRIC}_sum"), labels(None)),
                value: histogram.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9,
            });
            samples.push(CollectedMetric {
                key: MetricKey::with_labels(format!("{TURN_METRIC}_count"), labels(None)),
                value: count as f64,
            });
        }
        samples
    }

    /// The longest turn so far, counting one still in progress: a turn stuck
    /// on a hung await never finishes, and it is exactly the one to report.
    #[cfg(test)]
    pub fn worst_turn(&self) -> Option<TurnRecord> {
        let finished = self.worst.lock().ok().and_then(|worst| *worst);
        let running = self
            .in_flight
            .lock()
            .ok()
            .and_then(|turn| *turn)
            .map(|turn| TurnRecord {
                branch: turn.branch,
                detail: turn.detail,
                took: turn.started.elapsed(),
            });
        match (finished, running) {
            (Some(finished), Some(running)) if running.took > finished.took => Some(running),
            (Some(finished), _) => Some(finished),
            (None, running) => running,
        }
    }

    /// Forget the worst turn, so a scenario measures only what it provokes.
    #[cfg(test)]
    pub fn reset_worst_turn(&self) {
        if let Ok(mut worst) = self.worst.lock() {
            *worst = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(samples: &[CollectedMetric], name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        samples
            .iter()
            .find(|sample| {
                sample.key.name.as_str() == name
                    && sample.key.labels.len() == labels.len()
                    && labels.iter().all(|(key, value)| {
                        sample.key.labels.get(*key).map(String::as_str) == Some(value)
                    })
            })
            .map(|sample| sample.value)
    }

    fn record(meter: &LoopTurnMeter, branch: LoopBranch, took: Duration) {
        let mut turn = meter.begin(branch, None);
        turn.started -= took;
        meter.finish(turn);
    }

    #[test]
    fn a_fresh_meter_exports_nothing() {
        assert!(LoopTurnMeter::new().samples().is_empty());
    }

    #[test]
    fn turns_land_in_cumulative_buckets_per_branch() {
        let meter = LoopTurnMeter::new();
        record(&meter, LoopBranch::Command, Duration::from_millis(3));
        record(&meter, LoopBranch::Command, Duration::from_millis(300));
        record(&meter, LoopBranch::HealthTick, Duration::from_secs(20));
        let samples = meter.samples();

        let bucket = |branch, le| {
            value(
                &samples,
                "bun_agent_loop_turn_seconds_bucket",
                &[("branch", branch), ("le", le)],
            )
        };
        assert_eq!(bucket("command", "0.001"), Some(0.0));
        assert_eq!(bucket("command", "0.005"), Some(1.0));
        assert_eq!(bucket("command", "0.25"), Some(1.0));
        assert_eq!(bucket("command", "0.5"), Some(2.0));
        assert_eq!(bucket("command", "+Inf"), Some(2.0));
        // Past the last bound only `+Inf` counts it.
        assert_eq!(bucket("health_tick", "10"), Some(0.0));
        assert_eq!(bucket("health_tick", "+Inf"), Some(1.0));
        assert_eq!(
            value(
                &samples,
                "bun_agent_loop_turn_seconds_count",
                &[("branch", "command")]
            ),
            Some(2.0)
        );
        let sum = value(
            &samples,
            "bun_agent_loop_turn_seconds_sum",
            &[("branch", "command")],
        )
        .unwrap();
        assert!((0.3..0.31).contains(&sum), "sum {sum}");
        // Branches that never ran export nothing.
        assert_eq!(bucket("snapshot", "+Inf"), None);
    }

    #[test]
    fn the_worst_turn_counts_one_still_in_progress() {
        let meter = LoopTurnMeter::new();
        record(&meter, LoopBranch::Command, Duration::from_millis(40));
        record(&meter, LoopBranch::DeployOp, Duration::from_millis(20));
        let worst = meter.worst_turn().unwrap();
        assert_eq!(worst.branch, LoopBranch::Command);

        let mut stuck = meter.begin(LoopBranch::HealthTick, None);
        stuck.started -= Duration::from_secs(3);
        *meter.in_flight.lock().unwrap() = Some(stuck);
        let worst = meter.worst_turn().unwrap();
        assert_eq!(worst.branch, LoopBranch::HealthTick);
        assert!(worst.took >= Duration::from_secs(3));

        meter.finish(stuck);
        meter.reset_worst_turn();
        assert!(meter.worst_turn().is_none());
    }
}

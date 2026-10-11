//! Job metrics a node exports for the task attempts it runs.
//!
//! Every node counts the attempts it ran itself, so the numbers don't live in
//! the leader's memory and don't reset when leadership moves. Series carry
//! four labels only: `namespace`, `definition` (the job or array name),
//! `profile` (a manifest's resource profile, `default` otherwise) and `node`.
//! A task index or run id never becomes a label: a million-task run is the
//! same handful of series as a one-task run, and the number of label sets a
//! node keeps is capped at [`MAX_JOB_SERIES`].
//!
//! The node's executor calls [`JobTelemetry::observe`] each time the leader
//! syncs it, with each run's live counters. The collection loop turns the
//! totals into Mayo samples and measures CPU and memory for the running
//! attempts, either from their cgroups (Linux) or their processes.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::collector::{CollectedMetric, SystemCollector};
use super::types::MetricKey;

/// Most label sets one node keeps. Past it, the longest-idle set with no
/// running attempts is dropped (its counters start again if it comes back).
pub const MAX_JOB_SERIES: usize = 1024;

/// Upper bounds of the task duration buckets, in milliseconds. The last
/// bucket, `+Inf`, catches the rest. Matches the run state's buckets.
pub const DURATION_BUCKETS_MS: [u64; 15] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384,
];

/// The only labels a job series carries, apart from `node`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobLabels {
    /// The run's namespace.
    pub namespace: String,
    /// The job definition or array name.
    pub definition: String,
    /// The resource profile within a manifest; `default` for a plain run.
    pub profile: String,
}

impl JobLabels {
    fn metric_labels(&self, node: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("namespace".to_string(), self.namespace.clone()),
            ("definition".to_string(), self.definition.clone()),
            ("profile".to_string(), self.profile.clone()),
            ("node".to_string(), node.to_string()),
        ])
    }
}

/// One run's counters on this node, since the node started running it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttemptCounts {
    /// Attempts started.
    pub started: u64,
    /// Tasks that succeeded.
    pub succeeded: u64,
    /// Tasks that failed for good.
    pub failed: u64,
    /// Retries.
    pub retried: u64,
}

impl AttemptCounts {
    /// What happened between `earlier` and `self`. A counter that went
    /// backwards means the run was opened afresh (the node restarted), so
    /// everything it counts now is new.
    fn since(self, earlier: Self) -> Self {
        let delta = |now: u64, then: u64| if now >= then { now - then } else { now };
        Self {
            started: delta(self.started, earlier.started),
            succeeded: delta(self.succeeded, earlier.succeeded),
            failed: delta(self.failed, earlier.failed),
            retried: delta(self.retried, earlier.retried),
        }
    }
}

/// Where to measure a running attempt's CPU and memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageTarget {
    /// A cgroup v2 directory (an executor or a fresh task's group).
    Cgroup(PathBuf),
    /// A host process, where there is no cgroup to read.
    Pid(u32),
}

/// Latest CPU and memory for one label set's running attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct JobUsage {
    /// Attempts running now.
    pub running: u64,
    /// CPU in percent of one core, summed over the attempts; `None` before
    /// the first measurement.
    pub cpu_percent: Option<f64>,
    /// Memory in bytes, summed; `None` before the first measurement.
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct Totals {
    counts: AttemptCounts,
    durations: [u64; 16],
    touched: u64,
}

#[derive(Debug, Clone)]
struct ActiveRun {
    labels: JobLabels,
    last: AttemptCounts,
    running: u64,
    targets: Vec<UsageTarget>,
}

#[derive(Debug, Default)]
struct Inner {
    totals: BTreeMap<JobLabels, Totals>,
    runs: BTreeMap<u64, ActiveRun>,
    usage: BTreeMap<JobLabels, JobUsage>,
    clock: u64,
}

impl Inner {
    fn totals(&mut self, labels: &JobLabels) -> &mut Totals {
        self.clock += 1;
        if !self.totals.contains_key(labels) && self.totals.len() >= MAX_JOB_SERIES {
            let running: std::collections::BTreeSet<&JobLabels> =
                self.runs.values().map(|run| &run.labels).collect();
            let idle = self
                .totals
                .iter()
                .filter(|(labels, _)| !running.contains(labels))
                .min_by_key(|(_, totals)| totals.touched)
                .or_else(|| self.totals.iter().min_by_key(|(_, totals)| totals.touched))
                .map(|(labels, _)| labels.clone());
            if let Some(idle) = idle {
                self.totals.remove(&idle);
                self.usage.remove(&idle);
            }
        }
        let clock = self.clock;
        let totals = self.totals.entry(labels.clone()).or_default();
        totals.touched = clock;
        totals
    }
}

/// A node's job counters, running gauges and usage, shared by its executor,
/// its collection loop and `relish top`.
#[derive(Debug, Default)]
pub struct JobTelemetry {
    // A plain mutex: every critical section is a few map operations with no
    // `.await` inside, called from both async tasks and sync code.
    inner: std::sync::Mutex<Inner>,
}

impl JobTelemetry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a run's latest counters on this node, how many of its
    /// attempts are running, and where to measure them.
    pub fn observe(
        &self,
        batch_id: u64,
        labels: &JobLabels,
        counts: AttemptCounts,
        running: u64,
        targets: Vec<UsageTarget>,
    ) {
        let mut inner = self.lock();
        let last = inner
            .runs
            .get(&batch_id)
            .filter(|run| run.labels == *labels)
            .map_or_else(AttemptCounts::default, |run| run.last);
        let delta = counts.since(last);
        let totals = inner.totals(labels);
        totals.counts.started += delta.started;
        totals.counts.succeeded += delta.succeeded;
        totals.counts.failed += delta.failed;
        totals.counts.retried += delta.retried;
        inner.runs.insert(
            batch_id,
            ActiveRun {
                labels: labels.clone(),
                last: counts,
                running,
                targets,
            },
        );
    }

    /// Add one finished chunk's final-attempt durations.
    pub fn record_durations(&self, labels: &JobLabels, durations: &[u64; 16]) {
        let mut inner = self.lock();
        let totals = inner.totals(labels);
        for (total, count) in totals.durations.iter_mut().zip(durations) {
            *total += count;
        }
    }

    /// Stop tracking runs this node no longer runs. Their totals stay.
    pub fn retain_runs(&self, mut keep: impl FnMut(u64) -> bool) {
        self.lock().runs.retain(|id, _| keep(*id));
    }

    /// Every label set with running attempts, and where to measure them.
    pub fn usage_targets(&self) -> BTreeMap<JobLabels, (u64, Vec<UsageTarget>)> {
        let inner = self.lock();
        let mut targets: BTreeMap<JobLabels, (u64, Vec<UsageTarget>)> = BTreeMap::new();
        for run in inner.runs.values() {
            if run.running == 0 && run.targets.is_empty() {
                continue;
            }
            let entry = targets.entry(run.labels.clone()).or_default();
            entry.0 += run.running;
            entry.1.extend(run.targets.iter().cloned());
        }
        targets
    }

    /// Replace the latest usage, as measured by the collection loop.
    pub fn set_usage(&self, usage: BTreeMap<JobLabels, JobUsage>) {
        self.lock().usage = usage;
    }

    /// The latest usage of every label set with running attempts.
    pub fn usage(&self) -> BTreeMap<JobLabels, JobUsage> {
        self.lock().usage.clone()
    }

    /// Mayo samples for every label set this node keeps: counters, the
    /// cumulative duration histogram, and the running gauge.
    pub fn samples(&self, node: &str) -> Vec<CollectedMetric> {
        let inner = self.lock();
        let mut running: BTreeMap<&JobLabels, u64> = BTreeMap::new();
        for run in inner.runs.values() {
            *running.entry(&run.labels).or_default() += run.running;
        }
        let mut samples = Vec::new();
        for (labels, totals) in &inner.totals {
            let base = labels.metric_labels(node);
            let metric = |name: &str, value: u64| CollectedMetric {
                key: MetricKey::with_labels(name, base.clone()),
                value: value as f64,
            };
            samples.push(metric("job_attempts_started_total", totals.counts.started));
            samples.push(metric("job_tasks_succeeded_total", totals.counts.succeeded));
            samples.push(metric("job_tasks_failed_total", totals.counts.failed));
            samples.push(metric("job_task_retries_total", totals.counts.retried));
            samples.push(metric(
                "job_tasks_running",
                running.get(labels).copied().unwrap_or(0),
            ));
            let mut cumulative = 0;
            for (index, count) in totals.durations.iter().enumerate() {
                cumulative += count;
                let bound = DURATION_BUCKETS_MS
                    .get(index)
                    .map_or_else(|| "+Inf".to_string(), u64::to_string);
                let mut bucket = base.clone();
                bucket.insert("le".to_string(), bound);
                samples.push(CollectedMetric {
                    key: MetricKey::with_labels("job_task_duration_ms_bucket", bucket),
                    value: cumulative as f64,
                });
            }
            samples.push(metric("job_task_duration_ms_count", cumulative));
        }
        samples
    }

    /// Measure every running label set's CPU and memory, remember it for
    /// `relish top`, and return it as `job_cpu_percent` and
    /// `job_memory_bytes` samples.
    pub fn sample_usage(
        &self,
        collector: &SystemCollector,
        cgroups: &mut CgroupCpu,
        node: &str,
    ) -> Vec<CollectedMetric> {
        let targets = self.usage_targets();
        let mut usage = BTreeMap::new();
        let mut samples = Vec::new();
        for (labels, (running, targets)) in targets {
            let mut measured = JobUsage {
                running,
                ..JobUsage::default()
            };
            for target in &targets {
                let reading = match target {
                    UsageTarget::Pid(pid) => collector.process_usage(*pid),
                    UsageTarget::Cgroup(path) => cgroups.read(path),
                };
                let Some((cpu, memory)) = reading else {
                    continue;
                };
                if let Some(cpu) = cpu {
                    *measured.cpu_percent.get_or_insert(0.0) += cpu;
                }
                *measured.memory_bytes.get_or_insert(0) += memory;
            }
            let base = labels.metric_labels(node);
            if let Some(cpu) = measured.cpu_percent {
                samples.push(CollectedMetric {
                    key: MetricKey::with_labels("job_cpu_percent", base.clone()),
                    value: cpu,
                });
            }
            if let Some(memory) = measured.memory_bytes {
                samples.push(CollectedMetric {
                    key: MetricKey::with_labels("job_memory_bytes", base),
                    value: memory as f64,
                });
            }
            usage.insert(labels, measured);
        }
        cgroups.forget_unseen();
        self.set_usage(usage);
        samples
    }
}

/// CPU percent from a cgroup's cumulative `usage_usec`, which needs the
/// previous reading of the same group.
#[derive(Debug, Default)]
pub struct CgroupCpu {
    previous: HashMap<PathBuf, (Instant, u64)>,
    seen: std::collections::HashSet<PathBuf>,
}

impl CgroupCpu {
    /// CPU percent (`None` on the first reading) and memory in bytes, or
    /// `None` when the group is gone.
    // cgroupfs is an in-memory kernel interface, like /proc: reading it
    // never waits on a disk, so this is fine on the collection task.
    fn read(&mut self, path: &Path) -> Option<(Option<f64>, u64)> {
        let memory = std::fs::read_to_string(path.join("memory.current"))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;
        let usage = std::fs::read_to_string(path.join("cpu.stat"))
            .ok()
            .and_then(|stat| parse_usage_usec(&stat));
        let now = Instant::now();
        self.seen.insert(path.to_path_buf());
        let cpu = usage.and_then(|usage| {
            let previous = self.previous.insert(path.to_path_buf(), (now, usage));
            let (then, before) = previous?;
            let elapsed = now.duration_since(then).as_micros();
            (elapsed > 0 && usage >= before)
                .then(|| (usage - before) as f64 * 100.0 / elapsed as f64)
        });
        Some((cpu, memory))
    }

    fn forget_unseen(&mut self) {
        let seen = std::mem::take(&mut self.seen);
        self.previous.retain(|path, _| seen.contains(path));
    }
}

/// The `usage_usec` line of a cgroup v2 `cpu.stat`.
fn parse_usage_usec(stat: &str) -> Option<u64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(definition: &str) -> JobLabels {
        JobLabels {
            namespace: "ops".into(),
            definition: definition.into(),
            profile: "default".into(),
        }
    }

    fn counts(started: u64, succeeded: u64) -> AttemptCounts {
        AttemptCounts {
            started,
            succeeded,
            failed: 0,
            retried: 0,
        }
    }

    fn value(samples: &[CollectedMetric], name: &str, definition: &str) -> f64 {
        samples
            .iter()
            .find(|sample| {
                sample.key.name.as_str() == name
                    && sample.key.labels.get("definition").map(String::as_str) == Some(definition)
            })
            .map(|sample| sample.value)
            .unwrap()
    }

    #[test]
    fn job_metrics_never_carry_a_task_index_label() {
        let telemetry = JobTelemetry::default();
        // A million-task run on one node: many syncs, many chunks.
        for sync in 1..=100u64 {
            telemetry.observe(
                7,
                &labels("render"),
                counts(sync * 10_000, sync * 10_000),
                256,
                Vec::new(),
            );
            telemetry.record_durations(&labels("render"), &[1; 16]);
        }
        let samples = telemetry.samples("node-a");
        let allowed = ["namespace", "definition", "profile", "node", "le"];
        for sample in &samples {
            for key in sample.key.labels.keys() {
                assert!(
                    allowed.contains(&key.as_str()),
                    "label {key} on {:?}",
                    sample.key
                );
            }
        }
        // Counters, running, 16 buckets and a count: one fixed set per labels.
        assert_eq!(samples.len(), 5 + 16 + 1);
        assert_eq!(
            value(&samples, "job_attempts_started_total", "render"),
            1_000_000.0
        );
        assert_eq!(value(&samples, "job_tasks_running", "render"), 256.0);

        // However many definitions come and go, the label sets stay bounded.
        for index in 0..(MAX_JOB_SERIES as u64 * 3) {
            telemetry.record_durations(&labels(&format!("one-off-{index}")), &[0; 16]);
        }
        let label_sets: std::collections::BTreeSet<_> = telemetry
            .samples("node-a")
            .into_iter()
            .map(|sample| sample.key.labels.get("definition").cloned())
            .collect();
        assert!(label_sets.len() <= MAX_JOB_SERIES, "{}", label_sets.len());
        assert!(
            label_sets.contains(&Some("render".to_string())),
            "a running label set was evicted"
        );
    }

    #[test]
    fn job_counters_survive_a_leader_change() {
        let telemetry = JobTelemetry::default();
        telemetry.observe(7, &labels("render"), counts(10, 8), 2, Vec::new());
        // A new leader takes over and keeps syncing the same run: the node's
        // counters keep counting from where they were, not from zero.
        telemetry.observe(7, &labels("render"), counts(15, 13), 2, Vec::new());
        assert_eq!(
            value(
                &telemetry.samples("a"),
                "job_attempts_started_total",
                "render"
            ),
            15.0
        );
        // Even if this node restarts and reopens the run, its exported
        // counter only goes up.
        telemetry.observe(7, &labels("render"), counts(3, 3), 1, Vec::new());
        let samples = telemetry.samples("a");
        assert_eq!(
            value(&samples, "job_attempts_started_total", "render"),
            18.0
        );
        assert_eq!(value(&samples, "job_tasks_succeeded_total", "render"), 16.0);
        // The run finishing on this node leaves its totals in place.
        telemetry.retain_runs(|_| false);
        let samples = telemetry.samples("a");
        assert_eq!(
            value(&samples, "job_attempts_started_total", "render"),
            18.0
        );
        assert_eq!(value(&samples, "job_tasks_running", "render"), 0.0);
    }

    #[test]
    fn duration_buckets_are_cumulative() {
        let telemetry = JobTelemetry::default();
        let mut durations = [0; 16];
        durations[0] = 2;
        durations[15] = 1;
        telemetry.record_durations(&labels("render"), &durations);
        let samples = telemetry.samples("a");
        let bucket = |le: &str| {
            samples
                .iter()
                .find(|sample| sample.key.labels.get("le").map(String::as_str) == Some(le))
                .map(|sample| sample.value)
        };
        assert_eq!(bucket("1"), Some(2.0));
        assert_eq!(bucket("16384"), Some(2.0));
        assert_eq!(bucket("+Inf"), Some(3.0));
    }

    #[test]
    fn cpu_stat_usage_is_parsed() {
        assert_eq!(
            parse_usage_usec("usage_usec 1500\nuser_usec 1000\nsystem_usec 500\n"),
            Some(1500)
        );
        assert_eq!(parse_usage_usec("user_usec 1\n"), None);
    }

    #[test]
    fn a_cgroup_needs_two_readings_for_cpu() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("memory.current"), "4096\n").unwrap();
        std::fs::write(directory.path().join("cpu.stat"), "usage_usec 1000\n").unwrap();
        let mut cgroups = CgroupCpu::default();
        assert_eq!(cgroups.read(directory.path()), Some((None, 4096)));
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(directory.path().join("cpu.stat"), "usage_usec 3000\n").unwrap();
        let (cpu, memory) = cgroups.read(directory.path()).unwrap();
        assert_eq!(memory, 4096);
        assert!(cpu.is_some_and(|cpu| cpu > 0.0), "{cpu:?}");
        assert_eq!(cgroups.read(&directory.path().join("gone")), None);
    }

    #[test]
    fn a_running_process_is_measured_into_usage() {
        let telemetry = JobTelemetry::default();
        let mut collector = SystemCollector::new();
        let mut cgroups = CgroupCpu::default();
        let pid = std::process::id();
        telemetry.observe(
            7,
            &labels("render"),
            counts(1, 0),
            1,
            vec![UsageTarget::Pid(pid)],
        );
        collector.refresh();
        let samples = telemetry.sample_usage(&collector, &mut cgroups, "a");
        assert!(
            samples
                .iter()
                .any(|sample| sample.key.name.as_str() == "job_memory_bytes")
        );
        let usage = telemetry.usage();
        let render = usage[&labels("render")];
        assert_eq!(render.running, 1);
        assert!(render.memory_bytes.is_some_and(|bytes| bytes > 0));
    }
}

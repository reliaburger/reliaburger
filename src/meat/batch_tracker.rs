//! Batch completion tracking.
//!
//! Tracks the status of submitted batch jobs. The batch submission
//! returns immediately with a `BatchId`; completion is tracked
//! asynchronously as nodes report job completion.
//!
//! Since 12b.2 (JOB4) the tracked state lives in two places depending
//! on the deployment shape. Clustered, [`BatchDurableState`] is a field
//! of the Raft `DesiredState`: ids come from a replicated counter (so a
//! restarted leader never reuses one) and job transitions are Raft
//! entries (so a new leader picks up in-flight batches exactly where
//! the old one left them). Standalone, the in-memory [`BatchTracker`]
//! wraps the same durable-state type, so both paths share one
//! transition table and one summary calculation.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::batch::BatchId;
use super::types::NodeId;

/// How long a terminal (fully completed/failed) batch is kept before
/// registration-time pruning removes it.
pub const TERMINAL_RETENTION_SECS: u64 = 3600;

/// Ceiling on retained terminal batches, newest kept (the deploy
/// history cap-at-50 precedent).
pub const MAX_TERMINAL_BATCHES: usize = 50;

/// Status of a single job within a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    /// Scheduled but not yet started.
    Pending,
    /// Currently running on a node.
    Running,
    /// Completed successfully.
    Completed,
    /// Failed (exceeded retry limit or fatal error).
    Failed,
    /// The scheduler found no node with capacity for it (terminal).
    Unschedulable,
}

impl JobStatus {
    /// Whether the job can never change state again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Unschedulable
        )
    }
}

/// Outcome of a valid job report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOutcome {
    /// The transition was applied.
    Applied,
    /// The job was already in the reported state; nothing changed
    /// (idempotent duplicate — retried callbacks land here).
    Duplicate,
}

/// Why a job report was rejected.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ReportError {
    #[error("batch {batch_id} is not tracked")]
    UnknownBatch { batch_id: u64 },
    #[error("job {job:?} is not part of this batch")]
    UnknownJob { job: String },
    #[error("illegal job transition from {from:?} to {to:?}")]
    IllegalTransition { from: JobStatus, to: JobStatus },
    #[error("status {status:?} cannot be reported by a node")]
    NotReportable { status: JobStatus },
}

/// Tracked state for a single job in a batch. Serialised into the Raft
/// state machine, so every field is data, not process handles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchJobRecord {
    /// Job name (unique within the batch).
    pub name: String,
    /// Namespace the job deploys into — the single authoritative value
    /// resolved at submit time (JOB3).
    pub namespace: String,
    /// Node the job was assigned to; `None` for unschedulable jobs.
    pub node: Option<NodeId>,
    /// Current status.
    pub status: JobStatus,
}

/// One tracked batch: its jobs and when it was submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRecord {
    /// Per-job records, in allocation order.
    pub jobs: Vec<BatchJobRecord>,
    /// Submission time as seconds since the Unix epoch. Wall-clock (not
    /// `Instant`) because the record crosses the Raft wire and must
    /// survive a restart.
    pub submitted_at_epoch_secs: u64,
}

impl BatchRecord {
    /// Whether every job has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.jobs.iter().all(|j| j.status.is_terminal())
    }

    /// Apply a reported transition to one job, validating it.
    ///
    /// Only `Running`, `Completed` and `Failed` are reportable; a
    /// duplicate terminal report is an idempotent `Duplicate`, and a
    /// conflicting one (`Completed` → `Failed`) is rejected.
    pub fn report(
        &mut self,
        job_name: &str,
        status: JobStatus,
    ) -> Result<ReportOutcome, ReportError> {
        if !matches!(
            status,
            JobStatus::Running | JobStatus::Completed | JobStatus::Failed
        ) {
            return Err(ReportError::NotReportable { status });
        }
        let job = self
            .jobs
            .iter_mut()
            .find(|j| j.name == job_name)
            .ok_or_else(|| ReportError::UnknownJob {
                job: job_name.to_string(),
            })?;
        if job.status == status {
            return Ok(ReportOutcome::Duplicate);
        }
        let legal = matches!(
            (job.status, status),
            (JobStatus::Pending, _)
                | (JobStatus::Running, JobStatus::Completed | JobStatus::Failed)
        );
        if !legal {
            return Err(ReportError::IllegalTransition {
                from: job.status,
                to: status,
            });
        }
        job.status = status;
        Ok(ReportOutcome::Applied)
    }

    /// Compute the progress summary, with `now_epoch_secs` supplying
    /// the clock (callers pass wall time; tests pass fixed values).
    pub fn summary(&self, batch_id: u64, now_epoch_secs: u64) -> BatchSummary {
        let total = self.jobs.len();
        let count = |s: JobStatus| self.jobs.iter().filter(|j| j.status == s).count();
        let completed = count(JobStatus::Completed);
        let failed = count(JobStatus::Failed);
        let unschedulable = count(JobStatus::Unschedulable);
        let pending = total - completed - failed - unschedulable;
        BatchSummary {
            batch_id,
            total,
            pending,
            completed,
            failed,
            unschedulable,
            done: self.is_terminal(),
            elapsed_secs: now_epoch_secs.saturating_sub(self.submitted_at_epoch_secs),
        }
    }
}

/// Summary of a batch's progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSummary {
    /// Batch identifier.
    pub batch_id: u64,
    /// Total number of jobs in the batch.
    pub total: usize,
    /// Jobs still pending or running.
    pub pending: usize,
    /// Jobs completed successfully.
    pub completed: usize,
    /// Jobs that failed.
    pub failed: usize,
    /// Jobs the scheduler could not place (JOB3: these used to be
    /// silently omitted from the batch).
    #[serde(default)]
    pub unschedulable: usize,
    /// Whether all jobs have finished (completed, failed or unschedulable).
    pub done: bool,
    /// Seconds since batch submission.
    pub elapsed_secs: u64,
}

/// The durable half of batch tracking: a monotonic id counter plus the
/// tracked batches. Lives inside the Raft `DesiredState` when a
/// council exists (`#[serde(default)]` there, so pre-12b.2 snapshots
/// load cleanly), and inside the in-memory [`BatchTracker`] standalone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchDurableState {
    /// Next batch id to allocate. Monotonic; never reset or reused.
    pub next_batch_id: u64,
    /// Tracked batches as `(id, record)` pairs, oldest first.
    pub batches: Vec<(u64, BatchRecord)>,
}

impl Default for BatchDurableState {
    fn default() -> Self {
        Self {
            next_batch_id: 1,
            batches: Vec::new(),
        }
    }
}

impl BatchDurableState {
    /// Register a batch: allocate the next id, prune stale terminal
    /// batches (using the new record's clock, which keeps the pruning
    /// deterministic across Raft replicas), and store the record.
    pub fn register(&mut self, record: BatchRecord) -> u64 {
        // Guard against a zero counter from a hand-built default.
        let id = self.next_batch_id.max(1);
        self.next_batch_id = id + 1;
        self.prune_terminal(record.submitted_at_epoch_secs);
        self.batches.push((id, record));
        id
    }

    /// Drop terminal batches older than the retention window, and cap
    /// how many terminal batches are kept (newest win).
    fn prune_terminal(&mut self, now_epoch_secs: u64) {
        self.batches.retain(|(_, record)| {
            !(record.is_terminal()
                && now_epoch_secs.saturating_sub(record.submitted_at_epoch_secs)
                    > TERMINAL_RETENTION_SECS)
        });
        let terminal = self.batches.iter().filter(|(_, r)| r.is_terminal()).count();
        if terminal > MAX_TERMINAL_BATCHES {
            let mut to_drop = terminal - MAX_TERMINAL_BATCHES;
            self.batches.retain(|(_, record)| {
                if to_drop > 0 && record.is_terminal() {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Look up a batch by id.
    pub fn get(&self, batch_id: u64) -> Option<&BatchRecord> {
        self.batches
            .iter()
            .find(|(id, _)| *id == batch_id)
            .map(|(_, record)| record)
    }

    /// Apply a job report to a tracked batch, validating the transition.
    pub fn report(
        &mut self,
        batch_id: u64,
        job_name: &str,
        status: JobStatus,
    ) -> Result<ReportOutcome, ReportError> {
        let record = self
            .batches
            .iter_mut()
            .find(|(id, _)| *id == batch_id)
            .map(|(_, record)| record)
            .ok_or(ReportError::UnknownBatch { batch_id })?;
        record.report(job_name, status)
    }
}

/// Seconds since the Unix epoch, saturating at zero on a pre-1970 clock.
pub fn epoch_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// In-memory tracker for standalone (council-less) nodes. Same records
/// and transition rules as the Raft-durable path, without the
/// durability — a standalone restart loses in-flight batches, which is
/// exactly what happens to their jobs too.
pub struct BatchTracker {
    state: BatchDurableState,
}

impl BatchTracker {
    /// Create a new tracker.
    pub fn new() -> Self {
        Self {
            state: BatchDurableState::default(),
        }
    }

    /// Register a new batch record. Returns the assigned BatchId.
    pub fn register(&mut self, record: BatchRecord) -> BatchId {
        BatchId(self.state.register(record))
    }

    /// Apply a job report, validating the transition.
    pub fn report(
        &mut self,
        batch_id: u64,
        job_name: &str,
        status: JobStatus,
    ) -> Result<ReportOutcome, ReportError> {
        self.state.report(batch_id, job_name, status)
    }

    /// Look up a batch record by id.
    pub fn get(&self, batch_id: u64) -> Option<BatchRecord> {
        self.state.get(batch_id).cloned()
    }

    /// Get a summary of a batch's progress.
    pub fn summary(&self, batch_id: u64) -> Option<BatchSummary> {
        self.state
            .get(batch_id)
            .map(|record| record.summary(batch_id, epoch_now_secs()))
    }
}

impl Default for BatchTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_of(jobs: &[(&str, Option<&str>, JobStatus)]) -> BatchRecord {
        BatchRecord {
            jobs: jobs
                .iter()
                .map(|(name, node, status)| BatchJobRecord {
                    name: name.to_string(),
                    namespace: "default".to_string(),
                    node: node.map(NodeId::new),
                    status: *status,
                })
                .collect(),
            submitted_at_epoch_secs: 1_000_000,
        }
    }

    fn pending(names: &[&str]) -> BatchRecord {
        let jobs: Vec<(&str, Option<&str>, JobStatus)> = names
            .iter()
            .map(|n| (*n, Some("n1"), JobStatus::Pending))
            .collect();
        record_of(&jobs)
    }

    #[test]
    fn register_assigns_incrementing_ids() {
        let mut tracker = BatchTracker::new();
        let id1 = tracker.register(pending(&["j1"]));
        let id2 = tracker.register(pending(&["j2"]));
        assert_eq!(id1, BatchId(1));
        assert_eq!(id2, BatchId(2));
    }

    #[test]
    fn summary_starts_all_pending() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1", "j2", "j3"]));
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.pending, 3);
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.unschedulable, 0);
        assert!(!summary.done);
    }

    #[test]
    fn completed_reports_update_summary() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1", "j2"]));

        tracker.report(id.0, "j1", JobStatus::Completed).unwrap();
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.pending, 1);
        assert!(!summary.done);

        tracker.report(id.0, "j2", JobStatus::Completed).unwrap();
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.pending, 0);
        assert!(summary.done);
    }

    #[test]
    fn failed_reports_update_summary() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1", "j2"]));

        tracker.report(id.0, "j1", JobStatus::Completed).unwrap();
        tracker.report(id.0, "j2", JobStatus::Failed).unwrap();
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.pending, 0);
        assert!(summary.done);
    }

    #[test]
    fn unschedulable_jobs_count_in_the_summary_and_as_done() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(record_of(&[
            ("placed", Some("n1"), JobStatus::Pending),
            ("homeless", None, JobStatus::Unschedulable),
        ]));
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.unschedulable, 1);
        assert_eq!(summary.pending, 1);
        assert!(!summary.done);

        tracker
            .report(id.0, "placed", JobStatus::Completed)
            .unwrap();
        assert!(tracker.summary(id.0).unwrap().done);
    }

    #[test]
    fn summary_nonexistent_batch_returns_none() {
        let tracker = BatchTracker::new();
        assert!(tracker.summary(999).is_none());
    }

    #[test]
    fn report_for_a_nonexistent_job_is_rejected() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1"]));
        let err = tracker
            .report(id.0, "nonexistent", JobStatus::Completed)
            .unwrap_err();
        assert!(matches!(err, ReportError::UnknownJob { .. }));
        assert_eq!(tracker.summary(id.0).unwrap().completed, 0);
    }

    #[test]
    fn report_for_a_nonexistent_batch_is_rejected() {
        let mut tracker = BatchTracker::new();
        let err = tracker.report(42, "j1", JobStatus::Completed).unwrap_err();
        assert_eq!(err, ReportError::UnknownBatch { batch_id: 42 });
    }

    #[test]
    fn duplicate_terminal_report_is_idempotent() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1"]));
        assert_eq!(
            tracker.report(id.0, "j1", JobStatus::Completed).unwrap(),
            ReportOutcome::Applied
        );
        assert_eq!(
            tracker.report(id.0, "j1", JobStatus::Completed).unwrap(),
            ReportOutcome::Duplicate
        );
        // No double counting.
        assert_eq!(tracker.summary(id.0).unwrap().completed, 1);
    }

    #[test]
    fn conflicting_terminal_report_is_an_illegal_transition() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1"]));
        tracker.report(id.0, "j1", JobStatus::Completed).unwrap();
        let err = tracker.report(id.0, "j1", JobStatus::Failed).unwrap_err();
        assert_eq!(
            err,
            ReportError::IllegalTransition {
                from: JobStatus::Completed,
                to: JobStatus::Failed,
            }
        );
        // Backwards transitions are equally rejected.
        let err = tracker.report(id.0, "j1", JobStatus::Running).unwrap_err();
        assert!(matches!(err, ReportError::IllegalTransition { .. }));
    }

    #[test]
    fn unschedulable_jobs_reject_all_reports() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(record_of(&[("homeless", None, JobStatus::Unschedulable)]));
        for status in [JobStatus::Running, JobStatus::Completed, JobStatus::Failed] {
            let err = tracker.report(id.0, "homeless", status).unwrap_err();
            assert!(matches!(err, ReportError::IllegalTransition { .. }));
        }
    }

    #[test]
    fn forged_statuses_are_not_reportable() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1"]));
        for status in [JobStatus::Pending, JobStatus::Unschedulable] {
            let err = tracker.report(id.0, "j1", status).unwrap_err();
            assert_eq!(err, ReportError::NotReportable { status });
        }
    }

    #[test]
    fn running_report_then_completion() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(pending(&["j1"]));
        assert_eq!(
            tracker.report(id.0, "j1", JobStatus::Running).unwrap(),
            ReportOutcome::Applied
        );
        assert_eq!(
            tracker.report(id.0, "j1", JobStatus::Running).unwrap(),
            ReportOutcome::Duplicate
        );
        assert_eq!(
            tracker.report(id.0, "j1", JobStatus::Completed).unwrap(),
            ReportOutcome::Applied
        );
    }

    #[test]
    fn registration_prunes_terminal_batches_past_retention() {
        let mut state = BatchDurableState::default();
        let mut old_done = pending(&["j"]);
        old_done.jobs[0].status = JobStatus::Completed;
        old_done.submitted_at_epoch_secs = 1_000;
        let old_id = state.register(old_done);

        let mut still_running = pending(&["j"]);
        still_running.submitted_at_epoch_secs = 1_000;
        let running_id = state.register(still_running);

        // A new registration far in the future prunes the terminal batch
        // but never an in-flight one.
        let mut fresh = pending(&["j"]);
        fresh.submitted_at_epoch_secs = 1_000 + TERMINAL_RETENTION_SECS + 1;
        let fresh_id = state.register(fresh);

        assert!(state.get(old_id).is_none(), "terminal batch pruned");
        assert!(state.get(running_id).is_some(), "in-flight batch kept");
        assert!(state.get(fresh_id).is_some());
        // Ids stay monotonic after pruning.
        assert_eq!(state.next_batch_id, fresh_id + 1);
    }

    #[test]
    fn registration_caps_retained_terminal_batches() {
        let mut state = BatchDurableState::default();
        for _ in 0..(MAX_TERMINAL_BATCHES + 10) {
            let mut done = pending(&["j"]);
            done.jobs[0].status = JobStatus::Completed;
            done.submitted_at_epoch_secs = 1_000;
            state.register(done);
        }
        let terminal = state
            .batches
            .iter()
            .filter(|(_, r)| r.is_terminal())
            .count();
        assert!(
            terminal <= MAX_TERMINAL_BATCHES + 1,
            "terminal batches must stay capped, found {terminal}"
        );
    }
}

//! Batch completion tracking.
//!
//! Tracks the status of submitted batch jobs. The batch submission
//! returns immediately with a `BatchId`; completion is tracked
//! asynchronously as nodes report job completion via the reporting tree.

use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::batch::BatchId;
use super::types::NodeId;

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
}

/// Tracked state for a single job in a batch.
#[derive(Debug, Clone)]
struct TrackedJob {
    name: String,
    #[allow(dead_code)] // Used when querying per-node status (Phase 9)
    node: NodeId,
    status: JobStatus,
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
    /// Whether all jobs have finished (completed or failed).
    pub done: bool,
    /// Seconds since batch submission.
    pub elapsed_secs: u64,
}

/// Tracks completion status of batch submissions.
pub struct BatchTracker {
    batches: HashMap<u64, BatchState>,
    next_id: u64,
}

struct BatchState {
    jobs: Vec<TrackedJob>,
    submitted_at: Instant,
}

impl BatchTracker {
    /// Create a new tracker.
    pub fn new() -> Self {
        Self {
            batches: HashMap::new(),
            next_id: 1,
        }
    }

    /// Register a new batch. Returns the assigned BatchId.
    pub fn register(&mut self, assignments: &[(String, NodeId)]) -> BatchId {
        let id = self.next_id;
        self.next_id += 1;

        let jobs = assignments
            .iter()
            .map(|(name, node)| TrackedJob {
                name: name.clone(),
                node: node.clone(),
                status: JobStatus::Pending,
            })
            .collect();

        self.batches.insert(
            id,
            BatchState {
                jobs,
                submitted_at: Instant::now(),
            },
        );

        BatchId(id)
    }

    /// Mark a job as completed.
    pub fn mark_completed(&mut self, batch_id: u64, job_name: &str) {
        if let Some(state) = self.batches.get_mut(&batch_id)
            && let Some(job) = state.jobs.iter_mut().find(|j| j.name == job_name)
        {
            job.status = JobStatus::Completed;
        }
    }

    /// Mark a job as failed.
    pub fn mark_failed(&mut self, batch_id: u64, job_name: &str) {
        if let Some(state) = self.batches.get_mut(&batch_id)
            && let Some(job) = state.jobs.iter_mut().find(|j| j.name == job_name)
        {
            job.status = JobStatus::Failed;
        }
    }

    /// Get a summary of a batch's progress.
    pub fn summary(&self, batch_id: u64) -> Option<BatchSummary> {
        let state = self.batches.get(&batch_id)?;
        let total = state.jobs.len();
        let completed = state
            .jobs
            .iter()
            .filter(|j| j.status == JobStatus::Completed)
            .count();
        let failed = state
            .jobs
            .iter()
            .filter(|j| j.status == JobStatus::Failed)
            .count();
        let pending = total - completed - failed;

        Some(BatchSummary {
            batch_id,
            total,
            pending,
            completed,
            failed,
            done: pending == 0,
            elapsed_secs: state.submitted_at.elapsed().as_secs(),
        })
    }

    /// Remove completed batches older than the given age.
    pub fn gc(&mut self, max_age: std::time::Duration) {
        self.batches.retain(|_, state| {
            let all_done = state
                .jobs
                .iter()
                .all(|j| j.status == JobStatus::Completed || j.status == JobStatus::Failed);
            !(all_done && state.submitted_at.elapsed() > max_age)
        });
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

    #[test]
    fn register_assigns_incrementing_ids() {
        let mut tracker = BatchTracker::new();
        let id1 = tracker.register(&[("j1".into(), NodeId::new("n1"))]);
        let id2 = tracker.register(&[("j2".into(), NodeId::new("n1"))]);
        assert_eq!(id1, BatchId(1));
        assert_eq!(id2, BatchId(2));
    }

    #[test]
    fn summary_starts_all_pending() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(&[
            ("j1".into(), NodeId::new("n1")),
            ("j2".into(), NodeId::new("n1")),
            ("j3".into(), NodeId::new("n2")),
        ]);
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.pending, 3);
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.failed, 0);
        assert!(!summary.done);
    }

    #[test]
    fn mark_completed_updates_summary() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(&[
            ("j1".into(), NodeId::new("n1")),
            ("j2".into(), NodeId::new("n1")),
        ]);

        tracker.mark_completed(id.0, "j1");
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.pending, 1);
        assert!(!summary.done);

        tracker.mark_completed(id.0, "j2");
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.pending, 0);
        assert!(summary.done);
    }

    #[test]
    fn mark_failed_updates_summary() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(&[
            ("j1".into(), NodeId::new("n1")),
            ("j2".into(), NodeId::new("n1")),
        ]);

        tracker.mark_completed(id.0, "j1");
        tracker.mark_failed(id.0, "j2");
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.pending, 0);
        assert!(summary.done);
    }

    #[test]
    fn summary_nonexistent_batch_returns_none() {
        let tracker = BatchTracker::new();
        assert!(tracker.summary(999).is_none());
    }

    #[test]
    fn mark_nonexistent_job_is_noop() {
        let mut tracker = BatchTracker::new();
        let id = tracker.register(&[("j1".into(), NodeId::new("n1"))]);
        tracker.mark_completed(id.0, "nonexistent");
        let summary = tracker.summary(id.0).unwrap();
        assert_eq!(summary.completed, 0); // no change
    }
}

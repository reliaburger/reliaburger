//! The job and task-array attempts running on this node right now.
//!
//! App instances live in the supervisor, but job attempts don't: they run
//! through the task runtime, either in a container of their own or as one
//! command in a reusable executor. Smoker still needs to find them, to kill
//! or pause an attempt, to throttle its cgroup, or to fault the calls it
//! makes. So every runner lists each attempt here while it runs, and the
//! agent and the API read the list.
//!
//! The list is shared between the task runtime (which writes it from
//! whatever task runs the attempt) and the agent loop (which reads it), so it
//! sits behind a `std::sync::Mutex`. Every critical section is a short
//! in-memory map update or copy, and none of them is held across an
//! `.await`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::grill::InstanceId;
use crate::smoker::types::WorkloadKind;

/// How to reach a running attempt's processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptHandle {
    /// The attempt is a runtime instance of its own: the runtime names its
    /// main pid, and it has its own cgroup and network namespace.
    Instance(InstanceId),
    /// The attempt is one command in a reusable executor. Every process the
    /// command starts lands in this task cgroup, and nothing else does, so
    /// signalling the cgroup's processes reaches this task alone.
    TaskGroup(PathBuf),
}

/// One attempt running on this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAttempt {
    /// The job's namespace.
    pub namespace: String,
    /// The job (definition) name: what `relish fault` targets.
    pub job: String,
    /// The run (batch) id.
    pub batch_id: u64,
    /// Tasks in the run; one means a plain job.
    pub task_count: u32,
    /// The task's index.
    pub index: u32,
    /// The attempt number, from 1.
    pub attempt: u8,
    /// Where its processes are.
    pub handle: AttemptHandle,
    /// The attempt's own cgroup, when the runtime placed it in one that
    /// nothing else shares. `None` for a host process and for a command in a
    /// reusable executor.
    pub cgroup: Option<PathBuf>,
    /// The reusable executor the command runs in, if it shares one with
    /// other tasks over time.
    pub shared_executor: Option<InstanceId>,
}

impl LiveAttempt {
    /// Whether the attempt's run is a job or a task array.
    pub fn kind(&self) -> WorkloadKind {
        WorkloadKind::of_run(self.task_count)
    }

    /// Whether this attempt is one a fault on `job` in `namespace`, scoped to
    /// `task_index` if given, would reach.
    pub fn matches(&self, namespace: &str, job: &str, task_index: Option<u32>) -> bool {
        self.namespace == namespace
            && self.job == job
            && task_index.is_none_or(|index| index == self.index)
    }

    /// What a node reports about this attempt to the others.
    pub fn status(&self) -> AttemptStatus {
        AttemptStatus {
            namespace: self.namespace.clone(),
            job: self.job.clone(),
            batch_id: self.batch_id,
            task_count: self.task_count,
            index: self.index,
            attempt: self.attempt,
            shared_executor: self.shared_executor.as_ref().map(|id| id.0.clone()),
        }
    }
}

/// A running attempt as one node reports it to another, for routing a fault
/// to the node that runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptStatus {
    /// The job's namespace.
    pub namespace: String,
    /// The job name.
    pub job: String,
    /// The run (batch) id.
    pub batch_id: u64,
    /// Tasks in the run.
    pub task_count: u32,
    /// The task's index.
    pub index: u32,
    /// The attempt number.
    pub attempt: u8,
    /// The reusable executor the command shares, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_executor: Option<String>,
}

/// Every attempt running on this node.
#[derive(Debug, Default)]
pub struct LiveAttempts {
    next: AtomicU64,
    // A std mutex: see the module comment.
    entries: Mutex<BTreeMap<u64, LiveAttempt>>,
}

impl LiveAttempts {
    /// An empty list.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// List `attempt` until the returned listing is dropped.
    pub fn register(self: &Arc<Self>, attempt: LiveAttempt) -> AttemptListing {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, attempt);
        AttemptListing {
            attempts: Arc::clone(self),
            key,
        }
    }

    /// A copy of every attempt listed now, oldest first.
    pub fn snapshot(&self) -> Vec<LiveAttempt> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// The attempts of `job` in `namespace`, limited to one task index if
    /// given, oldest first.
    pub fn matching(&self, namespace: &str, job: &str, task_index: Option<u32>) -> Vec<LiveAttempt> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|attempt| attempt.matches(namespace, job, task_index))
            .cloned()
            .collect()
    }

    /// What this node reports to the others.
    pub fn statuses(&self) -> Vec<AttemptStatus> {
        self.snapshot().iter().map(LiveAttempt::status).collect()
    }
}

/// Keeps one attempt listed for as long as it lives, however its runner
/// returns.
#[derive(Debug)]
pub struct AttemptListing {
    attempts: Arc<LiveAttempts>,
    key: u64,
}

impl Drop for AttemptListing {
    fn drop(&mut self) {
        self.attempts
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(job: &str, index: u32) -> LiveAttempt {
        LiveAttempt {
            namespace: "default".into(),
            job: job.into(),
            batch_id: 7,
            task_count: 4,
            index,
            attempt: 1,
            handle: AttemptHandle::TaskGroup(PathBuf::from("/nowhere")),
            cgroup: None,
            shared_executor: None,
        }
    }

    #[test]
    fn an_attempt_is_listed_until_its_listing_drops() {
        let attempts = LiveAttempts::new();
        let listing = attempts.register(attempt("render", 0));
        assert_eq!(attempts.snapshot().len(), 1);
        drop(listing);
        assert!(attempts.snapshot().is_empty());
    }

    #[test]
    fn matching_confines_a_job_to_its_namespace_and_task_index() {
        let attempts = LiveAttempts::new();
        let _zero = attempts.register(attempt("render", 0));
        let _one = attempts.register(attempt("render", 1));
        let _other = attempts.register(attempt("encode", 1));
        assert_eq!(attempts.matching("default", "render", None).len(), 2);
        let one = attempts.matching("default", "render", Some(1));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].index, 1);
        assert!(attempts.matching("team-a", "render", None).is_empty());
    }

    #[test]
    fn a_run_of_one_task_is_a_job_and_more_is_an_array() {
        let mut single = attempt("migrate", 0);
        single.task_count = 1;
        assert_eq!(single.kind(), WorkloadKind::Job);
        assert_eq!(attempt("render", 0).kind(), WorkloadKind::TaskArray);
    }
}

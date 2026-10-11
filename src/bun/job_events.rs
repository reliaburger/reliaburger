//! One event per job run, not one per task.
//!
//! The leader compares each committed snapshot of the job state with what it
//! has already reported and announces two kinds of change: a run reaching a
//! terminal state (succeeded, failed, cancelled), and a run whose outcome
//! became unknown because a node holding its work went quiet. Both come from
//! the replicated state, never from task results, so a run of a million tasks
//! is still one event.
//!
//! The tracker lives in the leader's memory. A new leader starts by taking a
//! snapshot without reporting anything, so a leader change doesn't announce
//! every retained run again. The price is that a run finishing in the second
//! before a leader change may go unannounced; its outcome is still in the
//! definition's durable history.

use std::collections::BTreeSet;

use crate::meat::task_array_state::TaskArrayStatus;
use crate::meat::task_array_store::TaskArrays;

/// How a run changed, as far as an operator cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunChange {
    /// Every task succeeded.
    Succeeded,
    /// At least one task failed for good.
    Failed,
    /// Someone stopped it.
    Cancelled,
    /// A node holding its work went quiet; nobody knows whether those tasks
    /// ran. The run waits for an acknowledged replay.
    UnknownOutcome,
}

/// One run-level change to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTransition {
    /// The run's batch id (a manifest's parent id for a profiled run).
    pub run: u64,
    /// The job or array name.
    pub name: String,
    /// Its namespace.
    pub namespace: String,
    /// What happened.
    pub change: RunChange,
    /// Tasks that succeeded, across every profile.
    pub succeeded: u64,
    /// Tasks that failed for good, across every profile.
    pub failed: u64,
    /// Tasks never run because the run stopped first.
    pub not_run: u64,
    /// Nodes whose outcome is unknown (only for [`RunChange::UnknownOutcome`]).
    pub unknown_owners: Vec<String>,
}

impl RunTransition {
    /// Whether this change should reach the alert webhook.
    pub fn notifies(&self) -> bool {
        matches!(self.change, RunChange::Failed | RunChange::UnknownOutcome)
    }

    /// The event this change records.
    pub fn event_kind(&self) -> (super::events::EventKind, super::events::EventSeverity) {
        use super::events::{EventKind, EventSeverity};
        match self.change {
            RunChange::Succeeded => (EventKind::JobCompleted, EventSeverity::Info),
            RunChange::Failed => (EventKind::JobFailed, EventSeverity::Critical),
            RunChange::Cancelled => (EventKind::JobCancelled, EventSeverity::Info),
            RunChange::UnknownOutcome => (EventKind::JobUnknown, EventSeverity::Critical),
        }
    }

    /// A one-line description for the event log and the webhook.
    pub fn message(&self) -> String {
        let job = format!("job {}/{} run {}", self.namespace, self.name, self.run);
        match self.change {
            RunChange::Succeeded => format!("{job} succeeded: {} tasks", self.succeeded),
            RunChange::Failed => format!(
                "{job} failed: {} succeeded, {} failed, {} not run; see relish batch results {} --failed",
                self.succeeded, self.failed, self.not_run, self.run
            ),
            RunChange::Cancelled => format!(
                "{job} cancelled: {} succeeded, {} failed, {} not run",
                self.succeeded, self.failed, self.not_run
            ),
            RunChange::UnknownOutcome => format!(
                "{job} has an unknown outcome on {}; check side effects, then relish batch replay",
                self.unknown_owners.join(", ")
            ),
        }
    }

    /// The webhook notification for a failed or unknown run.
    pub fn alert(&self) -> Option<crate::mayo::webhook::JobRunAlert> {
        use crate::mayo::webhook::{JobAlertKind, JobRunAlert};
        let kind = match self.change {
            RunChange::Failed => JobAlertKind::Failed,
            RunChange::UnknownOutcome => JobAlertKind::UnknownOutcome,
            RunChange::Succeeded | RunChange::Cancelled => return None,
        };
        Some(JobRunAlert {
            kind,
            namespace: self.namespace.clone(),
            job: self.name.clone(),
            run: self.run,
            message: self.message(),
        })
    }
}

/// What the leader has already reported.
#[derive(Debug, Default)]
pub struct JobEventTracker {
    /// Whether the first snapshot after gaining leadership has been taken.
    seeded: bool,
    /// Runs whose terminal state was reported (or predates leadership).
    finished: BTreeSet<u64>,
    /// Runs currently reported as having an unknown outcome.
    unknown: BTreeSet<u64>,
}

impl JobEventTracker {
    /// Forget everything; the next [`Self::observe`] reports nothing.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The changes since the last call, at most one per run each.
    pub fn observe(&mut self, arrays: &TaskArrays) -> Vec<RunTransition> {
        let current = runs(arrays);
        let mut changes = Vec::new();
        for run in &current {
            if let Some(change) = run.terminal
                && self.finished.insert(run.transition.run)
                && self.seeded
            {
                changes.push(RunTransition {
                    change,
                    ..run.transition.clone()
                });
            }
            if run.transition.unknown_owners.is_empty() || run.terminal.is_some() {
                self.unknown.remove(&run.transition.run);
            } else if self.unknown.insert(run.transition.run) && self.seeded {
                changes.push(RunTransition {
                    change: RunChange::UnknownOutcome,
                    ..run.transition.clone()
                });
            }
        }
        // Forget pruned runs, so memory stays bounded by retention.
        let live: BTreeSet<u64> = current.iter().map(|run| run.transition.run).collect();
        self.finished.retain(|id| live.contains(id));
        self.unknown.retain(|id| live.contains(id));
        self.seeded = true;
        changes
    }
}

struct RunView {
    transition: RunTransition,
    terminal: Option<RunChange>,
}

/// Every run the store keeps, with a manifest's profiles folded into one.
fn runs(arrays: &TaskArrays) -> Vec<RunView> {
    let mut views = Vec::new();
    let mut children = BTreeSet::new();
    for (id, manifest) in arrays.manifests() {
        let mut view = RunView {
            transition: RunTransition {
                run: id,
                name: manifest.name.clone(),
                namespace: manifest.namespace.clone(),
                change: RunChange::Succeeded,
                succeeded: 0,
                failed: 0,
                not_run: 0,
                unknown_owners: Vec::new(),
            },
            terminal: Some(RunChange::Succeeded),
        };
        for (_, child) in &manifest.cohorts {
            children.insert(*child);
            let Some(record) = arrays.get(*child) else {
                continue;
            };
            let summary = record.state.summary();
            view.transition.succeeded += summary.succeeded;
            view.transition.failed += summary.failed;
            view.transition.not_run += summary.not_run;
            view.transition
                .unknown_owners
                .extend(unknown_owners(arrays, *child));
            view.terminal = worst(view.terminal, change_of(summary.status));
        }
        views.push(view);
    }
    for (id, record) in arrays.iter() {
        if children.contains(&id) {
            continue;
        }
        let summary = record.state.summary();
        views.push(RunView {
            transition: RunTransition {
                run: id,
                name: record.name.clone(),
                namespace: record.namespace.clone(),
                change: RunChange::Succeeded,
                succeeded: summary.succeeded,
                failed: summary.failed,
                not_run: summary.not_run,
                unknown_owners: unknown_owners(arrays, id),
            },
            terminal: change_of(summary.status),
        });
    }
    views
}

fn unknown_owners(arrays: &TaskArrays, id: u64) -> Vec<String> {
    arrays
        .jobs()
        .run(id)
        .map(|run| {
            run.unknown_owners
                .iter()
                .map(|node| node.0.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn change_of(status: TaskArrayStatus) -> Option<RunChange> {
    match status {
        TaskArrayStatus::Running | TaskArrayStatus::Stopping => None,
        TaskArrayStatus::Succeeded => Some(RunChange::Succeeded),
        TaskArrayStatus::Failed | TaskArrayStatus::CompletedWithFailures => Some(RunChange::Failed),
        TaskArrayStatus::Cancelled => Some(RunChange::Cancelled),
    }
}

/// A manifest is finished only when every profile is, and it reports its
/// worst profile: a failure outranks a cancel, which outranks success.
fn worst(left: Option<RunChange>, right: Option<RunChange>) -> Option<RunChange> {
    let rank = |change: RunChange| match change {
        RunChange::Succeeded => 0,
        RunChange::Cancelled => 1,
        RunChange::Failed | RunChange::UnknownOutcome => 2,
    };
    match (left, right) {
        (Some(left), Some(right)) => Some(if rank(right) > rank(left) {
            right
        } else {
            left
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::job::JobSpec;
    use crate::meat::NodeId;
    use crate::meat::index_set::IndexRangeSet;
    use crate::meat::task_array::{ChunkId, TaskArraySpec};
    use crate::meat::task_array_state::ChunkResult;
    use crate::meat::task_array_store::TaskArrayWrite;

    fn template() -> Box<JobSpec> {
        Box::new(toml::from_str("runtime='process'\nexec='/bin/true'").unwrap())
    }

    fn register(arrays: &mut TaskArrays, id: u64, count: u32, chunk_size: u32) {
        arrays
            .apply(
                &TaskArrayWrite::Register {
                    name: "nightly".into(),
                    namespace: "ops".into(),
                    template: template(),
                    spec: TaskArraySpec {
                        chunk_size,
                        ..TaskArraySpec::with_count(count)
                    },
                    submitted_at_epoch_secs: 1,
                },
                || id,
            )
            .unwrap();
    }

    /// Grant every chunk to one node, then report every task failed.
    fn fail_everything(arrays: &mut TaskArrays, id: u64) {
        let spec = arrays.get(id).unwrap().state.spec.clone();
        let node = NodeId::new("worker");
        let chunks = spec.chunk_count();
        arrays
            .apply(
                &TaskArrayWrite::Sync {
                    now_epoch_secs: 2,
                    batch_id: id,
                    results: Vec::new(),
                    grants: vec![(node.clone(), IndexRangeSet::from_range(0..=chunks - 1))],
                },
                || 0,
            )
            .unwrap();
        let results = (0..chunks)
            .map(|chunk| {
                let range = spec.chunk_range(ChunkId(chunk)).unwrap();
                let tasks = range.end() - range.start() + 1;
                (
                    node.clone(),
                    ChunkResult {
                        duration_counts: [0; 16],
                        chunk: ChunkId(chunk),
                        attempt: 1,
                        succeeded: 0,
                        failed_count: tasks,
                        failed_indices: IndexRangeSet::from_range(range),
                        not_run: 0,
                        retried: 0,
                        retry_causes: Default::default(),
                    },
                )
            })
            .collect();
        arrays
            .apply(
                &TaskArrayWrite::Sync {
                    now_epoch_secs: 3,
                    batch_id: id,
                    results,
                    grants: Vec::new(),
                },
                || 0,
            )
            .unwrap();
    }

    #[test]
    fn a_run_reaching_failed_records_one_event() {
        let mut arrays = TaskArrays::default();
        let mut tracker = JobEventTracker::default();
        register(&mut arrays, 1, 1, 1);
        assert!(tracker.observe(&arrays).is_empty(), "still running");

        fail_everything(&mut arrays, 1);
        let changes = tracker.observe(&arrays);
        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert_eq!(change.change, RunChange::Failed);
        assert_eq!((change.run, change.failed), (1, 1));
        assert!(change.notifies());
        assert_eq!(
            change.event_kind(),
            (
                crate::bun::events::EventKind::JobFailed,
                crate::bun::events::EventSeverity::Critical
            )
        );
        assert!(change.message().contains("ops/nightly"));
        assert!(tracker.observe(&arrays).is_empty(), "reported once");
    }

    #[test]
    fn a_million_task_run_records_one_event_not_one_per_task() {
        let mut arrays = TaskArrays::default();
        let mut tracker = JobEventTracker::default();
        register(&mut arrays, 1, 1_000_000, 1000);
        tracker.observe(&arrays);
        fail_everything(&mut arrays, 1);
        let changes = tracker.observe(&arrays);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].failed, 1_000_000);
    }

    #[test]
    fn a_new_leader_does_not_announce_runs_that_finished_before_it() {
        let mut arrays = TaskArrays::default();
        register(&mut arrays, 1, 1, 1);
        fail_everything(&mut arrays, 1);
        let mut tracker = JobEventTracker::default();
        assert!(tracker.observe(&arrays).is_empty());
        tracker.reset();
        assert!(tracker.observe(&arrays).is_empty());
    }

    #[test]
    fn an_unknown_outcome_records_one_event_and_notifies() {
        use crate::meat::job::{JobDefinition, JobWrite, RunTrigger};
        let mut arrays = TaskArrays::default();
        let mut tracker = JobEventTracker::default();
        arrays
            .apply(
                &TaskArrayWrite::Job(Box::new(JobWrite::Put {
                    name: "migrate".into(),
                    namespace: "ops".into(),
                    definition: Box::new(JobDefinition::from_spec(*template())),
                    trigger: Some(RunTrigger::Manual {
                        request_id: "once".into(),
                    }),
                    now_epoch_secs: 1,
                })),
                || 1,
            )
            .unwrap();
        let node = NodeId::new("lost");
        arrays
            .apply(
                &TaskArrayWrite::Sync {
                    now_epoch_secs: 2,
                    batch_id: 1,
                    results: Vec::new(),
                    grants: vec![(node.clone(), IndexRangeSet::from_range(0..=0))],
                },
                || 0,
            )
            .unwrap();
        tracker.observe(&arrays);
        arrays
            .apply(&TaskArrayWrite::Unknown { batch_id: 1, node }, || 0)
            .unwrap();
        let changes = tracker.observe(&arrays);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, RunChange::UnknownOutcome);
        assert_eq!(changes[0].unknown_owners, vec!["lost".to_string()]);
        let alert = changes[0].alert().unwrap();
        assert_eq!(
            alert.kind,
            crate::mayo::webhook::JobAlertKind::UnknownOutcome
        );
        assert!(tracker.observe(&arrays).is_empty(), "reported once");
    }

    #[test]
    fn successes_and_cancels_record_events_but_send_no_webhook() {
        let mut arrays = TaskArrays::default();
        let mut tracker = JobEventTracker::default();
        register(&mut arrays, 1, 4, 4);
        tracker.observe(&arrays);
        arrays
            .apply(
                &TaskArrayWrite::Cancel {
                    batch_id: 1,
                    now_epoch_secs: 2,
                },
                || 0,
            )
            .unwrap();
        let changes = tracker.observe(&arrays);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, RunChange::Cancelled);
        assert!(!changes[0].notifies());
        assert!(changes[0].alert().is_none());
    }
}

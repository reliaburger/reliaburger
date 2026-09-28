//! Every task array the cluster knows about, as the Raft state machine
//! holds them.
//!
//! [`TaskArrays`] is one field of the replicated `DesiredState`. It changes
//! only through [`TaskArrayWrite`]s, which the leader proposes and every
//! replica applies in log order with [`TaskArrays::apply`]. Standalone nodes
//! (no council) apply the same writes to an in-memory copy, so there's one
//! set of rules either way.
//!
//! The writes are few by design: one `Register` per submission, at most one
//! `Sync` per array per leader tick (carrying every finished chunk and every
//! new grant since the last one), and a `Cancel` or `Requeue` when those
//! happen. A million tasks cost a few hundred entries, not millions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::index_set::IndexRangeSet;
use super::task_array::{TaskArraySpec, TaskArraySpecError, validate_template};
use super::task_array_state::{ChunkResult, TaskArrayState};
use super::types::NodeId;
use crate::config::job::JobSpec;

/// How long a finished array stays readable (status, results, logs)
/// before a later registration prunes it.
pub const TERMINAL_RETENTION_SECS: u64 = 3600;

/// Most finished arrays kept, newest first. Each can hold up to about
/// 160 KiB of failed-index ranges, so this bounds the snapshot too.
pub const MAX_TERMINAL_ARRAYS: usize = 20;

/// Most arrays running at once. The leader syncs every node for every
/// running array once a second, so this bounds that work.
pub const MAX_ACTIVE_ARRAYS: usize = 64;

/// One submitted array: what to run, and how far it has got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArrayRecord {
    /// The name given at submission (`relish run --batch NAME`).
    pub name: String,
    /// Namespace the array belongs to.
    pub namespace: String,
    /// The job every task runs, with `{index}` placeholders.
    pub template: JobSpec,
    /// Chunks, grants and counts.
    pub state: TaskArrayState,
}

/// A change to the set of task arrays. Carried by one Raft entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskArrayWrite {
    /// Submit a new array. The id comes from the cluster's batch counter,
    /// so array and ordinary batch ids never collide.
    Register {
        name: String,
        namespace: String,
        template: Box<JobSpec>,
        spec: TaskArraySpec,
        /// The submitter's clock; `apply` never reads its own.
        submitted_at_epoch_secs: u64,
    },
    /// Record finished chunks, then hand out new ones.
    Sync {
        batch_id: u64,
        /// Chunks nodes finished, each fenced by its grant attempt.
        results: Vec<(NodeId, ChunkResult)>,
        /// New grants, planned by the leader against the state with
        /// `results` applied.
        grants: Vec<(NodeId, IndexRangeSet)>,
    },
    /// Stop an array: nothing new starts, running chunks drain.
    Cancel { batch_id: u64 },
    /// A node went quiet: take its chunks back at the next attempt.
    Requeue { batch_id: u64, node: NodeId },
}

/// What an applied write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskArrayApplied {
    /// A new array exists under this id.
    Registered { batch_id: u64 },
    /// A sync went through. Refused items are stale (a lost node's late
    /// report, a grant raced by a cancel) and are skipped, not fatal.
    Synced {
        results_applied: u32,
        results_refused: u32,
        grants_applied: u32,
        grants_refused: u32,
    },
    /// The array is stopping (or had already stopped).
    Cancelled,
    /// These chunks went back to the queue (or were written off, if the
    /// array had stopped).
    Requeued { chunks: IndexRangeSet },
}

/// Why a write was refused as a whole.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskArrayStoreError {
    #[error("task array {batch_id} not found")]
    UnknownArray { batch_id: u64 },
    #[error("invalid task array: {0}")]
    Invalid(#[from] TaskArraySpecError),
    #[error("a task array needs a name")]
    EmptyName,
    #[error("{active} task arrays are already running; the limit is {MAX_ACTIVE_ARRAYS}")]
    TooManyActive { active: usize },
}

/// The replicated set of task arrays, keyed by batch id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskArrays {
    arrays: BTreeMap<u64, TaskArrayRecord>,
}

impl TaskArrays {
    /// Apply one write. `allocate_id` is called only for a registration
    /// that will succeed, so a refused one doesn't burn an id.
    pub fn apply(
        &mut self,
        write: &TaskArrayWrite,
        allocate_id: impl FnOnce() -> u64,
    ) -> Result<TaskArrayApplied, TaskArrayStoreError> {
        match write {
            TaskArrayWrite::Register {
                name,
                namespace,
                template,
                spec,
                submitted_at_epoch_secs,
            } => {
                if name.trim().is_empty() {
                    return Err(TaskArrayStoreError::EmptyName);
                }
                validate_template(template)?;
                let state = TaskArrayState::new(spec.clone(), *submitted_at_epoch_secs)?;
                self.prune(*submitted_at_epoch_secs);
                let active = self.active().count();
                if active >= MAX_ACTIVE_ARRAYS {
                    return Err(TaskArrayStoreError::TooManyActive { active });
                }
                let batch_id = allocate_id();
                self.arrays.insert(
                    batch_id,
                    TaskArrayRecord {
                        name: name.clone(),
                        namespace: namespace.clone(),
                        template: (**template).clone(),
                        state,
                    },
                );
                Ok(TaskArrayApplied::Registered { batch_id })
            }
            TaskArrayWrite::Sync {
                batch_id,
                results,
                grants,
            } => {
                let state = &mut self.record_mut(*batch_id)?.state;
                let mut applied = (0, 0, 0, 0);
                for (node, result) in results {
                    match state.complete(node, result) {
                        Ok(_) => applied.0 += 1,
                        Err(_) => applied.1 += 1,
                    }
                }
                for (node, chunks) in grants {
                    match state.grant(node, chunks) {
                        Ok(()) => applied.2 += 1,
                        Err(_) => applied.3 += 1,
                    }
                }
                Ok(TaskArrayApplied::Synced {
                    results_applied: applied.0,
                    results_refused: applied.1,
                    grants_applied: applied.2,
                    grants_refused: applied.3,
                })
            }
            TaskArrayWrite::Cancel { batch_id } => {
                self.record_mut(*batch_id)?.state.cancel();
                Ok(TaskArrayApplied::Cancelled)
            }
            TaskArrayWrite::Requeue { batch_id, node } => {
                let chunks = self.record_mut(*batch_id)?.state.requeue_node(node);
                Ok(TaskArrayApplied::Requeued { chunks })
            }
        }
    }

    fn record_mut(&mut self, batch_id: u64) -> Result<&mut TaskArrayRecord, TaskArrayStoreError> {
        self.arrays
            .get_mut(&batch_id)
            .ok_or(TaskArrayStoreError::UnknownArray { batch_id })
    }

    /// Drop finished arrays past the retention window, then keep at most
    /// [`MAX_TERMINAL_ARRAYS`] of the rest (newest ids win). `now` comes
    /// from the write, so every replica prunes the same arrays.
    fn prune(&mut self, now_epoch_secs: u64) {
        self.arrays.retain(|_, record| {
            !(record.state.status().is_terminal()
                && now_epoch_secs.saturating_sub(record.state.submitted_at_epoch_secs)
                    > TERMINAL_RETENTION_SECS)
        });
        let terminal: Vec<u64> = self
            .arrays
            .iter()
            .filter(|(_, record)| record.state.status().is_terminal())
            .map(|(id, _)| *id)
            .collect();
        let excess = terminal.len().saturating_sub(MAX_TERMINAL_ARRAYS);
        for id in terminal.into_iter().take(excess) {
            self.arrays.remove(&id);
        }
    }

    /// Look up an array.
    pub fn get(&self, batch_id: u64) -> Option<&TaskArrayRecord> {
        self.arrays.get(&batch_id)
    }

    /// Every array, oldest id first.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &TaskArrayRecord)> {
        self.arrays.iter().map(|(id, record)| (*id, record))
    }

    /// Arrays that haven't reached a terminal status.
    pub fn active(&self) -> impl Iterator<Item = (u64, &TaskArrayRecord)> {
        self.iter()
            .filter(|(_, record)| !record.state.status().is_terminal())
    }

    /// Whether any array is still running or stopping.
    pub fn has_active(&self) -> bool {
        self.active().next().is_some()
    }

    /// Ids of every array still held (running or finished but retained).
    pub fn ids(&self) -> Vec<u64> {
        self.arrays.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meat::task_array::ChunkId;
    use crate::meat::task_array_state::TaskArrayStatus;

    fn template() -> Box<JobSpec> {
        Box::new(JobSpec {
            image: None,
            command: None,
            schedule: None,
            run_before: Vec::new(),
            memory: None,
            cpu: None,
            env: BTreeMap::new(),
            namespace: None,
            exec: Some("/usr/bin/true".into()),
            script: None,
        })
    }

    fn register(count: u32, chunk_size: u32, at: u64) -> TaskArrayWrite {
        TaskArrayWrite::Register {
            name: "render".to_string(),
            namespace: "default".to_string(),
            template: template(),
            spec: TaskArraySpec {
                chunk_size,
                ..TaskArraySpec::with_count(count)
            },
            submitted_at_epoch_secs: at,
        }
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    fn counter() -> impl FnMut() -> u64 {
        let mut next = 0;
        move || {
            next += 1;
            next
        }
    }

    fn registered(arrays: &mut TaskArrays, write: &TaskArrayWrite, id: u64) -> u64 {
        match arrays.apply(write, || id).unwrap() {
            TaskArrayApplied::Registered { batch_id } => batch_id,
            other => panic!("expected a registration, got {other:?}"),
        }
    }

    fn finished(chunk: u32, attempt: u8, succeeded: u32) -> ChunkResult {
        ChunkResult {
            chunk: ChunkId(chunk),
            attempt,
            succeeded,
            failed_indices: IndexRangeSet::new(),
            not_run: 0,
            retried: 0,
        }
    }

    fn sync(
        batch_id: u64,
        results: Vec<(NodeId, ChunkResult)>,
        grants: Vec<(NodeId, IndexRangeSet)>,
    ) -> TaskArrayWrite {
        TaskArrayWrite::Sync {
            batch_id,
            results,
            grants,
        }
    }

    #[test]
    fn register_stores_the_array_under_the_allocated_id() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(10, 4, 100), 7);
        assert_eq!(id, 7);
        let record = arrays.get(7).unwrap();
        assert_eq!(record.name, "render");
        assert_eq!(record.state.spec.chunk_count(), 3);
        assert_eq!(record.state.submitted_at_epoch_secs, 100);
        assert!(arrays.has_active());
    }

    #[test]
    fn refused_registration_does_not_allocate_an_id() {
        let mut arrays = TaskArrays::default();
        let mut allocated = false;
        let result = arrays.apply(&register(0, 4, 1), || {
            allocated = true;
            1
        });
        assert!(matches!(result, Err(TaskArrayStoreError::Invalid(_))));
        assert!(!allocated);
    }

    #[test]
    fn register_refuses_an_image_template() {
        let mut arrays = TaskArrays::default();
        let mut image = template();
        image.exec = None;
        image.image = Some("alpine:3".to_string());
        let write = TaskArrayWrite::Register {
            name: "x".to_string(),
            namespace: "default".to_string(),
            template: image,
            spec: TaskArraySpec::with_count(3),
            submitted_at_epoch_secs: 1,
        };
        assert!(matches!(
            arrays.apply(&write, || 1),
            Err(TaskArrayStoreError::Invalid(
                TaskArraySpecError::UnsupportedTemplate { .. }
            ))
        ));
    }

    #[test]
    fn register_refuses_an_empty_name() {
        let mut arrays = TaskArrays::default();
        let write = TaskArrayWrite::Register {
            name: "  ".to_string(),
            namespace: "default".to_string(),
            template: template(),
            spec: TaskArraySpec::with_count(3),
            submitted_at_epoch_secs: 1,
        };
        assert_eq!(
            arrays.apply(&write, || 1),
            Err(TaskArrayStoreError::EmptyName)
        );
    }

    #[test]
    fn register_refuses_past_the_active_limit() {
        let mut arrays = TaskArrays::default();
        let mut ids = counter();
        for _ in 0..MAX_ACTIVE_ARRAYS {
            arrays.apply(&register(4, 4, 1), &mut ids).unwrap();
        }
        assert_eq!(
            arrays.apply(&register(4, 4, 1), &mut ids),
            Err(TaskArrayStoreError::TooManyActive {
                active: MAX_ACTIVE_ARRAYS
            })
        );
    }

    #[test]
    fn registration_prunes_finished_arrays_past_retention() {
        let mut arrays = TaskArrays::default();
        let old = registered(&mut arrays, &register(4, 4, 100), 1);
        arrays
            .apply(&TaskArrayWrite::Cancel { batch_id: old }, || 0)
            .unwrap();
        assert!(arrays.get(old).unwrap().state.status().is_terminal());

        // Within the window it stays; past it, the next registration drops it.
        registered(
            &mut arrays,
            &register(4, 4, 100 + TERMINAL_RETENTION_SECS),
            2,
        );
        assert!(arrays.get(old).is_some());
        registered(
            &mut arrays,
            &register(4, 4, 101 + TERMINAL_RETENTION_SECS),
            3,
        );
        assert!(arrays.get(old).is_none());
    }

    #[test]
    fn registration_keeps_only_the_newest_finished_arrays() {
        let mut arrays = TaskArrays::default();
        for id in 1..=(MAX_TERMINAL_ARRAYS as u64 + 5) {
            registered(&mut arrays, &register(4, 4, 1), id);
            arrays
                .apply(&TaskArrayWrite::Cancel { batch_id: id }, || 0)
                .unwrap();
        }
        registered(&mut arrays, &register(4, 4, 1), 1000);
        let terminal: Vec<u64> = arrays
            .iter()
            .filter(|(_, r)| r.state.status().is_terminal())
            .map(|(id, _)| id)
            .collect();
        assert_eq!(terminal.len(), MAX_TERMINAL_ARRAYS);
        assert_eq!(terminal.first(), Some(&6), "the oldest go first");
    }

    #[test]
    fn sync_retires_results_before_granting() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(8, 4, 1), 1);
        arrays
            .apply(
                &sync(
                    id,
                    vec![],
                    vec![(node("a"), IndexRangeSet::from_range(0..=0))],
                ),
                || 0,
            )
            .unwrap();
        // One write both retires chunk 0 and grants chunk 1 to the same node.
        let applied = arrays
            .apply(
                &sync(
                    id,
                    vec![(node("a"), finished(0, 1, 4))],
                    vec![(node("a"), IndexRangeSet::from_range(1..=1))],
                ),
                || 0,
            )
            .unwrap();
        assert_eq!(
            applied,
            TaskArrayApplied::Synced {
                results_applied: 1,
                results_refused: 0,
                grants_applied: 1,
                grants_refused: 0,
            }
        );
        let summary = arrays.get(id).unwrap().state.summary();
        assert_eq!(summary.succeeded, 4);
        assert_eq!(summary.held, 4);
    }

    #[test]
    fn sync_skips_stale_items_without_refusing_the_entry() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(8, 4, 1), 1);
        arrays
            .apply(
                &sync(
                    id,
                    vec![],
                    vec![(node("a"), IndexRangeSet::from_range(0..=0))],
                ),
                || 0,
            )
            .unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Requeue {
                    batch_id: id,
                    node: node("a"),
                },
                || 0,
            )
            .unwrap();
        // "a"'s late report is fenced; granting a chunk already held fails
        // too; the good grant still lands.
        let applied = arrays
            .apply(
                &sync(
                    id,
                    vec![(node("a"), finished(0, 1, 4))],
                    vec![
                        (node("b"), IndexRangeSet::from_range(0..=0)),
                        (node("c"), IndexRangeSet::from_range(0..=0)),
                    ],
                ),
                || 0,
            )
            .unwrap();
        assert_eq!(
            applied,
            TaskArrayApplied::Synced {
                results_applied: 0,
                results_refused: 1,
                grants_applied: 1,
                grants_refused: 1,
            }
        );
        let state = &arrays.get(id).unwrap().state;
        assert_eq!(state.attempt_of(ChunkId(0)), 2);
        assert!(state.held_by(&node("b")).is_some());
    }

    #[test]
    fn writes_to_an_unknown_array_are_refused() {
        let mut arrays = TaskArrays::default();
        for write in [
            sync(9, vec![], vec![]),
            TaskArrayWrite::Cancel { batch_id: 9 },
            TaskArrayWrite::Requeue {
                batch_id: 9,
                node: node("a"),
            },
        ] {
            assert_eq!(
                arrays.apply(&write, || 0),
                Err(TaskArrayStoreError::UnknownArray { batch_id: 9 })
            );
        }
    }

    #[test]
    fn cancel_stops_the_queue_and_requeue_writes_off_held_chunks() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(12, 4, 1), 1);
        arrays
            .apply(
                &sync(
                    id,
                    vec![],
                    vec![(node("a"), IndexRangeSet::from_range(0..=0))],
                ),
                || 0,
            )
            .unwrap();
        arrays
            .apply(&TaskArrayWrite::Cancel { batch_id: id }, || 0)
            .unwrap();
        let state = &arrays.get(id).unwrap().state;
        assert_eq!(state.status(), TaskArrayStatus::Stopping);
        assert_eq!(state.summary().not_run, 8);

        let applied = arrays
            .apply(
                &TaskArrayWrite::Requeue {
                    batch_id: id,
                    node: node("a"),
                },
                || 0,
            )
            .unwrap();
        assert_eq!(
            applied,
            TaskArrayApplied::Requeued {
                chunks: IndexRangeSet::from_range(0..=0)
            }
        );
        let state = &arrays.get(id).unwrap().state;
        assert_eq!(state.status(), TaskArrayStatus::Cancelled);
        assert!(!arrays.has_active());
    }

    #[test]
    fn writes_and_the_store_round_trip_through_json() {
        let mut arrays = TaskArrays::default();
        let write = register(10, 4, 5);
        let json = serde_json::to_string(&write).unwrap();
        let back: TaskArrayWrite = serde_json::from_str(&json).unwrap();
        assert_eq!(back, write);
        registered(&mut arrays, &back, 3);
        let json = serde_json::to_string(&arrays).unwrap();
        let back: TaskArrays = serde_json::from_str(&json).unwrap();
        assert_eq!(back, arrays);
    }
}

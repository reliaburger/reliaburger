//! The leader's view of one task array: which chunks are queued, which
//! node holds which, and what the finished ones produced.
//!
//! Everything here is a pure, deterministic state machine. It's shaped to
//! live inside the Raft `DesiredState` (every operation takes its inputs
//! as arguments and never reads a clock). The replicated task-array store
//! applies these operations in committed log order.
//!
//! The state never holds a record per task. Chunks move between three
//! [`IndexRangeSet`]s (queued, granted per node, done), and a finished
//! chunk contributes only counts plus its failed indices.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::index_set::IndexRangeSet;
use super::task_array::{ChunkId, TaskArraySpec, TaskArraySpecError};
use super::types::NodeId;

/// Most failed-index ranges kept per array. Past this, failures are
/// counted in [`TaskArrayState::failed_overflow`] and the full list lives
/// only in the nodes' ledgers. 10,000 sparse failures is about 160 KiB of
/// JSON.
pub const MAX_FAILED_RANGES: usize = 10_000;
/// Bound sparse failures in each worker control report.
pub const MAX_CHUNK_FAILED_RANGES: usize = 256;

/// Fewest chunks the grant policy keeps queued on a node, so a node that
/// finishes one chunk always has the next one ready.
pub const MIN_GRANT_DEPTH: u64 = 2;
/// Cap extra prefetch learned from verified command durations. This queues work,
/// not resource reservations; the original two-slot-round floor still applies.
pub const MAX_LEARNED_GRANT_DEPTH: u64 = 16;
const LOOKAHEAD_MILLISECONDS: u128 = 2000;

/// Why an array stopped before running every task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// Someone cancelled it.
    Cancelled,
    /// More indices failed for good than `max_failed_indexes` allows.
    TooManyFailures,
}

/// Lifecycle of a task array, derived from its chunks and counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskArrayStatus {
    /// Chunks are queued or held by nodes.
    Running,
    /// Stopped (cancelled or failed); nodes are still returning chunks.
    Stopping,
    /// Every task succeeded.
    Succeeded,
    /// Every task ran; some indices failed, within the allowed number.
    CompletedWithFailures,
    /// Stopped because too many indices failed.
    Failed,
    /// Stopped by a cancel.
    Cancelled,
}

impl TaskArrayStatus {
    /// Whether nothing will change any more.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::Stopping)
    }
}

/// What a node reports when it has finished one chunk. Every task in the
/// chunk is accounted for exactly once: succeeded, failed or not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkResult {
    /// Mergeable final-attempt duration buckets: 1,2,4,...,16384 ms,+Inf.
    pub duration_counts: [u64; 16],
    /// The chunk.
    pub chunk: ChunkId,
    /// The grant attempt the node was running; fences stale reports.
    pub attempt: u64,
    /// Tasks that exited 0.
    pub succeeded: u32,
    /// Total failures, including those omitted from the bounded preview.
    pub failed_count: u32,
    /// A bounded preview of indices that failed on every attempt.
    pub failed_indices: IndexRangeSet,
    /// Tasks never started because the array stopped.
    pub not_run: u32,
    /// Extra attempts spent on retries across the chunk.
    pub retried: u32,
}

/// Outcome of a valid chunk completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOutcome {
    /// The chunk was retired and its counts added.
    Applied,
    /// The chunk was already done; nothing changed (a retried report).
    Duplicate,
}

/// Why an operation on a task array was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskArrayError {
    #[error("chunk {chunk} doesn't exist in this array")]
    UnknownChunk { chunk: u32 },
    #[error("chunk {chunk} isn't queued")]
    NotQueued { chunk: u32 },
    #[error("chunk {chunk} isn't held by node {node}")]
    NotHeld { chunk: u32, node: String },
    #[error(
        "chunk {chunk} report is for attempt {reported}, but the current grant is attempt {current}"
    )]
    StaleAttempt {
        chunk: u32,
        reported: u64,
        current: u64,
    },
    #[error("chunk {chunk} has {expected} tasks but the report accounts for {reported}")]
    CountMismatch {
        chunk: u32,
        expected: u64,
        reported: u64,
    },
    #[error("chunk {chunk} report names failed indices outside the chunk")]
    IndexOutsideChunk { chunk: u32 },
    #[error("chunk {chunk} report exceeds the failed-index preview bound")]
    FailurePreviewTooLarge { chunk: u32 },
    #[error("the array has stopped; no more chunks can be granted")]
    Stopped,
    #[error("execution owner must contain 1–128 bytes without control characters")]
    InvalidOwner,
}

/// Aggregate progress of one array, computed without touching tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArraySummary {
    /// Tasks in the array.
    pub total: u64,
    /// Tasks that succeeded.
    pub succeeded: u64,
    /// Indices that failed for good.
    pub failed: u64,
    /// Tasks that never ran because the array stopped.
    pub not_run: u64,
    /// Extra attempts spent on retries.
    pub retried: u64,
    /// Tasks in chunks not yet granted.
    pub queued: u64,
    /// Tasks in chunks nodes hold (running or about to).
    pub held: u64,
    /// Chunks retired.
    pub chunks_done: u64,
    /// Lifecycle status.
    pub status: TaskArrayStatus,
}

/// The leader's durable view of one task array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArrayState {
    /// Count and policy.
    pub spec: TaskArraySpec,
    /// Submission time from the request, never from `apply`'s clock.
    pub submitted_at_epoch_secs: u64,
    /// Chunk ids waiting for a node.
    queued: IndexRangeSet,
    /// Chunk ids each node holds.
    grants: BTreeMap<NodeId, IndexRangeSet>,
    /// Grant attempt per chunk, stored only once it's above 1.
    attempts: BTreeMap<u32, u64>,
    /// Chunk ids retired.
    done: IndexRangeSet,
    /// Accepted terminal grant per chunk; cancelled unstarted chunks have none.
    accepted: BTreeMap<NodeId, BTreeMap<u64, IndexRangeSet>>,
    /// Indices that failed for good, up to [`MAX_FAILED_RANGES`] ranges.
    failed_indices: IndexRangeSet,
    /// Failed indices not stored because the range cap was reached.
    pub failed_overflow: u64,
    succeeded: u64,
    failed: u64,
    not_run: u64,
    retried: u64,
    stopped: Option<StopReason>,
    duration_counts: [u64; 16],
}

fn valid_node_id(node: &NodeId) -> bool {
    !node.0.is_empty() && node.0.len() <= 128 && !node.0.chars().any(char::is_control)
}

impl TaskArrayState {
    /// Refuse inconsistent progress before a reopened snapshot can authorise work.
    pub(crate) fn validate_snapshot(&self) -> Result<(), String> {
        self.spec.validate().map_err(|error| error.to_string())?;
        let chunks = self.spec.chunk_count();
        let valid_ranges =
            |ranges: &IndexRangeSet, bound: u32| ranges.ranges().all(|range| *range.end() < bound);
        let mut partition = IndexRangeSet::new();
        let mut partition_size = 0u64;
        for ranges in std::iter::once(&self.queued)
            .chain(self.grants.values())
            .chain(std::iter::once(&self.done))
        {
            if !valid_ranges(ranges, chunks) {
                return Err("chunk progress exceeds the run".into());
            }
            partition_size = partition_size
                .checked_add(ranges.len())
                .ok_or("chunk partition overflow")?;
            partition.extend_from(ranges);
        }
        if partition_size != u64::from(chunks) || partition.len() != partition_size {
            return Err("chunk partition is incomplete or overlaps".into());
        }
        if self
            .grants
            .keys()
            .chain(self.accepted.keys())
            .any(|node| !valid_node_id(node))
        {
            return Err("invalid execution owner".into());
        }
        if self
            .attempts
            .iter()
            .any(|(chunk, attempt)| *chunk >= chunks || *attempt < 2 || self.done.contains(*chunk))
        {
            return Err("invalid grant attempt".into());
        }
        let mut accepted = IndexRangeSet::new();
        for grants in self.accepted.values() {
            for (attempt, ranges) in grants {
                if *attempt == 0
                    || ranges.is_empty()
                    || !ranges.ranges().all(|range| self.done.contains_range(range))
                {
                    return Err("accepted grant has no retired progress".into());
                }
                let expected = accepted.len() + ranges.len();
                accepted.extend_from(ranges);
                if accepted.len() != expected {
                    return Err("accepted grants overlap".into());
                }
            }
        }
        let accounted = self
            .succeeded
            .checked_add(self.failed)
            .and_then(|count| count.checked_add(self.not_run))
            .ok_or("task counter overflow")?;
        if accounted != self.tasks_in(&self.done)
            || self.failed_indices.range_count() > MAX_FAILED_RANGES
            || !valid_ranges(&self.failed_indices, self.spec.count)
            || self.failed_indices.len().checked_add(self.failed_overflow) != Some(self.failed)
        {
            return Err("task counters disagree with retired chunks".into());
        }
        if self
            .duration_counts
            .iter()
            .try_fold(0u64, |total, count| total.checked_add(*count))
            .is_none_or(|count| count > self.succeeded + self.failed)
        {
            return Err("duration counts exceed executed tasks".into());
        }
        if self.stopped.is_some() && !self.queued.is_empty() {
            return Err("stopped run retains queued work".into());
        }
        Ok(())
    }

    /// A new array with every chunk queued.
    pub fn new(
        spec: TaskArraySpec,
        submitted_at_epoch_secs: u64,
    ) -> Result<Self, TaskArraySpecError> {
        spec.validate()?;
        let queued = IndexRangeSet::from_range(0..=spec.chunk_count() - 1);
        Ok(Self {
            spec,
            submitted_at_epoch_secs,
            queued,
            grants: BTreeMap::new(),
            attempts: BTreeMap::new(),
            done: IndexRangeSet::new(),
            accepted: BTreeMap::new(),
            failed_indices: IndexRangeSet::new(),
            failed_overflow: 0,
            succeeded: 0,
            failed: 0,
            not_run: 0,
            retried: 0,
            stopped: None,
            duration_counts: [0; 16],
        })
    }

    /// Chunks not yet granted.
    pub fn queued(&self) -> &IndexRangeSet {
        &self.queued
    }

    /// Chunks retired.
    pub fn done(&self) -> &IndexRangeSet {
        &self.done
    }

    /// Grant whose terminal report was accepted for this chunk.
    pub fn accepted_grant(&self, chunk: ChunkId) -> Option<(NodeId, u64)> {
        self.accepted.iter().find_map(|(node, generations)| {
            generations
                .iter()
                .find(|(_, chunks)| chunks.contains(chunk.0))
                .map(|(grant, _)| (node.clone(), *grant))
        })
    }

    /// Mergeable terminal-attempt duration distribution for accepted chunks.
    pub fn duration_counts(&self) -> &[u64; 16] {
        &self.duration_counts
    }

    /// Chunks `node` holds, if any.
    pub fn held_by(&self, node: &NodeId) -> Option<&IndexRangeSet> {
        self.grants.get(node)
    }

    /// Nodes holding at least one chunk, in order.
    pub fn holders(&self) -> impl Iterator<Item = &NodeId> {
        self.grants.keys()
    }

    /// Indices recorded as failed (see [`Self::failed_overflow`] for the rest).
    pub fn failed_indices(&self) -> &IndexRangeSet {
        &self.failed_indices
    }

    /// Why the array stopped early, if it did.
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stopped
    }

    /// The grant attempt a chunk is on: 1 until a node loss re-queues it.
    pub fn attempt_of(&self, chunk: ChunkId) -> u64 {
        self.attempts.get(&chunk.0).copied().unwrap_or(1)
    }

    /// Hand `chunks` to `node`. Every chunk must be queued; nothing moves
    /// unless all of them are.
    pub fn grant(&mut self, node: &NodeId, chunks: &IndexRangeSet) -> Result<(), TaskArrayError> {
        if !valid_node_id(node) {
            return Err(TaskArrayError::InvalidOwner);
        }
        if self.stopped.is_some() {
            return Err(TaskArrayError::Stopped);
        }
        for range in chunks.ranges() {
            if !self.queued.contains_range(range.clone()) {
                let chunk = range
                    .clone()
                    .find(|c| !self.queued.contains(*c))
                    .unwrap_or(*range.start());
                return Err(self.not_queued_error(chunk));
            }
        }
        if chunks.is_empty() {
            return Ok(());
        }
        for range in chunks.ranges() {
            self.queued.remove_range(range);
        }
        self.grants
            .entry(node.clone())
            .or_default()
            .extend_from(chunks);
        Ok(())
    }

    fn not_queued_error(&self, chunk: u32) -> TaskArrayError {
        if chunk >= self.spec.chunk_count() {
            TaskArrayError::UnknownChunk { chunk }
        } else {
            TaskArrayError::NotQueued { chunk }
        }
    }

    /// Retire a chunk `node` finished. Only the node currently holding the
    /// chunk, at the current attempt, may retire it; a repeat report of a
    /// done chunk is an idempotent [`CompletionOutcome::Duplicate`].
    pub fn complete(
        &mut self,
        node: &NodeId,
        result: &ChunkResult,
    ) -> Result<CompletionOutcome, TaskArrayError> {
        let chunk = result.chunk.0;
        let range = self
            .spec
            .chunk_range(result.chunk)
            .ok_or(TaskArrayError::UnknownChunk { chunk })?;
        if self.done.contains(chunk) {
            return Ok(CompletionOutcome::Duplicate);
        }
        let held = self.grants.get(node).is_some_and(|set| set.contains(chunk));
        if !held {
            return Err(TaskArrayError::NotHeld {
                chunk,
                node: node.0.clone(),
            });
        }
        let current = self.attempt_of(result.chunk);
        if result.attempt != current {
            return Err(TaskArrayError::StaleAttempt {
                chunk,
                reported: result.attempt,
                current,
            });
        }
        if result.failed_indices.range_count() > MAX_CHUNK_FAILED_RANGES {
            return Err(TaskArrayError::FailurePreviewTooLarge { chunk });
        }
        let expected = u64::from(range.end() - range.start()) + 1;
        let failed = u64::from(result.failed_count);
        let reported = u64::from(result.succeeded) + failed + u64::from(result.not_run);
        if reported != expected || result.failed_indices.len() > failed {
            return Err(TaskArrayError::CountMismatch {
                chunk,
                expected,
                reported,
            });
        }
        if result
            .failed_indices
            .ranges()
            .any(|r| r.start() < range.start() || r.end() > range.end())
        {
            return Err(TaskArrayError::IndexOutsideChunk { chunk });
        }

        self.release(node, chunk);
        self.attempts.remove(&chunk);
        self.done.insert(chunk);
        self.accepted
            .entry(node.clone())
            .or_default()
            .entry(result.attempt)
            .or_default()
            .insert(chunk);
        for (total, count) in self.duration_counts.iter_mut().zip(result.duration_counts) {
            *total += count;
        }
        self.succeeded += u64::from(result.succeeded);
        self.failed += failed;
        self.not_run += u64::from(result.not_run);
        self.retried += u64::from(result.retried);
        self.record_failures(&result.failed_indices);
        self.failed_overflow += failed - result.failed_indices.len();
        if let Some(limit) = self.spec.max_failed_indexes
            && self.failed > u64::from(limit)
        {
            self.stop(StopReason::TooManyFailures);
        }
        Ok(CompletionOutcome::Applied)
    }

    fn release(&mut self, node: &NodeId, chunk: u32) {
        if let Some(set) = self.grants.get_mut(node) {
            set.remove(chunk);
            if set.is_empty() {
                self.grants.remove(node);
            }
        }
    }

    fn record_failures(&mut self, failures: &IndexRangeSet) {
        for range in failures.ranges() {
            if self.failed_indices.range_count() >= MAX_FAILED_RANGES {
                self.failed_overflow += u64::from(range.end() - range.start()) + 1;
            } else {
                self.failed_indices.insert_range(range);
            }
        }
    }

    /// Take back every chunk `node` holds (the node is gone). They return
    /// to the queue at the next attempt, so a late report from the lost
    /// node is fenced off. If the array has stopped they aren't re-run;
    /// their tasks count as not run. Returns the chunks taken back.
    pub fn requeue_node(&mut self, node: &NodeId) -> IndexRangeSet {
        let Some(chunks) = self.grants.remove(node) else {
            return IndexRangeSet::new();
        };
        if self.stopped.is_some() {
            self.not_run += self.tasks_in(&chunks);
            for chunk in chunks.iter() {
                self.attempts.remove(&chunk);
            }
            self.done.extend_from(&chunks);
        } else {
            for chunk in chunks.iter() {
                let next = self.attempt_of(ChunkId(chunk)).saturating_add(1);
                self.attempts.insert(chunk, next);
            }
            self.queued.extend_from(&chunks);
        }
        chunks
    }

    /// Stop the array: no more grants, queued chunks count as not run.
    /// Chunks nodes already hold drain as they report. Cancelling a
    /// stopped array keeps the first reason.
    pub fn cancel(&mut self) {
        self.stop(StopReason::Cancelled);
    }

    fn stop(&mut self, reason: StopReason) {
        if self.stopped.is_some() {
            return;
        }
        self.stopped = Some(reason);
        let queued = std::mem::take(&mut self.queued);
        self.not_run += self.tasks_in(&queued);
        self.done.extend_from(&queued);
    }

    fn tasks_in_chunk(&self, chunk: u32) -> u64 {
        self.spec
            .chunk_range(ChunkId(chunk))
            .map_or(0, |r| u64::from(r.end() - r.start()) + 1)
    }

    /// Number of tasks in a set of chunks, without iterating tasks: every
    /// chunk is full except possibly the last one.
    pub fn tasks_in(&self, chunks: &IndexRangeSet) -> u64 {
        let last_chunk = self.spec.chunk_count().saturating_sub(1);
        let mut tasks = chunks.len() * u64::from(self.spec.chunk_size);
        if chunks.contains(last_chunk) {
            tasks -= u64::from(self.spec.chunk_size) - self.tasks_in_chunk(last_chunk);
        }
        tasks
    }

    /// Current lifecycle status.
    pub fn status(&self) -> TaskArrayStatus {
        let drained = self.queued.is_empty() && self.grants.is_empty();
        match (self.stopped, drained) {
            (Some(_), false) => TaskArrayStatus::Stopping,
            (Some(StopReason::Cancelled), true) => TaskArrayStatus::Cancelled,
            (Some(StopReason::TooManyFailures), true) => TaskArrayStatus::Failed,
            (None, false) => TaskArrayStatus::Running,
            (None, true) if self.failed == 0 => TaskArrayStatus::Succeeded,
            (None, true) => TaskArrayStatus::CompletedWithFailures,
        }
    }

    /// Progress counts, computed from the chunk sets.
    pub fn summary(&self) -> TaskArraySummary {
        let held = self.grants.values().map(|set| self.tasks_in(set)).sum();
        TaskArraySummary {
            total: u64::from(self.spec.count),
            succeeded: self.succeeded,
            failed: self.failed,
            not_run: self.not_run,
            retried: self.retried,
            queued: self.tasks_in(&self.queued),
            held,
            chunks_done: self.done.len(),
            status: self.status(),
        }
    }
}

/// One node's capacity for an array, as the grant policy sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSlots {
    /// The node.
    pub node: NodeId,
    /// Tasks of this array the node can run at once; zero means it can't
    /// take any (draining, unready, or not able to run host processes).
    pub slots: u32,
}

/// How many chunks a node with `slots` slots should hold: enough for two
/// full rounds of its slots, and never fewer than [`MIN_GRANT_DEPTH`].
pub fn grant_depth(slots: u32, chunk_size: u32) -> u64 {
    let wanted = (2 * u64::from(slots)).div_ceil(u64::from(chunk_size.max(1)));
    wanted.max(MIN_GRANT_DEPTH)
}

impl TaskArrayState {
    fn observed_grant_depth(&self, slots: u32) -> u64 {
        let baseline = grant_depth(slots, self.spec.chunk_size);
        // The overflow bucket has no finite upper bound. Unknown/slow work
        // retains the small window rather than inventing a throughput estimate.
        if self.duration_counts[15] != 0 {
            return baseline;
        }
        let mut samples = 0u128;
        let mut milliseconds = 0u128;
        for (bucket, count) in self.duration_counts[..15].iter().enumerate() {
            samples += u128::from(*count);
            milliseconds += u128::from(*count) * (1u128 << bucket);
        }
        if milliseconds == 0 {
            return baseline;
        }
        // u32 slots, sixteen u64 counts and these fixed bounds fit in u128.
        // Use upper bucket bounds: estimates stay conservative. Two seconds
        // cover receipt acceptance and delivery of the committed next grant.
        let tasks = u128::from(slots) * LOOKAHEAD_MILLISECONDS * samples / milliseconds;
        let chunks = tasks.div_ceil(u128::from(self.spec.chunk_size.max(1)));
        baseline.max(chunks.min(u128::from(MAX_LEARNED_GRANT_DEPTH)) as u64)
    }
}

/// Decide which queued chunks to hand to which node. Each node is topped
/// up to its baseline [`grant_depth`] or learned bounded lookahead, the
/// emptiest nodes first (ties by name), and
/// always with the lowest queued chunk ids. It's pull-shaped load
/// balancing: a fast node empties its chunks sooner and gets more, so
/// there's no up-front split to go wrong. The state isn't changed; the
/// caller applies the plan with [`TaskArrayState::grant`] (through Raft,
/// once wired).
pub fn plan_grants(state: &TaskArrayState, nodes: &[NodeSlots]) -> Vec<(NodeId, IndexRangeSet)> {
    if state.stop_reason().is_some() {
        return Vec::new();
    }
    let held = |node: &NodeId| state.held_by(node).map_or(0, IndexRangeSet::len);
    let mut order: Vec<&NodeSlots> = Vec::with_capacity(nodes.len());
    for candidate in nodes {
        if candidate.slots > 0 && !order.iter().any(|n| n.node == candidate.node) {
            order.push(candidate);
        }
    }
    order.sort_by(|a, b| held(&a.node).cmp(&held(&b.node)).then(a.node.cmp(&b.node)));

    let mut queue = state.queued().clone();
    let mut plan = Vec::new();
    for candidate in order {
        if queue.is_empty() {
            break;
        }
        let depth = state.observed_grant_depth(candidate.slots);
        let wanted = depth.saturating_sub(held(&candidate.node));
        let chunks = queue.take_first(wanted);
        if !chunks.is_empty() {
            plan.push((candidate.node.clone(), chunks));
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(entries: &[(&str, u32)]) -> Vec<NodeSlots> {
        entries
            .iter()
            .map(|(name, slots)| NodeSlots {
                node: node(name),
                slots: *slots,
            })
            .collect()
    }

    fn apply(state: &mut TaskArrayState, plan: &[(NodeId, IndexRangeSet)]) {
        for (holder, granted) in plan {
            state.grant(holder, granted).unwrap();
        }
    }

    #[test]
    fn depth_covers_two_rounds_of_slots() {
        assert_eq!(grant_depth(1, 1024), MIN_GRANT_DEPTH);
        assert_eq!(grant_depth(512, 1024), MIN_GRANT_DEPTH);
        assert_eq!(grant_depth(2048, 1024), 4);
        assert_eq!(grant_depth(10, 3), 7);
    }

    #[test]
    fn fast_receipts_keep_a_bounded_report_round_of_work_ready() {
        let mut state = TaskArrayState::new(spec(100_000, 1000), 1).unwrap();
        state.grant(&node("a"), &chunks(0..=1)).unwrap();
        for chunk in 0..2 {
            let mut result = all_ok(&state, chunk);
            result.duration_counts[0] = 1000;
            state.complete(&node("a"), &result).unwrap();
        }
        let plan = plan_grants(&state, &slots(&[("a", 27), ("blocked", 0)]));
        assert_eq!(plan, vec![(node("a"), chunks(2..=17))]);
        apply(&mut state, &plan);
        assert!(plan_grants(&state, &slots(&[("a", 27)])).is_empty());
    }

    #[test]
    fn slow_or_overflow_receipts_keep_the_original_small_grant_window() {
        for bucket in [10, 15] {
            let mut state = TaskArrayState::new(spec(100_000, 1000), 1).unwrap();
            state.grant(&node("a"), &chunks(0..=0)).unwrap();
            let mut result = all_ok(&state, 0);
            result.duration_counts[bucket] = 1000;
            state.complete(&node("a"), &result).unwrap();
            assert_eq!(
                plan_grants(&state, &slots(&[("a", 27)])),
                vec![(node("a"), chunks(1..=2))]
            );
        }
    }

    #[test]
    fn plan_tops_nodes_up_with_the_lowest_chunks() {
        let mut state = TaskArrayState::new(spec(10_000, 100), 1).unwrap();
        let plan = plan_grants(&state, &slots(&[("n2", 8), ("n1", 8)]));
        assert_eq!(
            plan,
            vec![(node("n1"), chunks(0..=1)), (node("n2"), chunks(2..=3))]
        );
        apply(&mut state, &plan);
        // Already at depth: nothing more until a chunk finishes.
        assert!(plan_grants(&state, &slots(&[("n1", 8), ("n2", 8)])).is_empty());
        let finished = all_ok(&state, 2);
        state.complete(&node("n2"), &finished).unwrap();
        assert_eq!(
            plan_grants(&state, &slots(&[("n1", 8), ("n2", 8)])),
            vec![(node("n2"), chunks(4..=4))]
        );
    }

    #[test]
    fn nodes_without_slots_get_nothing() {
        let state = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        let plan = plan_grants(&state, &slots(&[("n1", 0), ("n2", 4)]));
        assert_eq!(plan, vec![(node("n2"), chunks(0..=1))]);
    }

    #[test]
    fn a_node_listed_twice_is_planned_once() {
        let state = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        let plan = plan_grants(&state, &slots(&[("n1", 4), ("n1", 4)]));
        assert_eq!(plan, vec![(node("n1"), chunks(0..=1))]);
    }

    #[test]
    fn an_empty_queue_or_a_stopped_array_plans_nothing() {
        let mut state = TaskArrayState::new(spec(200, 100), 1).unwrap();
        let plan = plan_grants(&state, &slots(&[("n1", 4)]));
        apply(&mut state, &plan);
        assert!(plan_grants(&state, &slots(&[("n2", 4)])).is_empty());

        let mut cancelled = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        cancelled.cancel();
        assert!(plan_grants(&cancelled, &slots(&[("n1", 4)])).is_empty());
    }

    #[test]
    fn the_emptiest_node_is_served_first_when_chunks_run_short() {
        let mut state = TaskArrayState::new(spec(400, 100), 1).unwrap();
        state.grant(&node("a"), &chunks(0..=0)).unwrap();
        let plan = plan_grants(&state, &slots(&[("a", 4), ("b", 4)]));
        assert_eq!(
            plan,
            vec![(node("b"), chunks(1..=2)), (node("a"), chunks(3..=3))]
        );
    }

    #[test]
    fn the_same_input_plans_the_same_grants() {
        let state = TaskArrayState::new(spec(100_000, 100), 1).unwrap();
        let nodes = slots(&[("n3", 300), ("n1", 50), ("n2", 1000)]);
        let reference = plan_grants(&state, &nodes);
        for _ in 0..10 {
            assert_eq!(plan_grants(&state, &nodes), reference);
        }
    }

    /// Every chunk is in exactly one of queued, held or done.
    fn assert_partitioned(state: &TaskArrayState) {
        let mut seen = IndexRangeSet::new();
        let mut total = 0;
        let sets = std::iter::once(state.queued())
            .chain(state.holders().filter_map(|n| state.held_by(n)))
            .chain(std::iter::once(state.done()));
        for set in sets {
            total += set.len();
            seen.extend_from(set);
        }
        let chunk_count = state.spec.chunk_count();
        assert_eq!(total, u64::from(chunk_count), "a chunk is in two places");
        assert_eq!(seen, IndexRangeSet::from_range(0..=chunk_count - 1));
    }

    #[derive(Debug, Clone)]
    enum Step {
        Plan,
        Complete { node: usize, pick: usize },
        Lose { node: usize },
        Cancel,
    }

    fn step() -> impl proptest::strategy::Strategy<Value = Step> {
        use proptest::prelude::*;
        prop_oneof![
            4 => Just(Step::Plan),
            6 => (0usize..3, 0usize..8).prop_map(|(node, pick)| Step::Complete { node, pick }),
            1 => (0usize..3).prop_map(|node| Step::Lose { node }),
            1 => Just(Step::Cancel),
        ]
    }

    proptest::proptest! {
        #[test]
        fn chunks_are_never_lost_or_duplicated(
            steps in proptest::collection::vec(step(), 1..80),
            count in 1u32..3000,
            chunk_size in 1u32..200,
        ) {
            let Ok(mut state) = TaskArrayState::new(spec(count, chunk_size), 1) else {
                return Ok(());
            };
            let names = ["n1", "n2", "n3"];
            let nodes = slots(&[("n1", 3), ("n2", 200), ("n3", 1)]);
            for step in steps {
                match step {
                    Step::Plan => {
                        let plan = plan_grants(&state, &nodes);
                        apply(&mut state, &plan);
                    }
                    Step::Complete { node: which, pick } => {
                        let holder = node(names[which]);
                        let held: Vec<u32> = state
                            .held_by(&holder)
                            .map(|set| set.iter().collect())
                            .unwrap_or_default();
                        if let Some(&chunk) = held.get(pick % held.len().max(1)) {
                            let result = all_ok(&state, chunk);
                            proptest::prop_assert_eq!(
                                state.complete(&holder, &result),
                                Ok(CompletionOutcome::Applied)
                            );
                        }
                    }
                    Step::Lose { node: which } => {
                        state.requeue_node(&node(names[which]));
                    }
                    Step::Cancel => state.cancel(),
                }
                assert_partitioned(&state);
                let summary = state.summary();
                proptest::prop_assert_eq!(
                    summary.succeeded + summary.failed + summary.not_run + summary.queued + summary.held,
                    summary.total
                );
            }
        }
    }

    /// The million-task control-plane budget: run a whole 1M array with
    /// 1% of indices failing for good on three nodes, one simulated sync
    /// round at a time, and check what the leader ends up storing and how
    /// many Raft entries it would have written.
    #[test]
    fn a_million_task_array_fits_the_control_plane_budget() {
        let mut state = TaskArrayState::new(TaskArraySpec::with_count(1_000_000), 1).unwrap();
        let nodes = slots(&[("n1", 16), ("n2", 16), ("n3", 16)]);
        let mut rounds = 0u64;
        while !state.status().is_terminal() {
            rounds += 1;
            // One sync round, which the leader writes as one Raft entry:
            // every node reports what it finished, then gets topped up.
            let holders: Vec<NodeId> = state.holders().cloned().collect();
            for holder in holders {
                let held: Vec<u32> = state.held_by(&holder).unwrap().iter().collect();
                for chunk in held {
                    let range = state.spec.chunk_range(ChunkId(chunk)).unwrap();
                    let mut failed = IndexRangeSet::new();
                    for index in range.clone().filter(|i| i % 100 == 42) {
                        failed.insert(index);
                    }
                    let tasks = range.end() - range.start() + 1;
                    let result = ChunkResult {
                        duration_counts: [0; 16],
                        chunk: ChunkId(chunk),
                        attempt: 1,
                        succeeded: tasks - failed.len() as u32,
                        failed_count: failed.len() as u32,
                        failed_indices: failed,
                        not_run: 0,
                        retried: 0,
                    };
                    state.complete(&holder, &result).unwrap();
                }
            }
            let plan = plan_grants(&state, &nodes);
            apply(&mut state, &plan);
            assert!(rounds < 1_000, "the array must finish");
        }
        let summary = state.summary();
        assert_eq!(summary.status, TaskArrayStatus::CompletedWithFailures);
        assert_eq!(summary.failed, 10_000);
        assert_eq!(summary.succeeded, 990_000);
        // 977 chunks, three nodes taking two at a time: about 163 rounds,
        // however many tasks each chunk holds.
        let per_round = 3 * MIN_GRANT_DEPTH;
        assert!(rounds <= 977u64.div_ceil(per_round) + 2, "{rounds} rounds");
        let bytes = serde_json::to_vec(&state).unwrap().len();
        assert!(bytes <= 256 * 1024, "state is {bytes} bytes");
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    fn spec(count: u32, chunk_size: u32) -> TaskArraySpec {
        TaskArraySpec {
            chunk_size,
            ..TaskArraySpec::with_count(count)
        }
    }

    fn chunks(range: std::ops::RangeInclusive<u32>) -> IndexRangeSet {
        IndexRangeSet::from_range(range)
    }

    fn all_ok(state: &TaskArrayState, chunk: u32) -> ChunkResult {
        let range = state.spec.chunk_range(ChunkId(chunk)).unwrap();
        ChunkResult {
            duration_counts: [0; 16],
            chunk: ChunkId(chunk),
            attempt: state.attempt_of(ChunkId(chunk)),
            succeeded: range.end() - range.start() + 1,
            failed_count: 0,
            failed_indices: IndexRangeSet::new(),
            not_run: 0,
            retried: 0,
        }
    }

    #[test]
    fn a_new_array_queues_every_chunk() {
        let state = TaskArrayState::new(spec(10_000, 1000), 1).unwrap();
        assert_eq!(state.queued(), &chunks(0..=9));
        let summary = state.summary();
        assert_eq!(summary.total, 10_000);
        assert_eq!(summary.queued, 10_000);
        assert_eq!(summary.status, TaskArrayStatus::Running);
    }

    #[test]
    fn an_invalid_spec_is_refused() {
        assert!(TaskArrayState::new(spec(0, 10), 1).is_err());
    }

    #[test]
    fn grant_moves_chunks_to_the_node() {
        let mut state = TaskArrayState::new(spec(10_000, 1000), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=2)).unwrap();
        assert_eq!(state.held_by(&node("n1")), Some(&chunks(0..=2)));
        assert_eq!(state.queued(), &chunks(3..=9));
        assert_eq!(state.summary().held, 3000);
    }

    #[test]
    fn grant_refuses_chunks_that_arent_queued_and_moves_nothing() {
        let mut state = TaskArrayState::new(spec(10_000, 1000), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=1)).unwrap();
        let before = state.clone();
        assert_eq!(
            state.grant(&node("n2"), &chunks(1..=3)),
            Err(TaskArrayError::NotQueued { chunk: 1 })
        );
        assert_eq!(
            state.grant(&node("n2"), &chunks(9..=10)),
            Err(TaskArrayError::UnknownChunk { chunk: 10 })
        );
        assert_eq!(state, before);
    }

    #[test]
    fn completion_retires_the_chunk_and_adds_the_counts() {
        let mut state = TaskArrayState::new(spec(2500, 1000), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=2)).unwrap();
        let mut result = all_ok(&state, 2);
        result.succeeded = 498;
        result.failed_count = 2;
        result.failed_indices = IndexRangeSet::from_range(2042..=2043);
        result.retried = 7;
        assert_eq!(
            state.complete(&node("n1"), &result),
            Ok(CompletionOutcome::Applied)
        );
        let summary = state.summary();
        assert_eq!(summary.succeeded, 498);
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.retried, 7);
        assert_eq!(summary.chunks_done, 1);
        assert_eq!(summary.held, 2000);
        assert_eq!(state.failed_indices(), &chunks(2042..=2043));
    }

    #[test]
    fn a_repeated_completion_is_a_duplicate() {
        let mut state = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=0)).unwrap();
        let result = all_ok(&state, 0);
        state.complete(&node("n1"), &result).unwrap();
        let after = state.clone();
        assert_eq!(
            state.complete(&node("n1"), &result),
            Ok(CompletionOutcome::Duplicate)
        );
        assert_eq!(state, after);
    }

    #[test]
    fn completion_from_a_node_that_doesnt_hold_the_chunk_is_refused() {
        let mut state = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=0)).unwrap();
        let result = all_ok(&state, 0);
        assert!(matches!(
            state.complete(&node("n2"), &result),
            Err(TaskArrayError::NotHeld { chunk: 0, .. })
        ));
        let queued = all_ok(&state, 5);
        assert!(matches!(
            state.complete(&node("n1"), &queued),
            Err(TaskArrayError::NotHeld { chunk: 5, .. })
        ));
    }

    #[test]
    fn a_report_that_miscounts_the_chunk_is_refused() {
        let mut state = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=0)).unwrap();
        let mut result = all_ok(&state, 0);
        result.succeeded = 99;
        assert_eq!(
            state.complete(&node("n1"), &result),
            Err(TaskArrayError::CountMismatch {
                chunk: 0,
                expected: 100,
                reported: 99
            })
        );
        result.failed_count = 1;
        result.failed_indices = IndexRangeSet::from_range(100..=100);
        assert_eq!(
            state.complete(&node("n1"), &result),
            Err(TaskArrayError::IndexOutsideChunk { chunk: 0 })
        );
    }

    #[test]
    fn requeue_returns_unfinished_chunks_at_the_next_attempt() {
        let mut state = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=2)).unwrap();
        let done = all_ok(&state, 0);
        state.complete(&node("n1"), &done).unwrap();
        let stale = all_ok(&state, 1);

        let taken = state.requeue_node(&node("n1"));
        assert_eq!(taken, chunks(1..=2));
        assert_eq!(state.queued(), &chunks(1..=9));
        assert_eq!(state.attempt_of(ChunkId(1)), 2);
        assert_eq!(state.attempt_of(ChunkId(3)), 1);
        assert!(state.held_by(&node("n1")).is_none());

        // The lost node comes back with a report for its old grant.
        state.grant(&node("n2"), &chunks(1..=1)).unwrap();
        assert!(matches!(
            state.complete(&node("n1"), &stale),
            Err(TaskArrayError::NotHeld { .. })
        ));
        assert_eq!(
            state.complete(&node("n2"), &stale),
            Err(TaskArrayError::StaleAttempt {
                chunk: 1,
                reported: 1,
                current: 2
            })
        );
        let fresh = all_ok(&state, 1);
        assert_eq!(fresh.attempt, 2);
        assert_eq!(
            state.complete(&node("n2"), &fresh),
            Ok(CompletionOutcome::Applied)
        );
    }

    #[test]
    fn requeue_of_an_unknown_node_changes_nothing() {
        let mut state = TaskArrayState::new(spec(1000, 100), 1).unwrap();
        let before = state.clone();
        assert!(state.requeue_node(&node("ghost")).is_empty());
        assert_eq!(state, before);
    }

    #[test]
    fn cancel_empties_the_queue_and_drains_held_chunks() {
        let mut state = TaskArrayState::new(spec(1050, 100), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=1)).unwrap();
        state.cancel();
        assert!(state.queued().is_empty());
        assert_eq!(state.status(), TaskArrayStatus::Stopping);
        // Chunks 2..=10 never ran: 8 full chunks plus the 50-task tail.
        assert_eq!(state.summary().not_run, 850);
        assert_eq!(
            state.grant(&node("n2"), &chunks(2..=2)),
            Err(TaskArrayError::Stopped)
        );

        let mut partial = all_ok(&state, 0);
        partial.succeeded = 40;
        partial.not_run = 60;
        state.complete(&node("n1"), &partial).unwrap();
        // The node holding chunk 1 is lost after the cancel: not re-run.
        state.requeue_node(&node("n1"));
        let summary = state.summary();
        assert_eq!(summary.status, TaskArrayStatus::Cancelled);
        assert_eq!(summary.succeeded + summary.failed + summary.not_run, 1050);
        assert!(state.queued().is_empty());
    }

    #[test]
    fn too_many_failures_stop_the_array() {
        let mut limited = spec(1000, 100);
        limited.max_failed_indexes = Some(1);
        let mut state = TaskArrayState::new(limited, 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=0)).unwrap();
        let mut result = all_ok(&state, 0);
        result.succeeded = 98;
        result.failed_count = 2;
        result.failed_indices = IndexRangeSet::from_range(10..=11);
        state.complete(&node("n1"), &result).unwrap();
        assert_eq!(state.stop_reason(), Some(StopReason::TooManyFailures));
        assert_eq!(state.status(), TaskArrayStatus::Failed);
        assert_eq!(state.summary().not_run, 900);
    }

    #[test]
    fn terminal_status_reflects_failures() {
        let mut state = TaskArrayState::new(spec(200, 100), 1).unwrap();
        state.grant(&node("n1"), &chunks(0..=1)).unwrap();
        let ok = all_ok(&state, 0);
        state.complete(&node("n1"), &ok).unwrap();
        let mut one_failed = all_ok(&state, 1);
        one_failed.succeeded = 99;
        one_failed.failed_count = 1;
        one_failed.failed_indices = IndexRangeSet::from_range(142..=142);
        state.complete(&node("n1"), &one_failed).unwrap();
        assert_eq!(state.status(), TaskArrayStatus::CompletedWithFailures);
        assert!(state.status().is_terminal());

        let mut clean = TaskArrayState::new(spec(100, 100), 1).unwrap();
        clean.grant(&node("n1"), &chunks(0..=0)).unwrap();
        let ok = all_ok(&clean, 0);
        clean.complete(&node("n1"), &ok).unwrap();
        assert_eq!(clean.status(), TaskArrayStatus::Succeeded);
    }

    #[test]
    fn failed_indices_past_the_range_cap_are_counted_not_stored() {
        let count = (MAX_FAILED_RANGES as u32 + 10) * 2;
        let mut state = TaskArrayState::new(spec(count, 512), 1).unwrap();
        for chunk in 0..state.spec.chunk_count() {
            state.grant(&node("n1"), &chunks(chunk..=chunk)).unwrap();
            let range = state.spec.chunk_range(ChunkId(chunk)).unwrap();
            let tasks = range.end() - range.start() + 1;
            let mut failures = IndexRangeSet::new();
            for i in range.step_by(2) {
                failures.insert(i);
            }
            let result = ChunkResult {
                duration_counts: [0; 16],
                chunk: ChunkId(chunk),
                attempt: 1,
                succeeded: tasks - failures.len() as u32,
                failed_count: failures.len() as u32,
                failed_indices: failures,
                not_run: 0,
                retried: 0,
            };
            state.complete(&node("n1"), &result).unwrap();
        }
        assert_eq!(state.failed_indices().range_count(), MAX_FAILED_RANGES);
        assert_eq!(state.failed_overflow, 10);
        assert_eq!(state.summary().failed, u64::from(count / 2));
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = TaskArrayState::new(spec(1000, 100), 42).unwrap();
        state.grant(&node("n1"), &chunks(0..=3)).unwrap();
        state.requeue_node(&node("n1"));
        let json = serde_json::to_string(&state).unwrap();
        let back: TaskArrayState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }
}

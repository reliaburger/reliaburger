//! The node's side of task arrays: run the chunks the leader granted,
//! remember what finished, and answer the leader's sync.
//!
//! The leader calls [`TaskArrayNode::sync`] about once a second (through
//! `POST /v1/batch/array/sync`, or directly when the leader is this node).
//! The request says which chunks this node holds, at which grant attempt;
//! the node starts the ones it isn't running, stops the ones it no longer
//! holds, and answers with the chunks it finished, its free slots and its
//! counters. Everything the node reports is derived from what the leader
//! sent, so a restarted leader or a restarted node converge on the next
//! sync without any extra handshake.
//!
//! Each array gets a directory under `<data>/task-arrays/<batch id>/` with
//! the ledger and the captured output of failed tasks. The node keeps it
//! while the cluster still lists the array, so `relish batch results` and
//! `relish batch logs` work after the array finishes, and deletes it once
//! the leader stops listing the array.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::task_executor::{
    Attempt, CapturedOutput, ChunkWork, FakeRunner, PoolConfig, ProcessRunner, TaskFinal,
    TaskInvocation, TaskPool, TaskRecord, TaskRunner,
};
use super::task_ledger::{self, GroupCommit, Ledger, LedgerError, LedgerHandle};
use crate::config::process_workloads::ProcessWorkloadsConfig;
use crate::meat::task_array::{ChunkId, TaskArraySpec};
use crate::meat::task_array_state::ChunkResult;

/// Name of the per-node directory holding every array's files.
pub const TASK_ARRAYS_DIR: &str = "task-arrays";

/// Most result rows one request returns.
pub const MAX_RESULT_ROWS: usize = 1_000_000;

/// One chunk the leader says this node holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldChunk {
    /// The chunk.
    pub chunk: ChunkId,
    /// Its grant attempt; a result is only good for this attempt.
    pub attempt: u8,
}

/// What the leader tells a node about one running array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrayAssignment {
    /// The array's batch id.
    pub batch_id: u64,
    /// Count and policy.
    pub spec: TaskArraySpec,
    /// Host binary every task runs.
    pub program: PathBuf,
    /// Argument template, with `{index}` placeholders.
    pub args: Vec<String>,
    /// Plain environment variables from the template.
    pub env: Vec<(String, String)>,
    /// Chunks this node holds.
    pub held: Vec<HeldChunk>,
    /// The array has stopped: finish nothing new, report what's held.
    pub stopping: bool,
}

/// The leader's sync call to one node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSyncRequest {
    /// Every array the cluster still keeps. A node deletes the files of
    /// any array not listed here.
    pub known: Vec<u64>,
    /// The running arrays, with this node's share of each.
    pub arrays: Vec<ArrayAssignment>,
}

/// Counters for one array on one node, since the node started it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeArrayCounters {
    /// Attempts running now.
    pub running: u64,
    /// Attempts started.
    pub attempts_started: u64,
    /// Tasks that succeeded.
    pub succeeded: u64,
    /// Tasks that failed for good.
    pub failed: u64,
    /// Retries.
    pub retried: u64,
}

/// A node's answer about one array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrayProgress {
    /// The array.
    pub batch_id: u64,
    /// Tasks of this array the node can run at once; zero when it can't
    /// run the array at all.
    pub slots: u32,
    /// Why `slots` is zero, if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// Chunks finished at the attempt the leader asked for, not yet
    /// retired. Sent on every sync until the leader stops listing them.
    pub finished: Vec<ChunkResult>,
    /// Live counters.
    pub counters: NodeArrayCounters,
}

/// A node's answer to one sync call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSyncResponse {
    /// One entry per assignment in the request, in the same order.
    pub arrays: Vec<ArrayProgress>,
}

/// One task's durable outcome, as `relish batch results` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResultRow {
    /// The task's index.
    pub index: u32,
    /// Attempts made.
    pub attempts: u8,
    /// Whether it succeeded.
    pub succeeded: bool,
    /// The last attempt's exit code (negative for a signal), if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Wall time of the last attempt.
    pub run_ms: u32,
}

/// Why a results or logs read failed.
#[derive(Debug, thiserror::Error)]
pub enum TaskArrayNodeError {
    #[error("this node holds no files for task array {batch_id}")]
    UnknownArray { batch_id: u64 },
    #[error("this node kept no output for task {index}")]
    NoOutput { index: u32 },
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("task array file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Runs task attempts: real processes on a node, a fake in tests.
///
/// An enum rather than a generic parameter so the API state can hold one
/// concrete node type whichever runner it was built with.
pub enum NodeRunner {
    /// Spawn host processes.
    Process(ProcessRunner),
    /// Compute outcomes without processes (tests and benchmarks).
    Fake(FakeRunner),
}

impl TaskRunner for NodeRunner {
    async fn run(
        &self,
        task: &TaskInvocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Attempt {
        match self {
            Self::Process(runner) => runner.run(task, timeout, cancel).await,
            Self::Fake(runner) => runner.run(task, timeout, cancel).await,
        }
    }
}

/// How a node runs task arrays.
pub struct TaskArrayNodeConfig {
    /// Where array directories live (`<data>/task-arrays`).
    pub root: PathBuf,
    /// The node's host-binary policy; a task array's binary must be on
    /// its allowlist.
    pub policy: ProcessWorkloadsConfig,
    /// Slots per array when the array doesn't cap it (the CPU count).
    pub default_concurrency: u32,
    /// Retry backoff base and cap.
    pub backoff: (Duration, Duration),
    /// Ledger group-commit policy.
    pub group_commit: GroupCommit,
}

impl TaskArrayNodeConfig {
    /// The production settings under `data_dir`.
    pub fn for_data_dir(data_dir: &Path, policy: ProcessWorkloadsConfig) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        let defaults = PoolConfig::with_concurrency(1);
        Self {
            root: data_dir.join(TASK_ARRAYS_DIR),
            policy,
            default_concurrency: u32::try_from(cpus).unwrap_or(u32::MAX),
            backoff: (defaults.backoff_base, defaults.backoff_cap),
            group_commit: GroupCommit::default(),
        }
    }
}

/// One array this node is running.
struct ArrayRun {
    pool: Arc<TaskPool<NodeRunner>>,
    ledger: LedgerHandle,
    cancel: CancellationToken,
    /// Chunks started, by chunk id: the attempt and its cancel token.
    chunks: HashMap<u32, (u8, CancellationToken)>,
    /// Results of finished chunks, by chunk id and attempt.
    finished: Arc<Mutex<HashMap<(u32, u8), ChunkResult>>>,
    /// Records read back from an earlier run's ledger, by chunk, used
    /// once when that chunk starts again.
    resumed: HashMap<u32, Vec<TaskRecord>>,
}

/// The node's task-array executor.
pub struct TaskArrayNode {
    config: TaskArrayNodeConfig,
    runner: Arc<NodeRunner>,
    arrays: Mutex<HashMap<u64, ArrayRun>>,
}

impl TaskArrayNode {
    /// An executor running attempts through `runner`.
    pub fn new(config: TaskArrayNodeConfig, runner: NodeRunner) -> Self {
        Self {
            config,
            runner: Arc::new(runner),
            arrays: Mutex::new(HashMap::new()),
        }
    }

    /// Where this node keeps one array's files.
    pub fn array_dir(&self, batch_id: u64) -> PathBuf {
        self.config.root.join(batch_id.to_string())
    }

    /// Apply the leader's view and report progress. Never fails as a
    /// whole: an array this node can't run is answered with zero slots
    /// and the reason.
    pub async fn sync(&self, request: &NodeSyncRequest) -> NodeSyncResponse {
        let mut arrays = self.arrays.lock().await;
        let running: Vec<u64> = request.arrays.iter().map(|a| a.batch_id).collect();
        let finished_ids: Vec<u64> = arrays
            .keys()
            .copied()
            .filter(|id| !running.contains(id))
            .collect();
        for id in finished_ids {
            // Finished or forgotten: stop anything still going. The
            // directory stays while the cluster lists the array.
            if let Some(run) = arrays.remove(&id) {
                run.cancel.cancel();
            }
        }
        self.delete_unknown(&request.known).await;

        let mut response = NodeSyncResponse::default();
        for assignment in &request.arrays {
            if let Err(reason) = self.admit(assignment) {
                response.arrays.push(refused(assignment.batch_id, reason));
                continue;
            }
            let run = match arrays.entry(assignment.batch_id) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => match self.open(assignment).await {
                    Ok(run) => entry.insert(run),
                    Err(error) => {
                        let reason = format!("can't open the task ledger: {error}");
                        response.arrays.push(refused(assignment.batch_id, reason));
                        continue;
                    }
                },
            };
            response.arrays.push(self.reconcile(run, assignment).await);
        }
        response
    }

    fn concurrency(&self, spec: &TaskArraySpec) -> u32 {
        let default = self.config.default_concurrency.max(1);
        spec.per_node_concurrency
            .map_or(default, |cap| cap.min(default).max(1))
    }

    /// Whether this node may run the array's binary at all.
    fn admit(&self, assignment: &ArrayAssignment) -> Result<(), String> {
        if !self.config.policy.is_binary_allowed(&assignment.program) {
            return Err(format!(
                "{} isn't in this node's [process_workloads] allowed_binaries",
                assignment.program.display()
            ));
        }
        if self.config.policy.mount_isolation {
            // TODO(million-jobs M7): run tasks inside the mount namespace
            // the ordinary process workloads use.
            return Err(
                "task arrays don't run under [process_workloads] mount_isolation yet".to_string(),
            );
        }
        Ok(())
    }

    async fn open(&self, assignment: &ArrayAssignment) -> Result<ArrayRun, TaskArrayNodeError> {
        let directory = self.array_dir(assignment.batch_id);
        let spec = assignment.spec.clone();
        let (ledger, resumed) = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(directory.join("output"))?;
            let path = directory.join("ledger");
            let resumed = if path.exists() {
                group_by_chunk(&spec, task_ledger::replay(&path)?.records)
            } else {
                HashMap::new()
            };
            Ok::<_, TaskArrayNodeError>((Ledger::open(&path)?, resumed))
        })
        .await??;
        let (handle, _writer) = task_ledger::spawn_writer(ledger, self.config.group_commit);
        let config = PoolConfig {
            concurrency: self.concurrency(&assignment.spec),
            backoff_base: self.config.backoff.0,
            backoff_cap: self.config.backoff.1,
        };
        Ok(ArrayRun {
            pool: Arc::new(TaskPool::new(Arc::clone(&self.runner), config)),
            ledger: handle,
            cancel: CancellationToken::new(),
            chunks: HashMap::new(),
            finished: Arc::new(Mutex::new(HashMap::new())),
            resumed,
        })
    }

    /// Bring one array in line with the leader's view of it.
    async fn reconcile(&self, run: &mut ArrayRun, assignment: &ArrayAssignment) -> ArrayProgress {
        if assignment.stopping {
            run.cancel.cancel();
        }
        let held: BTreeMap<u32, u8> = assignment
            .held
            .iter()
            .map(|h| (h.chunk.0, h.attempt))
            .collect();

        // Stop chunks the leader took back (or re-granted at a new attempt).
        run.chunks.retain(|chunk, (attempt, cancel)| {
            let keep = held.get(chunk) == Some(attempt);
            if !keep {
                cancel.cancel();
            }
            keep
        });

        let mut finished = Vec::new();
        {
            let mut done = run.finished.lock().await;
            // Retired or taken back: forget it.
            done.retain(|(chunk, attempt), _| held.get(chunk) == Some(attempt));
            finished.extend(done.values().cloned());
        }
        finished.sort_by_key(|result| result.chunk);

        for (&chunk, &attempt) in &held {
            if run.chunks.contains_key(&chunk) {
                continue;
            }
            let cancel = run.cancel.child_token();
            run.chunks.insert(chunk, (attempt, cancel.clone()));
            let work = ChunkWork {
                batch_id: assignment.batch_id,
                spec: assignment.spec.clone(),
                chunk: ChunkId(chunk),
                grant_attempt: attempt,
                program: assignment.program.clone(),
                args: assignment.args.clone(),
                env: assignment.env.clone(),
            };
            let resumed = run.resumed.remove(&chunk).unwrap_or_default();
            tokio::spawn(run_chunk(
                Arc::clone(&run.pool),
                run.ledger.clone(),
                Arc::clone(&run.finished),
                self.array_dir(assignment.batch_id).join("output"),
                work,
                resumed,
                cancel,
            ));
        }

        let counters = run.pool.counters();
        ArrayProgress {
            batch_id: assignment.batch_id,
            slots: self.concurrency(&assignment.spec),
            refused: None,
            finished,
            counters: NodeArrayCounters {
                running: counters.running.load(Ordering::Relaxed),
                attempts_started: counters.attempts_started.load(Ordering::Relaxed),
                succeeded: counters.succeeded.load(Ordering::Relaxed),
                failed: counters.failed.load(Ordering::Relaxed),
                retried: counters.retried.load(Ordering::Relaxed),
            },
        }
    }

    /// Delete the directories of arrays the cluster no longer lists.
    async fn delete_unknown(&self, known: &[u64]) {
        let root = self.config.root.clone();
        let known = known.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            let entries = match std::fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u64>().ok())
                else {
                    continue;
                };
                if !known.contains(&id) {
                    std::fs::remove_dir_all(entry.path())?;
                }
            }
            Ok(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("bun: task arrays: cleanup failed: {error}"),
            Err(error) => eprintln!("bun: task arrays: cleanup task failed: {error}"),
        }
    }

    /// The durable outcome of every task this node ran for the array, in
    /// index order, at most `limit` rows; with `failed_only`, just the
    /// failures.
    pub async fn results(
        &self,
        batch_id: u64,
        failed_only: bool,
        limit: usize,
    ) -> Result<Vec<TaskResultRow>, TaskArrayNodeError> {
        let path = self.array_dir(batch_id).join("ledger");
        // One past the cap, so the leader can tell a capped answer apart.
        let limit = limit.min(MAX_RESULT_ROWS + 1);
        tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Err(TaskArrayNodeError::UnknownArray { batch_id });
            }
            let mut last: BTreeMap<u32, TaskRecord> = BTreeMap::new();
            for record in task_ledger::replay(&path)?.records {
                if record.outcome != TaskFinal::NotRun {
                    last.insert(record.index, record);
                }
            }
            Ok(last
                .into_values()
                .filter(|record| !failed_only || record.outcome == TaskFinal::Failed)
                .take(limit)
                .map(|record| TaskResultRow {
                    index: record.index,
                    attempts: record.attempts,
                    succeeded: record.outcome == TaskFinal::Succeeded,
                    exit_code: record.exit_code,
                    run_ms: record.run_ms,
                })
                .collect())
        })
        .await?
    }

    /// The captured output of a failed task: its first and last bytes.
    /// Only failed tasks keep output.
    pub async fn task_output(
        &self,
        batch_id: u64,
        index: u32,
    ) -> Result<Vec<u8>, TaskArrayNodeError> {
        let directory = self.array_dir(batch_id);
        let path = directory.join("output").join(index.to_string());
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if tokio::fs::metadata(&directory).await.is_err() {
                    Err(TaskArrayNodeError::UnknownArray { batch_id })
                } else {
                    Err(TaskArrayNodeError::NoOutput { index })
                }
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn refused(batch_id: u64, reason: String) -> ArrayProgress {
    ArrayProgress {
        batch_id,
        slots: 0,
        refused: Some(reason),
        finished: Vec::new(),
        counters: NodeArrayCounters::default(),
    }
}

/// Last terminal record per index, grouped by chunk.
fn group_by_chunk(spec: &TaskArraySpec, records: Vec<TaskRecord>) -> HashMap<u32, Vec<TaskRecord>> {
    let mut last: BTreeMap<u32, TaskRecord> = BTreeMap::new();
    for record in records {
        if record.outcome != TaskFinal::NotRun {
            last.insert(record.index, record);
        }
    }
    let mut chunks: HashMap<u32, Vec<TaskRecord>> = HashMap::new();
    for record in last.into_values() {
        if let Some(chunk) = spec.chunk_of(record.index) {
            chunks.entry(chunk.0).or_default().push(record);
        }
    }
    chunks
}

/// Run one chunk, make its records durable, keep failed tasks' output,
/// then publish the result for the next sync.
async fn run_chunk(
    pool: Arc<TaskPool<NodeRunner>>,
    ledger: LedgerHandle,
    finished: Arc<Mutex<HashMap<(u32, u8), ChunkResult>>>,
    output_dir: PathBuf,
    work: ChunkWork,
    resumed: Vec<TaskRecord>,
    cancel: CancellationToken,
) {
    let outcome = pool.resume_chunk(&work, resumed, &cancel).await;
    let outputs: Vec<(u32, CapturedOutput)> = outcome
        .records
        .iter()
        .filter_map(|record| record.output.clone().map(|output| (record.index, output)))
        .collect();
    if !outputs.is_empty() {
        let written =
            tokio::task::spawn_blocking(move || write_outputs(&output_dir, &outputs)).await;
        if !matches!(written, Ok(Ok(()))) {
            eprintln!(
                "bun: task array {}: couldn't keep failed tasks' output",
                work.batch_id
            );
        }
    }
    // The leader learns of the chunk only once its records are durable,
    // so `relish batch results` never misses a retired chunk. If the disk
    // fails the result still goes back: the leader's counts don't depend
    // on this node's ledger.
    if let Err(error) = ledger.append(outcome.records).await {
        eprintln!(
            "bun: task array {}: ledger write failed: {error}",
            work.batch_id
        );
    }
    // Keyed by attempt too: a taken-back run finishing late mustn't
    // overwrite the result of the chunk's newer grant on this node.
    finished
        .lock()
        .await
        .insert((work.chunk.0, work.grant_attempt), outcome.result);
}

fn write_outputs(directory: &Path, outputs: &[(u32, CapturedOutput)]) -> std::io::Result<()> {
    for (index, output) in outputs {
        let mut bytes = output.head.clone();
        if !output.tail.is_empty() {
            let kept = (output.head.len() + output.tail.len()) as u64;
            let skipped = output.total_bytes.saturating_sub(kept);
            if skipped > 0 {
                bytes
                    .extend_from_slice(format!("\n[... {skipped} bytes skipped ...]\n").as_bytes());
            }
            bytes.extend_from_slice(&output.tail);
        }
        std::fs::write(directory.join(index.to_string()), bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::task_executor::AttemptOutcome;
    use crate::meat::index_set::IndexRangeSet;

    const BINARY: &str = "/bin/sh";

    fn policy(allowed: &[&str], mount_isolation: bool) -> ProcessWorkloadsConfig {
        ProcessWorkloadsConfig {
            allowed_binaries: allowed.iter().map(PathBuf::from).collect(),
            mount_isolation,
            ..ProcessWorkloadsConfig::default()
        }
    }

    fn config(root: &Path, policy: ProcessWorkloadsConfig) -> TaskArrayNodeConfig {
        TaskArrayNodeConfig {
            root: root.to_path_buf(),
            policy,
            default_concurrency: 8,
            backoff: (Duration::from_millis(1), Duration::from_millis(2)),
            group_commit: GroupCommit {
                interval: Duration::from_millis(5),
                max_records: 4096,
            },
        }
    }

    fn fake_node(root: &Path, runner: FakeRunner) -> TaskArrayNode {
        TaskArrayNode::new(
            config(root, policy(&[BINARY], false)),
            NodeRunner::Fake(runner),
        )
    }

    fn assignment(batch_id: u64, count: u32, held: &[(u32, u8)]) -> ArrayAssignment {
        ArrayAssignment {
            batch_id,
            spec: TaskArraySpec {
                chunk_size: 10,
                ..TaskArraySpec::with_count(count)
            },
            program: PathBuf::from(BINARY),
            args: vec!["-c".to_string(), "exit 0".to_string()],
            env: Vec::new(),
            held: held
                .iter()
                .map(|&(chunk, attempt)| HeldChunk {
                    chunk: ChunkId(chunk),
                    attempt,
                })
                .collect(),
            stopping: false,
        }
    }

    fn request(arrays: Vec<ArrayAssignment>) -> NodeSyncRequest {
        NodeSyncRequest {
            known: arrays.iter().map(|a| a.batch_id).collect(),
            arrays,
        }
    }

    /// Sync until the node reports `chunks` finished chunks, or fail.
    async fn sync_until_finished(
        node: &TaskArrayNode,
        request: &NodeSyncRequest,
        chunks: usize,
    ) -> ArrayProgress {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let progress = node.sync(request).await.arrays.remove(0);
            if progress.finished.len() >= chunks {
                return progress;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "only {} of {chunks} chunks finished: {progress:?}",
                progress.finished.len()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn held_chunks_run_and_are_reported_at_their_attempt() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let sync = request(vec![assignment(1, 25, &[(0, 1), (2, 3)])]);
        let progress = sync_until_finished(&node, &sync, 2).await;
        assert_eq!(progress.slots, 8);
        assert_eq!(progress.refused, None);
        let finished: Vec<(u32, u8, u32)> = progress
            .finished
            .iter()
            .map(|r| (r.chunk.0, r.attempt, r.succeeded))
            .collect();
        assert_eq!(finished, vec![(0, 1, 10), (2, 3, 5)]);
        assert_eq!(progress.counters.succeeded, 15);
    }

    #[tokio::test]
    async fn a_finished_chunk_is_reported_until_the_leader_retires_it() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let sync = request(vec![assignment(1, 20, &[(0, 1)])]);
        sync_until_finished(&node, &sync, 1).await;
        // Still held: reported again, in case the leader missed it.
        assert_eq!(node.sync(&sync).await.arrays[0].finished.len(), 1);
        // Retired: the leader stops listing it, and the node forgets it.
        let retired = request(vec![assignment(1, 20, &[])]);
        assert!(node.sync(&retired).await.arrays[0].finished.is_empty());
    }

    #[tokio::test]
    async fn a_result_for_an_older_attempt_is_never_reported() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        sync_until_finished(&node, &request(vec![assignment(1, 20, &[(0, 1)])]), 1).await;
        // The leader re-granted chunk 0 at attempt 2: only that run counts.
        let regranted = request(vec![assignment(1, 20, &[(0, 2)])]);
        let progress = sync_until_finished(&node, &regranted, 1).await;
        assert_eq!(progress.finished.len(), 1);
        assert_eq!(progress.finished[0].attempt, 2);
    }

    #[tokio::test]
    async fn a_binary_off_the_allowlist_gets_no_slots() {
        let root = tempfile::tempdir().unwrap();
        let node = TaskArrayNode::new(
            config(root.path(), policy(&["/usr/bin/true"], false)),
            NodeRunner::Fake(FakeRunner::always_succeeds()),
        );
        let progress = node
            .sync(&request(vec![assignment(1, 20, &[(0, 1)])]))
            .await
            .arrays
            .remove(0);
        assert_eq!(progress.slots, 0);
        assert!(progress.refused.unwrap().contains("allowed_binaries"));
        assert!(progress.finished.is_empty());
    }

    #[tokio::test]
    async fn mount_isolation_keeps_task_arrays_off_the_node() {
        let root = tempfile::tempdir().unwrap();
        let node = TaskArrayNode::new(
            config(root.path(), policy(&[BINARY], true)),
            NodeRunner::Fake(FakeRunner::always_succeeds()),
        );
        let progress = node
            .sync(&request(vec![assignment(1, 20, &[])]))
            .await
            .arrays
            .remove(0);
        assert_eq!(progress.slots, 0);
        assert!(progress.refused.unwrap().contains("mount_isolation"));
    }

    #[tokio::test]
    async fn per_node_concurrency_caps_the_slots() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let mut capped = assignment(1, 20, &[]);
        capped.spec.per_node_concurrency = Some(3);
        let progress = node.sync(&request(vec![capped])).await.arrays.remove(0);
        assert_eq!(progress.slots, 3);
    }

    #[tokio::test]
    async fn stopping_reports_held_chunks_with_unrun_tasks() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(
            root.path(),
            FakeRunner::new(Duration::from_secs(30), |_| AttemptOutcome::Exited {
                code: 0,
            }),
        );
        let mut array = assignment(1, 20, &[(0, 1), (1, 1)]);
        node.sync(&request(vec![array.clone()])).await;
        array.stopping = true;
        let progress = sync_until_finished(&node, &request(vec![array]), 2).await;
        let not_run: u32 = progress.finished.iter().map(|r| r.not_run).sum();
        assert_eq!(not_run, 20);
    }

    #[tokio::test]
    async fn a_restarted_node_runs_only_what_its_ledger_lacks() {
        let root = tempfile::tempdir().unwrap();
        let sync = request(vec![assignment(1, 20, &[(0, 1)])]);
        {
            let first = fake_node(root.path(), FakeRunner::always_succeeds());
            sync_until_finished(&first, &sync, 1).await;
        }
        let second = fake_node(root.path(), FakeRunner::always_succeeds());
        let progress = sync_until_finished(&second, &sync, 1).await;
        assert_eq!(progress.finished[0].succeeded, 10);
        assert_eq!(progress.counters.attempts_started, 0, "nothing ran twice");
    }

    #[tokio::test]
    async fn files_stay_while_the_cluster_lists_the_array_and_go_after() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(
            root.path(),
            FakeRunner::new(Duration::ZERO, |task| AttemptOutcome::Exited {
                code: i32::from(task.index == 3),
            }),
        );
        sync_until_finished(&node, &request(vec![assignment(1, 10, &[(0, 1)])]), 1).await;

        // Finished: no longer assigned, but still known.
        node.sync(&NodeSyncRequest {
            known: vec![1],
            arrays: Vec::new(),
        })
        .await;
        let failed = node.results(1, true, 100).await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].index, 3);
        assert_eq!(failed[0].exit_code, Some(1));
        assert_eq!(node.results(1, false, 100).await.unwrap().len(), 10);
        assert_eq!(node.results(1, false, 4).await.unwrap().len(), 4);

        // Pruned: the directory goes.
        node.sync(&NodeSyncRequest::default()).await;
        assert!(!node.array_dir(1).exists());
        assert!(matches!(
            node.results(1, false, 100).await,
            Err(TaskArrayNodeError::UnknownArray { batch_id: 1 })
        ));
    }

    #[tokio::test]
    async fn a_failed_process_keeps_its_output() {
        let root = tempfile::tempdir().unwrap();
        let node = TaskArrayNode::new(
            config(root.path(), policy(&[BINARY], false)),
            NodeRunner::Process(ProcessRunner::default()),
        );
        let mut array = assignment(4, 3, &[(0, 1)]);
        array.spec.max_attempts = 1;
        array.args = vec![
            "-c".to_string(),
            "echo task {index}; test {index} -ne 1".to_string(),
        ];
        let progress = sync_until_finished(&node, &request(vec![array]), 1).await;
        assert_eq!(
            progress.finished[0].failed_indices,
            IndexRangeSet::from_range(1..=1)
        );
        let output = node.task_output(4, 1).await.unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "task 1\n");
        assert!(matches!(
            node.task_output(4, 0).await,
            Err(TaskArrayNodeError::NoOutput { index: 0 })
        ));
        assert!(matches!(
            node.task_output(5, 0).await,
            Err(TaskArrayNodeError::UnknownArray { batch_id: 5 })
        ));
    }
}

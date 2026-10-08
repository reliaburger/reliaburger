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
use tokio::sync::{Mutex, Semaphore};
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
pub const MAX_RESULT_ROWS: usize = 1000;

/// Tasks examined by a detail page, even when its failure filter returns no rows.
pub const RESULT_PAGE_SPAN: u32 = 4096;

/// One chunk the leader says this node holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldChunk {
    /// The chunk.
    pub chunk: ChunkId,
    /// Its grant attempt; a result is only good for this attempt.
    pub attempt: u64,
}

/// What the leader tells a node about one running array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArrayAssignment {
    /// Owned container template; host assignments retain the allowlist gate.
    pub template: Option<Box<crate::config::job::JobSpec>>,
    /// Per-task resource request; a queued chunk does not reserve this multiplied by its size.
    pub resources: crate::meat::Resources,
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
    /// Whether a missing outcome may be executed again after worker restart.
    pub replay_unknown: bool,
}

/// Orders cluster recovery, leadership and committed assignment revisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ControlVersion {
    /// Disaster recovery epoch.
    pub epoch: u64,
    /// Raft leadership term.
    pub term: u64,
    /// Applied log index of the assignment snapshot.
    pub index: u64,
}

/// The leader's sync call to one node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeSyncRequest {
    /// Persistent fence for delayed control messages.
    pub version: ControlVersion,
    /// Every array the cluster still keeps. A node deletes the files of
    /// any array not listed here.
    pub known: Vec<u64>,
    /// The running arrays, with this node's share of each.
    pub arrays: Vec<ArrayAssignment>,
}

/// Counters for one array on one node, since the node started it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeArrayCounters {
    /// Executor callers, including preparation and resource wait.
    pub running: u64,
    /// Last observed verified command starts awaiting positive cleanup.
    /// Absent for runtimes without command-level start receipts.
    pub active_commands: Option<u64>,
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
    /// Grant that produced this outcome.
    pub grant_attempt: u64,
    /// The task's index.
    pub index: u32,
    /// Attempts made.
    pub attempts: u8,
    /// Whether it succeeded.
    pub succeeded: bool,
    /// Cancelled execution with no accepted success or terminal failure.
    #[serde(default)]
    pub not_run: bool,
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
    /// Execute containers with the node's configured owned runtime.
    Owned(Box<super::task_runtime::OwnedRunner<crate::grill::AnyGrill>>),
    /// Spawn host processes.
    Process(ProcessRunner),
    /// Compute outcomes without processes (tests and benchmarks).
    Fake(FakeRunner),
}

impl TaskRunner for NodeRunner {
    async fn active_commands(
        &self,
        batch_id: u64,
        template: Option<&crate::config::job::JobSpec>,
    ) -> Option<u64> {
        match self {
            Self::Owned(runner) => runner.active_commands(batch_id, template).await,
            Self::Process(_) | Self::Fake(_) => None,
        }
    }
    fn owns_admission(&self, task: &TaskInvocation) -> bool {
        matches!(self, Self::Owned(runner) if runner.owns_admission(task))
    }
    async fn run(
        &self,
        task: &TaskInvocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Attempt {
        match self {
            Self::Owned(runner) => runner.run(task, timeout, cancel).await,
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
    /// Safety cap per node and array, further bounded by the shared budget.
    pub default_concurrency: u32,
    /// Retry backoff base and cap.
    pub backoff: (Duration, Duration),
    /// Ledger group-commit policy.
    pub group_commit: GroupCommit,
}

impl TaskArrayNodeConfig {
    /// The production settings under `data_dir`.
    pub fn for_data_dir(data_dir: &Path, policy: ProcessWorkloadsConfig) -> Self {
        // A CPU fraction can admit several attempts per core. The shared
        // resource budget, plus this explicit safety cap, bounds execution.
        let safety_cap = 256usize;
        let defaults = PoolConfig::with_concurrency(1);
        Self {
            root: data_dir.join(TASK_ARRAYS_DIR),
            policy,
            default_concurrency: u32::try_from(safety_cap).unwrap_or(u32::MAX),
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
    chunks: HashMap<u32, (u64, CancellationToken)>,
    /// Results of finished chunks, by chunk id and attempt.
    finished: Arc<Mutex<HashMap<(u32, u64), ChunkResult>>>,
    /// Records read back from an earlier run's ledger, by chunk, used
    /// once when that chunk starts again.
    resumed: HashMap<u32, Vec<TaskRecord>>,
    highest: BTreeMap<u32, u64>,
    failure: Arc<Mutex<Option<String>>>,
}

enum ControlCheck {
    Accepted,
    Stale,
    RecoveryRequired,
}

/// The node's task-array executor.
pub struct TaskArrayNode {
    config: TaskArrayNodeConfig,
    runner: Arc<NodeRunner>,
    arrays: Mutex<HashMap<u64, ArrayRun>>,
    slots: Arc<Semaphore>,
    budget: Arc<super::execution_budget::ExecutionBudget>,
    indexes: Mutex<HashMap<u64, Arc<super::task_result_index::TaskResultIndex>>>,
}

impl TaskArrayNode {
    /// An executor running attempts through `runner`.
    pub fn new(config: TaskArrayNodeConfig, runner: NodeRunner) -> Self {
        let slots = Arc::new(Semaphore::new(config.default_concurrency.max(1) as usize));
        let budget = super::execution_budget::ExecutionBudget::new(crate::meat::Resources::new(
            u64::from(config.default_concurrency.max(1)) * 1000,
            u64::MAX,
            0,
        ));
        if let NodeRunner::Owned(runner) = &runner {
            runner.set_budget(budget.clone());
        }
        Self {
            config,
            slots,
            budget,
            indexes: Mutex::new(HashMap::new()),
            runner: Arc::new(runner),
            arrays: Mutex::new(HashMap::new()),
        }
    }

    /// Use the exact admission ledger the node's supervisor uses for apps.
    pub fn with_budget(mut self, budget: Arc<super::execution_budget::ExecutionBudget>) -> Self {
        if let NodeRunner::Owned(runner) = self.runner.as_ref() {
            runner.set_budget(budget.clone());
        }
        self.budget = budget;
        self
    }

    /// The private runtime slot of an active singleton; absent for bulk or synthetic runners.
    pub(crate) fn singleton_runtime(
        &self,
        run: u64,
    ) -> Option<(
        crate::grill::AnyGrill,
        crate::grill::InstanceId,
        super::task_runtime::SingletonLogBinding,
    )> {
        match self.runner.as_ref() {
            NodeRunner::Owned(runner) => runner.singleton_runtime(run),
            _ => None,
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
        let path = self.config.root.join("control.json");
        let version = request.version;
        let checked = tokio::task::spawn_blocking(move || {
            let directory = path.parent().expect("control has parent");
            std::fs::create_dir_all(directory)?;
            // The directory entry itself must survive before a fence can.
            let parent = directory
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::File::open(parent)?.sync_all()?;
            let previous: ControlVersion = match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ControlVersion::default()
                }
                Err(error) => return Err(error),
            };
            if version.epoch < previous.epoch
                || (version.epoch == previous.epoch
                    && (version.term < previous.term || version.index < previous.index))
            {
                return Ok(ControlCheck::Stale);
            }
            let recovery = directory.join("recovery-required.json");
            match std::fs::read(&recovery) {
                Ok(_) => return Ok(ControlCheck::RecoveryRequired),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if version.epoch > previous.epoch {
                let has_old_arrays =
                    std::fs::read_dir(directory)?.try_fold(false, |found, entry| {
                        let entry = entry?;
                        Ok::<_, std::io::Error>(
                            found
                                || (entry.file_type()?.is_dir()
                                    && entry
                                        .file_name()
                                        .to_str()
                                        .is_some_and(|name| name.parse::<u64>().is_ok())),
                        )
                    })?;
                if has_old_arrays {
                    // Persist the refusal before cancellation. A restart or a later
                    // snapshot forgetting these arrays must never clear their fences.
                    persist_json(&recovery, &(previous, version))?;
                    return Ok(ControlCheck::RecoveryRequired);
                }
            }
            if version > previous {
                persist_json(&path, &version)?;
            }
            Ok::<_, std::io::Error>(ControlCheck::Accepted)
        })
        .await;
        if !matches!(checked, Ok(Ok(ControlCheck::Accepted))) {
            let recovery_required = matches!(checked, Ok(Ok(ControlCheck::RecoveryRequired)));
            if recovery_required {
                for run in arrays.values() {
                    run.cancel.cancel();
                }
            }
            let reason = if recovery_required {
                "recovery epoch changed with existing array data; archive old data and re-enrol this worker with fresh data"
            } else {
                "stale control version or unavailable persistent fence"
            };
            return NodeSyncResponse {
                arrays: request
                    .arrays
                    .iter()
                    .map(|a| refused(a.batch_id, reason.into()))
                    .collect(),
            };
        }
        let running: Vec<u64> = request.arrays.iter().map(|a| a.batch_id).collect();
        let finished_ids: Vec<u64> = arrays
            .keys()
            .copied()
            .filter(|id| !running.contains(id))
            .collect();
        for id in finished_ids {
            // Finished or forgotten: stop anything still going. The
            // directory stays while the cluster lists the array.
            if request.known.contains(&id) {
                if let Some(run) = arrays.get(&id) {
                    run.cancel.cancel();
                }
            } else if let Some(run) = arrays.remove(&id) {
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
        if let Some(template) = assignment.template.as_ref() {
            template.validate_runtime().map_err(str::to_owned)?;
        }
        if assignment
            .template
            .as_ref()
            .is_some_and(|template| template.env.values().any(|value| value.is_encrypted()))
            && !matches!(self.runner.as_ref(), NodeRunner::Owned(_))
        {
            return Err("encrypted templates require the owned runtime and namespace keys".into());
        }
        if assignment.resources.cpu_millicores == 0
            || assignment.resources.memory_bytes == 0
            || assignment.resources.gpus != 0
        {
            return Err(
                "tasks require positive CPU and memory requests; GPU task execution is unavailable"
                    .into(),
            );
        }
        if !self.budget.capacity().fits(&assignment.resources) {
            return Err("task requests exceed this node's allocatable resources".into());
        }
        if let Some(template) = assignment.template.as_ref()
            && template.runtime == crate::config::job::JobRuntime::SharedRunc
        {
            let profile = super::reusable_executor::ExecutorProfile::new(template)
                .map_err(|error| error.to_string())?;
            if !self.budget.capacity().fits(&profile.reservation) {
                return Err("reusable profile plus helper exceeds node capacity".into());
            }
            return match self.runner.as_ref() {
                NodeRunner::Owned(runner) if runner.supports_containers() => Ok(()),
                NodeRunner::Fake(_) => Ok(()),
                _ => Err("shared-runc requires the rootful owned Linux runtime".into()),
            };
        }
        if assignment
            .template
            .as_ref()
            .is_some_and(|t| t.image.is_some())
        {
            return match self.runner.as_ref() {
                NodeRunner::Owned(runner)
                    if runner.supports_containers()
                        || (assignment.spec.count == 1
                            && assignment.template.as_ref().is_some_and(|template| {
                                runner.supports_singleton_image(template)
                            })) =>
                {
                    Ok(())
                }
                NodeRunner::Fake(_) => Ok(()),
                _ => Err("container tasks require the rootful owned Linux runtime".into()),
            };
        }
        if let NodeRunner::Owned(runner) = self.runner.as_ref() {
            if !runner.supports_host() {
                return Err("host arrays require the owned process runtime; use image tasks on Linux container nodes".into());
            }
            if assignment
                .template
                .as_ref()
                .is_some_and(|t| t.cpu.is_some() || t.memory.is_some())
            {
                return Err(
                    "the host process runtime cannot enforce CPU or memory limits; use image tasks"
                        .into(),
                );
            }
        }
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
        let held: std::collections::BTreeSet<_> =
            assignment.held.iter().map(|h| h.chunk.0).collect();
        let (ledger, resumed, highest) = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(directory.join("output"))?;
            std::fs::File::open(directory.parent().expect("array has parent"))?.sync_all()?;
            let path = directory.join("ledger");
            let resumed = if path.exists() {
                let mut records = Vec::new();
                task_ledger::scan(&path, |record| {
                    if spec
                        .chunk_of(record.index)
                        .is_some_and(|c| held.contains(&c.0))
                    {
                        records.push(record);
                    }
                    Ok(())
                })?;
                group_by_chunk(&spec, records)
            } else {
                HashMap::new()
            };
            let fence = directory.join("grants.json");
            let highest = match std::fs::read(&fence) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(error) => return Err(error.into()),
            };
            Ok::<_, TaskArrayNodeError>((Ledger::open(&path)?, resumed, highest))
        })
        .await??;
        self.indexes
            .lock()
            .await
            .insert(assignment.batch_id, ledger.index());
        let (handle, _writer) = task_ledger::spawn_writer(ledger, self.config.group_commit);
        let config = PoolConfig {
            concurrency: self.concurrency(&assignment.spec),
            backoff_base: self.config.backoff.0,
            backoff_cap: self.config.backoff.1,
        };
        Ok(ArrayRun {
            pool: Arc::new(
                TaskPool::with_node_slots(
                    Arc::clone(&self.runner),
                    config,
                    Arc::clone(&self.slots),
                )
                .with_budget(Arc::clone(&self.budget), assignment.resources),
            ),
            ledger: handle,
            cancel: CancellationToken::new(),
            chunks: HashMap::new(),
            finished: Arc::new(Mutex::new(HashMap::new())),
            resumed,
            highest,
            failure: Arc::new(Mutex::new(None)),
        })
    }

    /// Bring one array in line with the leader's view of it.
    async fn reconcile(&self, run: &mut ArrayRun, assignment: &ArrayAssignment) -> ArrayProgress {
        if let Some(reason) = run.failure.lock().await.clone() {
            return refused(assignment.batch_id, reason);
        }
        if assignment.stopping {
            run.cancel.cancel();
        }
        let mut held: BTreeMap<u32, u64> = assignment
            .held
            .iter()
            .map(|h| (h.chunk.0, h.attempt))
            .collect();
        let mut changed = false;
        for (&chunk, attempt) in &mut held {
            let highest = run.highest.entry(chunk).or_insert(0);
            if *attempt > *highest {
                *highest = *attempt;
                changed = true;
            } else {
                *attempt = *highest;
            }
        }
        if changed {
            run.pool.counters().unknown.store(false, Ordering::Relaxed);
            let path = self.array_dir(assignment.batch_id).join("grants.json");
            let highest = run.highest.clone();
            let persisted =
                tokio::task::spawn_blocking(move || persist_json(&path, &highest)).await;
            if !matches!(persisted, Ok(Ok(()))) {
                let reason = "cannot persist the grant fence".to_string();
                *run.failure.lock().await = Some(reason.clone());
                run.cancel.cancel();
                return refused(assignment.batch_id, reason);
            }
        }

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
            if !assignment.replay_unknown {
                let complete = run.resumed.get(&chunk).is_some_and(|records| {
                    assignment
                        .spec
                        .chunk_range(ChunkId(chunk))
                        .is_some_and(|range| {
                            let expected = usize::try_from(range.end() - range.start() + 1)
                                .unwrap_or(usize::MAX);
                            let unique: std::collections::BTreeSet<_> = records
                                .iter()
                                .filter(|record| record.grant_attempt == attempt)
                                .map(|record| record.index)
                                .collect();
                            unique.len() == expected
                        })
                });
                let marker = self
                    .array_dir(assignment.batch_id)
                    .join(format!("started-{chunk}-{attempt}"));
                let authorised =
                    tokio::task::spawn_blocking(move || match std::fs::metadata(&marker) {
                        Ok(_) => Ok(complete),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            crate::sesame::identity::atomic_write_mode(
                                &marker,
                                b"started",
                                Some(0o600),
                            )?;
                            Ok(true)
                        }
                        Err(error) => Err(error),
                    })
                    .await;
                if !matches!(authorised, Ok(Ok(true))) {
                    return refused(assignment.batch_id, "unknown execution or unavailable launch fence; acknowledged replay required".into());
                }
            }
            let cancel = run.cancel.child_token();
            run.chunks.insert(chunk, (attempt, cancel.clone()));
            let work = ChunkWork {
                template: assignment.template.clone(),
                batch_id: assignment.batch_id,
                spec: assignment.spec.clone(),
                chunk: ChunkId(chunk),
                grant_attempt: attempt,
                replay_unknown: assignment.replay_unknown,
                program: assignment.program.clone(),
                args: assignment.args.clone(),
                env: assignment.env.clone(),
            };
            let resumed = run
                .resumed
                .remove(&chunk)
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.grant_attempt == attempt)
                .collect();
            tokio::spawn(run_chunk(
                Arc::clone(&run.pool),
                run.ledger.clone(),
                Arc::clone(&run.finished),
                Arc::clone(&run.failure),
                run.cancel.clone(),
                self.array_dir(assignment.batch_id).join("output"),
                work,
                resumed,
                cancel,
            ));
        }

        if !assignment.stopping && run.pool.counters().unknown.load(Ordering::Relaxed) {
            return refused(
                assignment.batch_id,
                "unknown execution outcome; acknowledged replay required".into(),
            );
        }
        let counters = run.pool.counters();
        ArrayProgress {
            batch_id: assignment.batch_id,
            slots: {
                let reusable = assignment.template.as_deref().filter(|template| {
                    template.runtime == crate::config::job::JobRuntime::SharedRunc
                });
                let resources = reusable
                    .and_then(|template| {
                        super::reusable_executor::ExecutorProfile::new(template).ok()
                    })
                    .map_or(assignment.resources, |profile| profile.reservation);
                let available = self.budget.available();
                let fits = (available.cpu_millicores / resources.cpu_millicores)
                    .min(available.memory_bytes / resources.memory_bytes);
                #[cfg(target_os = "linux")]
                let fits = match (reusable, self.runner.as_ref()) {
                    (Some(template), NodeRunner::Owned(runner)) => {
                        u64::from(runner.reusable_capacity(template).await)
                    }
                    _ => fits,
                };
                let cap = self
                    .concurrency(&assignment.spec)
                    .min(if reusable.is_some() {
                        super::reusable_executor::MAX_EXECUTOR_SLOTS as u32
                    } else {
                        u32::MAX
                    });
                cap.min(u32::try_from(fits).unwrap_or(u32::MAX))
                    .saturating_add(
                        u32::try_from(counters.running.load(Ordering::Relaxed)).unwrap_or(u32::MAX),
                    )
                    .min(cap)
            },
            refused: None,
            finished,
            counters: NodeArrayCounters {
                running: counters.running.load(Ordering::Relaxed),
                active_commands: self
                    .runner
                    .active_commands(assignment.batch_id, assignment.template.as_deref())
                    .await,
                attempts_started: counters.attempts_started.load(Ordering::Relaxed),
                succeeded: counters.succeeded.load(Ordering::Relaxed),
                failed: counters.failed.load(Ordering::Relaxed),
                retried: counters.retried.load(Ordering::Relaxed),
            },
        }
    }

    /// Delete the directories of arrays the cluster no longer lists.
    async fn delete_unknown(&self, known: &[u64]) {
        self.indexes.lock().await.retain(|id, _| known.contains(id));
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
        self.results_page(batch_id, failed_only, limit, 0, u32::MAX)
            .await
    }

    /// Read one bounded, indexed range. Readers share the writer's database.
    pub async fn results_page(
        &self,
        batch_id: u64,
        failed_only: bool,
        limit: usize,
        start: u32,
        end: u32,
    ) -> Result<Vec<TaskResultRow>, TaskArrayNodeError> {
        let mut indexes = self.indexes.lock().await;
        let index = match indexes.entry(batch_id) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let path = self.array_dir(batch_id).join("ledger");
                if !path.exists() {
                    return Err(TaskArrayNodeError::UnknownArray { batch_id });
                }
                let index_path = path.with_extension("index.redb");
                if !index_path.exists() {
                    return Err(std::io::Error::other(
                        "result index unavailable; recover the worker ledger before reading detail",
                    )
                    .into());
                }
                let index = tokio::task::spawn_blocking(move || {
                    let index = super::task_result_index::TaskResultIndex::open(
                        &path.with_extension("index.redb"),
                    )?;
                    // Accepted outcomes already required this index's durable
                    // commit. Recovery rebuilds active writers; a read never
                    // scans the historical ledger merely to open a valid index.
                    Ok::<_, LedgerError>(index)
                })
                .await??;
                entry.insert(index).clone()
            }
        };
        drop(indexes);
        let limit = limit.min(MAX_RESULT_ROWS + 1);
        tokio::task::spawn_blocking(move || {
            let records = index.page(start, end, failed_only, limit)?;
            Ok(records
                .into_iter()
                .map(|record| TaskResultRow {
                    grant_attempt: record.grant_attempt,
                    index: record.index,
                    attempts: record.attempts,
                    succeeded: record.outcome == TaskFinal::Succeeded,
                    not_run: record.outcome == TaskFinal::NotRun,
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
        let row = self
            .results_page(batch_id, false, 1, index, index.saturating_add(1))
            .await?
            .into_iter()
            .next()
            .ok_or(TaskArrayNodeError::NoOutput { index })?;
        if row.not_run {
            return Err(TaskArrayNodeError::NoOutput { index });
        }
        self.task_output_grant(batch_id, index, row.grant_attempt)
            .await
    }

    /// Output from exactly the winning grant, never an older task execution.
    pub async fn task_output_grant(
        &self,
        batch_id: u64,
        index: u32,
        grant: u64,
    ) -> Result<Vec<u8>, TaskArrayNodeError> {
        let directory = self.array_dir(batch_id);
        let path = directory.join("output").join(format!("{index}-{grant}"));
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

fn persist_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    std::fs::File::open(path.parent().expect("fence has parent"))?.sync_all()
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
#[allow(clippy::too_many_arguments)]
async fn run_chunk(
    pool: Arc<TaskPool<NodeRunner>>,
    ledger: LedgerHandle,
    finished: Arc<Mutex<HashMap<(u32, u64), ChunkResult>>>,
    failure: Arc<Mutex<Option<String>>>,
    array_cancel: CancellationToken,
    output_dir: PathBuf,
    work: ChunkWork,
    resumed: Vec<TaskRecord>,
    cancel: CancellationToken,
) {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<TaskRecord>(1024);
    let stream_work = async {
        let result = async {
            let mut persisted = crate::meat::index_set::IndexRangeSet::new();
            while let Some(record) = receiver.recv().await {
                let mut records = vec![record];
                while records.len() < 4096 {
                    match receiver.try_recv() {
                        Ok(record) => records.push(record),
                        Err(_) => break,
                    }
                }
                let outputs: Vec<_> = records
                    .iter()
                    .filter_map(|r| r.output.clone().map(|o| (r.index, r.grant_attempt, o)))
                    .collect();
                if !outputs.is_empty() {
                    let directory = output_dir.clone();
                    tokio::task::spawn_blocking(move || write_outputs(&directory, &outputs))
                        .await??;
                }
                for record in &records {
                    persisted.insert(record.index);
                }
                ledger.append(records).await?;
            }
            Ok::<_, TaskArrayNodeError>(persisted)
        }
        .await;
        if let Err(error) = &result {
            *failure.lock().await = Some(format!("task durability unavailable: {error}"));
            array_cancel.cancel();
        }
        result
    };
    let (outcome, persisted) = tokio::join!(
        pool.resume_chunk_streaming(&work, resumed, &cancel, Some(sender)),
        stream_work
    );
    let durable = async {
        let persisted = persisted?;
        // Tasks cancelled before spawning have no stream record.
        let remaining: Vec<_> = outcome
            .records
            .iter()
            .filter(|r| !persisted.contains(r.index))
            .cloned()
            .collect();
        if !remaining.is_empty() {
            ledger.append(remaining).await?;
        }
        Ok::<_, TaskArrayNodeError>(())
    }
    .await;
    if let Err(error) = durable {
        *failure.lock().await = Some(format!("task durability unavailable: {error}"));
        array_cancel.cancel();
        eprintln!(
            "bun: task array {}: cannot acknowledge chunk: {error}",
            work.batch_id
        );
        return;
    }
    // Keyed by attempt too: a taken-back run finishing late mustn't
    // overwrite the result of the chunk's newer grant on this node.
    finished
        .lock()
        .await
        .insert((work.chunk.0, work.grant_attempt), outcome.result);
}

fn write_outputs(directory: &Path, outputs: &[(u32, u64, CapturedOutput)]) -> std::io::Result<()> {
    for (index, grant, output) in outputs {
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
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(directory.join(format!("{index}-{grant}")))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::File::open(directory)?.sync_all()
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

    fn assignment(batch_id: u64, count: u32, held: &[(u32, u64)]) -> ArrayAssignment {
        ArrayAssignment {
            template: None,
            batch_id,
            resources: crate::meat::Resources::new(1000, 64 << 20, 0),
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
            replay_unknown: true,
        }
    }

    #[tokio::test]
    async fn singleton_images_use_the_configured_owned_runtime_without_claiming_unsupported_limits()
    {
        let dir = tempfile::tempdir().unwrap();
        let mut task = assignment(1, 1, &[(0, 1)]);
        task.template = Some(Box::new(
            toml::from_str("image='proc-grill:image-ignored'\ncommand=['true']").unwrap(),
        ));
        let runner = super::super::task_runtime::OwnedRunner::new(crate::grill::AnyGrill::Process(
            crate::grill::ProcessGrill::new(),
        ));
        let node = TaskArrayNode::new(
            TaskArrayNodeConfig::for_data_dir(dir.path(), ProcessWorkloadsConfig::default()),
            NodeRunner::Owned(Box::new(runner)),
        );
        assert!(
            node.admit(&task).is_err(),
            "an image singleton must not fall back to the host backend"
        );
        task.template.as_mut().unwrap().cpu = Some(crate::config::types::ResourceRange {
            request: 100,
            limit: 100,
        });
        assert!(node.admit(&task).is_err());
        task.template.as_mut().unwrap().cpu = None;
        task.spec.count = 100;
        assert!(
            node.admit(&task).is_err(),
            "bulk containers still require the rootful Linux backend"
        );
    }

    fn request(arrays: Vec<ArrayAssignment>) -> NodeSyncRequest {
        NodeSyncRequest {
            version: ControlVersion::default(),
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
    async fn an_unfinished_conservative_grant_cannot_restart_without_replay() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let mut a = assignment(1, 10, &[]);
        a.replay_unknown = false;
        let req = request(vec![a.clone()]);
        assert!(node.sync(&req).await.arrays[0].refused.is_none());
        std::fs::write(node.array_dir(1).join("started-0-1"), b"started").unwrap();
        a.held = vec![HeldChunk {
            chunk: ChunkId(0),
            attempt: 1,
        }];
        let req = request(vec![a.clone()]);
        assert!(
            node.sync(&req).await.arrays[0]
                .refused
                .as_deref()
                .unwrap()
                .contains("unknown")
        );
        assert_eq!(
            node.arrays.lock().await[&1]
                .pool
                .counters()
                .attempts_started
                .load(Ordering::Relaxed),
            0
        );
        a.held[0].attempt = 2;
        let req = request(vec![a]);
        let progress = sync_until_finished(&node, &req, 1).await;
        assert_eq!(progress.finished[0].succeeded, 10);
    }

    #[tokio::test]
    async fn recovery_epoch_refuses_old_ledgers_persistently_without_deleting_them() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let mut old = request(vec![assignment(1, 10, &[(0, 5)])]);
        old.version = ControlVersion {
            epoch: 0,
            term: 7,
            index: 99,
        };
        sync_until_finished(&node, &old, 1).await;
        // The derived index can update shutdown metadata; compare authoritative files.
        let files: BTreeMap<_, _> = std::fs::read_dir(node.array_dir(1))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("ledger" | "grants.json")
                )
            })
            .map(|path| (path.clone(), std::fs::read(path).unwrap()))
            .collect();
        let mut restored = request(vec![assignment(1, 10, &[(0, 1)])]);
        restored.version = ControlVersion {
            epoch: 1,
            term: 1,
            index: 1,
        };
        let response = node.sync(&restored).await.arrays.remove(0);
        assert!(response.refused.unwrap().contains("recovery epoch"));
        assert!(response.finished.is_empty());
        drop(node);
        let restarted = fake_node(root.path(), FakeRunner::always_succeeds());
        // Even a later control snapshot forgetting the old submission must not erase its evidence.
        restarted
            .sync(&NodeSyncRequest {
                version: restored.version,
                ..Default::default()
            })
            .await;
        assert!(
            restarted.sync(&restored).await.arrays[0]
                .refused
                .as_ref()
                .unwrap()
                .contains("recovery epoch")
        );
        for (path, bytes) in files {
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "{} changed",
                path.display()
            );
        }
        let fresh = tempfile::tempdir().unwrap();
        let fresh_node = fake_node(fresh.path(), FakeRunner::always_succeeds());
        assert_eq!(
            sync_until_finished(&fresh_node, &restored, 1)
                .await
                .finished[0]
                .attempt,
            1
        );
    }

    #[tokio::test]
    async fn failed_grant_fence_keeps_refusing_after_the_disk_is_repaired() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        node.sync(&request(vec![assignment(1, 10, &[])])).await;
        let fence = node.array_dir(1).join("grants.json");
        std::fs::create_dir(&fence).unwrap();
        let grant = request(vec![assignment(1, 10, &[(0, 1)])]);
        let first = node.sync(&grant).await.arrays.remove(0);
        assert!(first.refused.unwrap().contains("grant fence"));
        std::fs::remove_dir(&fence).unwrap();
        let later = node.sync(&grant).await.arrays.remove(0);
        assert!(later.refused.unwrap().contains("grant fence"));
        assert!(later.finished.is_empty());
        assert_eq!(later.counters.attempts_started, 0);
    }

    #[tokio::test]
    async fn held_chunks_run_and_are_reported_at_their_attempt() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let sync = request(vec![assignment(1, 25, &[(0, 1), (2, 3)])]);
        let progress = sync_until_finished(&node, &sync, 2).await;
        assert_eq!(progress.slots, 8);
        assert_eq!(progress.refused, None);
        let finished: Vec<(u32, u64, u32)> = progress
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
    async fn reusable_advertisement_accounts_for_helper_overhead_and_pool_bound() {
        for (capacity, expected) in [
            (crate::meat::Resources::new(300, 96 << 20, 0), 2),
            (crate::meat::Resources::new(256_000, 16 << 30, 0), 32),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut options = config(root.path(), policy(&[BINARY], false));
            options.default_concurrency = 256;
            let node = TaskArrayNode::new(options, NodeRunner::Fake(FakeRunner::always_succeeds()))
                .with_budget(super::super::execution_budget::ExecutionBudget::new(
                    capacity,
                ));
            let mut array = assignment(1, 1000, &[]);
            array.resources = crate::meat::Resources::new(100, 32 << 20, 0);
            array.template = Some(Box::new(toml::from_str(
                "image='fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\nnamespace='tenant-a'\nruntime='shared-runc'\ncpu='100m'\nmemory='32Mi'"
            ).unwrap()));
            let progress = node.sync(&request(vec![array])).await.arrays.remove(0);
            assert_eq!(progress.refused, None);
            assert_eq!(
                progress.slots, expected,
                "advertisement must include helper requests and the bounded executor count"
            );
        }
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
            version: ControlVersion::default(),
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

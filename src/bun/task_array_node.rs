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
//! the ledger, its result index and the kept output of failed tasks (one
//! segment per chunk grant, see [`super::task_output`]). The node keeps it
//! while the cluster still lists the array, so `relish batch results` and
//! `relish batch logs` work after the array finishes, and deletes it once
//! the leader stops listing the array. The leader stops listing a finished
//! array an hour after it finishes, on a timer, whether or not anyone
//! submits more work.
//!
//! Task data counts toward disk pressure: when it grows past its share of
//! the filesystem, or less than a gibibyte of it is free, the node
//! advertises no free slots, so the leader grants it nothing new until
//! retention frees the space.

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

/// Most disk bytes a worker holds per finished task while the cluster still
/// lists its run: a 22-byte ledger record plus a 24-byte result-index slot,
/// rounded up for block headers and filesystem blocks. Kept failure output
/// comes on top, bounded per chunk. Everything goes once the run retires.
pub const STORAGE_BYTES_PER_TASK: u64 = 50;

/// Tasks examined by a detail page, even when its failure filter returns no rows.
pub const RESULT_PAGE_SPAN: u32 = 4096;

/// Largest leader sync call a node accepts. Every active run's template
/// rides in it, so it must cover the active-run cap at the largest template
/// (`control_state_stays_within_budget_at_the_global_cap` checks that).
pub const MAX_SYNC_REQUEST_BYTES: usize = 8 * 1024 * 1024;

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
    /// Labels for this node's job metrics: never a task index or run id.
    #[serde(default)]
    pub labels: crate::mayo::job_metrics::JobLabels,
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
    /// Start-lag histogram of pooled commands, in the summary's
    /// power-of-two millisecond buckets.
    #[serde(default)]
    pub start_lag_ms_counts: [u64; 16],
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
    /// Whether `refused` comes from this node's configuration or runtime
    /// (its allowlist, `mount_isolation`, what it can enforce), so it will
    /// refuse the array every time, rather than from a passing fault. The
    /// leader fails a run every node refuses this way (#777).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unrunnable: bool,
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
    #[error(
        "task {index} failed, but its output wasn't kept: a chunk keeps the output of its first {} failed tasks only; the exit code is in `relish batch results`",
        super::task_output::KEPT_FAILURES_PER_CHUNK
    )]
    OutputNotKept { index: u32 },
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("task array file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("busy: an earlier sync's disk work has not finished, so this node's disk may be stuck")]
    SyncBusy,
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

impl NodeRunner {
    /// Where to measure the CPU and memory of this run's attempts on this
    /// node right now. Empty for runners with nothing to measure.
    async fn usage_targets(
        &self,
        batch_id: u64,
        template: Option<&crate::config::job::JobSpec>,
    ) -> Vec<crate::mayo::job_metrics::UsageTarget> {
        match self {
            Self::Owned(runner) => runner.usage_targets(batch_id, template).await,
            Self::Process(_) | Self::Fake(_) => Vec::new(),
        }
    }
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
    async fn busy_slots(
        &self,
        batch_id: u64,
        template: Option<&crate::config::job::JobSpec>,
    ) -> Option<u64> {
        match self {
            Self::Owned(runner) => runner.busy_slots(batch_id, template).await,
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
    /// The node's audit log (`<data>/audit`), which counts towards
    /// task-data pressure alongside `root`. `None` counts task data alone.
    pub audit_dir: Option<PathBuf>,
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
            audit_dir: Some(data_dir.join(crate::bun::audit::AUDIT_DIRECTORY)),
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

/// The one sync allowed on this node's disk at a time (#770).
///
/// A sync holds it while it runs, and so does every blocking disk call the
/// sync starts. When the leader gives up on a sync and drops it, a call
/// stuck on the disk keeps running on its blocking thread, still holding
/// the claim, and later syncs are refused as busy instead of each starting
/// another thread that would hang on the same disk. The claim frees itself
/// when that call finally returns.
#[derive(Clone)]
struct SyncClaim {
    // Never read: the permit goes back to the gate when the last clone drops.
    _permit: Arc<tokio::sync::OwnedSemaphorePermit>,
}

impl SyncClaim {
    /// Take the claim, or fail at once if a sync or its disk work holds it.
    fn take(gate: &Arc<Semaphore>) -> Result<Self, TaskArrayNodeError> {
        let permit = Arc::clone(gate)
            .try_acquire_owned()
            .map_err(|_| TaskArrayNodeError::SyncBusy)?;
        Ok(Self {
            _permit: Arc::new(permit),
        })
    }

    /// Run blocking disk work on the blocking pool, holding the claim until
    /// the work returns, even if the sync awaiting it is dropped first.
    fn spawn_blocking<T, F>(&self, work: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let claim = self.clone();
        tokio::task::spawn_blocking(move || {
            let _claim = claim;
            work()
        })
    }
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
    telemetry: Arc<crate::mayo::job_metrics::JobTelemetry>,
    output_syncs: Arc<std::sync::atomic::AtomicU64>,
    /// fsyncs the control fence has cost, for [`Self::control_syncs`].
    control_syncs: Arc<std::sync::atomic::AtomicU64>,
    /// Whether this process has made `root`'s directory entry durable.
    root_durable: Arc<std::sync::atomic::AtomicBool>,
    /// One permit, for [`SyncClaim`].
    sync_gate: Arc<Semaphore>,
    /// Runs at the start of the control-fence check, so a test can make
    /// the disk hang there.
    #[cfg(test)]
    fence_probe: Option<Arc<dyn Fn() + Send + Sync>>,
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
            telemetry: Default::default(),
            output_syncs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            control_syncs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            root_durable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sync_gate: Arc::new(Semaphore::new(1)),
            runner: Arc::new(runner),
            arrays: Mutex::new(HashMap::new()),
            #[cfg(test)]
            fence_probe: None,
        }
    }

    /// Bounded node-local executor phase distributions for measurement tooling.
    #[cfg(target_os = "linux")]
    pub fn executor_timings(
        &self,
        runtime: crate::config::job::JobRuntime,
    ) -> Option<super::reusable_executor::timings::TimingSnapshot> {
        match self.runner.as_ref() {
            NodeRunner::Owned(runner) => runner.executor_timings(runtime),
            _ => None,
        }
    }

    /// Count this node's attempts into a telemetry shared with its metrics
    /// collection and `relish top`.
    pub fn with_telemetry(
        mut self,
        telemetry: Arc<crate::mayo::job_metrics::JobTelemetry>,
    ) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Job counters, running gauges and the latest usage on this node.
    pub fn telemetry(&self) -> &Arc<crate::mayo::job_metrics::JobTelemetry> {
        &self.telemetry
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

    /// How many fsyncs kept failure output has cost on this node.
    pub fn output_syncs(&self) -> u64 {
        self.output_syncs.load(Ordering::Relaxed)
    }

    /// How many fsyncs the control fence in [`Self::sync`] has cost on
    /// this node.
    pub fn control_syncs(&self) -> u64 {
        self.control_syncs.load(Ordering::Relaxed)
    }

    /// Whether task data and the audit log, or the filesystem holding them,
    /// are too full to take new grants. Blocking I/O.
    fn task_data_pressured(root: &Path, audit_dir: Option<&Path>) -> bool {
        let usage = match super::disk_pressure::FilesystemUsage::of(root) {
            Ok(usage) => usage,
            Err(error) => {
                eprintln!("bun: task arrays: cannot measure the data filesystem: {error}");
                return true;
            }
        };
        let bytes = super::disk_pressure::node_data_bytes(root, audit_dir);
        super::disk_pressure::task_data_pressured(
            bytes,
            usage,
            super::disk_pressure::TASK_DATA_MAX_PERCENT,
        )
    }

    /// Where this node keeps one array's files.
    pub fn array_dir(&self, batch_id: u64) -> PathBuf {
        self.config.root.join(batch_id.to_string())
    }

    /// Apply the leader's view and report progress. An array this node
    /// can't run is answered with zero slots and the reason.
    ///
    /// Fails as a whole only with [`TaskArrayNodeError::SyncBusy`], at
    /// once, while an earlier sync or the disk work it started is still
    /// running: a stuck disk then costs one blocking thread, not one per
    /// sync the leader gives up on.
    pub async fn sync(
        &self,
        request: &NodeSyncRequest,
    ) -> Result<NodeSyncResponse, TaskArrayNodeError> {
        let claim = SyncClaim::take(&self.sync_gate)?;
        let mut arrays = self.arrays.lock().await;
        let root = self.config.root.clone();
        let path = root.join("control.json");
        let version = request.version;
        let syncs = Arc::clone(&self.control_syncs);
        let root_durable = Arc::clone(&self.root_durable);
        #[cfg(test)]
        let probe = self.fence_probe.clone();
        let checked = claim
            .spawn_blocking(move || {
                #[cfg(test)]
                if let Some(probe) = probe {
                    probe();
                }
                let directory = root.as_path();
                let existed = directory.is_dir();
                std::fs::create_dir_all(directory)?;
                // The directory entry itself must survive before a fence can.
                // Once per process (a previous one may have crashed before its
                // flush) and whenever the directory had to be created: the
                // leader calls this every tick, and on macOS each fsync is a
                // whole-device flush (#809).
                if !existed || !root_durable.load(Ordering::Acquire) {
                    let parent = directory
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or(Path::new("."));
                    std::fs::File::open(parent)?.sync_all()?;
                    syncs.fetch_add(1, Ordering::Relaxed);
                    root_durable.store(true, Ordering::Release);
                }
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
                    // The fence file and its directory.
                    syncs.fetch_add(2, Ordering::Relaxed);
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
            return Ok(NodeSyncResponse {
                arrays: request
                    .arrays
                    .iter()
                    .map(|a| refused(a.batch_id, reason.into()))
                    .collect(),
            });
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
        self.delete_unknown(&claim, &request.known).await;
        self.telemetry.retain_runs(|id| running.contains(&id));
        let root = self.config.root.clone();
        let audit_dir = self.config.audit_dir.clone();
        let pressured = claim
            .spawn_blocking(move || Self::task_data_pressured(&root, audit_dir.as_deref()))
            .await
            .unwrap_or(true);

        let mut response = NodeSyncResponse::default();
        for assignment in &request.arrays {
            if let Err(reason) = self.admit(assignment) {
                response.arrays.push(ArrayProgress {
                    unrunnable: true,
                    ..refused(assignment.batch_id, reason)
                });
                continue;
            }
            // Too big for this node isn't too big for every node: a larger
            // one may join, so the run waits rather than failing.
            if let Err(reason) = self.fits(assignment) {
                response.arrays.push(refused(assignment.batch_id, reason));
                continue;
            }
            let run = match arrays.entry(assignment.batch_id) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => match self.open(&claim, assignment).await {
                    Ok(run) => entry.insert(run),
                    Err(error) => {
                        let reason = format!("can't open the task ledger: {error}");
                        response.arrays.push(refused(assignment.batch_id, reason));
                        continue;
                    }
                },
            };
            let mut progress = self.reconcile(&claim, run, assignment).await;
            if pressured {
                // Chunks already granted still run and report; the leader
                // just sees no room for more until retention frees space.
                progress.slots = 0;
            }
            response.arrays.push(progress);
        }
        Ok(response)
    }

    fn concurrency(&self, spec: &TaskArraySpec) -> u32 {
        let default = self.config.default_concurrency.max(1);
        spec.per_node_concurrency
            .map_or(default, |cap| cap.min(default).max(1))
    }

    /// Why this node can never run `assignment`, if it can't: the same
    /// check every sync makes, for the leader's admission of a run when
    /// this node is the only one that could take it.
    pub fn refusal(&self, assignment: &ArrayAssignment) -> Option<String> {
        self.admit(assignment).err()
    }

    /// Whether the array's tasks, with any executor helper, fit inside this
    /// node's allocatable resources at all.
    fn fits(&self, assignment: &ArrayAssignment) -> Result<(), String> {
        let capacity = self.budget.capacity();
        if !capacity.fits(&assignment.resources) {
            return Err("task requests exceed this node's allocatable resources".into());
        }
        let Some(template) = assignment.template.as_ref() else {
            return Ok(());
        };
        let helper = match (template.runtime, self.runner.as_ref()) {
            (crate::config::job::JobRuntime::SharedRunc, _) => {
                Some("reusable profile plus helper exceeds node capacity")
            }
            (_, NodeRunner::Owned(runner))
                if template.image.is_none() && runner.supports_host_limits() =>
            {
                Some("host profile plus helper exceeds node capacity")
            }
            _ => None,
        };
        if let Some(reason) = helper
            && let Ok(profile) = super::reusable_executor::ExecutorProfile::new(template)
            && !capacity.fits(&profile.reservation)
        {
            return Err(reason.into());
        }
        Ok(())
    }

    /// Whether this node may run the array's binary at all, whatever its
    /// size: a refusal here holds for every attempt ([`Self::fits`] covers
    /// size).
    fn admit(&self, assignment: &ArrayAssignment) -> Result<(), String> {
        if let Some(template) = assignment.template.as_ref() {
            template.validate_runtime().map_err(str::to_owned)?;
            // An allowlist or allow_from this node can't hold would leave
            // every attempt to fail at launch; refuse the array up front.
            let enforced = match self.runner.as_ref() {
                NodeRunner::Owned(runner) => runner.enforces_network_policy(template),
                NodeRunner::Fake(_) => true,
                NodeRunner::Process(_) => {
                    super::task_runtime::egress_allow(template).is_empty()
                        && super::task_runtime::allow_from(template)
                            .is_none_or(|sources| sources.is_empty())
                }
            };
            if !enforced {
                return Err(super::task_runtime::NETWORK_POLICY_UNENFORCEABLE.into());
            }
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
        if let Some(template) = assignment.template.as_ref()
            && template.runtime == crate::config::job::JobRuntime::SharedRunc
        {
            super::reusable_executor::ExecutorProfile::new(template)
                .map_err(|error| error.to_string())?;
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
                && !runner.supports_host_limits()
            {
                return Err(
                    "the host process runtime cannot enforce CPU or memory limits; use image tasks"
                        .into(),
                );
            }
        }
        if let NodeRunner::Owned(runner) = self.runner.as_ref()
            && runner.supports_host_limits()
            && let Some(template) = assignment.template.as_ref()
        {
            super::reusable_executor::ExecutorProfile::new(template)
                .map_err(|error| error.to_string())?;
        }
        if !self.config.policy.is_binary_allowed(&assignment.program) {
            return Err(format!(
                "{} isn't in this node's [process_workloads] allowed_binaries",
                assignment.program.display()
            ));
        }
        if self.config.policy.mount_isolation {
            // TODO(#714): run tasks inside the mount namespace
            // the ordinary process workloads use.
            return Err(
                "host jobs don't run under [process_workloads] mount_isolation yet \
                 (on by default on Linux); set mount_isolation = false to run them \
                 unisolated on the host, or give the job an image"
                    .to_string(),
            );
        }
        Ok(())
    }

    async fn open(
        &self,
        claim: &SyncClaim,
        assignment: &ArrayAssignment,
    ) -> Result<ArrayRun, TaskArrayNodeError> {
        let directory = self.array_dir(assignment.batch_id);
        let spec = assignment.spec.clone();
        let held: std::collections::BTreeSet<_> =
            assignment.held.iter().map(|h| h.chunk.0).collect();
        let (ledger, resumed, highest) = claim
            .spawn_blocking(move || {
                std::fs::create_dir_all(directory.join("output"))?;
                let parent = directory
                    .parent()
                    .ok_or_else(|| std::io::Error::other("array directory has no parent"))?;
                std::fs::File::open(parent)?.sync_all()?;
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
    async fn reconcile(
        &self,
        claim: &SyncClaim,
        run: &mut ArrayRun,
        assignment: &ArrayAssignment,
    ) -> ArrayProgress {
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
            let persisted = claim
                .spawn_blocking(move || persist_json(&path, &highest))
                .await;
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
                let authorised = claim
                    .spawn_blocking(move || match std::fs::metadata(&marker) {
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
                OutputSink {
                    directory: self.array_dir(assignment.batch_id).join("output"),
                    syncs: Arc::clone(&self.output_syncs),
                },
                work,
                resumed,
                cancel,
                (Arc::clone(&self.telemetry), assignment.labels.clone()),
            ));
        }

        if !assignment.stopping && run.pool.counters().unknown.load(Ordering::Relaxed) {
            return refused(
                assignment.batch_id,
                "unknown execution outcome; acknowledged replay required".into(),
            );
        }
        let counters = run.pool.counters();
        let active_commands = self
            .runner
            .active_commands(assignment.batch_id, assignment.template.as_deref())
            .await;
        let busy_slots = self
            .runner
            .busy_slots(assignment.batch_id, assignment.template.as_deref())
            .await;
        let usage_targets = self
            .runner
            .usage_targets(assignment.batch_id, assignment.template.as_deref())
            .await;
        self.telemetry.observe(
            assignment.batch_id,
            &assignment.labels,
            crate::mayo::job_metrics::AttemptCounts {
                started: counters.attempts_started.load(Ordering::Relaxed),
                succeeded: counters.succeeded.load(Ordering::Relaxed),
                failed: counters.failed.load(Ordering::Relaxed),
                retried: counters.retried.load(Ordering::Relaxed),
            },
            busy_slots
                .or(active_commands)
                .unwrap_or_else(|| counters.running.load(Ordering::Relaxed)),
            usage_targets,
        );
        ArrayProgress {
            batch_id: assignment.batch_id,
            slots: {
                let reusable = assignment.template.as_deref().filter(|template| {
                    template.runtime == crate::config::job::JobRuntime::SharedRunc ||
                        (template.runtime == crate::config::job::JobRuntime::Process &&
                         matches!(self.runner.as_ref(), NodeRunner::Owned(runner) if runner.supports_host_limits()))
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
                let cap = slot_cap(
                    self.concurrency(&assignment.spec),
                    assignment.spec.per_node_concurrency.is_some(),
                    reusable.is_some(),
                );
                // A pool-run caller counts as running while it still waits
                // for a slot or admission; only a slot with resources charged
                // is in use. Started commands alone would miss millisecond
                // commands that begin and end between samples.
                let busy = match (reusable, busy_slots) {
                    (Some(_), Some(held)) => held,
                    _ => counters.running.load(Ordering::Relaxed),
                };
                cap.min(u32::try_from(fits).unwrap_or(u32::MAX))
                    .saturating_add(u32::try_from(busy).unwrap_or(u32::MAX))
                    .min(cap)
            },
            refused: None,
            unrunnable: false,
            finished,
            counters: NodeArrayCounters {
                running: counters.running.load(Ordering::Relaxed),
                active_commands,
                attempts_started: counters.attempts_started.load(Ordering::Relaxed),
                succeeded: counters.succeeded.load(Ordering::Relaxed),
                failed: counters.failed.load(Ordering::Relaxed),
                retried: counters.retried.load(Ordering::Relaxed),
                start_lag_ms_counts: std::array::from_fn(|bucket| {
                    counters.start_lag_counts[bucket].load(Ordering::Relaxed)
                }),
            },
        }
    }

    /// Delete the directories of arrays the cluster no longer lists.
    async fn delete_unknown(&self, claim: &SyncClaim, known: &[u64]) {
        self.indexes.lock().await.retain(|id, _| known.contains(id));
        let root = self.config.root.clone();
        let known = known.to_vec();
        let result = claim
            .spawn_blocking(move || {
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
                let index_path = path.with_extension("index");
                if !index_path.exists() {
                    return Err(std::io::Error::other(
                        "result index unavailable; recover the worker ledger before reading detail",
                    )
                    .into());
                }
                let index = tokio::task::spawn_blocking(move || {
                    let index = super::task_result_index::TaskResultIndex::open(
                        &path.with_extension("index"),
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
    /// Only failed tasks keep output, and only the first
    /// [`super::task_output::KEPT_FAILURES_PER_CHUNK`] of each chunk.
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
        let output = directory.join("output");
        if let Some(bytes) =
            tokio::task::spawn_blocking(move || super::task_output::read(&output, index, grant))
                .await??
        {
            return Ok(bytes);
        }
        if tokio::fs::metadata(&directory).await.is_err() {
            return Err(TaskArrayNodeError::UnknownArray { batch_id });
        }
        // A failure at this grant with no output went past its chunk's share.
        let failed_here = self
            .results_page(batch_id, true, 1, index, index.saturating_add(1))
            .await
            .is_ok_and(|rows| rows.iter().any(|row| row.grant_attempt == grant));
        if failed_here {
            Err(TaskArrayNodeError::OutputNotKept { index })
        } else {
            Err(TaskArrayNodeError::NoOutput { index })
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
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("fence file has no parent directory"))?;
    std::fs::File::open(parent)?.sync_all()
}

fn refused(batch_id: u64, reason: String) -> ArrayProgress {
    ArrayProgress {
        batch_id,
        slots: 0,
        refused: Some(reason),
        unrunnable: false,
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

/// Where a chunk keeps its failed tasks' output.
struct OutputSink {
    directory: PathBuf,
    syncs: Arc<std::sync::atomic::AtomicU64>,
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
    sink: OutputSink,
    work: ChunkWork,
    resumed: Vec<TaskRecord>,
    cancel: CancellationToken,
    (telemetry, labels): (
        Arc<crate::mayo::job_metrics::JobTelemetry>,
        crate::mayo::job_metrics::JobLabels,
    ),
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
                    .filter_map(|r| r.output.as_ref().map(|o| (r.index, render_output(o))))
                    .collect();
                if !outputs.is_empty() {
                    let directory = sink.directory.clone();
                    let range = work.spec.chunk_range(work.chunk).unwrap_or(0..=u32::MAX);
                    let grant = work.grant_attempt;
                    let appended = tokio::task::spawn_blocking(move || {
                        super::task_output::append(&directory, &range, grant, &outputs)
                    })
                    .await??;
                    if appended.synced {
                        sink.syncs.fetch_add(1, Ordering::Relaxed);
                    }
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
    telemetry.record_durations(&labels, &outcome.result.duration_counts);
    // Keyed by attempt too: a taken-back run finishing late mustn't
    // overwrite the result of the chunk's newer grant on this node.
    finished
        .lock()
        .await
        .insert((work.chunk.0, work.grant_attempt), outcome.result);
}

/// The most attempts this node advertises for one array at once.
///
/// Reusable runtimes can't exceed their executor pool, and without an
/// explicit cap they stay below it (see `DEFAULT_REUSABLE_CONCURRENCY`).
fn slot_cap(concurrency: u32, explicit: bool, reusable: bool) -> u32 {
    use super::reusable_executor::{DEFAULT_REUSABLE_CONCURRENCY, MAX_EXECUTOR_SLOTS};
    match (reusable, explicit) {
        (false, _) => concurrency,
        (true, true) => concurrency.min(MAX_EXECUTOR_SLOTS as u32),
        (true, false) => concurrency.min(DEFAULT_REUSABLE_CONCURRENCY),
    }
}

/// A captured output as kept on disk: head, a skipped-bytes marker, tail.
fn render_output(output: &CapturedOutput) -> Vec<u8> {
    let mut bytes = output.head.clone();
    if !output.tail.is_empty() {
        let kept = (output.head.len() + output.tail.len()) as u64;
        let skipped = output.total_bytes.saturating_sub(kept);
        if skipped > 0 {
            bytes.extend_from_slice(format!("\n[... {skipped} bytes skipped ...]\n").as_bytes());
        }
        bytes.extend_from_slice(&output.tail);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_task_data_pressure_counts_the_audit_log() {
        let config = TaskArrayNodeConfig::for_data_dir(
            Path::new("/var/lib/reliaburger"),
            ProcessWorkloadsConfig::default(),
        );
        assert_eq!(
            config.audit_dir.as_deref(),
            Some(Path::new("/var/lib/reliaburger/audit"))
        );
    }
    use crate::bun::task_executor::AttemptOutcome;
    use crate::meat::index_set::IndexRangeSet;

    #[test]
    fn reusable_arrays_without_a_cap_stay_below_the_executor_pool() {
        use crate::bun::reusable_executor::{DEFAULT_REUSABLE_CONCURRENCY, MAX_EXECUTOR_SLOTS};
        assert!((DEFAULT_REUSABLE_CONCURRENCY as usize) < MAX_EXECUTOR_SLOTS);
        assert_eq!(slot_cap(256, false, true), DEFAULT_REUSABLE_CONCURRENCY);
        assert_eq!(slot_cap(8, false, true), 8);
        // An explicit cap is honoured up to the pool size.
        assert_eq!(slot_cap(32, true, true), MAX_EXECUTOR_SLOTS as u32);
        assert_eq!(slot_cap(256, true, true), MAX_EXECUTOR_SLOTS as u32);
        // Fresh containers are bounded only by the node budget.
        assert_eq!(slot_cap(256, false, false), 256);
    }

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
            audit_dir: None,
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
            labels: crate::mayo::job_metrics::JobLabels {
                namespace: "default".into(),
                definition: "render".into(),
                profile: "default".into(),
            },
        }
    }

    #[tokio::test]
    async fn templates_with_network_policy_need_a_node_that_enforces_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut task = assignment(1, 1, &[(0, 1)]);
        task.template = Some(Box::new(
            toml::from_str(
                "image='proc-grill:image-ignored'\ncommand=['true']\n[egress]\nallow=['192.0.2.1:443']",
            )
            .unwrap(),
        ));
        let runner = super::super::task_runtime::OwnedRunner::new(crate::grill::AnyGrill::Process(
            crate::grill::ProcessGrill::new(),
        ));
        let node = TaskArrayNode::new(
            TaskArrayNodeConfig::for_data_dir(dir.path(), ProcessWorkloadsConfig::default()),
            NodeRunner::Owned(Box::new(runner)),
        );
        let refused = node.admit(&task).unwrap_err();
        assert!(refused.contains("egress and allow_from"), "{refused}");
        let template = task.template.as_mut().unwrap();
        template.egress = None;
        template.firewall = Some(crate::config::app::FirewallSpec {
            allow_from: vec!["frontend/web".into()],
        });
        assert!(node.admit(&task).unwrap_err().contains("allow_from"));
    }

    #[tokio::test]
    async fn image_tasks_are_refused_on_a_process_only_node_with_or_without_limits() {
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
            let progress = node.sync(request).await.unwrap().arrays.remove(0);
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
        assert!(node.sync(&req).await.unwrap().arrays[0].refused.is_none());
        std::fs::write(node.array_dir(1).join("started-0-1"), b"started").unwrap();
        a.held = vec![HeldChunk {
            chunk: ChunkId(0),
            attempt: 1,
        }];
        let req = request(vec![a.clone()]);
        assert!(
            node.sync(&req).await.unwrap().arrays[0]
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
    async fn a_sync_flushes_the_disk_only_when_the_fence_changes() {
        // On macOS every fsync is a whole-device flush, and the leader
        // syncs each node every tick: an unchanged fence must cost none.
        let data = tempfile::tempdir().unwrap();
        let root = data.path().join("tasks");
        let node = fake_node(&root, FakeRunner::always_succeeds());
        let mut sync = request(Vec::new());
        sync.version = ControlVersion {
            epoch: 0,
            term: 1,
            index: 1,
        };
        node.sync(&sync).await.unwrap();
        // The new directory's entry, the fence file and its directory.
        assert_eq!(node.control_syncs(), 3);
        node.sync(&sync).await.unwrap();
        node.sync(&sync).await.unwrap();
        assert_eq!(
            node.control_syncs(),
            3,
            "an unchanged fence flushed the disk"
        );
        sync.version.index = 2;
        node.sync(&sync).await.unwrap();
        // A new fence rewrites its file; the directory entry is durable.
        assert_eq!(node.control_syncs(), 5);
        // A directory removed underneath the node is made durable again.
        std::fs::remove_dir_all(&root).unwrap();
        node.sync(&sync).await.unwrap();
        assert_eq!(node.control_syncs(), 8);
        assert!(root.join("control.json").exists());
    }

    /// A disk that hangs every read until it is released, counting the
    /// blocking threads that have entered it.
    #[derive(Default)]
    struct StuckDisk {
        entered: std::sync::atomic::AtomicUsize,
        released: std::sync::Mutex<bool>,
        wake: std::sync::Condvar,
    }

    impl StuckDisk {
        fn read(&self) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }

        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }
    }

    /// Releases the disk when a test ends, even by failing, so the
    /// runtime's shutdown doesn't wait forever on a hung blocking thread.
    struct Unstick(Arc<StuckDisk>);

    impl Drop for Unstick {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn on_stuck_disk(root: &Path) -> (TaskArrayNode, Arc<StuckDisk>) {
        let mut node = fake_node(root, FakeRunner::always_succeeds());
        let disk = Arc::new(StuckDisk::default());
        let hung = Arc::clone(&disk);
        node.fence_probe = Some(Arc::new(move || hung.read()));
        (node, disk)
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "never {what}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn syncs_the_leader_gives_up_on_a_stuck_disk_hold_one_blocking_thread() {
        let root = tempfile::tempdir().unwrap();
        let (node, disk) = on_stuck_disk(root.path());
        let _unstick = Unstick(Arc::clone(&disk));
        let sync = request(vec![assignment(1, 10, &[])]);
        // The leader's deadline drops the call mid-read, as a timed-out
        // HTTP request drops the handler.
        let first = tokio::time::timeout(Duration::from_millis(20), node.sync(&sync)).await;
        assert!(first.is_err(), "a sync finished on a stuck disk");
        wait_until("entered the stuck read", || disk.entered() == 1).await;
        for _ in 0..50 {
            let retry = tokio::time::timeout(Duration::from_millis(20), node.sync(&sync)).await;
            assert!(
                matches!(retry, Ok(Err(TaskArrayNodeError::SyncBusy))),
                "a retry was not refused as busy: {retry:?}"
            );
        }
        assert_eq!(disk.entered(), 1, "retries started more stuck reads");
        disk.release();
        // The stuck read finishes on its own, and the next sync runs.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let answer = loop {
            match node.sync(&sync).await {
                Err(TaskArrayNodeError::SyncBusy) => {}
                answer => break answer.unwrap(),
            }
            assert!(tokio::time::Instant::now() < deadline, "never recovered");
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        assert_eq!(answer.arrays[0].refused, None);
        assert_eq!(disk.entered(), 2);
    }

    #[tokio::test]
    async fn a_sync_still_running_its_disk_work_refuses_a_concurrent_one() {
        let root = tempfile::tempdir().unwrap();
        let (node, disk) = on_stuck_disk(root.path());
        let _unstick = Unstick(Arc::clone(&disk));
        let node = Arc::new(node);
        let sync = request(Vec::new());
        let running = {
            let (node, sync) = (Arc::clone(&node), sync.clone());
            tokio::spawn(async move { node.sync(&sync).await })
        };
        wait_until("entered the stuck read", || disk.entered() == 1).await;
        let concurrent = tokio::time::timeout(Duration::from_millis(20), node.sync(&sync)).await;
        assert!(
            matches!(concurrent, Ok(Err(TaskArrayNodeError::SyncBusy))),
            "a concurrent sync was not refused as busy: {concurrent:?}"
        );
        disk.release();
        assert!(running.await.unwrap().is_ok());
        assert!(node.sync(&sync).await.is_ok());
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
        let response = node.sync(&restored).await.unwrap().arrays.remove(0);
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
            .await
            .unwrap();
        assert!(
            restarted.sync(&restored).await.unwrap().arrays[0]
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
        node.sync(&request(vec![assignment(1, 10, &[])]))
            .await
            .unwrap();
        let fence = node.array_dir(1).join("grants.json");
        std::fs::create_dir(&fence).unwrap();
        let grant = request(vec![assignment(1, 10, &[(0, 1)])]);
        let first = node.sync(&grant).await.unwrap().arrays.remove(0);
        assert!(first.refused.unwrap().contains("grant fence"));
        std::fs::remove_dir(&fence).unwrap();
        let later = node.sync(&grant).await.unwrap().arrays.remove(0);
        assert!(later.refused.unwrap().contains("grant fence"));
        assert!(later.finished.is_empty());
        assert_eq!(later.counters.attempts_started, 0);
    }

    #[tokio::test]
    async fn a_node_counts_its_attempts_into_job_telemetry() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let sync = request(vec![assignment(1, 25, &[(0, 1), (2, 3)])]);
        sync_until_finished(&node, &sync, 2).await;
        // The counters in the response lag the finished chunks by one sync.
        node.sync(&sync).await.unwrap();
        let samples = node.telemetry().samples("node-a");
        let value = |name: &str| {
            samples
                .iter()
                .find(|sample| sample.key.name.as_str() == name)
                .map(|sample| sample.value)
        };
        assert_eq!(value("job_tasks_succeeded_total"), Some(15.0));
        assert_eq!(value("job_task_duration_ms_count"), Some(15.0));
        let labels = &samples[0].key.labels;
        assert_eq!(labels.get("definition").map(String::as_str), Some("render"));
        assert_eq!(labels.get("node").map(String::as_str), Some("node-a"));

        // Once the leader stops listing the run, nothing of it is running.
        node.sync(&request(Vec::new())).await.unwrap();
        assert!(node.telemetry().usage_targets().is_empty());
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
        assert_eq!(node.sync(&sync).await.unwrap().arrays[0].finished.len(), 1);
        // Retired: the leader stops listing it, and the node forgets it.
        let retired = request(vec![assignment(1, 20, &[])]);
        assert!(
            node.sync(&retired).await.unwrap().arrays[0]
                .finished
                .is_empty()
        );
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
    async fn tasks_too_big_for_this_node_are_refused_but_not_for_good() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let mut greedy = assignment(1, 20, &[]);
        greedy.resources.cpu_millicores = u64::MAX;
        let progress = node
            .sync(&request(vec![greedy]))
            .await
            .unwrap()
            .arrays
            .remove(0);
        assert_eq!(progress.slots, 0);
        assert!(
            !progress.unrunnable,
            "a bigger node may still take it, so the run must wait"
        );
        assert!(progress.refused.unwrap().contains("allocatable"));
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
            .unwrap()
            .arrays
            .remove(0);
        assert_eq!(progress.slots, 0);
        assert!(progress.unrunnable);
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
            .unwrap()
            .arrays
            .remove(0);
        assert_eq!(progress.slots, 0);
        assert!(
            progress.unrunnable,
            "the node's configuration refuses it every time"
        );
        assert!(progress.refused.unwrap().contains("mount_isolation"));
    }

    #[tokio::test]
    async fn per_node_concurrency_caps_the_slots() {
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        let mut capped = assignment(1, 20, &[]);
        capped.spec.per_node_concurrency = Some(3);
        let progress = node
            .sync(&request(vec![capped]))
            .await
            .unwrap()
            .arrays
            .remove(0);
        assert_eq!(progress.slots, 3);
    }

    #[tokio::test]
    async fn reusable_advertisement_accounts_for_helper_overhead_and_pool_bound() {
        use crate::bun::reusable_executor::{DEFAULT_REUSABLE_CONCURRENCY, MAX_EXECUTOR_SLOTS};
        for (capacity, explicit, expected) in [
            (crate::meat::Resources::new(300, 96 << 20, 0), None, 2),
            (
                crate::meat::Resources::new(256_000, 16 << 30, 0),
                None,
                DEFAULT_REUSABLE_CONCURRENCY,
            ),
            (
                crate::meat::Resources::new(256_000, 16 << 30, 0),
                Some(256),
                MAX_EXECUTOR_SLOTS as u32,
            ),
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
            array.spec.per_node_concurrency = explicit;
            array.template = Some(Box::new(toml::from_str(
                "image='fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\nnamespace='tenant-a'\nruntime='shared-runc'\ncpu='100m'\nmemory='32Mi'"
            ).unwrap()));
            let progress = node
                .sync(&request(vec![array]))
                .await
                .unwrap()
                .arrays
                .remove(0);
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
        node.sync(&request(vec![array.clone()])).await.unwrap();
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
        .await
        .unwrap();
        let failed = node.results(1, true, 100).await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].index, 3);
        assert_eq!(failed[0].exit_code, Some(1));
        assert_eq!(node.results(1, false, 100).await.unwrap().len(), 10);
        assert_eq!(node.results(1, false, 4).await.unwrap().len(), 4);

        // Pruned: the directory goes.
        node.sync(&NodeSyncRequest::default()).await.unwrap();
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

    fn failing_everywhere(
        root: &Path,
        count: u32,
        chunk_size: u32,
    ) -> (TaskArrayNode, ArrayAssignment) {
        let node = fake_node(
            root,
            FakeRunner::new(Duration::ZERO, |_| AttemptOutcome::Exited { code: 3 }),
        );
        let chunks: Vec<_> = (0..count.div_ceil(chunk_size)).map(|c| (c, 1)).collect();
        let mut array = assignment(9, count, &chunks);
        array.spec.chunk_size = chunk_size;
        array.spec.max_attempts = 1;
        (node, array)
    }

    #[tokio::test]
    async fn failure_output_is_kept_only_for_the_first_failures_of_a_chunk() {
        use super::super::task_output::KEPT_FAILURES_PER_CHUNK;
        let root = tempfile::tempdir().unwrap();
        let (node, array) = failing_everywhere(root.path(), 100, 100);
        let progress = sync_until_finished(&node, &request(vec![array]), 1).await;
        assert_eq!(progress.finished[0].failed_count, 100);

        let mut kept = 0;
        for index in 0..100 {
            match node.task_output(9, index).await {
                Ok(_) => kept += 1,
                Err(TaskArrayNodeError::OutputNotKept { index: i }) => assert_eq!(i, index),
                Err(other) => panic!("task {index}: {other}"),
            }
        }
        assert_eq!(
            kept, KEPT_FAILURES_PER_CHUNK,
            "the rest are counted, not kept"
        );
        let message = TaskArrayNodeError::OutputNotKept { index: 99 }.to_string();
        assert!(message.contains("wasn't kept"), "{message}");
        // Every failure still has its durable record and exit code.
        assert_eq!(node.results(9, true, 1000).await.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn failure_outputs_share_one_segment_and_one_fsync_per_batch() {
        use super::super::task_output::KEPT_FAILURES_PER_CHUNK;
        let root = tempfile::tempdir().unwrap();
        let (node, array) = failing_everywhere(root.path(), 400, 100);
        sync_until_finished(&node, &request(vec![array]), 4).await;

        let segments: Vec<_> = std::fs::read_dir(node.array_dir(9).join("output"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            segments.len(),
            4,
            "one file per chunk, not per task: {segments:?}"
        );
        // At most one fsync per batch, and never one per kept output.
        let syncs = node.output_syncs();
        assert!(
            (4..=4 * KEPT_FAILURES_PER_CHUNK as u64).contains(&syncs),
            "{syncs} output fsyncs"
        );
    }

    #[tokio::test]
    async fn retired_array_directory_is_deleted_without_a_new_registration() {
        use crate::meat::task_array_store::{TERMINAL_RETENTION_SECS, TaskArrayWrite, TaskArrays};
        let root = tempfile::tempdir().unwrap();
        let node = fake_node(root.path(), FakeRunner::always_succeeds());
        sync_until_finished(&node, &request(vec![assignment(1, 10, &[(0, 1)])]), 1).await;

        // The leader's view: the array finished at t=100 and nothing new arrives.
        let mut arrays = TaskArrays::default();
        let template: crate::config::job::JobSpec =
            toml::from_str("runtime='process'\nexec='/bin/sh'\ncommand=['-c','true']").unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Register {
                    name: "render".into(),
                    namespace: "default".into(),
                    template: Box::new(template),
                    spec: TaskArraySpec::with_count(10),
                    submitted_at_epoch_secs: 100,
                },
                || 1,
            )
            .unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Cancel {
                    batch_id: 1,
                    now_epoch_secs: 100,
                },
                || 0,
            )
            .unwrap();
        let known = |arrays: &TaskArrays| NodeSyncRequest {
            version: ControlVersion::default(),
            known: arrays.ids(),
            arrays: Vec::new(),
        };
        node.sync(&known(&arrays)).await.unwrap();
        assert!(
            node.array_dir(1).exists(),
            "kept while the cluster lists it"
        );

        // The leader's timer write is all it takes.
        let now = 101 + TERMINAL_RETENTION_SECS;
        assert!(arrays.has_expired(now, TERMINAL_RETENTION_SECS));
        arrays
            .apply(
                &TaskArrayWrite::Prune {
                    now_epoch_secs: now,
                    retention_secs: TERMINAL_RETENTION_SECS,
                },
                || 0,
            )
            .unwrap();
        node.sync(&known(&arrays)).await.unwrap();
        assert!(!node.array_dir(1).exists());
    }
}
